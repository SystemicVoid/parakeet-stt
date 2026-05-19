//! Client Session dispatch policy for Daemon messages.
//!
//! This Module owns how Daemon `ServerMessage` values affect Client PTT state,
//! Overlay routing, Injection enqueueing, and parent-focus handoff.

use std::collections::HashMap;

use anyhow::Result;
use tokio::time::Instant as TokioInstant;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::injector::ParentFocusCapture;
use crate::injector_runtime::{
    EnqueueFailure, InjectionJob, InjectionOrigin, InjectorWorkerHandle,
    INJECTION_ENQUEUE_TIMEOUT_MS, INJECTION_QUEUE_CAPACITY,
};
use crate::overlay_router::{OverlayRouter, OverlaySink};
use crate::protocol::ServerMessage;
use crate::state::PttState;

#[derive(Debug, Clone)]
pub(crate) struct CapturedParentFocus {
    pub(crate) focus: ParentFocusCapture,
    pub(crate) captured_at: TokioInstant,
}

pub(crate) async fn handle_server_message<S: OverlaySink>(
    message: ServerMessage,
    state: &mut PttState,
    overlay_router: &mut OverlayRouter<S>,
    injector_worker: &InjectorWorkerHandle,
    parent_focus_by_session: &mut HashMap<Uuid, CapturedParentFocus>,
    last_hotkey_up_at: Option<TokioInstant>,
    last_stop_message: Option<(Uuid, TokioInstant)>,
) -> Result<()> {
    match message {
        ServerMessage::SessionStarted { session_id, .. } => {
            info!(session = %session_id, "session started ack");
            overlay_router.note_session_started(session_id);
        }
        ServerMessage::FinalResult {
            session_id,
            text,
            latency_ms,
            audio_ms,
            ..
        } => {
            if !final_result_belongs_to_active_session(state, session_id) {
                log_rejected_final_result(session_id, state, InjectionOrigin::RawFinalResult);
                return Ok(());
            }

            let hotkey_up_elapsed_ms_at_enqueue = elapsed_ms_since(last_hotkey_up_at);
            let stop_message_elapsed_ms_at_enqueue =
                last_stop_message.and_then(|(stopped_session_id, instant)| {
                    (stopped_session_id == session_id).then(|| instant.elapsed().as_millis() as u64)
                });
            info!(
                session = %session_id,
                origin = InjectionOrigin::RawFinalResult.as_str(),
                daemon_latency_ms = latency_ms,
                audio_ms,
                state_at_enqueue = state_label(state),
                hotkey_up_elapsed_ms_at_enqueue,
                stop_message_elapsed_ms_at_enqueue,
                "final result received"
            );
            match injector_worker
                .enqueue(
                    InjectionJob::new(session_id, text, latency_ms, audio_ms)
                        .with_origin(InjectionOrigin::RawFinalResult)
                        .with_enqueue_timing(
                            hotkey_up_elapsed_ms_at_enqueue,
                            stop_message_elapsed_ms_at_enqueue,
                        )
                        .with_parent_focus(take_parent_focus_for_enqueue(
                            parent_focus_by_session,
                            session_id,
                        )),
                )
                .await
            {
                Ok(()) => {
                    debug!(session = %session_id, "final result queued for injector worker");
                }
                Err(EnqueueFailure::Timeout) => {
                    warn!(
                        session = %session_id,
                        queue_capacity = INJECTION_QUEUE_CAPACITY,
                        enqueue_timeout_ms = INJECTION_ENQUEUE_TIMEOUT_MS,
                        "injector queue remained full; dropping final result injection job"
                    );
                }
                Err(EnqueueFailure::WorkerGone) => {
                    warn!(
                        session = %session_id,
                        "injector worker unavailable; dropping final result injection job"
                    );
                }
            }
            state.reset();
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
            if let Some(session_id) = session_id {
                parent_focus_by_session.remove(&session_id);
            }
            state.reset();
        }
        ServerMessage::InterimState {
            session_id,
            seq,
            state: interim_state,
        } => {
            overlay_router.route_interim_state(
                session_id_from_state(state),
                session_id,
                seq,
                interim_state,
            );
        }
        ServerMessage::InterimText {
            session_id,
            seq,
            text,
        } => {
            overlay_router.route_interim_text(session_id_from_state(state), session_id, seq, text);
        }
        ServerMessage::AudioLevel {
            session_id,
            level_db,
        } => {
            overlay_router.route_audio_level(session_id_from_state(state), session_id, level_db);
        }
        ServerMessage::SessionEnded { session_id, reason } => {
            parent_focus_by_session.remove(&session_id);
            overlay_router.route_session_ended(session_id_from_state(state), session_id, reason);
        }
        ServerMessage::SessionWarning {
            session_id,
            warning: _,
            remaining_seconds,
            limit_seconds,
        } => {
            info!(
                session = %session_id,
                remaining_seconds,
                limit_seconds,
                "session warning: approaching limit"
            );
            overlay_router.route_session_warning(session_id);
        }
        ServerMessage::Status { .. } => {}
    }
    Ok(())
}

