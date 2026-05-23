//! Client application loop for Parakeet Client PTT Sessions.
//!
//! This module owns the Client runtime loop: PTT hotkey events, daemon Session
//! message dispatch, Overlay routing, and calls into Client Session Injection
//! dispatch.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::sync::mpsc;
use tokio::time::{
    sleep, timeout, Duration as TokioDuration, Instant as TokioInstant, MissedTickBehavior,
};
use tracing::{debug, info, warn};

use crate::audio_feedback::AudioFeedback;
use crate::client::WsClient;
use crate::client_session::{
    classify_error_code, handle_server_message, ClientFocusRouter, ClientInjectionDispatcher,
    ClientLlmQueryRuntime, ClientSessionAction, ClientSessionIgnoredHotkeyReason,
    ClientSessionRuntime, ClientSessionStartBlocker, LlmProgressOutcome, LlmQueryRequest,
    SessionIntent,
};
use crate::config::ClientConfig;
use crate::hotkey::{
    ensure_input_access, parse_pre_modifier_key_names, spawn_hotkey_loop, HotkeyEvent,
    HotkeyIntent, HotkeyTasks,
};
use crate::injector_runtime::{
    build_injection_runner, spawn_injector_worker, InjectionJob, InjectionJobRunner,
    InjectionOrigin, INJECTION_ENQUEUE_TIMEOUT_MS,
};
use crate::llm::LlmAnswerer;
use crate::overlay_router::{OverlayRouter, OverlaySink};
use crate::protocol::{start_message, stop_message, ClientMessage, DaemonStatus, ServerMessage};
use crate::surface_focus::WaylandFocusCache;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait DaemonConnection: Send {
    fn send<'a>(&'a mut self, message: &'a ClientMessage) -> BoxFuture<'a, Result<()>>;

    fn next_message<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<ServerMessage>>>;
}

pub trait DaemonConnector: Send + Sync {
    fn connect<'a>(
        &'a self,
        config: &'a ClientConfig,
    ) -> BoxFuture<'a, Result<Box<dyn DaemonConnection>>>;
}

pub struct WsDaemonConnector;

struct WsDaemonConnection {
    client: WsClient,
}

impl DaemonConnector for WsDaemonConnector {
    fn connect<'a>(
        &'a self,
        config: &'a ClientConfig,
    ) -> BoxFuture<'a, Result<Box<dyn DaemonConnection>>> {
        Box::pin(async move {
            let client = WsClient::connect(config).await?;
            Ok(Box::new(WsDaemonConnection { client }) as Box<dyn DaemonConnection>)
        })
    }
}

impl DaemonConnection for WsDaemonConnection {
    fn send<'a>(&'a mut self, message: &'a ClientMessage) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.client.send(message).await })
    }

    fn next_message<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<ServerMessage>>> {
        Box::pin(async move { self.client.next_message().await })
    }
}

pub struct HotkeyRuntime {
    events: mpsc::UnboundedReceiver<HotkeyEvent>,
    listener_count: usize,
    talk_key: evdev::Key,
    llm_pre_modifier_keys: Vec<evdev::Key>,
    llm_pre_modifier_key_name: String,
    _tasks: Option<HotkeyTasks>,
}

#[cfg(test)]
impl HotkeyRuntime {
    pub(crate) fn new_for_tests(events: mpsc::UnboundedReceiver<HotkeyEvent>) -> Self {
        Self {
            events,
            listener_count: 1,
            talk_key: evdev::Key::KEY_RIGHTCTRL,
            llm_pre_modifier_keys: Vec::new(),
            llm_pre_modifier_key_name: "KEY_SHIFT".to_string(),
            _tasks: None,
        }
    }
}

pub trait HotkeySource: Send {
    fn start(&mut self, config: &ClientConfig) -> Result<HotkeyRuntime>;
}

pub struct EvdevHotkeySource {
    llm_pre_modifier_key_name: String,
}

impl EvdevHotkeySource {
    pub fn new(llm_pre_modifier_key_name: String) -> Self {
        Self {
            llm_pre_modifier_key_name,
        }
    }
}

impl HotkeySource for EvdevHotkeySource {
    fn start(&mut self, config: &ClientConfig) -> Result<HotkeyRuntime> {
        let talk_key = crate::hotkey::parse_key_name(&config.hotkey)
            .with_context(|| format!("invalid --hotkey value '{}'", config.hotkey))?;
        let llm_pre_modifier_keys = parse_pre_modifier_key_names(&self.llm_pre_modifier_key_name)
            .with_context(|| {
            format!(
                "invalid --llm-pre-modifier-key value '{}'",
                self.llm_pre_modifier_key_name
            )
        })?;

        ensure_input_access()?;
        let (hk_tx, events) = mpsc::unbounded_channel();
        let tasks = spawn_hotkey_loop(hk_tx, talk_key, llm_pre_modifier_keys.clone())?;
        Ok(HotkeyRuntime {
            events,
            listener_count: tasks.len(),
            talk_key,
            llm_pre_modifier_keys,
            llm_pre_modifier_key_name: self.llm_pre_modifier_key_name.clone(),
            _tasks: Some(tasks),
        })
    }
}

pub struct ClientPorts {
    audio_feedback: AudioFeedback,
    daemon_connector: Arc<dyn DaemonConnector>,
    injection_runner: Arc<dyn InjectionJobRunner>,
    overlay_sink: Box<dyn OverlaySink>,
    focus_cache: Option<WaylandFocusCache>,
    hotkey_source: Box<dyn HotkeySource>,
    llm_answerer: Arc<dyn LlmAnswerer>,
}

impl ClientPorts {
    pub fn new(
        audio_feedback: AudioFeedback,
        daemon_connector: Arc<dyn DaemonConnector>,
        injection_runner: Arc<dyn InjectionJobRunner>,
        overlay_sink: Box<dyn OverlaySink>,
        focus_cache: Option<WaylandFocusCache>,
        hotkey_source: Box<dyn HotkeySource>,
        llm_answerer: Arc<dyn LlmAnswerer>,
    ) -> Self {
        Self {
            audio_feedback,
            daemon_connector,
            injection_runner,
            overlay_sink,
            focus_cache,
            hotkey_source,
            llm_answerer,
        }
    }
}

const EVENT_LOOP_LAG_TICK_MS: u64 = 10;
const EVENT_LOOP_LAG_LOG_INTERVAL_SECS: u64 = 30;
const HOTKEY_INTENT_DIAGNOSTIC_LOG_INTERVAL_EVENTS: u64 = 20;

#[derive(Debug, Default)]
struct HotkeyIntentDiagnostics {
    hotkey_down_total: u64,
    hotkey_down_dictate_total: u64,
    hotkey_down_llm_query_total: u64,
    hotkey_down_ignored_total: u64,
    hotkey_up_total: u64,
    hotkey_up_ignored_total: u64,
    llm_busy_reject_total: u64,
    last_logged_hotkey_events: u64,
}

impl HotkeyIntentDiagnostics {
    fn note_hotkey_down(&mut self, intent: SessionIntent) {
        self.hotkey_down_total += 1;
        match intent {
            SessionIntent::Dictate => self.hotkey_down_dictate_total += 1,
            SessionIntent::LlmQuery => self.hotkey_down_llm_query_total += 1,
        }
    }

    fn note_hotkey_down_ignored(&mut self) {
        self.hotkey_down_ignored_total += 1;
    }

    fn note_hotkey_up(&mut self) {
        self.hotkey_up_total += 1;
    }

    fn note_hotkey_up_ignored(&mut self) {
        self.hotkey_up_ignored_total += 1;
    }

    fn note_llm_busy_reject(&mut self) {
        self.llm_busy_reject_total += 1;
    }

    fn maybe_log_summary(&mut self, reason: &'static str) {
        let hotkey_events = self.hotkey_down_total + self.hotkey_up_total;
        if hotkey_events == 0 {
            return;
        }
        if hotkey_events
            < self.last_logged_hotkey_events + HOTKEY_INTENT_DIAGNOSTIC_LOG_INTERVAL_EVENTS
        {
            return;
        }
        self.last_logged_hotkey_events = hotkey_events;
        self.log_summary(reason);
    }

    fn log_summary(&self, reason: &'static str) {
        if self.hotkey_down_total == 0 && self.hotkey_up_total == 0 {
            return;
        }
        info!(
            reason,
            hotkey_down_total = self.hotkey_down_total,
            hotkey_down_dictate_total = self.hotkey_down_dictate_total,
            hotkey_down_llm_query_total = self.hotkey_down_llm_query_total,
            hotkey_down_ignored_total = self.hotkey_down_ignored_total,
            hotkey_up_total = self.hotkey_up_total,
            hotkey_up_ignored_total = self.hotkey_up_ignored_total,
            llm_busy_reject_total = self.llm_busy_reject_total,
            "hotkey intent routing diagnostics"
        );
    }
}

fn percentile_value(sorted_samples: &[u64], percentile: u64) -> u64 {
    if sorted_samples.is_empty() {
        return 0;
    }

    let pct = percentile.min(100) as usize;
    let len = sorted_samples.len();
    let idx = ((len - 1) * pct) / 100;
    sorted_samples[idx]
}

