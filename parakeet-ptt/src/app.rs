//! Client application loop for Parakeet Client PTT Sessions.
//!
//! This module owns the Client runtime loop: PTT hotkey events, daemon Session
//! message dispatch, Overlay routing, and Injection queue coordination.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::{
    sleep, timeout, Duration as TokioDuration, Instant as TokioInstant, MissedTickBehavior,
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::audio_feedback::AudioFeedback;
use crate::client::WsClient;
use crate::client_session::{
    classify_error_code, elapsed_ms_since, final_result_belongs_to_active_session,
    handle_server_message, log_rejected_final_result, session_id_from_state, state_label,
    take_parent_focus_for_enqueue, CapturedParentFocus,
};
use crate::config::ClientConfig;
use crate::hotkey::{
    ensure_input_access, parse_pre_modifier_key_names, spawn_hotkey_loop, HotkeyEvent,
    HotkeyIntent, HotkeyTasks,
};
use crate::injector::{injector_metrics_snapshot, ParentFocusCapture};
use crate::injector_runtime::{
    build_injection_runner, spawn_injector_worker, EnqueueFailure, InjectionErrorKind,
    InjectionJob, InjectionJobRunner, InjectionOrigin, InjectionReport, InjectorWorkerHandle,
    INJECTION_ENQUEUE_TIMEOUT_MS, INJECTION_QUEUE_CAPACITY,
};
use crate::llm::{sanitize_model_answer, LlmAnswerer, LlmProgress};
use crate::overlay_router::{OverlayRouter, OverlaySink};
use crate::protocol::{start_message, stop_message, ClientMessage, ServerMessage};
use crate::state::PttState;
use crate::surface_focus::{WaylandFocusCache, WaylandFocusObservation};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionIntent {
    Dictate,
    LlmQuery,
}

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

