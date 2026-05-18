//! LLM answer ports for Parakeet Client query Sessions.
//!
//! This module owns Client-facing LLM answer generation: the `LlmAnswerer`
//! port, production HTTP/SSE adapter, answer sanitization, and test adapters.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;
use uuid::Uuid;

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
#[allow(dead_code)]
pub type LlmDeltaStream<'a> = BoxStream<'a, Result<LlmDelta>>;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmDelta {
    pub content: String,
}

pub trait LlmAnswerer: Send + Sync {
    fn label(&self) -> String;

    #[allow(dead_code)]
    fn stream_answer<'a>(&'a self, prompt: &'a str) -> LlmDeltaStream<'a>;

    fn health<'a>(&'a self) -> BoxFuture<'a, bool>;

    fn answer<'a>(
        &'a self,
        session_id: Uuid,
        transcript: String,
        progress_tx: mpsc::UnboundedSender<LlmProgress>,
    ) -> BoxFuture<'a, Result<String>>;
}

#[derive(Clone)]
pub struct HttpLlmAnswerer {
    config: LlmRuntimeConfig,
}

impl HttpLlmAnswerer {
    pub fn new(config: LlmRuntimeConfig) -> Self {
        Self { config }
    }
}

impl LlmAnswerer for HttpLlmAnswerer {
    fn label(&self) -> String {
        self.config.base_url.to_string()
    }

    fn stream_answer<'a>(&'a self, prompt: &'a str) -> LlmDeltaStream<'a> {
        let mut config = self.config.clone();
        config.overlay_stream = true;
        let prompt = prompt.to_string();
        let (delta_tx, delta_rx) = mpsc::unbounded_channel::<Result<LlmDelta>>();

        let stream_task = tokio::spawn(async move {
            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<LlmProgress>();
            let forward_tx = delta_tx.clone();
            let forward_task = tokio::spawn(async move {
                while let Some(progress) = progress_rx.recv().await {
                    let LlmProgress::Delta { delta, .. } = progress else {
                        continue;
                    };
                    if forward_tx.send(Ok(LlmDelta { content: delta })).is_err() {
                        break;
                    }
                }
            });

            let result =
                fetch_llm_streamed_answer(&config, Uuid::nil(), &prompt, &progress_tx).await;
            drop(progress_tx);
            let _ = forward_task.await;
            if let Err(err) = result {
                let _ = delta_tx.send(Err(err));
            }
        });
        drop(stream_task);

        receiver_delta_stream(delta_rx)
    }

    fn health<'a>(&'a self) -> BoxFuture<'a, bool> {
        Box::pin(async move { probe_llm_health_once(&self.config).await })
    }

    fn answer<'a>(
        &'a self,
        session_id: Uuid,
        transcript: String,
        progress_tx: mpsc::UnboundedSender<LlmProgress>,
    ) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            fetch_llm_streamed_answer(&self.config, session_id, &transcript, &progress_tx).await
        })
    }
}

#[derive(Debug, Clone)]
pub struct LlmRuntimeConfig {
    pub base_url: url::Url,
    pub model: String,
    pub timeout: Duration,
    pub max_tokens: u32,
    pub temperature: f32,
    pub system_prompt: String,
    pub overlay_stream: bool,
}

#[derive(Debug)]
pub enum LlmProgress {
    Delta {
        session_id: Uuid,
        delta: String,
    },
    Finished {
        session_id: Uuid,
        transcript: String,
        daemon_latency_ms: u64,
        daemon_audio_ms: u64,
        result: std::result::Result<String, String>,
    },
}

pub fn build_http_llm_answerer(config: LlmRuntimeConfig) -> Arc<dyn LlmAnswerer> {
    Arc::new(HttpLlmAnswerer::new(config))
}

fn receiver_delta_stream(
    receiver: mpsc::UnboundedReceiver<Result<LlmDelta>>,
) -> LlmDeltaStream<'static> {
    Box::pin(stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|next| (next, receiver))
    }))
}

fn llm_chat_completions_url(base: &url::Url) -> Result<url::Url> {
    let mut url = base.clone();
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url.join("chat/completions")
        .context("failed to build llama chat/completions URL")
}

fn llm_health_url(base: &url::Url) -> Result<url::Url> {
    let mut url = base.clone();
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url.join("health")
        .context("failed to build llama health URL")
}