fn spawn_event_loop_lag_monitor() {
    tokio::spawn(async move {
        let tick = TokioDuration::from_millis(EVENT_LOOP_LAG_TICK_MS.max(1));
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut last_log = TokioInstant::now();
        let mut lag_samples_ms = Vec::<u64>::with_capacity(4096);

        loop {
            let scheduled = interval.tick().await;
            let now = TokioInstant::now();
            let lag_ms = now.saturating_duration_since(scheduled).as_millis() as u64;
            lag_samples_ms.push(lag_ms);

            if last_log.elapsed() >= TokioDuration::from_secs(EVENT_LOOP_LAG_LOG_INTERVAL_SECS) {
                lag_samples_ms.sort_unstable();
                let p50 = percentile_value(&lag_samples_ms, 50);
                let p95 = percentile_value(&lag_samples_ms, 95);
                let p99 = percentile_value(&lag_samples_ms, 99);
                info!(
                    sample_count = lag_samples_ms.len(),
                    lag_p50_ms = p50,
                    lag_p95_ms = p95,
                    lag_p99_ms = p99,
                    target_p99_ms = INJECTION_ENQUEUE_TIMEOUT_MS,
                    "event loop lag window summary"
                );
                lag_samples_ms.clear();
                last_log = TokioInstant::now();
            }
        }
    });
}

pub async fn run_demo(
    config: ClientConfig,
    override_text: Option<String>,
    audio_feedback: AudioFeedback,
) -> Result<()> {
    info!(endpoint = %config.endpoint, "Connecting to parakeet-stt-daemon");
    let mut client = WsClient::connect(&config).await?;
    let injector_runner = build_injection_runner(&config);
    let (injector_worker, mut injection_reports) = spawn_injector_worker(injector_runner);

    let mut session_runtime = ClientSessionRuntime::new();
    let Some(session_id) = session_runtime.begin_listening(SessionIntent::Dictate) else {
        return Err(anyhow!("failed to start session state"));
    };

    client
        .send(&start_message(session_id, Some("auto".to_string())))
        .await?;
    info!(session = %session_id, "start_session sent");

    // For demo purposes we immediately stop after starting.
    client.send(&stop_message(session_id)).await?;
    session_runtime
        .stop_listening()
        .ok_or_else(|| anyhow!("failed to stop session state"))?;

    let mut demo_injection_succeeded = false;
    while let Some(message) = client.next_message().await? {
        match message {
            ServerMessage::SessionStarted { session_id, .. } => {
                info!(session = %session_id, "session started ack");
            }
            ServerMessage::FinalResult {
                session_id,
                text,
                latency_ms,
                audio_ms,
                ..
            } => {
                if !session_runtime.final_result_belongs_to_active_session(session_id) {
                    session_runtime.log_rejected_final_result(session_id, InjectionOrigin::Demo);
                    continue;
                }

                let to_inject = override_text.as_deref().unwrap_or(&text).to_string();
                info!(
                    session = %session_id,
                    daemon_latency_ms = latency_ms,
                    audio_ms,
                    "final result received"
                );
                injector_worker
                    .enqueue(
                        InjectionJob::new(session_id, to_inject, latency_ms, audio_ms)
                            .with_origin(InjectionOrigin::Demo),
                    )
                    .await
                    .map_err(|failure| {
                        anyhow!("failed to enqueue demo injection job: {:?}", failure)
                    })?;
                let report = timeout(TokioDuration::from_secs(5), injection_reports.recv())
                    .await
                    .context("timed out waiting for demo injection report")?
                    .ok_or_else(|| anyhow!("demo injection worker dropped before reporting"))?;
                if let Some(error) = report.error {
                    return Err(anyhow!("demo injection failed: {error}"));
                }
                demo_injection_succeeded = true;
                audio_feedback.play_completion();
                session_runtime.reset();
                break;
            }
            ServerMessage::Error {
                session_id,
                code,
                message,
            } => {
                let error_kind = classify_error_code(&code);
                warn!(
                    session = ?session_id,
                    error_code = %code,
                    error_kind,
                    "daemon error: {}",
                    message
                );
                return Err(anyhow!(
                    "demo failed: daemon returned error for session {:?}: {}: {}",
                    session_id,
                    code,
                    message
                ));
            }
            other => {
                debug!(?other, "ignoring server message");
            }
        }
    }

    if !demo_injection_succeeded {
        return Err(anyhow!("demo did not reach final injection"));
    }

    Ok(())
}