pub(crate) fn take_parent_focus_for_enqueue(
    parent_focus_by_session: &mut HashMap<Uuid, CapturedParentFocus>,
    session_id: Uuid,
) -> Option<ParentFocusCapture> {
    parent_focus_by_session.remove(&session_id).map(|captured| {
        let mut focus = captured.focus;
        focus.captured_elapsed_ms = Some(captured.captured_at.elapsed().as_millis() as u64);
        focus
    })
}

pub(crate) fn session_id_from_state(state: &PttState) -> Option<Uuid> {
    match *state {
        PttState::Idle => None,
        PttState::Listening { session_id } | PttState::WaitingResult { session_id } => {
            Some(session_id)
        }
    }
}

pub(crate) fn final_result_belongs_to_active_session(state: &PttState, session_id: Uuid) -> bool {
    session_id_from_state(state) == Some(session_id)
}

pub(crate) fn log_rejected_final_result(
    session_id: Uuid,
    state: &PttState,
    origin: InjectionOrigin,
) {
    warn!(
        session = %session_id,
        active_session = ?session_id_from_state(state),
        origin = origin.as_str(),
        state_at_receive = state_label(state),
        "ignoring final result for non-active session"
    );
}

pub(crate) fn state_label(state: &PttState) -> &'static str {
    match state {
        PttState::Idle => "idle",
        PttState::Listening { .. } => "listening",
        PttState::WaitingResult { .. } => "waiting_result",
    }
}

pub(crate) fn elapsed_ms_since(instant: Option<TokioInstant>) -> Option<u64> {
    instant.map(|value| value.elapsed().as_millis() as u64)
}

