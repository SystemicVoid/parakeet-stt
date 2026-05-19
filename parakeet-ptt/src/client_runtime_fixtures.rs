use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::anyhow;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::app::{ClientPorts, DaemonConnection, DaemonConnector, HotkeyRuntime, HotkeySource};
use crate::audio_feedback::AudioFeedback;
use crate::config::ClientConfig;
use crate::hotkey::{HotkeyEvent, HotkeyIntent};
use crate::injector_runtime::{
    InjectionJob, InjectionJobRunner, InjectionRunError, InjectionRunOutput,
};
use crate::llm::{LlmAnswerer, LlmDelta, LlmDeltaStream, LlmProgress};
use crate::overlay_router::{OverlayEvent, OverlaySink};
use crate::protocol::{ClientMessage, ServerMessage};

type TestBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type RecordedLlmRequests = Arc<Mutex<Vec<(Uuid, String)>>>;

pub(crate) struct ClientRuntimeHarness {
    config: ClientConfig,
    ports: ClientPorts,
    controls: ClientRuntimeControls,
}

impl ClientRuntimeHarness {
    pub(crate) fn new(config: ClientConfig) -> Self {
        let (hotkey_tx, hotkey_rx) = mpsc::unbounded_channel();
        let (sent_tx, sent_rx) = mpsc::unbounded_channel();
        let (daemon_tx, daemon_rx) = mpsc::unbounded_channel();
        let (injection_runner, injections) = RecordingInjectionRunner::shared();
        let (overlay_sink, overlay_events) = RecordingOverlaySink::shared();
        let (llm_answerer, llm_requests) = TestLlmAnswerer::shared();
        let ports = ClientPorts::new(
            AudioFeedback::new(false, None, 0),
            Arc::new(TestDaemonConnector::new(TestDaemonConnection {
                sent: sent_tx,
                incoming: daemon_rx,
            })),
            injection_runner,
            Box::new(overlay_sink),
            None,
            Box::new(TestHotkeySource::new(hotkey_rx)),
            llm_answerer,
        );

        Self {
            config,
            ports,
            controls: ClientRuntimeControls {
                hotkey_tx,
                sent_rx,
                injections,
                overlay_events,
                daemon_tx,
                llm_requests,
            },
        }
    }

    pub(crate) fn into_parts(self) -> (ClientConfig, ClientPorts, ClientRuntimeControls) {
        (self.config, self.ports, self.controls)
    }
}

pub(crate) struct ClientRuntimeControls {
    hotkey_tx: mpsc::UnboundedSender<HotkeyEvent>,
    sent_rx: mpsc::UnboundedReceiver<ClientMessage>,
    injections: Arc<Mutex<Vec<String>>>,
    overlay_events: Arc<Mutex<Vec<OverlayEvent>>>,
    daemon_tx: mpsc::UnboundedSender<ServerMessage>,
    llm_requests: RecordedLlmRequests,
}

impl ClientRuntimeControls {
    pub(crate) fn send_hotkey_down(&self, intent: HotkeyIntent) {
        self.hotkey_tx
            .send(HotkeyEvent::Down { intent })
            .expect("test hotkey event should send");
    }

    pub(crate) fn send_hotkey_up(&self) {
        self.hotkey_tx
            .send(HotkeyEvent::Up)
            .expect("test hotkey event should send");
    }

    pub(crate) fn send_daemon_message(&self, message: ServerMessage) {
        self.daemon_tx
            .send(message)
            .expect("test daemon message should send");
    }

    pub(crate) async fn next_sent_message(&mut self, wait: Duration) -> ClientMessage {
        tokio::time::timeout(wait, self.sent_rx.recv())
            .await
            .expect("client message should be sent before timeout")
            .expect("sent-message channel should stay open")
    }

    pub(crate) fn recorded_injections(&self) -> Vec<String> {
        self.injections
            .lock()
            .expect("recorded injection lock should be available")
            .clone()
    }

    pub(crate) fn recorded_overlay_events(&self) -> Vec<OverlayEvent> {
        self.overlay_events
            .lock()
            .expect("recorded overlay lock should be available")
            .clone()
    }

    pub(crate) fn recorded_llm_requests(&self) -> Vec<(Uuid, String)> {
        self.llm_requests
            .lock()
            .expect("recorded LLM request lock should be available")
            .clone()
    }
}