pub async fn run(config: ClientConfig, ports: ClientPorts) -> Result<()> {
    let ClientPorts {
        audio_feedback,
        daemon_connector,
        injection_runner,
        overlay_sink,
        focus_cache,
        mut hotkey_source,
        llm_answerer,
    } = ports;

    info!(
        endpoint = %config.endpoint,
        hotkey = %config.hotkey,
        completion_sound = audio_feedback.is_enabled(),
        "Starting hotkey loop"
    );
    let hotkey_runtime = hotkey_source.start(&config)?;
    let (injector_worker, mut injection_reports) = spawn_injector_worker(injection_runner);
    let injection_dispatcher = ClientInjectionDispatcher::new(injector_worker.clone());
    let mut focus_router = ClientFocusRouter::new(focus_cache);
    let mut overlay_router = OverlayRouter::new(overlay_sink);
    spawn_event_loop_lag_monitor();

    let mut session_runtime = ClientSessionRuntime::new();
    let mut hk_rx = hotkey_runtime.events;
    info!(
        devices = hotkey_runtime.listener_count,
        talk_key = ?hotkey_runtime.talk_key,
        llm_pre_modifier_keys = ?hotkey_runtime.llm_pre_modifier_keys,
        llm_pre_modifier_key = %hotkey_runtime.llm_pre_modifier_key_name,
        "Hotkey listeners started"
    );

    let llm_label = llm_answerer.label();
    let llm_health = llm_answerer.health().await;
    if llm_health {
        info!(base_url = %llm_label, "llama-server health probe succeeded");
    } else {
        warn!(
            base_url = %llm_label,
            "llama-server health probe failed; LLM query mode will fall back to raw transcript on error"
        );
    }

    fetch_status_once(&config).await;

    let mut backoff = TokioDuration::from_millis(500);
    let mut llm_query_runtime = ClientLlmQueryRuntime::new(Arc::clone(&llm_answerer));
    let mut hotkey_intent_diagnostics = HotkeyIntentDiagnostics::default();

    loop {
        match daemon_connector.connect(&config).await {
            Ok(mut daemon) => {
                info!("Connected to daemon");
                backoff = TokioDuration::from_millis(500);

                let run_loop = async {
                    loop {
                        tokio::select! {
                            Some(evt) = hk_rx.recv() => {
                                match evt {
                                    HotkeyEvent::Down { intent } => {
                                        let intent = session_intent_from_hotkey(intent);
                                        hotkey_intent_diagnostics.note_hotkey_down(intent);
                                        let start_blocker = llm_query_runtime
                                            .is_busy()
                                            .then_some(ClientSessionStartBlocker::LlmBusy);
                                        match session_runtime
                                            .handle_hotkey_down(intent, start_blocker)
                                        {
                                            ClientSessionAction::StartSession {
                                                session_id,
                                                intent,
                                            } => {
                                                let message =
                                                    start_message(session_id, Some("auto".to_string()));
                                                send_message(daemon.as_mut(), &message).await?;
                                                info!(
                                                    session = %session_id,
                                                    ?intent,
                                                    "start_session sent (hotkey down)"
                                                );
                                            }
                                            ClientSessionAction::IgnoreHotkeyDown {
                                                reason: ClientSessionIgnoredHotkeyReason::LlmBusy,
                                                ..
                                            } => {
                                                warn!(
                                                    "ignoring hotkey down while LLM response is in progress"
                                                );
                                                let busy =
                                                    llm_query_runtime.note_busy_overlay_rejection();
                                                overlay_router
                                                    .route_llm_answer_state_with_output_hint(
                                                        busy.session_id,
                                                        busy.seq,
                                                        busy.state,
                                                        || focus_router.next_overlay_output_hint(),
                                                    );
                                                overlay_router.route_session_ended(
                                                    None,
                                                    busy.session_id,
                                                    Some("busy".to_string()),
                                                );
                                                focus_router.reset_overlay_target();
                                                hotkey_intent_diagnostics.note_llm_busy_reject();
                                                hotkey_intent_diagnostics
                                                    .maybe_log_summary("hotkey_down_busy");
                                                continue;
                                            }
                                            ClientSessionAction::IgnoreHotkeyDown {
                                                reason,
                                                snapshot,
                                            } => {
                                                hotkey_intent_diagnostics.note_hotkey_down_ignored();
                                                debug!(
                                                    state = snapshot.state,
                                                    active_session = ?snapshot.active_session_id,
                                                    active_intent = ?snapshot.active_intent,
                                                    llm_busy = llm_query_runtime.is_busy(),
                                                    reason = reason.as_str(),
                                                    "ignoring hotkey down because client is not idle"
                                                );
                                            }
                                            other => {
                                                unreachable!("hotkey down produced non-down action: {other:?}");
                                            }
                                        }
                                        hotkey_intent_diagnostics.maybe_log_summary("hotkey_down");
                                    }
                                    HotkeyEvent::Up => {
                                        hotkey_intent_diagnostics.note_hotkey_up();
                                        match session_runtime.handle_hotkey_up() {
                                            ClientSessionAction::StopSession { session_id } => {
                                                let message = stop_message(session_id);
                                                send_message(daemon.as_mut(), &message).await?;
                                                let stop_sent_at = TokioInstant::now();
                                                focus_router
                                                    .record_stop_target(session_id, stop_sent_at);
                                                session_runtime.record_stop_message_sent(
                                                    session_id,
                                                    stop_sent_at,
                                                );
                                                info!(
                                                    session = %session_id,
                                                    "stop_session sent (hotkey up)"
                                                );
                                            }
                                            ClientSessionAction::IgnoreHotkeyUp {
                                                reason,
                                                snapshot,
                                            } => {
                                                hotkey_intent_diagnostics.note_hotkey_up_ignored();
                                                debug!(
                                                    state = snapshot.state,
                                                    active_session = ?snapshot.active_session_id,
                                                    active_intent = ?snapshot.active_intent,
                                                    llm_busy = llm_query_runtime.is_busy(),
                                                    reason = reason.as_str(),
                                                    "ignoring hotkey up because no listening session is active"
                                                );
                                            }
                                            other => {
                                                unreachable!("hotkey up produced non-up action: {other:?}");
                                            }
                                        }
                                        hotkey_intent_diagnostics.maybe_log_summary("hotkey_up");
                                    }
                                }
                            }
                            next = daemon.next_message() => {
                                match next {
                                    Ok(Some(message)) => {
                                        if let Some(session_id) =
                                            llm_query_runtime.defer_session_end_if_needed(
                                                &message,
                                                &session_runtime,
                                            )
                                        {
                                            debug!(
                                                session = %session_id,
                                                "deferring daemon session_ended until llm answer injection"
                                            );
                                            continue;
                                        }

                                        if session_runtime.active_intent()
                                            == Some(SessionIntent::LlmQuery)
                                        {
                                            if let ServerMessage::FinalResult { session_id, .. } =
                                                &message
                                            {
                                                if !session_runtime
                                                    .final_result_belongs_to_active_session(
                                                        *session_id,
                                                    )
                                                {
                                                    session_runtime.log_rejected_final_result(
                                                        *session_id,
                                                        InjectionOrigin::LlmAnswer,
                                                    );
                                                    continue;
                                                }
                                            }
                                        }

                                        match message {
                                            ServerMessage::FinalResult {
                                                session_id,
                                                text,
                                                latency_ms,
                                                audio_ms,
                                                ..
                                            } if session_runtime.active_intent()
                                                == Some(SessionIntent::LlmQuery) => {
                                                let started = llm_query_runtime.start_answer(
                                                    LlmQueryRequest::new(
                                                        session_id,
                                                        text,
                                                        latency_ms,
                                                        audio_ms,
                                                    ),
                                                    &mut session_runtime,
                                                );
                                                overlay_router.route_llm_answer_state_with_output_hint(
                                                    started.session_id,
                                                    started.seq,
                                                    started.state,
                                                    || focus_router.next_overlay_output_hint(),
                                                );
                                            }
                                            known => {
                                                handle_server_message(
                                                    known,
                                                    &mut session_runtime,
                                                    &mut focus_router,
                                                    &mut overlay_router,
                                                    &injection_dispatcher,
                                                ).await?;
                                            }
                                        }
                                    }
                                    Ok(None) => {
                                        warn!("websocket stream ended");
                                        break;
                                    }
                                    Err(err) => {
                                        warn!("websocket error: {}", err);
                                        break;
                                    }
                                }
                            }
                            Some(report) = injection_reports.recv() => {
                                injection_dispatcher.handle_report(
                                    report,
                                    &mut overlay_router,
                                    &audio_feedback,
                                );
                            }
                            Some(progress) = llm_query_runtime.recv_progress() => {
                                match llm_query_runtime.handle_progress(progress, &mut session_runtime) {
                                    LlmProgressOutcome::Delta(update) => {
                                        overlay_router.route_llm_answer_delta_with_output_hint(
                                            update.session_id,
                                            update.seq,
                                            update.text,
                                            || focus_router.next_overlay_output_hint(),
                                        );
                                    }
                                    LlmProgressOutcome::Finished(answer) => {
                                        overlay_router.route_session_ended(
                                            None,
                                            answer.session_id,
                                            answer.session_end_reason.clone(),
                                        );
                                        focus_router.reset_overlay_target();
                                        injection_dispatcher
                                            .dispatch_llm_answer(
                                                answer.injection,
                                                &mut focus_router,
                                            )
                                            .await;
                                    }
                                    LlmProgressOutcome::Ignored => {}
                                }
                            }
                        }
                    }
                    Result::<()>::Ok(())
                }.await;

                if let Err(err) = run_loop {
                    warn!("session loop ended with error: {err}");
                }
                hotkey_intent_diagnostics.log_summary("daemon_connection_drop");
                let reset_action = session_runtime.handle_connection_drop();
                debug!(
                    ?reset_action,
                    "client session coordinator reset after daemon connection drop"
                );
                llm_query_runtime.reset_for_connection_drop();
                focus_router.reset_for_connection_drop();
                warn!("Reconnecting to daemon after drop");
            }
            Err(err) => {
                warn!(
                    "Connection to daemon failed: {} (retrying in {:.1?})",
                    err, backoff
                );
                sleep(backoff).await;
                backoff = (backoff * 2).min(TokioDuration::from_secs(10));
            }
        }
    }
}

async fn send_message(
    daemon: &mut dyn DaemonConnection,
    message: &crate::protocol::ClientMessage,
) -> Result<()> {
    daemon.send(message).await
}

fn session_intent_from_hotkey(intent: HotkeyIntent) -> SessionIntent {
    match intent {
        HotkeyIntent::Dictate => SessionIntent::Dictate,
        HotkeyIntent::LlmQuery => SessionIntent::LlmQuery,
    }
}

fn format_daemon_status(status: &DaemonStatus) -> String {
    let payload = serde_json::to_value(ServerMessage::Status(status.clone()))
        .expect("daemon status should serialize");
    let fields = payload
        .as_object()
        .expect("daemon status should serialize to an object")
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("Daemon status: {fields}")
}