fn extract_delta_content(payload: &serde_json::Value) -> Option<&str> {
    payload
        .get("choices")?
        .get(0)?
        .get("delta")?
        .get("content")?
        .as_str()
}

pub(crate) fn sanitize_model_answer(raw: &str) -> String {
    let mut output = raw.to_string();
    while let Some(start) = output.find("<think>") {
        let Some(end_relative) = output[start..].find("</think>") else {
            output.truncate(start);
            break;
        };
        let end = start + end_relative + "</think>".len();
        output.replace_range(start..end, "");
    }

    let trimmed = output.trim();
    trimmed.to_string()
}

pub(crate) fn drain_sse_lines(buffer: &mut Vec<u8>, flush_partial: bool) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    while let Some(line_end) = buffer.iter().position(|byte| *byte == b'\n') {
        let mut raw_line = buffer.drain(..=line_end).collect::<Vec<_>>();
        raw_line.pop();
        if raw_line.ends_with(b"\r") {
            raw_line.pop();
        }
        let line = std::str::from_utf8(&raw_line)
            .context("llama SSE stream contained invalid UTF-8 in a line")?;
        lines.push(line.to_string());
    }

    if flush_partial && !buffer.is_empty() {
        let line = std::str::from_utf8(buffer)
            .context("llama SSE stream ended with invalid UTF-8 in trailing bytes")?;
        lines.push(line.trim_end_matches('\r').to_string());
        buffer.clear();
    }

    Ok(lines)
}

async fn fetch_llm_streamed_answer(
    llm: &LlmRuntimeConfig,
    session_id: Uuid,
    transcript: &str,
    progress_tx: &mpsc::UnboundedSender<LlmProgress>,
) -> Result<String> {
    let request_url = llm_chat_completions_url(&llm.base_url)?;
    let client = reqwest::Client::builder()
        .timeout(llm.timeout)
        .build()
        .context("failed to build reqwest client for llama")?;

    let request_body = json!({
        "model": llm.model,
        "stream": true,
        "messages": [
            {"role": "system", "content": llm.system_prompt},
            {"role": "user", "content": transcript},
        ],
        "max_tokens": llm.max_tokens,
        "temperature": llm.temperature,
        "chat_template_kwargs": {"enable_thinking": false},
        "reasoning_format": "none",
        "reasoning_in_content": false
    });

    let response = client
        .post(request_url.clone())
        .json(&request_body)
        .send()
        .await
        .with_context(|| format!("failed to reach llama endpoint {}", request_url))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable body>".to_string());
        anyhow::bail!("llama returned status {} with body: {}", status, body);
    }

    let mut stream = response.bytes_stream();
    let mut buffer = Vec::<u8>::new();
    let mut assembled = String::new();

    let mut process_sse_line = |line: &str| -> Result<bool> {
        if line.is_empty() || !line.starts_with("data:") {
            return Ok(false);
        }

        let payload = line[5..].trim();
        if payload == "[DONE]" {
            return Ok(true);
        }

        let parsed: serde_json::Value = serde_json::from_str(payload).with_context(|| {
            format!("failed to parse llama SSE data payload as JSON: {payload}")
        })?;
        if let Some(delta) = extract_delta_content(&parsed).filter(|value| !value.is_empty()) {
            assembled.push_str(delta);
            if llm.overlay_stream {
                let _ = progress_tx.send(LlmProgress::Delta {
                    session_id,
                    delta: delta.to_string(),
                });
            }
        }

        Ok(false)
    };

    while let Some(next_chunk) = stream.next().await {
        let chunk = next_chunk.context("failed reading llama stream chunk")?;
        buffer.extend_from_slice(&chunk);

        for line in drain_sse_lines(&mut buffer, false)? {
            if process_sse_line(line.trim())? {
                return Ok(assembled);
            }
        }
    }

    for line in drain_sse_lines(&mut buffer, true)? {
        if process_sse_line(line.trim())? {
            return Ok(assembled);
        }
    }

    Ok(assembled)
}