pub(crate) struct RecordingInjectionRunner {
    seen: Arc<Mutex<Vec<String>>>,
}

impl RecordingInjectionRunner {
    pub(crate) fn shared() -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                seen: Arc::clone(&seen),
            }),
            seen,
        )
    }
}

impl InjectionJobRunner for RecordingInjectionRunner {
    fn run(&self, job: &InjectionJob) -> Result<InjectionRunOutput, InjectionRunError> {
        self.seen
            .lock()
            .expect("recording lock should be available")
            .push(job.text.to_string());
        Ok(InjectionRunOutput::default())
    }
}

pub(crate) struct RecordingOverlaySink {
    seen: Arc<Mutex<Vec<OverlayEvent>>>,
}

impl RecordingOverlaySink {
    pub(crate) fn shared() -> (Self, Arc<Mutex<Vec<OverlayEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                seen: Arc::clone(&seen),
            },
            seen,
        )
    }
}

impl OverlaySink for RecordingOverlaySink {
    fn on_overlay_event(&mut self, event: OverlayEvent) {
        self.seen
            .lock()
            .expect("overlay recording lock should be available")
            .push(event);
    }
}

struct TestHotkeySource {
    events: Option<mpsc::UnboundedReceiver<HotkeyEvent>>,
}

impl TestHotkeySource {
    fn new(events: mpsc::UnboundedReceiver<HotkeyEvent>) -> Self {
        Self {
            events: Some(events),
        }
    }
}

impl HotkeySource for TestHotkeySource {
    fn start(&mut self, _config: &ClientConfig) -> anyhow::Result<HotkeyRuntime> {
        Ok(HotkeyRuntime::new_for_tests(
            self.events
                .take()
                .expect("test hotkey source should only be started once"),
        ))
    }
}

struct TestDaemonConnector {
    connection: Mutex<Option<TestDaemonConnection>>,
}

impl TestDaemonConnector {
    fn new(connection: TestDaemonConnection) -> Self {
        Self {
            connection: Mutex::new(Some(connection)),
        }
    }
}

impl DaemonConnector for TestDaemonConnector {
    fn connect<'a>(
        &'a self,
        _config: &'a ClientConfig,
    ) -> TestBoxFuture<'a, anyhow::Result<Box<dyn DaemonConnection>>> {
        Box::pin(async move {
            let connection = self
                .connection
                .lock()
                .expect("test connection lock should be available")
                .take()
                .expect("test connector should only be connected once");
            Ok(Box::new(connection) as Box<dyn DaemonConnection>)
        })
    }
}

struct TestDaemonConnection {
    sent: mpsc::UnboundedSender<ClientMessage>,
    incoming: mpsc::UnboundedReceiver<ServerMessage>,
}

impl DaemonConnection for TestDaemonConnection {
    fn send<'a>(&'a mut self, message: &'a ClientMessage) -> TestBoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.sent
                .send(message.clone())
                .map_err(|_| anyhow!("test sent-message receiver dropped"))
        })
    }

    fn next_message<'a>(&'a mut self) -> TestBoxFuture<'a, anyhow::Result<Option<ServerMessage>>> {
        Box::pin(async move { Ok(self.incoming.recv().await) })
    }
}

struct TestLlmAnswerer {
    requests: RecordedLlmRequests,
}

impl TestLlmAnswerer {
    fn shared() -> (Arc<Self>, RecordedLlmRequests) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                requests: Arc::clone(&requests),
            }),
            requests,
        )
    }
}

impl LlmAnswerer for TestLlmAnswerer {
    fn label(&self) -> String {
        "test-llm".to_string()
    }

    fn stream_answer<'a>(&'a self, _prompt: &'a str) -> LlmDeltaStream<'a> {
        Box::pin(futures::stream::iter([Ok(LlmDelta {
            content: "test answer".to_string(),
        })]))
    }

    fn health<'a>(&'a self) -> TestBoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    fn answer<'a>(
        &'a self,
        session_id: Uuid,
        transcript: String,
        _progress_tx: mpsc::UnboundedSender<LlmProgress>,
    ) -> TestBoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            self.requests
                .lock()
                .expect("recorded LLM request lock should be available")
                .push((session_id, transcript));
            Ok("test answer".to_string())
        })
    }
}