fn maybe_defer_llm_session_end(
    message: &ServerMessage,
    state: &PttState,
    active_intent: Option<SessionIntent>,
    llm_in_flight_session: Option<Uuid>,
) -> Option<(Uuid, Option<String>)> {
    let ServerMessage::SessionEnded { session_id, reason } = message else {
        return None;
    };

    let waiting_for_llm_final = active_intent == Some(SessionIntent::LlmQuery)
        && session_id_from_state(state) == Some(*session_id);
    let llm_generation_running = llm_in_flight_session == Some(*session_id);

    if waiting_for_llm_final || llm_generation_running {
        Some((*session_id, reason.clone()))
    } else {
        None
    }
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

    let mut state = PttState::new();
    let Some(session_id) = state.begin_listening() else {
        return Err(anyhow!("failed to start session state"));
    };

    client
        .send(&start_message(session_id, Some("auto".to_string())))
        .await?;
    info!(session = %session_id, "start_session sent");

    // For demo purposes we immediately stop after starting.
    client.send(&stop_message(session_id)).await?;
    state.stop_listening();

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
                if !final_result_belongs_to_active_session(&state, session_id) {
                    log_rejected_final_result(session_id, &state, InjectionOrigin::Demo);
                    continue;
                }

                let to_inject = override_text.as_deref().unwrap_or(&text).to_string();
                info!(
                    session = %session_id,
                    latency_ms,
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
                state.reset();
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
    let focus_cache_for_parent_capture = focus_cache.clone();
    let mut overlay_router = OverlayRouter::new(overlay_sink, focus_cache);
    spawn_event_loop_lag_monitor();

    let mut state = PttState::new();
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
    let mut llm_busy = false;
    let mut active_intent: Option<SessionIntent> = None;
    let mut llm_in_flight_session: Option<Uuid> = None;
    let mut llm_seq: HashMap<Uuid, u64> = HashMap::new();
    let mut llm_overlay_text: HashMap<Uuid, String> = HashMap::new();
    let mut llm_deferred_session_end: HashMap<Uuid, Option<String>> = HashMap::new();
    let mut llm_busy_overlay_seq: u64 = 0;
    let llm_busy_overlay_session = Uuid::nil();
    let (llm_tx, mut llm_rx) = mpsc::unbounded_channel::<LlmProgress>();
    let mut hotkey_intent_diagnostics = HotkeyIntentDiagnostics::default();
    let mut last_hotkey_up_at: Option<TokioInstant> = None;
    let mut last_stop_message: Option<(Uuid, TokioInstant)> = None;
    let mut parent_focus_by_session = HashMap::<Uuid, CapturedParentFocus>::new();

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
                                        if llm_busy {
                                            warn!("ignoring hotkey down while LLM response is in progress");
                                            llm_busy_overlay_seq = llm_busy_overlay_seq.saturating_add(1);
                                            overlay_router.route_interim_state(
                                                None,
                                                llm_busy_overlay_session,
                                                llm_busy_overlay_seq,
                                                "LLM busy; wait for current answer".to_string(),
                                            );
                                            overlay_router.route_session_ended(
                                                None,
                                                llm_busy_overlay_session,
                                                Some("busy".to_string()),
                                            );
                                            hotkey_intent_diagnostics.note_llm_busy_reject();
                                            hotkey_intent_diagnostics.maybe_log_summary("hotkey_down_busy");
                                            continue;
                                        }
                                        if let Some(session_id) = state.begin_listening() {
                                            active_intent = Some(intent);
                                            let message = start_message(session_id, Some("auto".to_string()));
                                            send_message(daemon.as_mut(), &message).await?;
                                            info!(session = %session_id, ?intent, "start_session sent (hotkey down)");
                                        } else {
                                            hotkey_intent_diagnostics.note_hotkey_down_ignored();
                                            debug!(
                                                ?state,
                                                ?active_intent,
                                                "ignoring hotkey down because client is not idle"
                                            );
                                        }
                                        hotkey_intent_diagnostics.maybe_log_summary("hotkey_down");
                                    }
                                    HotkeyEvent::Up => {
                                        hotkey_intent_diagnostics.note_hotkey_up();
                                        if let Some(session_id) = state.stop_listening() {
                                            let message = stop_message(session_id);
                                            send_message(daemon.as_mut(), &message).await?;
                                            let now = TokioInstant::now();
                                            last_hotkey_up_at = Some(now);
                                            last_stop_message = Some((session_id, now));
                                            if let Some(focus) = capture_parent_focus(focus_cache_for_parent_capture.as_ref()) {
                                                parent_focus_by_session.insert(
                                                    session_id,
                                                    CapturedParentFocus {
                                                        focus,
                                                        captured_at: now,
                                                    },
                                                );
                                            }
                                            info!(session = %session_id, "stop_session sent (hotkey up)");
                                        } else {
                                            hotkey_intent_diagnostics.note_hotkey_up_ignored();
                                            debug!(
                                                ?state,
                                                ?active_intent,
                                                "ignoring hotkey up because no listening session is active"
                                            );
                                        }
                                        hotkey_intent_diagnostics.maybe_log_summary("hotkey_up");
                                    }
                                }
                            }
                            next = daemon.next_message() => {
                                match next {
                                    Ok(Some(message)) => {
                                        if let Some((session_id, reason)) = maybe_defer_llm_session_end(
                                            &message,
                                            &state,
                                            active_intent,
                                            llm_in_flight_session,
                                        ) {
                                            llm_deferred_session_end.insert(session_id, reason);
                                            debug!(
                                                session = %session_id,
                                                "deferring daemon session_ended until llm answer injection"
                                            );
                                            continue;
                                        }

                                        if let ServerMessage::FinalResult { session_id, .. } = &message {
                                            if !final_result_belongs_to_active_session(&state, *session_id) {
                                                log_rejected_final_result(
                                                    *session_id,
                                                    &state,
                                                    match active_intent {
                                                        Some(SessionIntent::LlmQuery) => InjectionOrigin::LlmAnswer,
                                                        _ => InjectionOrigin::RawFinalResult,
                                                    },
                                                );
                                                continue;
                                            }
                                        }

                                        match message {
                                            ServerMessage::FinalResult {
                                                session_id,
                                                text,
                                                latency_ms,
                                                audio_ms,
                                                ..
                                            } if active_intent == Some(SessionIntent::LlmQuery) => {
                                                info!(
                                                    session = %session_id,
                                                    latency_ms,
                                                    audio_ms,
                                                    "final result received in llm_query mode"
                                                );
                                                llm_busy = true;
                                                llm_in_flight_session = Some(session_id);
                                                let seq = llm_seq.entry(session_id).or_insert(0);
                                                *seq = seq.saturating_add(1);
                                                overlay_router.route_interim_state(
                                                    None,
                                                    session_id,
                                                    *seq,
                                                    "Generating answer...".to_string(),
                                                );
                                                state.reset();
                                                let answerer = Arc::clone(&llm_answerer);
                                                let progress_tx = llm_tx.clone();
                                                tokio::spawn(async move {
                                                    let llm_result = answerer
                                                        .answer(session_id, text.clone(), progress_tx.clone())
                                                        .await
                                                        .map_err(|err| format!("{err:#}"));
                                                    let _ = progress_tx.send(LlmProgress::Finished {
                                                        session_id,
                                                        transcript: text,
                                                        daemon_latency_ms: latency_ms,
                                                        daemon_audio_ms: audio_ms,
                                                        result: llm_result,
                                                    });
                                                });
                                                active_intent = None;
                                            }
                                            known => {
                                                let clear_intent = matches!(
                                                    &known,
                                                    ServerMessage::FinalResult { .. } | ServerMessage::Error { .. }
                                                );
                                                handle_server_message(
                                                    known,
                                                    &mut state,
                                                    &mut overlay_router,
                                                    &injector_worker,
                                                    &mut parent_focus_by_session,
                                                    last_hotkey_up_at,
                                                    last_stop_message,
                                                ).await?;
                                                if clear_intent {
                                                    active_intent = None;
                                                }
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
                                handle_injection_report(
                                    &injector_worker,
                                    report,
                                    &mut overlay_router,
                                    &audio_feedback,
                                );
                            }
                            Some(progress) = llm_rx.recv() => {
                                match progress {
                                    LlmProgress::Delta { session_id, delta } => {
                                        if llm_in_flight_session != Some(session_id) {
                                            debug!(
                                                session = %session_id,
                                                in_flight_session = ?llm_in_flight_session,
                                                "ignoring stale llm delta for non-active session"
                                            );
                                            continue;
                                        }
                                        let entry = llm_overlay_text.entry(session_id).or_default();
                                        entry.push_str(&delta);
                                        let seq = llm_seq.entry(session_id).or_insert(0);
                                        *seq = seq.saturating_add(1);
                                        overlay_router.route_interim_text(None, session_id, *seq, entry.clone());
                                    }
                                    LlmProgress::Finished {
                                        session_id,
                                        transcript,
                                        daemon_latency_ms,
                                        daemon_audio_ms,
                                        result,
                                    } => {
                                        if llm_in_flight_session != Some(session_id) {
                                            warn!(
                                                session = %session_id,
                                                in_flight_session = ?llm_in_flight_session,
                                                "ignoring stale llm completion for non-active session"
                                            );
                                            llm_seq.remove(&session_id);
                                            llm_overlay_text.remove(&session_id);
                                            llm_deferred_session_end.remove(&session_id);
                                            continue;
                                        }

                                        llm_busy = false;
                                        llm_in_flight_session = None;
                                        llm_seq.remove(&session_id);
                                        llm_overlay_text.remove(&session_id);
                                        let session_end_reason =
                                            llm_deferred_session_end.remove(&session_id).flatten();
                                        let session_end_was_deferred = session_end_reason.is_some();
                                        overlay_router.route_session_ended(
                                            None,
                                            session_id,
                                            session_end_reason,
                                        );
                                        let fallback_transcript = transcript.clone();
                                        let response_text = match result {
                                            Ok(answer) => {
                                                let sanitized = sanitize_model_answer(&answer);
                                                info!(
                                                    session = %session_id,
                                                    answer_chars = sanitized.chars().count(),
                                                    "llm response completed"
                                                );
                                                sanitized
                                            }
                                            Err(error) => {
                                                warn!(
                                                    session = %session_id,
                                                    error = %error,
                                                    "llm generation failed; falling back to raw transcript"
                                                );
                                                fallback_transcript.clone()
                                            }
                                        };

                                        let to_inject = if response_text.trim().is_empty() {
                                            warn!(session = %session_id, "llm response empty after sanitization; falling back to transcript");
                                            fallback_transcript
                                        } else {
                                            response_text
                                        };
                                        let hotkey_up_elapsed_ms_at_enqueue =
                                            elapsed_ms_since(last_hotkey_up_at);
                                        let stop_message_elapsed_ms_at_enqueue =
                                            last_stop_message.and_then(|(stopped_session_id, instant)| {
                                                (stopped_session_id == session_id)
                                                    .then(|| instant.elapsed().as_millis() as u64)
                                            });
                                        info!(
                                            session = %session_id,
                                            origin = InjectionOrigin::LlmAnswer.as_str(),
                                            state_at_enqueue = state_label(&state),
                                            session_end_was_deferred,
                                            hotkey_up_elapsed_ms_at_enqueue,
                                            stop_message_elapsed_ms_at_enqueue,
                                            response_chars = to_inject.chars().count(),
                                            "queueing llm answer injection job"
                                        );

                                        match injector_worker
                                            .enqueue(
                                                InjectionJob::new(
                                                    session_id,
                                                    to_inject,
                                                    daemon_latency_ms,
                                                    daemon_audio_ms,
                                                )
                                                .with_origin(InjectionOrigin::LlmAnswer)
                                                .with_enqueue_timing(
                                                    hotkey_up_elapsed_ms_at_enqueue,
                                                    stop_message_elapsed_ms_at_enqueue,
                                                )
                                                .with_parent_focus(take_parent_focus_for_enqueue(
                                                    &mut parent_focus_by_session,
                                                    session_id,
                                                )),
                                            )
                                            .await
                                        {
                                            Ok(()) => debug!(session = %session_id, "llm final answer queued for injector worker"),
                                            Err(EnqueueFailure::Timeout) => {
                                                warn!(
                                                    session = %session_id,
                                                    queue_capacity = INJECTION_QUEUE_CAPACITY,
                                                    enqueue_timeout_ms = INJECTION_ENQUEUE_TIMEOUT_MS,
                                                    "injector queue remained full; dropping llm final answer"
                                                );
                                            }
                                            Err(EnqueueFailure::WorkerGone) => {
                                                warn!(session = %session_id, "injector worker unavailable; dropping llm final answer");
                                            }
                                        }
                                    }
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
                state.reset();
                active_intent = None;
                clear_transient_session_state(TransientSessionState {
                    llm_busy: &mut llm_busy,
                    llm_in_flight_session: &mut llm_in_flight_session,
                    llm_seq: &mut llm_seq,
                    llm_overlay_text: &mut llm_overlay_text,
                    llm_deferred_session_end: &mut llm_deferred_session_end,
                    parent_focus_by_session: &mut parent_focus_by_session,
                    last_hotkey_up_at: &mut last_hotkey_up_at,
                    last_stop_message: &mut last_stop_message,
                });
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

fn handle_injection_report(
    worker: &InjectorWorkerHandle,
    report: InjectionReport,
    overlay_router: &mut OverlayRouter<impl OverlaySink>,
    audio_feedback: &AudioFeedback,
) {
    worker.metrics().note_report(&report);
    let success = report.error_kind.is_none() && report.error.is_none();
    match (report.error_kind, report.error) {
        (Some(error_kind), Some(error)) => {
            warn!(
                session = %report.session_id,
                origin = report.origin.as_str(),
                error_kind = error_kind.as_str(),
                daemon_latency_ms = report.daemon_latency_ms,
                daemon_audio_ms = report.daemon_audio_ms,
                queue_wait_ms = report.queue_wait_ms,
                run_ms = report.run_ms,
                total_worker_ms = report.total_worker_ms,
                hotkey_up_elapsed_ms_at_enqueue = report.hotkey_up_elapsed_ms_at_enqueue,
                stop_message_elapsed_ms_at_enqueue = report.stop_message_elapsed_ms_at_enqueue,
                hotkey_up_elapsed_ms_at_worker_start = report.hotkey_up_elapsed_ms_at_worker_start,
                stop_message_elapsed_ms_at_worker_start = report.stop_message_elapsed_ms_at_worker_start,
                error = %error,
                "injector worker reported failure"
            );
        }
        (None, None) => {
            info!(
                session = %report.session_id,
                origin = report.origin.as_str(),
                daemon_latency_ms = report.daemon_latency_ms,
                daemon_audio_ms = report.daemon_audio_ms,
                queue_wait_ms = report.queue_wait_ms,
                run_ms = report.run_ms,
                total_worker_ms = report.total_worker_ms,
                hotkey_up_elapsed_ms_at_enqueue = report.hotkey_up_elapsed_ms_at_enqueue,
                stop_message_elapsed_ms_at_enqueue = report.stop_message_elapsed_ms_at_enqueue,
                hotkey_up_elapsed_ms_at_worker_start = report.hotkey_up_elapsed_ms_at_worker_start,
                stop_message_elapsed_ms_at_worker_start = report.stop_message_elapsed_ms_at_worker_start,
                "injector worker completed job"
            );
            audio_feedback.play_completion();
        }
        (error_kind, error) => {
            warn!(
                session = %report.session_id,
                origin = report.origin.as_str(),
                error_kind = error_kind.map(InjectionErrorKind::as_str),
                daemon_latency_ms = report.daemon_latency_ms,
                daemon_audio_ms = report.daemon_audio_ms,
                queue_wait_ms = report.queue_wait_ms,
                run_ms = report.run_ms,
                total_worker_ms = report.total_worker_ms,
                hotkey_up_elapsed_ms_at_enqueue = report.hotkey_up_elapsed_ms_at_enqueue,
                stop_message_elapsed_ms_at_enqueue = report.stop_message_elapsed_ms_at_enqueue,
                hotkey_up_elapsed_ms_at_worker_start = report.hotkey_up_elapsed_ms_at_worker_start,
                stop_message_elapsed_ms_at_worker_start = report.stop_message_elapsed_ms_at_worker_start,
                error = ?error,
                "injector worker reported inconsistent error classification"
            );
        }
    }

    let processed = worker
        .metrics()
        .worker_success_total
        .load(Ordering::Relaxed)
        + worker
            .metrics()
            .worker_failure_total
            .load(Ordering::Relaxed);
    if processed.is_multiple_of(25) && processed > 0 {
        worker.metrics().log_summary();
        let snapshot = injector_metrics_snapshot();
        info!(
            clipboard_ready_success_total = snapshot.clipboard_ready_success_total,
            clipboard_ready_failure_total = snapshot.clipboard_ready_failure_total,
            clipboard_ready_duration_ms_total = snapshot.clipboard_ready_duration_ms_total,
            route_shortcut_success_total = snapshot.route_shortcut_success_total,
            route_shortcut_failure_total = snapshot.route_shortcut_failure_total,
            route_shortcut_duration_ms_total = snapshot.route_shortcut_duration_ms_total,
            backend_success_total = snapshot.backend_success_total,
            backend_failure_total = snapshot.backend_failure_total,
            backend_duration_ms_total = snapshot.backend_duration_ms_total,
            wl_copy_spawn_total = snapshot.wl_copy_spawn_total,
            wl_paste_spawn_total = snapshot.wl_paste_spawn_total,
            "injector stage metrics summary"
        );
    }

    overlay_router.route_injection_complete(report.session_id, success);
}

fn capture_parent_focus(focus_cache: Option<&WaylandFocusCache>) -> Option<ParentFocusCapture> {
    let cache = focus_cache?;
    match cache.observe(30_000, 500) {
        WaylandFocusObservation::Fresh {
            snapshot,
            cache_age_ms,
        } => Some(ParentFocusCapture {
            snapshot: Some(snapshot),
            source_selected: "wayland_cache".to_string(),
            wayland_cache_age_ms: Some(cache_age_ms),
            wayland_fallback_reason: None,
            captured_elapsed_ms: Some(0),
        }),
        WaylandFocusObservation::LowConfidence {
            snapshot,
            cache_age_ms,
            reason,
        } => Some(ParentFocusCapture {
            snapshot: Some(snapshot),
            source_selected: "wayland_cache_low_confidence".to_string(),
            wayland_cache_age_ms: Some(cache_age_ms),
            wayland_fallback_reason: Some(reason.to_string()),
            captured_elapsed_ms: Some(0),
        }),
        WaylandFocusObservation::Unavailable {
            reason,
            cache_age_ms,
        } => Some(ParentFocusCapture {
            snapshot: None,
            source_selected: "wayland_unavailable".to_string(),
            wayland_cache_age_ms: cache_age_ms,
            wayland_fallback_reason: Some(reason.to_string()),
            captured_elapsed_ms: Some(0),
        }),
    }
}

struct TransientSessionState<'a> {
    llm_busy: &'a mut bool,
    llm_in_flight_session: &'a mut Option<Uuid>,
    llm_seq: &'a mut HashMap<Uuid, u64>,
    llm_overlay_text: &'a mut HashMap<Uuid, String>,
    llm_deferred_session_end: &'a mut HashMap<Uuid, Option<String>>,
    parent_focus_by_session: &'a mut HashMap<Uuid, CapturedParentFocus>,
    last_hotkey_up_at: &'a mut Option<TokioInstant>,
    last_stop_message: &'a mut Option<(Uuid, TokioInstant)>,
}

fn clear_transient_session_state(transient: TransientSessionState<'_>) {
    let TransientSessionState {
        llm_busy,
        llm_in_flight_session,
        llm_seq,
        llm_overlay_text,
        llm_deferred_session_end,
        parent_focus_by_session,
        last_hotkey_up_at,
        last_stop_message,
    } = transient;

    *llm_busy = false;
    *llm_in_flight_session = None;
    llm_seq.clear();
    llm_overlay_text.clear();
    llm_deferred_session_end.clear();
    parent_focus_by_session.clear();
    *last_hotkey_up_at = None;
    *last_stop_message = None;
}

fn session_intent_from_hotkey(intent: HotkeyIntent) -> SessionIntent {
    match intent {
        HotkeyIntent::Dictate => SessionIntent::Dictate,
        HotkeyIntent::LlmQuery => SessionIntent::LlmQuery,
    }
}

#[derive(Debug, Deserialize)]
struct StatusInfo {
    state: Option<String>,
    sessions_active: Option<u32>,
    gpu_mem_mb: Option<u64>,
    device: Option<String>,
    effective_device: Option<String>,
    streaming_enabled: Option<bool>,
    stream_helper_active: Option<bool>,
    stream_fallback_reason: Option<String>,
    chunk_secs: Option<f64>,
    active_session_age_ms: Option<u64>,
    audio_stop_ms: Option<u64>,
    finalize_ms: Option<u64>,
    infer_ms: Option<u64>,
    send_ms: Option<u64>,
    last_audio_ms: Option<u64>,
    last_infer_ms: Option<u64>,
    last_send_ms: Option<u64>,
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
        Ok(response) => match response.json::<StatusInfo>().await {
            Ok(status) => {
                info!(
                    "Daemon status: state={:?}, sessions_active={:?}, device={:?}, effective_device={:?}, \
streaming={:?}, helper_active={:?}, fallback={:?}, chunk_secs={:?}, active_age_ms={:?}, \
audio_stop_ms={:?}, finalize_ms={:?}, infer_ms={:?}, send_ms={:?}, last_audio_ms={:?}, \
last_infer_ms={:?}, last_send_ms={:?}, gpu_mem_mb={:?}",
                    status.state,
                    status.sessions_active,
                    status.device,
                    status.effective_device,
                    status.streaming_enabled,
                    status.stream_helper_active,
                    status.stream_fallback_reason,
                    status.chunk_secs,
                    status.active_session_age_ms,
                    status.audio_stop_ms,
                    status.finalize_ms,
                    status.infer_ms,
                    status.send_ms,
                    status.last_audio_ms,
                    status.last_infer_ms,
                    status.last_send_ms,
                    status.gpu_mem_mb
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
    use std::collections::HashMap;
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

    use crate::audio_feedback::AudioFeedback;
    use crate::client_runtime_fixtures::{
        ClientRuntimeHarness, RecordingInjectionRunner, RecordingOverlaySink,
    };
    use crate::config::{
        ClientConfig, ClipboardOptions, InjectionConfig, InjectionMode, PasteBackendFailurePolicy,
        PasteKeyBackend, PasteShortcut,
    };
    use crate::hotkey::HotkeyIntent;
    use crate::injector::{
        FailInjector, InjectorContext, ParentFocusCapture, PasteChordSender, PasteKeySender,
        TextInjector,
    };
    use crate::overlay_router::{OverlayEvent, OverlayRouter};
    use crate::protocol::{ClientMessage, ServerMessage};
    use crate::state::PttState;

    use crate::injector::INJECTOR_JOB_TIMEOUT_MS;
    use crate::injector_runtime::{
        collect_pipe_reader, spawn_injector_worker_with_capacity, spawn_pipe_reader,
        EnqueueFailure, InProcessInjectorRunner, InjectionErrorKind, InjectionJob,
        InjectionJobRunner, InjectionOrigin, InjectionReport, InjectionRunError,
        InjectionRunOutput, InjectorSubprocessRunner, UinputSenderState,
    };
    use crate::llm::{drain_sse_lines, sanitize_model_answer};

    use super::{
        clear_transient_session_state, handle_injection_report, maybe_defer_llm_session_end, run,
        CapturedParentFocus, HotkeyIntentDiagnostics, SessionIntent, TransientSessionState,
    };

    #[test]
    fn clear_transient_session_state_resets_reconnect_caches() {
        let session_id = Uuid::new_v4();
        let mut llm_busy = true;
        let mut llm_in_flight_session = Some(session_id);
        let mut llm_seq = HashMap::from([(session_id, 7)]);
        let mut llm_overlay_text = HashMap::from([(session_id, "partial answer".to_string())]);
        let mut llm_deferred_session_end =
            HashMap::from([(session_id, Some("connection_drop".to_string()))]);
        let mut parent_focus_by_session = HashMap::from([(
            session_id,
            CapturedParentFocus {
                focus: ParentFocusCapture {
                    snapshot: None,
                    source_selected: "test".to_string(),
                    wayland_cache_age_ms: None,
                    wayland_fallback_reason: None,
                    captured_elapsed_ms: None,
                },
                captured_at: tokio::time::Instant::now(),
            },
        )]);
        let mut last_hotkey_up_at = Some(tokio::time::Instant::now());
        let mut last_stop_message = Some((session_id, tokio::time::Instant::now()));

        clear_transient_session_state(TransientSessionState {
            llm_busy: &mut llm_busy,
            llm_in_flight_session: &mut llm_in_flight_session,
            llm_seq: &mut llm_seq,
            llm_overlay_text: &mut llm_overlay_text,
            llm_deferred_session_end: &mut llm_deferred_session_end,
            parent_focus_by_session: &mut parent_focus_by_session,
            last_hotkey_up_at: &mut last_hotkey_up_at,
            last_stop_message: &mut last_stop_message,
        });

        assert!(!llm_busy);
        assert!(llm_in_flight_session.is_none());
        assert!(llm_seq.is_empty());
        assert!(llm_overlay_text.is_empty());
        assert!(llm_deferred_session_end.is_empty());
        assert!(parent_focus_by_session.is_empty());
        assert!(last_hotkey_up_at.is_none());
        assert!(last_stop_message.is_none());
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
            "ws://127.0.0.1:8765/ws",
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
    async fn app_injection_report_error_routes_failure_classification() {
        let (injector, _seen) = RecordingInjectionRunner::shared();
        let (worker, _reports) = spawn_injector_worker_with_capacity(injector, 4);
        let session_id = Uuid::new_v4();
        let (overlay_sink, seen_overlay_events) = RecordingOverlaySink::shared();
        let mut overlay_router = OverlayRouter::new(overlay_sink, None);

        handle_injection_report(
            &worker,
            InjectionReport {
                session_id,
                daemon_latency_ms: 20,
                daemon_audio_ms: 1000,
                origin: InjectionOrigin::RawFinalResult,
                queue_wait_ms: 1,
                run_ms: 2,
                total_worker_ms: 3,
                hotkey_up_elapsed_ms_at_enqueue: None,
                stop_message_elapsed_ms_at_enqueue: None,
                hotkey_up_elapsed_ms_at_worker_start: None,
                stop_message_elapsed_ms_at_worker_start: None,
                error_kind: Some(InjectionErrorKind::BackendFailure),
                error: Some("stage=backend synthetic failure".to_string()),
            },
            &mut overlay_router,
            &AudioFeedback::new(false, None, 0),
        );

        assert_eq!(
            InjectionErrorKind::BackendFailure.as_str(),
            "backend_failure"
        );
        assert_eq!(
            worker
                .metrics()
                .worker_backend_failure_total
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            seen_overlay_events
                .lock()
                .expect("overlay recording lock should be available")
                .as_slice(),
            &[OverlayEvent::InjectionComplete {
                session_id,
                success: false,
            }]
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

    #[test]
    fn maybe_defer_llm_session_end_for_query_or_inflight() {
        let session_id = Uuid::new_v4();
        let state = PttState::Listening { session_id };
        let message = ServerMessage::SessionEnded {
            session_id,
            reason: Some("normal".to_string()),
        };

        let deferred_for_query =
            maybe_defer_llm_session_end(&message, &state, Some(SessionIntent::LlmQuery), None);
        assert_eq!(
            deferred_for_query,
            Some((session_id, Some("normal".to_string())))
        );

        let deferred_for_inflight =
            maybe_defer_llm_session_end(&message, &PttState::Idle, None, Some(session_id));
        assert_eq!(
            deferred_for_inflight,
            Some((session_id, Some("normal".to_string())))
        );

        let not_deferred =
            maybe_defer_llm_session_end(&message, &state, Some(SessionIntent::Dictate), None);
        assert_eq!(not_deferred, None);
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