async fn probe_llm_health_once(llm: &LlmRuntimeConfig) -> bool {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build();
    let Ok(client) = client else {
        return false;
    };

    let Ok(health_url) = llm_health_url(&llm.base_url) else {
        return false;
    };

    match client.get(health_url).send().await {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;

    #[derive(Clone)]
    struct InMemoryLlmAnswerer {
        deltas: Vec<&'static str>,
        error: Option<&'static str>,
    }

    impl InMemoryLlmAnswerer {
        fn successful(deltas: Vec<&'static str>) -> Self {
            Self {
                deltas,
                error: None,
            }
        }

        fn failing(error: &'static str) -> Self {
            Self {
                deltas: Vec::new(),
                error: Some(error),
            }
        }
    }

    impl LlmAnswerer for InMemoryLlmAnswerer {
        fn label(&self) -> String {
            "in-memory-llm".to_string()
        }

        fn stream_answer<'a>(&'a self, _prompt: &'a str) -> LlmDeltaStream<'a> {
            if let Some(error) = self.error {
                return Box::pin(stream::once(async move { anyhow::bail!(error) }));
            }

            Box::pin(stream::iter(self.deltas.iter().map(|delta| {
                Ok(LlmDelta {
                    content: (*delta).to_string(),
                })
            })))
        }

        fn health<'a>(&'a self) -> BoxFuture<'a, bool> {
            Box::pin(async { true })
        }

        fn answer<'a>(
            &'a self,
            session_id: Uuid,
            transcript: String,
            progress_tx: mpsc::UnboundedSender<LlmProgress>,
        ) -> BoxFuture<'a, Result<String>> {
            Box::pin(async move {
                let mut answer = String::new();
                let mut deltas = self.stream_answer(&transcript);
                while let Some(delta) = deltas.next().await {
                    let delta = delta?;
                    answer.push_str(&delta.content);
                    let _ = progress_tx.send(LlmProgress::Delta {
                        session_id,
                        delta: delta.content,
                    });
                }
                Ok(answer)
            })
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn in_memory_llm_streams_several_deltas() {
        let session_id = Uuid::new_v4();
        let answerer = InMemoryLlmAnswerer::successful(vec!["alpha", " ", "beta"]);
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let streamed = answerer
            .stream_answer("question")
            .map(|delta| delta.map(|value| value.content))
            .try_collect::<Vec<_>>()
            .await
            .expect("streamed answer should succeed");

        let answer = answerer
            .answer(session_id, "question".to_string(), progress_tx)
            .await
            .expect("in-memory answer should succeed");

        assert_eq!(streamed, vec!["alpha", " ", "beta"]);
        assert_eq!(answer, "alpha beta");
        let mut deltas = Vec::new();
        while let Ok(progress) = progress_rx.try_recv() {
            match progress {
                LlmProgress::Delta {
                    session_id: delta_session_id,
                    delta,
                } => {
                    assert_eq!(delta_session_id, session_id);
                    deltas.push(delta);
                }
                LlmProgress::Finished { .. } => panic!("unexpected finished progress"),
            }
        }
        assert_eq!(deltas, vec!["alpha", " ", "beta"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn in_memory_llm_reports_error_path() {
        let answerer = InMemoryLlmAnswerer::failing("synthetic llm failure");
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();

        let error = answerer
            .answer(Uuid::new_v4(), "question".to_string(), progress_tx)
            .await
            .expect_err("in-memory answer should fail");

        assert!(error.to_string().contains("synthetic llm failure"));
        assert!(progress_rx.try_recv().is_err());
    }

    #[test]
    fn llm_endpoint_urls_preserve_configured_base_path() {
        let base = url::Url::parse("http://127.0.0.1:8080/api/v1").expect("base URL should parse");

        assert_eq!(
            llm_chat_completions_url(&base)
                .expect("chat URL should build")
                .as_str(),
            "http://127.0.0.1:8080/api/v1/chat/completions"
        );
        assert_eq!(
            llm_health_url(&base)
                .expect("health URL should build")
                .as_str(),
            "http://127.0.0.1:8080/api/v1/health"
        );
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
        let mut buffer = Vec::new();
        buffer.extend_from_slice(b"data: {\"choices\":[{\"delta\":{\"content\":\"");
        buffer.extend_from_slice(&[0xF0, 0x9F]);

        let first_lines = drain_sse_lines(&mut buffer, false).expect("first parse should succeed");
        assert!(first_lines.is_empty());

        buffer.extend_from_slice(&[0x99, 0x82]);
        buffer.extend_from_slice(b"\"}}]}\n");
        let lines = drain_sse_lines(&mut buffer, false).expect("second parse should succeed");

        assert_eq!(
            lines,
            vec!["data: {\"choices\":[{\"delta\":{\"content\":\"🙂\"}}]}".to_string()]
        );
        assert!(buffer.is_empty());
    }
}