pub(crate) fn classify_error_code(code: &str) -> &'static str {
    match code {
        "SESSION_BUSY" => "session_busy",
        "SESSION_NOT_FOUND" => "session_not_found",
        "SESSION_ABORTED" => "session_aborted",
        "AUDIO_DEVICE" => "audio_device",
        "MODEL" => "model",
        "INVALID_REQUEST" => "invalid_request",
        "UNEXPECTED" => "unexpected",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::config::OverlayMode;
    use crate::injector_runtime::{
        spawn_injector_worker_with_capacity, InjectionJob, InjectionJobRunner, InjectionRunError,
        InjectionRunOutput, InjectorWorkerHandle,
    };
    use crate::overlay_process::{
        OverlayProcessManager, OverlayProcessMetrics, OverlayProcessSink,
    };
    use crate::overlay_router::{
        NoopOverlaySink, OverlayEvent, OverlayRouter, OverlaySink, RuntimeOverlaySink,
    };
    use crate::protocol::ServerMessage;
    use crate::state::PttState;
    use anyhow::anyhow;
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Instant as TokioInstant};
    use uuid::Uuid;

    use super::handle_server_message;

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

    struct RecordingRunner {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl InjectionJobRunner for RecordingRunner {
        fn run(
            &self,
            job: &InjectionJob,
        ) -> std::result::Result<InjectionRunOutput, InjectionRunError> {
            self.seen
                .lock()
                .expect("recording lock should be available")
                .push(job.text.to_string());
            Ok(InjectionRunOutput::default())
        }
    }

    struct RecordingOverlaySink {
        seen: Arc<Mutex<Vec<OverlayEvent>>>,
    }

    impl OverlaySink for RecordingOverlaySink {
        fn on_overlay_event(&mut self, event: OverlayEvent) {
            self.seen
                .lock()
                .expect("overlay recording lock should be available")
                .push(event);
        }
    }

    async fn handle_server_message_for_tests<S: OverlaySink>(
        message: ServerMessage,
        state: &mut PttState,
        overlay_router: &mut OverlayRouter<S>,
        injector_worker: &InjectorWorkerHandle,
    ) -> anyhow::Result<()> {
        let mut parent_focus_by_session = HashMap::new();
        handle_server_message(
            message,
            state,
            overlay_router,
            injector_worker,
            &mut parent_focus_by_session,
            None,
            None,
        )
        .await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn final_result_enqueues_injection_job() {
        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);
        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink, None);

        handle_server_message_for_tests(
            ServerMessage::FinalResult {
                session_id,
                text: "direct final result".to_string(),
                latency_ms: 44,
                audio_ms: 1200,
                lang: Some("en".to_string()),
                confidence: Some(0.92),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("final result should enqueue");

        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("injection report should arrive")
            .expect("report channel should stay open");
        assert!(report.error.is_none());
        assert_eq!(
            seen_injection
                .lock()
                .expect("recording lock should be available")
                .as_slice(),
            &["direct final result".to_string()]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn final_result_report_includes_user_visible_completion_latency() {
        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);
        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink, None);
        let mut parent_focus_by_session = HashMap::new();
        let hotkey_up_at = TokioInstant::now() - Duration::from_millis(50);
        let stop_message_at = TokioInstant::now() - Duration::from_millis(40);

        handle_server_message(
            ServerMessage::FinalResult {
                session_id,
                text: "timed final result".to_string(),
                latency_ms: 44,
                audio_ms: 1200,
                lang: Some("en".to_string()),
                confidence: Some(0.92),
            },
            &mut state,
            &mut overlay_router,
            &worker,
            &mut parent_focus_by_session,
            Some(hotkey_up_at),
            Some((session_id, stop_message_at)),
        )
        .await
        .expect("final result should enqueue");

        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("injection report should arrive")
            .expect("report channel should stay open");
        assert!(report.error.is_none());
        assert_eq!(report.daemon_latency_ms, 44);
        assert_eq!(
            report.enqueue_to_injection_complete_ms,
            report.total_worker_ms
        );
        assert_eq!(
            report.hotkey_up_elapsed_ms_at_completion,
            report
                .hotkey_up_elapsed_ms_at_enqueue
                .map(|elapsed| elapsed.saturating_add(report.enqueue_to_injection_complete_ms))
        );
        assert_eq!(
            report.stop_message_elapsed_ms_at_completion,
            report
                .stop_message_elapsed_ms_at_enqueue
                .map(|elapsed| elapsed.saturating_add(report.enqueue_to_injection_complete_ms))
        );
        assert!(
            report.hotkey_up_elapsed_ms_at_completion
                >= report.hotkey_up_elapsed_ms_at_worker_start
        );
        assert!(
            report.stop_message_elapsed_ms_at_completion
                >= report.stop_message_elapsed_ms_at_worker_start
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_final_result_does_not_enqueue_injection_job() {
        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);
        let mut state = PttState::new();
        let active_session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        let stale_session_id = Uuid::new_v4();
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink, None);

        handle_server_message_for_tests(
            ServerMessage::FinalResult {
                session_id: stale_session_id,
                text: "stale private transcript".to_string(),
                latency_ms: 44,
                audio_ms: 1200,
                lang: Some("en".to_string()),
                confidence: Some(0.92),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("stale final result should be ignored without failing dispatch");

        assert!(
            matches!(state, PttState::WaitingResult { session_id } if session_id == active_session_id)
        );
        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 0);
        assert!(seen_injection
            .lock()
            .expect("recording lock should be available")
            .is_empty());
        assert!(timeout(Duration::from_millis(100), reports.recv())
            .await
            .is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn final_result_enqueues_without_waiting_for_injection_completion() {
        let calls = Arc::new(AtomicU64::new(0));
        let slow_injector = Arc::new(SlowRunner {
            calls: Arc::clone(&calls),
            sleep_ms: 120,
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(slow_injector, 8);

        let mut state = PttState::new();
        let session_id = state.begin_listening().expect("state should start");
        state.stop_listening();
        let mut overlay_router = OverlayRouter::new(NoopOverlaySink, None);
        let message = ServerMessage::FinalResult {
            session_id,
            text: "hello from daemon".to_string(),
            latency_ms: 60,
            audio_ms: 1900,
            lang: Some("en".to_string()),
            confidence: Some(0.99),
        };

        let started = Instant::now();
        handle_server_message_for_tests(message, &mut state, &mut overlay_router, &worker)
            .await
            .expect("server message should enqueue successfully");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(100),
            "handle_server_message should not wait for blocking injection, elapsed={elapsed:?}"
        );
        assert!(matches!(state, PttState::Idle));

        let report = timeout(Duration::from_secs(2), reports.recv())
            .await
            .expect("worker should report")
            .expect("report stream should remain open");
        assert!(report.error.is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interim_overlay_messages_route_without_injection_enqueue() {
        let seen_overlay_events = Arc::new(Mutex::new(Vec::<OverlayEvent>::new()));
        let mut overlay_router = OverlayRouter::new(
            RecordingOverlaySink {
                seen: Arc::clone(&seen_overlay_events),
            },
            None,
        );
        let injector_seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&injector_seen),
        });
        let (worker, _reports) = spawn_injector_worker_with_capacity(injector, 4);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        handle_server_message_for_tests(
            ServerMessage::InterimState {
                session_id,
                seq: 1,
                state: "listening".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("interim state should route to overlay");
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 2,
                text: "hello".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("interim text should route to overlay");
        handle_server_message_for_tests(
            ServerMessage::SessionEnded {
                session_id,
                reason: Some("normal".to_string()),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("session ended should route to overlay");

        assert!(matches!(state, PttState::WaitingResult { session_id: id } if id == session_id));
        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 0);
        assert_eq!(
            injector_seen
                .lock()
                .expect("recording lock should be available")
                .len(),
            0
        );

        let overlay_events = seen_overlay_events
            .lock()
            .expect("overlay recording lock should be available")
            .clone();
        assert_eq!(
            overlay_events,
            vec![
                OverlayEvent::InterimState {
                    session_id,
                    seq: 1,
                    state: "listening".to_string(),
                },
                OverlayEvent::InterimText {
                    session_id,
                    seq: 2,
                    text: "hello".to_string(),
                },
                OverlayEvent::SessionEnded {
                    session_id,
                    reason: Some("normal".to_string()),
                },
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mixed_stream_enqueues_exactly_one_final_result() {
        let seen_overlay_events = Arc::new(Mutex::new(Vec::<OverlayEvent>::new()));
        let mut overlay_router = OverlayRouter::new(
            RecordingOverlaySink {
                seen: Arc::clone(&seen_overlay_events),
            },
            None,
        );
        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 4);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        handle_server_message_for_tests(
            ServerMessage::InterimState {
                session_id,
                seq: 1,
                state: "processing".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("interim state should route");
        handle_server_message_for_tests(
            ServerMessage::FinalResult {
                session_id,
                text: "only final injects".to_string(),
                latency_ms: 40,
                audio_ms: 1200,
                lang: Some("en".to_string()),
                confidence: Some(0.9),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("final result should enqueue exactly once");
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 2,
                text: "post-final overlay".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("interim text should stay in overlay route");

        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("final result should produce one report")
            .expect("report channel should remain open");
        assert!(report.error.is_none());

        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen_injection
                .lock()
                .expect("recording lock should be available")
                .clone(),
            vec!["only final injects".to_string()]
        );
        assert!(timeout(Duration::from_millis(150), reports.recv())
            .await
            .is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn overlay_disconnect_does_not_block_final_result_injection() {
        let (overlay_tx, overlay_rx) = mpsc::unbounded_channel();
        drop(overlay_rx);
        let first_sink = OverlayProcessSink::from_sender_for_tests(
            overlay_tx,
            Arc::new(OverlayProcessMetrics::default()),
        );
        let sink_slot = Arc::new(Mutex::new(Some(first_sink)));
        let launcher = {
            let sink_slot = Arc::clone(&sink_slot);
            Arc::new(move |_mode, _output_name, _adaptive_width| {
                sink_slot
                    .lock()
                    .expect("sink slot lock should be available")
                    .take()
                    .ok_or_else(|| anyhow!("no overlay sink available"))
            })
        };
        let manager = OverlayProcessManager::new_for_tests(
            OverlayMode::LayerShell,
            true,
            launcher,
            Duration::ZERO,
        );
        let manager_metrics = Arc::clone(manager.metrics());
        let mut overlay_router =
            OverlayRouter::new(RuntimeOverlaySink::Process(Box::new(manager)), None);

        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 2);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 1,
                text: "overlay event while disconnected".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("overlay disconnect should be non-fatal");
        assert_eq!(
            manager_metrics
                .send_disconnect_total
                .load(Ordering::Relaxed),
            1
        );

        handle_server_message_for_tests(
            ServerMessage::FinalResult {
                session_id,
                text: "final survives overlay disconnect".to_string(),
                latency_ms: 33,
                audio_ms: 777,
                lang: Some("en".to_string()),
                confidence: Some(0.95),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("final result should still enqueue");

        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("final result should report")
            .expect("report stream should remain open");
        assert!(report.error.is_none());
        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen_injection
                .lock()
                .expect("recording lock should be available")
                .clone(),
            vec!["final survives overlay disconnect".to_string()]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repeated_overlay_failures_remain_non_fatal_to_final_injection() {
        fn disconnected_test_sink() -> OverlayProcessSink {
            let (overlay_tx, overlay_rx) = mpsc::unbounded_channel();
            drop(overlay_rx);
            OverlayProcessSink::from_sender_for_tests(
                overlay_tx,
                Arc::new(OverlayProcessMetrics::default()),
            )
        }

        let spawn_queue = Arc::new(Mutex::new(VecDeque::from([
            Ok(disconnected_test_sink()),
            Err(anyhow!(
                "failed to spawn overlay process '/tmp/parakeet-overlay': No such file or directory"
            )),
            Ok(disconnected_test_sink()),
            Err(anyhow!(
                "failed to spawn overlay process '/tmp/parakeet-overlay': No such file or directory"
            )),
            Ok(disconnected_test_sink()),
        ])));
        let launcher = {
            let spawn_queue = Arc::clone(&spawn_queue);
            Arc::new(move |_mode, _output_name, _adaptive_width| {
                spawn_queue
                    .lock()
                    .expect("spawn queue lock should be available")
                    .pop_front()
                    .unwrap_or_else(|| Err(anyhow!("no overlay sink available")))
            })
        };
        let manager = OverlayProcessManager::new_for_tests(
            OverlayMode::LayerShell,
            true,
            launcher,
            Duration::ZERO,
        );
        let manager_metrics = Arc::clone(manager.metrics());
        let mut overlay_router =
            OverlayRouter::new(RuntimeOverlaySink::Process(Box::new(manager)), None);

        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 2);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        for seq in 1..=4 {
            handle_server_message_for_tests(
                ServerMessage::InterimText {
                    session_id,
                    seq,
                    text: format!("overlay seq {seq}"),
                },
                &mut state,
                &mut overlay_router,
                &worker,
            )
            .await
            .expect("overlay failures should remain non-fatal");
        }

        handle_server_message_for_tests(
            ServerMessage::FinalResult {
                session_id,
                text: "final survives repeated overlay failures".to_string(),
                latency_ms: 12,
                audio_ms: 345,
                lang: Some("en".to_string()),
                confidence: Some(0.99),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("final result should still enqueue");

        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("final result should report")
            .expect("report stream should remain open");
        assert!(report.error.is_none());
        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen_injection
                .lock()
                .expect("recording lock should be available")
                .clone(),
            vec!["final survives repeated overlay failures".to_string()]
        );
        assert!(
            manager_metrics.spawn_failure_total.load(Ordering::Relaxed) >= 1,
            "at least one spawn failure should be recorded"
        );
        assert!(
            manager_metrics
                .send_disconnect_total
                .load(Ordering::Relaxed)
                >= 1,
            "at least one disconnect should be recorded"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn overlay_crash_restart_replays_current_state_and_preserves_final_injection() {
        let (tx_first, mut rx_first) = mpsc::unbounded_channel();
        let first_sink = OverlayProcessSink::from_sender_for_tests(
            tx_first,
            Arc::new(OverlayProcessMetrics::default()),
        );
        let (tx_second, mut rx_second) = mpsc::unbounded_channel();
        let second_sink = OverlayProcessSink::from_sender_for_tests(
            tx_second,
            Arc::new(OverlayProcessMetrics::default()),
        );

        let spawn_queue = Arc::new(Mutex::new(VecDeque::from([
            Ok(first_sink),
            Ok(second_sink),
        ])));
        let launcher = {
            let spawn_queue = Arc::clone(&spawn_queue);
            Arc::new(move |_mode, _output_name, _adaptive_width| {
                spawn_queue
                    .lock()
                    .expect("spawn queue lock should be available")
                    .pop_front()
                    .unwrap_or_else(|| Err(anyhow!("no overlay sink available")))
            })
        };
        let manager = OverlayProcessManager::new_for_tests(
            OverlayMode::LayerShell,
            true,
            launcher,
            Duration::ZERO,
        );
        let manager_metrics = Arc::clone(manager.metrics());
        let mut overlay_router =
            OverlayRouter::new(RuntimeOverlaySink::Process(Box::new(manager)), None);

        let seen_injection = Arc::new(Mutex::new(Vec::<String>::new()));
        let injector = Arc::new(RecordingRunner {
            seen: Arc::clone(&seen_injection),
        });
        let (worker, mut reports) = spawn_injector_worker_with_capacity(injector, 2);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 1,
                text: "old-state".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("first interim text should route");
        let first_seen = timeout(Duration::from_millis(100), rx_first.recv())
            .await
            .expect("first sink should receive old state")
            .expect("first sink channel should stay open");
        assert_eq!(
            first_seen,
            parakeet_ptt::overlay_ipc::OverlayIpcMessage::InterimText {
                session_id,
                seq: 1,
                text: "old-state".to_string(),
            }
        );

        drop(rx_first);

        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 2,
                text: "current-state".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("interim text after crash should remain non-fatal");

        let second_seen = timeout(Duration::from_millis(100), rx_second.recv())
            .await
            .expect("second sink should receive replayed current state")
            .expect("second sink channel should stay open");
        assert_eq!(
            second_seen,
            parakeet_ptt::overlay_ipc::OverlayIpcMessage::InterimText {
                session_id,
                seq: 2,
                text: "current-state".to_string(),
            }
        );
        assert!(timeout(Duration::from_millis(50), rx_second.recv())
            .await
            .is_err());

        handle_server_message_for_tests(
            ServerMessage::FinalResult {
                session_id,
                text: "final after overlay restart".to_string(),
                latency_ms: 45,
                audio_ms: 1000,
                lang: Some("en".to_string()),
                confidence: Some(0.98),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("final result should still enqueue");

        let report = timeout(Duration::from_secs(1), reports.recv())
            .await
            .expect("final result should produce one report")
            .expect("report channel should remain open");
        assert!(report.error.is_none());
        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 1);
        assert_eq!(
            seen_injection
                .lock()
                .expect("recording lock should be available")
                .clone(),
            vec!["final after overlay restart".to_string()]
        );
        assert_eq!(
            manager_metrics
                .send_disconnect_total
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(manager_metrics.replay_sent_total.load(Ordering::Relaxed), 1);
        assert_eq!(
            manager_metrics.spawn_success_total.load(Ordering::Relaxed),
            2
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_interim_sequences_are_dropped_on_overlay_path_only() {
        let seen_overlay_events = Arc::new(Mutex::new(Vec::<OverlayEvent>::new()));
        let mut overlay_router = OverlayRouter::new(
            RecordingOverlaySink {
                seen: Arc::clone(&seen_overlay_events),
            },
            None,
        );
        let injector = Arc::new(RecordingRunner {
            seen: Arc::new(Mutex::new(Vec::new())),
        });
        let (worker, _reports) = spawn_injector_worker_with_capacity(injector, 2);

        let mut state = PttState::new();
        let session_id = state
            .begin_listening()
            .expect("state should begin listening");
        state.stop_listening();
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 10,
                text: "newest".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("first interim text should route");
        handle_server_message_for_tests(
            ServerMessage::InterimText {
                session_id,
                seq: 9,
                text: "stale".to_string(),
            },
            &mut state,
            &mut overlay_router,
            &worker,
        )
        .await
        .expect("stale interim text should be dropped without failure");

        assert_eq!(worker.metrics().queued_total.load(Ordering::Relaxed), 0);
        let overlay_events = seen_overlay_events
            .lock()
            .expect("overlay recording lock should be available")
            .clone();
        assert_eq!(overlay_events.len(), 1);
        assert_eq!(
            overlay_events[0],
            OverlayEvent::InterimText {
                session_id,
                seq: 10,
                text: "newest".to_string(),
            }
        );
    }
}