async fn fetch_status_once(config: &ClientConfig) {
    let Some(url) = config.status_url() else {
        return;
    };
    let client = reqwest::Client::new();
    match client
        .get(url.clone())
        .timeout(Duration::from_secs(2))
        .send()
        .await
    {
        Ok(response) => match response.json::<ServerMessage>().await {
            Ok(ServerMessage::Status(status)) => {
                info!("{}", format_daemon_status(&status));
            }
            Ok(other) => {
                warn!(
                    "Failed to decode daemon status from {}: expected status message, got {:?}",
                    url, other
                );
            }
            Err(err) => {
                warn!("Failed to decode daemon status from {}: {}", url, err);
            }
        },
        Err(err) => {
            warn!("Failed to fetch daemon status from {}: {}", url, err);
        }
    };
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use std::time::Instant;

    use clap::Parser;
    use tokio::task::yield_now;
    use tokio::time::timeout;
    use uuid::Uuid;

    use crate::client_runtime_fixtures::{ClientRuntimeHarness, RecordingInjectionRunner};
    use crate::client_session::SessionIntent;
    use crate::config::{
        default_daemon_websocket_endpoint, ClientConfig, ClipboardOptions, InjectionConfig,
        InjectionMode, PasteBackendFailurePolicy, PasteKeyBackend, PasteShortcut,
    };
    use crate::hotkey::HotkeyIntent;
    use crate::injector::{
        FailInjector, InjectorContext, PasteChordSender, PasteKeySender, TextInjector,
    };
    use crate::overlay_router::{OverlayEvent, OverlayTextProducer};
    use crate::protocol::{ClientMessage, DaemonStatus, ServerMessage};

    use crate::injector::INJECTOR_JOB_TIMEOUT_MS;
    use crate::injector_runtime::{
        collect_pipe_reader, spawn_injector_worker_with_capacity, spawn_pipe_reader,
        EnqueueFailure, InProcessInjectorRunner, InjectionErrorKind, InjectionJob,
        InjectionJobRunner, InjectionRunError, InjectionRunOutput, InjectorSubprocessRunner,
        UinputSenderState,
    };
    use crate::llm::{drain_sse_lines, sanitize_model_answer};

    use super::{format_daemon_status, run, HotkeyIntentDiagnostics};

    #[test]
    fn daemon_status_accepts_minimal_protocol_payload() {
        let status = match serde_json::from_str::<ServerMessage>(
            r#"{"type":"status","state":"idle","sessions_active":0}"#,
        )
        .expect("minimal status payload should parse")
        {
            ServerMessage::Status(status) => status,
            other => panic!("expected status message, got {other:?}"),
        };

        assert_eq!(status.state, "idle");
        assert_eq!(status.sessions_active, 0);
        assert_eq!(status.stream_path_executed, None);
        assert_eq!(status.finalization_mode, None);
        assert_eq!(status.interim_transcript_enabled, None);
        assert_eq!(status.interim_transcript_last_source, None);
        assert_eq!(status.overlay_events_enabled, None);
        assert_eq!(status.gpu_mem_mb, None);

        let summary = format_daemon_status(&status);
        assert!(summary.contains("type=\"status\""));
        assert!(summary.contains("state=\"idle\""));
        assert!(summary.contains("sessions_active=0"));
        assert!(summary.contains("stream_path_executed=null"));
        assert!(summary.contains("finalization_mode=null"));
        assert!(summary.contains("interim_transcript_enabled=null"));
        assert!(summary.contains("gpu_mem_mb=null"));
    }

    #[test]
    fn daemon_status_standalone_type_accepts_status_payload() {
        let status: DaemonStatus =
            serde_json::from_str(r#"{"type":"status","state":"idle","sessions_active":0}"#)
                .expect("minimal status payload should parse");

        assert_eq!(status.state, "idle");
        assert_eq!(status.sessions_active, 0);
        assert_eq!(status.stream_path_executed, None);
        assert_eq!(status.finalization_mode, None);
        assert_eq!(status.interim_transcript_enabled, None);
        assert_eq!(status.interim_transcript_last_source, None);
        assert_eq!(status.overlay_events_enabled, None);
        assert_eq!(status.gpu_mem_mb, None);
    }

    #[test]
    fn daemon_status_preserves_and_formats_interim_truth_fixture() {
        let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../docs/protocol/fixtures/status_stream_fallback.json");
        let fixture = fs::read_to_string(fixture_path).expect("status fixture should be readable");
        let status = match serde_json::from_str::<ServerMessage>(&fixture)
            .expect("status fixture should parse")
        {
            ServerMessage::Status(status) => status,
            other => panic!("expected status message, got {other:?}"),
        };

        assert_eq!(status.state, "idle");
        assert_eq!(status.sessions_active, 0);
        assert_eq!(status.gpu_mem_mb, Some(1024));
        assert_eq!(status.device.as_deref(), Some("cuda"));
        assert_eq!(status.effective_device.as_deref(), Some("cuda"));
        assert_eq!(status.streaming_enabled, Some(true));
        assert_eq!(status.stream_helper_active, Some(false));
        assert_eq!(
            status.stream_helper_scope.as_deref(),
            Some("live_session_only")
        );
        assert_eq!(
            status.stream_fallback_reason.as_deref(),
            Some("init_failed:RuntimeError")
        );
        assert_eq!(status.stream_path_executed, Some(false));
        assert_eq!(status.stream_chunks_processed, Some(0));
        assert_eq!(status.finalization_mode.as_deref(), Some("offline_seal"));
        assert_eq!(
            status.final_audio_source.as_deref(),
            Some("canonical_session_audio")
        );
        assert_eq!(status.tail_trim_mode.as_deref(), Some("rms"));
        assert_eq!(status.vad_enabled, Some(false));
        assert_eq!(status.vad_active, Some(false));
        assert_eq!(status.vad_fallback_reason, None);
        assert_eq!(status.interim_transcript_enabled, Some(true));
        assert_eq!(
            status.interim_transcript_last_source.as_deref(),
            Some("live")
        );
        assert_eq!(status.interim_transcript_live_chunks_processed, Some(1));
        assert_eq!(
            status.interim_transcript_stop_replay_chunks_processed,
            Some(0)
        );
        assert_eq!(status.interim_transcript_updates_emitted, Some(1));
        assert_eq!(status.interim_transcript_live_updates_emitted, Some(1));
        assert_eq!(
            status.interim_transcript_stop_replay_updates_emitted,
            Some(0)
        );
        assert_eq!(status.interim_transcript_live_failed, Some(false));
        assert_eq!(status.interim_transcript_stop_replay_failed, Some(false));
        assert_eq!(status.interim_transcript_source_fallback_reason, None);
        assert_eq!(status.overlay_events_enabled, Some(true));
        assert_eq!(status.overlay_events_emitted, Some(3));
        assert_eq!(status.overlay_events_dropped, Some(1));
        assert_eq!(status.chunk_secs, Some(2.4));
        assert_eq!(status.active_session_age_ms, None);
        assert_eq!(status.audio_stop_ms, Some(0));
        assert_eq!(status.finalize_ms, Some(4));
        assert_eq!(status.infer_ms, Some(0));
        assert_eq!(status.send_ms, Some(0));
        assert_eq!(status.last_audio_ms, Some(2400));
        assert_eq!(status.last_infer_ms, Some(0));
        assert_eq!(status.last_send_ms, Some(0));

        let summary = format_daemon_status(&status);
        assert!(summary.contains("stream_helper_scope=\"live_session_only\""));
        assert!(summary.contains("stream_path_executed=false"));
        assert!(summary.contains("stream_fallback_reason=\"init_failed:RuntimeError\""));
        assert!(summary.contains("finalization_mode=\"offline_seal\""));
        assert!(summary.contains("final_audio_source=\"canonical_session_audio\""));
        assert!(summary.contains("tail_trim_mode=\"rms\""));
        assert!(summary.contains("interim_transcript_enabled=true"));
        assert!(summary.contains("interim_transcript_last_source=\"live\""));
        assert!(summary.contains("interim_transcript_updates_emitted=1"));
        assert!(summary.contains("overlay_events_emitted=3"));
        assert!(summary.contains("gpu_mem_mb=1024"));
    }

    #[test]
    fn daemon_status_format_mentions_current_runtime_truth_fields() {
        let schema_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../docs/protocol/schema/messages.schema.json");
        let schema: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(schema_path).expect("status schema should be readable"),
        )
        .expect("status schema should parse");
        let fields = schema
            .pointer("/$defs/StatusMessage/x-runtime-truth-field-groups")
            .and_then(serde_json::Value::as_object)
            .expect("schema should declare Runtime Truth groups")
            .values()
            .flat_map(|group_fields| {
                group_fields
                    .as_array()
                    .expect("Runtime Truth group should be an array")
                    .iter()
                    .map(|field| {
                        field
                            .as_str()
                            .expect("Runtime Truth field should be a string")
                            .to_string()
                    })
            })
            .collect::<BTreeSet<_>>();

        let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../docs/protocol/fixtures/status_stream_fallback.json");
        let fixture = fs::read_to_string(fixture_path).expect("status fixture should be readable");
        let status = match serde_json::from_str::<ServerMessage>(&fixture)
            .expect("status fixture should parse")
        {
            ServerMessage::Status(status) => status,
            other => panic!("expected status message, got {other:?}"),
        };
        let summary = format_daemon_status(&status);
        let formatted_fields = summary
            .strip_prefix("Daemon status: ")
            .expect("formatted status should use expected prefix")
            .split(", ")
            .map(|field| {
                field
                    .split_once('=')
                    .expect("formatted field should be key=value")
                    .0
                    .to_string()
            })
            .collect::<BTreeSet<_>>();

        assert_eq!(formatted_fields, fields);
    }

    struct SlowRunner {
        calls: Arc<AtomicU64>,
        sleep_ms: u64,
    }

    impl InjectionJobRunner for SlowRunner {
        fn run(
            &self,
            _job: &InjectionJob,
        ) -> std::result::Result<InjectionRunOutput, InjectionRunError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(self.sleep_ms));
            Ok(InjectionRunOutput::default())
        }
    }

    struct TimeoutThenRecordingRunner {
        calls: Arc<AtomicU64>,
        seen: Arc<Mutex<Vec<String>>>,
        timeout_run_ms: u64,
    }

    impl InjectionJobRunner for TimeoutThenRecordingRunner {
        fn run(
            &self,
            job: &InjectionJob,
        ) -> std::result::Result<InjectionRunOutput, InjectionRunError> {
            let call_index = self.calls.fetch_add(1, Ordering::Relaxed);
            if call_index == 0 {
                std::thread::sleep(Duration::from_millis(self.timeout_run_ms));
                return Err(InjectionRunError::ExecutionTimeout(format!(
                    "injector execution timed out after {INJECTOR_JOB_TIMEOUT_MS} ms"
                )));
            }

            self.seen
                .lock()
                .expect("recording lock should be available")
                .push(job.text.to_string());
            Ok(InjectionRunOutput::default())
        }
    }

    #[derive(Clone)]
    struct RecordingTextInjector {
        seen: Arc<Mutex<Vec<(String, Uuid)>>>,
    }

    impl TextInjector for RecordingTextInjector {
        fn inject(&self, _text: &str) -> anyhow::Result<()> {
            panic!("recording injector expects inject_with_context");
        }

        fn inject_with_context(
            &self,
            text: &str,
            context: Option<InjectorContext>,
        ) -> anyhow::Result<()> {
            let session_id = context
                .as_ref()
                .map(|value| value.session_id)
                .expect("in-process runner should pass injector context");
            self.seen
                .lock()
                .expect("recording injector lock should be available")
                .push((text.to_string(), session_id));
            Ok(())
        }
    }

    #[derive(Debug)]
    struct RecordingPasteChordSender {
        sends: Arc<AtomicU64>,
        fail: bool,
    }

    impl PasteChordSender for RecordingPasteChordSender {
        fn send_shortcut(&self, _shortcut: PasteShortcut) -> anyhow::Result<()> {
            self.sends.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                anyhow::bail!("stage=backend synthetic sender failure");
            }
            Ok(())
        }

        fn backend_config(&self) -> Option<String> {
            Some("test_sender".to_string())
        }
    }

    #[derive(Clone)]
    struct SenderDrivenInjector {
        seen: Arc<Mutex<Vec<(String, Uuid)>>>,
        sender: PasteKeySender,
    }

    impl TextInjector for SenderDrivenInjector {
        fn inject(&self, _text: &str) -> anyhow::Result<()> {
            panic!("sender-driven injector expects inject_with_context");
        }

        fn inject_with_context(
            &self,
            text: &str,
            context: Option<InjectorContext>,
        ) -> anyhow::Result<()> {
            let session_id = context
                .as_ref()
                .map(|value| value.session_id)
                .expect("sender-driven injector expects session context");
            self.seen
                .lock()
                .expect("sender-driven injector lock should be available")
                .push((text.to_string(), session_id));
            if let PasteKeySender::Uinput { sender, .. } = &self.sender {
                sender.send_shortcut(PasteShortcut::CtrlV)?;
            }
            Ok(())
        }
    }

    fn test_client_config() -> ClientConfig {
        ClientConfig::new(
            &default_daemon_websocket_endpoint(),
            None,
            "KEY_RIGHTCTRL".to_string(),
            InjectionConfig {
                uinput_dwell_ms: 18,
                injection_mode: InjectionMode::Paste,
                clipboard: ClipboardOptions {
                    key_backend: PasteKeyBackend::Uinput,
                    backend_failure_policy: PasteBackendFailurePolicy::CopyOnly,
                    post_chord_hold_ms: 700,
                    seat: None,
                    write_primary: false,
                },
            },
            Duration::from_secs(1),
        )
        .expect("test client config should be valid")
    }

    fn make_test_script(content: &str) -> PathBuf {
        let mut file = tempfile::Builder::new()
            .prefix(&format!("parakeet-ptt-worker-test-{}-", std::process::id()))
            .tempfile()
            .expect("test script should be creatable");
        file.write_all(content.as_bytes())
            .expect("test script should be writable");
        file.flush().expect("test script should flush");
        file.as_file()
            .sync_all()
            .expect("test script should sync before execution");
        let (persisted_file, path) = file
            .keep()
            .expect("test script path should persist until test cleanup");
        drop(persisted_file);
        let mut perms = fs::metadata(&path)
            .expect("test script should exist")
            .permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&path, perms).expect("test script should be executable");
        path
    }

    fn test_app_config() -> ClientConfig {
        ClientConfig::new(
            "test://daemon/ws",
            None,
            "KEY_RIGHTCTRL".to_string(),
            InjectionConfig {
                uinput_dwell_ms: 18,
                injection_mode: InjectionMode::Paste,
                clipboard: ClipboardOptions {
                    key_backend: PasteKeyBackend::Uinput,
                    backend_failure_policy: PasteBackendFailurePolicy::CopyOnly,
                    post_chord_hold_ms: 700,
                    seat: None,
                    write_primary: false,
                },
            },
            Duration::from_millis(25),
        )
        .expect("test app config should be valid")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn app_hotkey_press_sends_start_session_message() {
        let (config, ports, mut runtime) =
            ClientRuntimeHarness::new(test_app_config()).into_parts();

        let app_task = tokio::spawn(run(config, ports));
        runtime.send_hotkey_down(HotkeyIntent::Dictate);
        let sent = runtime.next_sent_message(Duration::from_millis(250)).await;
        app_task.abort();

        assert!(
            runtime.recorded_injections().is_empty(),
            "start-session hotkey path should not enqueue Injection"
        );
        assert!(
            runtime.recorded_overlay_events().is_empty(),
            "start-session hotkey path should not emit Overlay events before Daemon response"
        );

        match sent {
            ClientMessage::StartSession {
                mode,
                preferred_lang,
                ..
            } => {
                assert_eq!(mode, "push_to_talk");
                assert_eq!(preferred_lang.as_deref(), Some("auto"));
            }
            other => panic!("expected start_session, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn app_hotkey_release_sends_stop_session_message() {
        let (config, ports, mut runtime) =
            ClientRuntimeHarness::new(test_app_config()).into_parts();

        let app_task = tokio::spawn(run(config, ports));
        runtime.send_hotkey_down(HotkeyIntent::Dictate);
        let start = runtime.next_sent_message(Duration::from_millis(250)).await;
        let active_session_id = match start {
            ClientMessage::StartSession { session_id, .. } => session_id,
            other => panic!("expected start_session, got {other:?}"),
        };

        runtime.send_hotkey_up();
        let stop = runtime.next_sent_message(Duration::from_millis(250)).await;
        app_task.abort();

        match stop {
            ClientMessage::StopSession { session_id, .. } => {
                assert_eq!(session_id, active_session_id);
            }
            other => panic!("expected stop_session, got {other:?}"),
        }
        assert!(
            runtime.recorded_injections().is_empty(),
            "stop-session hotkey path should not enqueue Injection"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn app_llm_query_ignores_stale_final_before_answer_generation() {
        let (config, ports, mut runtime) =
            ClientRuntimeHarness::new(test_app_config()).into_parts();

        let app_task = tokio::spawn(run(config, ports));
        runtime.send_hotkey_down(HotkeyIntent::LlmQuery);
        let start = runtime.next_sent_message(Duration::from_millis(250)).await;
        let active_session_id = match start {
            ClientMessage::StartSession { session_id, .. } => session_id,
            other => panic!("expected start_session, got {other:?}"),
        };
        runtime.send_hotkey_up();
        let stop = runtime.next_sent_message(Duration::from_millis(250)).await;
        match stop {
            ClientMessage::StopSession { session_id, .. } => {
                assert_eq!(session_id, active_session_id);
            }
            other => panic!("expected stop_session, got {other:?}"),
        }

        runtime.send_daemon_message(ServerMessage::FinalResult {
            session_id: Uuid::new_v4(),
            text: "stale private transcript".to_string(),
            latency_ms: 44,
            audio_ms: 1200,
            lang: Some("en".to_string()),
            confidence: Some(0.92),
        });
        tokio::time::sleep(Duration::from_millis(75)).await;

        assert!(runtime.recorded_llm_requests().is_empty());
        assert!(runtime.recorded_injections().is_empty());

        runtime.send_daemon_message(ServerMessage::FinalResult {
            session_id: active_session_id,
            text: "current private transcript".to_string(),
            latency_ms: 55,
            audio_ms: 1500,
            lang: Some("en".to_string()),
            confidence: Some(0.95),
        });
        timeout(Duration::from_secs(1), async {
            loop {
                if !runtime.recorded_injections().is_empty() {
                    break;
                }
                yield_now().await;
            }
        })
        .await
        .expect("valid final result should produce an LLM answer injection");
        app_task.abort();

        assert_eq!(
            runtime.recorded_llm_requests(),
            vec![(active_session_id, "current private transcript".to_string())]
        );
        assert_eq!(
            runtime.recorded_injections(),
            vec!["test answer".to_string()]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn app_llm_answer_deltas_route_with_independent_overlay_producer() {
        let (config, ports, mut runtime) = ClientRuntimeHarness::new_with_llm_deltas(
            test_app_config(),
            ["answer", " delta"],
            "answer delta",
        )
        .into_parts();

        let app_task = tokio::spawn(run(config, ports));
        runtime.send_hotkey_down(HotkeyIntent::LlmQuery);
        let start = runtime.next_sent_message(Duration::from_millis(250)).await;
        let active_session_id = match start {
            ClientMessage::StartSession { session_id, .. } => session_id,
            other => panic!("expected start_session, got {other:?}"),
        };
        runtime.send_hotkey_up();
        let stop = runtime.next_sent_message(Duration::from_millis(250)).await;
        match stop {
            ClientMessage::StopSession { session_id, .. } => {
                assert_eq!(session_id, active_session_id);
            }
            other => panic!("expected stop_session, got {other:?}"),
        }

        runtime.send_daemon_message(ServerMessage::InterimText {
            session_id: active_session_id,
            seq: 10,
            text: "daemon interim transcript".to_string(),
        });
        runtime.send_daemon_message(ServerMessage::FinalResult {
            session_id: active_session_id,
            text: "private prompt".to_string(),
            latency_ms: 55,
            audio_ms: 1500,
            lang: Some("en".to_string()),
            confidence: Some(0.95),
        });
        timeout(Duration::from_secs(1), async {
            loop {
                let overlay_event_count = runtime
                    .recorded_overlay_events()
                    .into_iter()
                    .filter(|event| {
                        matches!(
                            event,
                            OverlayEvent::InterimState { .. } | OverlayEvent::InterimText { .. }
                        )
                    })
                    .count();
                if overlay_event_count >= 4 && !runtime.recorded_injections().is_empty() {
                    break;
                }
                yield_now().await;
            }
        })
        .await
        .expect("LLM deltas should route and final answer should inject");
        app_task.abort();

        assert_eq!(
            runtime.recorded_llm_requests(),
            vec![(active_session_id, "private prompt".to_string())]
        );
        assert_eq!(
            runtime.recorded_injections(),
            vec!["answer delta".to_string()]
        );

        let overlay_text_events: Vec<_> = runtime
            .recorded_overlay_events()
            .into_iter()
            .filter(|event| {
                matches!(
                    event,
                    OverlayEvent::InterimState { .. } | OverlayEvent::InterimText { .. }
                )
            })
            .collect();
        assert_eq!(
            overlay_text_events,
            vec![
                OverlayEvent::InterimText {
                    producer: OverlayTextProducer::DaemonSttInterim,
                    session_id: active_session_id,
                    seq: 10,
                    text: "daemon interim transcript".to_string(),
                },
                OverlayEvent::InterimState {
                    producer: OverlayTextProducer::LlmAnswerDelta,
                    session_id: active_session_id,
                    seq: 1,
                    state: "Generating answer...".to_string(),
                },
                OverlayEvent::InterimText {
                    producer: OverlayTextProducer::LlmAnswerDelta,
                    session_id: active_session_id,
                    seq: 2,
                    text: "answer".to_string(),
                },
                OverlayEvent::InterimText {
                    producer: OverlayTextProducer::LlmAnswerDelta,
                    session_id: active_session_id,
                    seq: 3,
                    text: "answer delta".to_string(),
                },
            ]
        );
    }

    #[test]
    fn cli_default_paste_key_backend_is_uinput() {
        let cli = crate::Cli::parse_from(["parakeet-ptt"]);
        assert!(matches!(
            cli.paste_key_backend,
            crate::CliPasteKeyBackend::Uinput
        ));
    }

    #[test]
    fn cli_test_injection_defaults_are_stable() {
        let cli = crate::Cli::parse_from(["parakeet-ptt", "--test-injection"]);
        assert!(cli.test_injection);
        assert_eq!(cli.test_injection_count, 1);
        assert_eq!(cli.test_injection_text_prefix, "Parakeet Test");
        assert_eq!(cli.test_injection_interval_ms, 150);
        assert_eq!(cli.test_injection_shortcut, None);
    }

    #[test]
    fn cli_test_injection_accepts_forced_shortcut() {
        let cli = crate::Cli::parse_from([
            "parakeet-ptt",
            "--test-injection",
            "--test-injection-shortcut",
            "ctrl-shift-v",
        ]);
        assert!(matches!(
            cli.test_injection_shortcut,
            Some(crate::CliTestInjectionShortcut::CtrlShiftV)
        ));
    }

    #[test]
    fn cli_completion_sound_volume_rejects_values_above_documented_range() {
        let cli = crate::Cli::parse_from(["parakeet-ptt", "--completion-sound-volume", "100"]);
        assert_eq!(cli.completion_sound_volume, 100);

        let error =
            crate::Cli::try_parse_from(["parakeet-ptt", "--completion-sound-volume", "101"])
                .expect_err("completion sound volume above 100 should be rejected");
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn cli_overlay_enabled_defaults_to_none() {
        let cli = crate::Cli::parse_from(["parakeet-ptt"]);
        assert_eq!(cli.overlay_enabled, None);
    }

    #[test]
    fn cli_overlay_enabled_accepts_boolean_values() {
        let cli_enabled = crate::Cli::parse_from(["parakeet-ptt", "--overlay-enabled", "true"]);
        assert_eq!(cli_enabled.overlay_enabled, Some(true));

        let cli_disabled = crate::Cli::parse_from(["parakeet-ptt", "--overlay-enabled", "false"]);
        assert_eq!(cli_disabled.overlay_enabled, Some(false));
    }

    #[test]
    fn cli_overlay_adaptive_width_defaults_to_none() {
        let cli = crate::Cli::parse_from(["parakeet-ptt"]);
        assert_eq!(cli.overlay_adaptive_width, None);
    }

    #[test]
    fn cli_overlay_adaptive_width_accepts_boolean_values() {
        let cli_enabled =
            crate::Cli::parse_from(["parakeet-ptt", "--overlay-adaptive-width", "true"]);
        assert_eq!(cli_enabled.overlay_adaptive_width, Some(true));

        let cli_disabled =
            crate::Cli::parse_from(["parakeet-ptt", "--overlay-adaptive-width", "false"]);
        assert_eq!(cli_disabled.overlay_adaptive_width, Some(false));
    }

    #[test]
    fn cli_llm_pre_modifier_and_llm_defaults_are_set() {
        let cli = crate::Cli::parse_from(["parakeet-ptt"]);
        assert_eq!(
            cli.llm_pre_modifier_key,
            crate::DEFAULT_LLM_PRE_MODIFIER_KEY
        );
        assert_eq!(cli.llm_base_url, crate::llm::default_llm_base_url());
        assert_eq!(cli.llm_model, crate::DEFAULT_LLM_MODEL);
        assert!(cli.llm_overlay_stream);
    }

    #[test]
    fn in_process_runner_reuses_sender_across_jobs_when_healthy() {
        let build_count = Arc::new(AtomicU64::new(0));
        let sender_create_count = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let config = test_client_config();
        let runner = InProcessInjectorRunner::new_for_tests(
            &config,
            Arc::new({
                let build_count = Arc::clone(&build_count);
                let seen = Arc::clone(&seen);
                move |_config, _sender, _focus_cache| {
                    build_count.fetch_add(1, Ordering::Relaxed);
                    Arc::new(RecordingTextInjector {
                        seen: Arc::clone(&seen),
                    })
                }
            }),
            Arc::new({
                let sender_create_count = Arc::clone(&sender_create_count);
                move |_config| {
                    sender_create_count.fetch_add(1, Ordering::Relaxed);
                    Ok(Arc::new(RecordingPasteChordSender {
                        sends: Arc::new(AtomicU64::new(0)),
                        fail: false,
                    }) as Arc<dyn PasteChordSender>)
                }
            }),
            Arc::new(|_| {}),
            Duration::from_millis(0),
            Duration::from_millis(5),
            None,
        );

        let session_one = Uuid::new_v4();
        let session_two = Uuid::new_v4();
        runner
            .run(&InjectionJob::new(session_one, "first".to_string(), 0, 0))
            .expect("first run should succeed");
        runner
            .run(&InjectionJob::new(session_two, "second".to_string(), 0, 0))
            .expect("second run should succeed");

        assert_eq!(build_count.load(Ordering::Relaxed), 2);
        assert_eq!(sender_create_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen.lock()
                .expect("recording injector lock should be available")
                .as_slice(),
            &[
                ("first".to_string(), session_one),
                ("second".to_string(), session_two)
            ]
        );
    }

    #[test]
    fn in_process_runner_reuses_focus_cache_across_jobs() {
        let sender_create_count = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed_focus_caches = Arc::new(Mutex::new(Vec::new()));
        let config = test_client_config();
        let shared_focus_cache = crate::surface_focus::WaylandFocusCache::new();
        let runner = InProcessInjectorRunner::new_for_tests(
            &config,
            Arc::new({
                let seen = Arc::clone(&seen);
                let observed_focus_caches = Arc::clone(&observed_focus_caches);
                move |_config, _sender, focus_cache| {
                    observed_focus_caches
                        .lock()
                        .expect("observed focus cache lock should be available")
                        .push(focus_cache.expect("runner should reuse a shared focus cache"));
                    Arc::new(RecordingTextInjector {
                        seen: Arc::clone(&seen),
                    })
                }
            }),
            Arc::new({
                let sender_create_count = Arc::clone(&sender_create_count);
                move |_config| {
                    sender_create_count.fetch_add(1, Ordering::Relaxed);
                    Ok(Arc::new(RecordingPasteChordSender {
                        sends: Arc::new(AtomicU64::new(0)),
                        fail: false,
                    }) as Arc<dyn PasteChordSender>)
                }
            }),
            Arc::new(|_| {}),
            Duration::from_millis(0),
            Duration::from_millis(5),
            Some(shared_focus_cache.clone()),
        );

        let session_one = Uuid::new_v4();
        let session_two = Uuid::new_v4();
        runner
            .run(&InjectionJob::new(session_one, "first".to_string(), 0, 0))
            .expect("first run should succeed");
        runner
            .run(&InjectionJob::new(session_two, "second".to_string(), 0, 0))
            .expect("second run should succeed");

        let observed_focus_caches = observed_focus_caches
            .lock()
            .expect("observed focus cache lock should be available");
        assert_eq!(observed_focus_caches.len(), 2);
        assert!(observed_focus_caches[0].shares_worker_with(&observed_focus_caches[1]));
        assert!(observed_focus_caches[0].shares_worker_with(&shared_focus_cache));
        assert_eq!(sender_create_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen.lock()
                .expect("recording injector lock should be available")
                .as_slice(),
            &[
                ("first".to_string(), session_one),
                ("second".to_string(), session_two)
            ]
        );
    }

    #[test]
    fn in_process_runner_commits_sender_usage_only_after_success() {
        let config = test_client_config();
        let runner = InProcessInjectorRunner::new_for_tests(
            &config,
            Arc::new(|_config, _sender, _focus_cache| {
                Arc::new(FailInjector::new("unused test injector"))
            }),
            Arc::new(|_config| {
                Ok(Arc::new(RecordingPasteChordSender {
                    sends: Arc::new(AtomicU64::new(0)),
                    fail: false,
                }) as Arc<dyn PasteChordSender>)
            }),
            Arc::new(|_| {}),
            Duration::from_millis(0),
            Duration::from_millis(5),
            None,
        );

        let sender = runner
            .prepare_paste_key_sender()
            .expect("preparing sender should succeed");
        let PasteKeySender::Uinput {
            metadata: Some(metadata),
            ..
        } = &sender
        else {
            panic!("paste mode should prepare a uinput sender");
        };
        assert!(metadata.fresh_device);
        assert_eq!(metadata.use_count_before_attempt, 0);

        {
            let manager = runner
                .sender_manager
                .lock()
                .expect("sender manager lock should be available");
            let UinputSenderState::Healthy(healthy) = &manager.state else {
                panic!("prepared sender should leave manager healthy");
            };
            assert!(healthy.fresh_pending);
            assert_eq!(healthy.use_count, 0);
        }

        runner.commit_successful_paste_key_sender(&sender);
        runner.commit_successful_paste_key_sender(&sender);

        {
            let manager = runner
                .sender_manager
                .lock()
                .expect("sender manager lock should be available");
            let UinputSenderState::Healthy(healthy) = &manager.state else {
                panic!("committed sender should keep manager healthy");
            };
            assert!(!healthy.fresh_pending);
            assert_eq!(healthy.use_count, 1);
        }

        let sender = runner
            .prepare_paste_key_sender()
            .expect("preparing reused sender should succeed");
        let PasteKeySender::Uinput {
            metadata: Some(metadata),
            ..
        } = &sender
        else {
            panic!("paste mode should keep using a uinput sender");
        };
        assert!(!metadata.fresh_device);
        assert_eq!(metadata.use_count_before_attempt, 1);
    }

    #[test]
    fn in_process_runner_retries_after_create_failure_without_restart() {
        let build_count = Arc::new(AtomicU64::new(0));
        let sender_create_count = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let config = test_client_config();
        let runner = InProcessInjectorRunner::new_for_tests(
            &config,
            Arc::new({
                let build_count = Arc::clone(&build_count);
                let seen = Arc::clone(&seen);
                move |_config, _sender, _focus_cache| {
                    build_count.fetch_add(1, Ordering::Relaxed);
                    Arc::new(RecordingTextInjector {
                        seen: Arc::clone(&seen),
                    })
                }
            }),
            Arc::new({
                let sender_create_count = Arc::clone(&sender_create_count);
                move |_config| {
                    let attempt = sender_create_count.fetch_add(1, Ordering::Relaxed);
                    if attempt == 0 {
                        anyhow::bail!("synthetic /dev/uinput unavailable");
                    }
                    Ok(Arc::new(RecordingPasteChordSender {
                        sends: Arc::new(AtomicU64::new(0)),
                        fail: false,
                    }) as Arc<dyn PasteChordSender>)
                }
            }),
            Arc::new(|_| {}),
            Duration::from_millis(0),
            Duration::from_millis(5),
            None,
        );

        let _session_one = Uuid::new_v4();
        runner
            .run(&InjectionJob::new(_session_one, "first".to_string(), 0, 0))
            .expect("copy-only fallback should keep first run alive");
        std::thread::sleep(Duration::from_millis(6));
        let session_two = Uuid::new_v4();
        runner
            .run(&InjectionJob::new(session_two, "second".to_string(), 0, 0))
            .expect("second run should recover after retry backoff");

        assert_eq!(sender_create_count.load(Ordering::Relaxed), 2);
        assert_eq!(build_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen.lock()
                .expect("recording injector lock should be available")
                .as_slice(),
            &[("second".to_string(), session_two)]
        );
    }

    #[test]
    fn in_process_runner_drops_sender_after_explicit_send_error() {
        let sender_create_count = Arc::new(AtomicU64::new(0));
        let send_count = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let config = test_client_config();
        let runner = InProcessInjectorRunner::new_for_tests(
            &config,
            Arc::new({
                let seen = Arc::clone(&seen);
                move |_config, sender, _focus_cache| {
                    Arc::new(SenderDrivenInjector {
                        seen: Arc::clone(&seen),
                        sender,
                    })
                }
            }),
            Arc::new({
                let sender_create_count = Arc::clone(&sender_create_count);
                let send_count = Arc::clone(&send_count);
                move |_config| {
                    let generation = sender_create_count.fetch_add(1, Ordering::Relaxed);
                    Ok(Arc::new(RecordingPasteChordSender {
                        sends: Arc::clone(&send_count),
                        fail: generation == 0,
                    }) as Arc<dyn PasteChordSender>)
                }
            }),
            Arc::new(|_| {}),
            Duration::from_millis(0),
            Duration::from_millis(5),
            None,
        );

        let first = runner.run(&InjectionJob::new(
            Uuid::new_v4(),
            "first".to_string(),
            0,
            0,
        ));
        assert!(matches!(first, Err(InjectionRunError::BackendFailure(_))));

        let second = runner.run(&InjectionJob::new(
            Uuid::new_v4(),
            "second".to_string(),
            0,
            0,
        ));
        assert!(second.is_ok());
        assert_eq!(sender_create_count.load(Ordering::Relaxed), 2);
        assert_eq!(send_count.load(Ordering::Relaxed), 2);
        assert_eq!(
            seen.lock()
                .expect("sender-driven injector lock should be available")
                .len(),
            2
        );
    }

    #[test]
    fn hotkey_intent_diagnostics_tracks_intent_split_and_ignored_paths() {
        let mut diagnostics = HotkeyIntentDiagnostics::default();
        diagnostics.note_hotkey_down(SessionIntent::Dictate);
        diagnostics.note_hotkey_down(SessionIntent::LlmQuery);
        diagnostics.note_hotkey_down_ignored();
        diagnostics.note_hotkey_up();
        diagnostics.note_hotkey_up_ignored();

        assert_eq!(diagnostics.hotkey_down_total, 2);
        assert_eq!(diagnostics.hotkey_down_dictate_total, 1);
        assert_eq!(diagnostics.hotkey_down_llm_query_total, 1);
        assert_eq!(diagnostics.hotkey_down_ignored_total, 1);
        assert_eq!(diagnostics.hotkey_up_total, 1);
        assert_eq!(diagnostics.hotkey_up_ignored_total, 1);
    }

    #[test]
    fn hotkey_intent_diagnostics_tracks_llm_busy_rejections() {
        let mut diagnostics = HotkeyIntentDiagnostics::default();
        diagnostics.note_llm_busy_reject();
        diagnostics.note_llm_busy_reject();

        assert_eq!(diagnostics.llm_busy_reject_total, 2);
    }

    #[test]
    fn sanitize_model_answer_strips_think_blocks_without_raw_fallback() {
        assert_eq!(sanitize_model_answer("<think>hidden</think>"), "");
        assert_eq!(sanitize_model_answer("<think>hidden"), "");
        assert_eq!(
            sanitize_model_answer("<think>hidden</think> visible"),
            "visible"
        );
    }

    #[test]
    fn drain_sse_lines_handles_utf8_split_across_chunks() {
        let mut buffer = Vec::<u8>::new();

        let first = b"data: {\"choices\":[{\"delta\":{\"content\":\"caf";
        let second = b"\xC3\xA9\"}}]}\n";

        buffer.extend_from_slice(first);
        let first_lines = drain_sse_lines(&mut buffer, false).expect("first parse should succeed");
        assert!(first_lines.is_empty());

        buffer.extend_from_slice(second);
        let lines = drain_sse_lines(&mut buffer, false).expect("second parse should succeed");
        assert_eq!(
            lines,
            vec!["data: {\"choices\":[{\"delta\":{\"content\":\"café\"}}]}"]
        );
        assert!(buffer.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn injector_worker_preserves_fifo_order() {
        let (injector, seen) = RecordingInjectionRunner::shared();
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);

        worker
            .enqueue(InjectionJob::new(Uuid::new_v4(), "one".to_string(), 10, 20))
            .await
            .expect("first enqueue should pass");
        worker
            .enqueue(InjectionJob::new(Uuid::new_v4(), "two".to_string(), 11, 21))
            .await
            .expect("second enqueue should pass");
        worker
            .enqueue(InjectionJob::new(
                Uuid::new_v4(),
                "three".to_string(),
                12,
                22,
            ))
            .await
            .expect("third enqueue should pass");

        for _ in 0..3 {
            let report = timeout(Duration::from_secs(1), reports.recv())
                .await
                .expect("each report should arrive")
                .expect("worker should keep report channel open");
            assert!(report.error.is_none());
        }

        let ordered = seen
            .lock()
            .expect("recording lock should be available")
            .clone();
        assert_eq!(ordered, vec!["one", "two", "three"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn injector_worker_recovers_after_execution_timeout() {
        let calls = Arc::new(AtomicU64::new(0));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(TimeoutThenRecordingRunner {
            calls: Arc::clone(&calls),
            seen: Arc::clone(&seen),
            timeout_run_ms: INJECTOR_JOB_TIMEOUT_MS + 75,
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);

        let first_session = Uuid::new_v4();
        let second_session = Uuid::new_v4();
        worker
            .enqueue(InjectionJob::new(
                first_session,
                "first wedges".to_string(),
                1,
                1,
            ))
            .await
            .expect("first enqueue should pass");
        worker
            .enqueue(InjectionJob::new(
                second_session,
                "second still works".to_string(),
                2,
                2,
            ))
            .await
            .expect("second enqueue should pass");

        let first_report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("first report should arrive")
            .expect("report stream should remain open");
        assert_eq!(first_report.session_id, first_session);
        assert_eq!(
            first_report.error_kind,
            Some(InjectionErrorKind::ExecutionTimeout)
        );
        assert!(
            first_report
                .error
                .as_deref()
                .is_some_and(|error| error.contains("timed out")),
            "timeout report should explain the failure"
        );
        worker.metrics().note_report(&first_report);

        let second_report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("second report should arrive")
            .expect("report stream should remain open");
        assert_eq!(second_report.session_id, second_session);
        assert!(second_report.error.is_none());
        assert_eq!(second_report.error_kind, None);
        worker.metrics().note_report(&second_report);
        assert_eq!(
            seen.lock()
                .expect("recording lock should be available")
                .clone(),
            vec!["second still works".to_string()]
        );
        assert_eq!(
            worker
                .metrics()
                .worker_execution_timeout_total
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            worker
                .metrics()
                .worker_backend_failure_total
                .load(Ordering::Relaxed),
            0
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn injector_worker_timeout_kills_subprocess_tree_before_next_job_runs() {
        let log_path = std::env::temp_dir().join(format!(
            "parakeet-ptt-injector-worker-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("current time should be after epoch")
                .as_nanos()
        ));
        let script = make_test_script(
            "#!/usr/bin/env bash\nset -euo pipefail\nlog_path=\"$1\"\ntext=\"$(cat)\"\nif [ \"$text\" = \"first wedges\" ]; then\n  (\n    sleep 0.35\n    printf '%s\\n' \"$text\" >>\"$log_path\"\n  ) &\n  sleep 0.35\n  wait\n  exit 0\nfi\nprintf '%s\\n' \"$text\" >>\"$log_path\"\n",
        );
        let runner = Arc::new(InjectorSubprocessRunner::new_for_tests(
            script.clone(),
            vec![OsString::from(log_path.as_os_str())],
            Duration::from_millis(INJECTOR_JOB_TIMEOUT_MS),
        ));
        let (worker, mut reports) = spawn_injector_worker_with_capacity(runner, 4);

        let first_session = Uuid::new_v4();
        let second_session = Uuid::new_v4();
        worker
            .enqueue(InjectionJob::new(
                first_session,
                "first wedges".to_string(),
                1,
                1,
            ))
            .await
            .expect("first enqueue should pass");
        worker
            .enqueue(InjectionJob::new(
                second_session,
                "second survives".to_string(),
                2,
                2,
            ))
            .await
            .expect("second enqueue should pass");

        let first_report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("first report should arrive")
            .expect("report stream should remain open");
        assert_eq!(first_report.session_id, first_session);
        assert_eq!(
            first_report.error_kind,
            Some(InjectionErrorKind::ExecutionTimeout)
        );

        let second_report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("second report should arrive")
            .expect("report stream should remain open");
        assert_eq!(second_report.session_id, second_session);
        assert_eq!(second_report.error_kind, None);

        tokio::time::sleep(Duration::from_millis(450)).await;

        let written = fs::read_to_string(&log_path).expect("log file should be readable");
        assert_eq!(written, "second survives\n");

        fs::remove_file(&script).expect("test script should be removable");
        fs::remove_file(&log_path).expect("log file should be removable");
    }

    #[test]
    fn injector_subprocess_runner_does_not_wait_for_background_grandchild_stderr_close() {
        let log_path = std::env::temp_dir().join(format!(
            "parakeet-ptt-inherited-stderr-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("current time should be after epoch")
                .as_nanos()
        ));
        let script = make_test_script(
            "#!/usr/bin/env bash\nset -euo pipefail\nlog_path=\"$1\"\npayload=\"$(cat)\"\nprintf '%s %s\\n' \"$(date +%s%3N)\" \"$payload\" >>\"$log_path\"\n(sleep 0.4) &\nexit 0\n",
        );
        let runner = InjectorSubprocessRunner::new_for_tests(
            script.clone(),
            vec![OsString::from(log_path.as_os_str())],
            Duration::from_secs(2),
        );

        let started = Instant::now();
        runner
            .run(&InjectionJob::new(
                Uuid::new_v4(),
                "clipboard helper should not pin the worker".to_string(),
                0,
                0,
            ))
            .expect("runner should treat the injector subprocess as successful");
        let elapsed = started.elapsed();

        let written = fs::read_to_string(&log_path).expect("log file should be readable");
        assert!(
            written.contains("clipboard helper should not pin the worker"),
            "script should record the injected payload"
        );
        assert!(
            elapsed < Duration::from_millis(200),
            "runner should not wait for inherited stderr handles from background helpers, elapsed={elapsed:?}"
        );

        fs::remove_file(&script).expect("test script should be removable");
        fs::remove_file(&log_path).expect("log file should be removable");
    }

    #[test]
    fn pipe_reader_applies_deadline_only_after_it_is_started() {
        let (mut writer, reader) = UnixStream::pair().expect("unix stream pair should open");
        writer
            .write_all(b"partial stderr")
            .expect("writer should accept bytes");

        let stderr_reader = spawn_pipe_reader(reader, Duration::from_millis(40));
        assert!(
            matches!(
                stderr_reader
                    .receiver
                    .recv_timeout(Duration::from_millis(80)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "reader should keep waiting for EOF until the post-exit deadline is armed"
        );

        let started = Instant::now();
        stderr_reader.start_deadline();
        let outcome = collect_pipe_reader(stderr_reader, "stderr", Duration::from_millis(200))
            .expect("reader should return once the post-exit drain deadline elapses");
        let elapsed = started.elapsed();

        assert_eq!(outcome.bytes, b"partial stderr");
        assert!(
            outcome.timed_out,
            "open writers should force a timed post-exit drain result instead of blocking for EOF"
        );
        assert!(
            elapsed < Duration::from_millis(150),
            "pipe reader should stop itself near the post-exit drain deadline, elapsed={elapsed:?}"
        );

        drop(writer);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn background_helper_lifetime_does_not_delay_following_injection_jobs() {
        let log_path = std::env::temp_dir().join(format!(
            "parakeet-ptt-background-helper-queue-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("current time should be after epoch")
                .as_nanos()
        ));
        let script = make_test_script(
            "#!/usr/bin/env bash\nset -euo pipefail\nlog_path=\"$1\"\npayload=\"$(cat)\"\nprintf '%s %s\\n' \"$(date +%s%3N)\" \"$payload\" >>\"$log_path\"\n(sleep 0.4) &\nexit 0\n",
        );
        let runner = Arc::new(InjectorSubprocessRunner::new_for_tests(
            script.clone(),
            vec![OsString::from(log_path.as_os_str())],
            Duration::from_secs(2),
        ));
        let (worker, mut reports) = spawn_injector_worker_with_capacity(runner, 4);

        worker
            .enqueue(InjectionJob::new(Uuid::new_v4(), "first".to_string(), 1, 1))
            .await
            .expect("first enqueue should pass");
        worker
            .enqueue(InjectionJob::new(
                Uuid::new_v4(),
                "second".to_string(),
                2,
                2,
            ))
            .await
            .expect("second enqueue should pass");

        timeout(Duration::from_millis(250), reports.recv())
            .await
            .expect("first report should not wait for background helper exit")
            .expect("report stream should remain open");
        timeout(Duration::from_millis(250), reports.recv())
            .await
            .expect("second report should not be blocked behind the prior helper lifetime")
            .expect("report stream should remain open");

        let written = fs::read_to_string(&log_path).expect("log file should be readable");
        let entries = written
            .lines()
            .map(|line| {
                let (timestamp, payload) = line
                    .split_once(' ')
                    .expect("log line should contain timestamp and payload");
                (
                    timestamp
                        .parse::<u128>()
                        .expect("timestamp should parse as milliseconds"),
                    payload.to_string(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(entries.len(), 2, "both jobs should have executed");
        assert_eq!(entries[0].1, "first");
        assert_eq!(entries[1].1, "second");
        assert!(
            entries[1].0.saturating_sub(entries[0].0) < 200,
            "second job should start promptly instead of waiting for prior clipboard helper teardown"
        );

        fs::remove_file(&script).expect("test script should be removable");
        fs::remove_file(&log_path).expect("log file should be removable");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn enqueue_times_out_when_queue_remains_saturated() {
        let slow = Arc::new(SlowRunner {
            calls: Arc::new(AtomicU64::new(0)),
            sleep_ms: 200,
        });
        let (worker, _reports) = spawn_injector_worker_with_capacity(slow, 1);

        worker
            .enqueue(InjectionJob::new(Uuid::new_v4(), "first".to_string(), 1, 1))
            .await
            .expect("first enqueue should pass");

        yield_now().await;

        worker
            .enqueue(InjectionJob::new(
                Uuid::new_v4(),
                "second".to_string(),
                2,
                2,
            ))
            .await
            .expect("second enqueue should fill queue");

        let third = worker
            .enqueue(InjectionJob::new(Uuid::new_v4(), "third".to_string(), 3, 3))
            .await;
        assert_eq!(third, Err(EnqueueFailure::Timeout));
    }
}
