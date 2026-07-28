use std::time::Duration;

use crate::{OpenAiAuthSnapshot, monotonic_now_ns};
use http::header;
use tokio::time::{Instant, timeout_at};
use tokio_tungstenite::tungstenite::Utf8Bytes;

use crate::{EncodedRequest, ResponsesError, socket::ReceivedText};

const RESPONSES_LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";
const TURN_STATE_HEADER: &str = "x-codex-turn-state";

#[derive(Clone)]
pub(crate) struct ResponsesHttp {
    client: reqwest::Client,
    stream_idle_timeout: Duration,
}

pub(crate) struct ResponsesHttpStream {
    body: HttpBody,
    decoder: SseDecoder,
    ended: bool,
    stream_idle_timeout: Duration,
}

enum HttpBody {
    Network(reqwest::Response),
    #[cfg(test)]
    Test(tokio::sync::mpsc::UnboundedReceiver<TestChunk>),
}

enum BodyRead {
    Activity,
    Empty,
    End,
}

#[cfg(test)]
struct TestChunk {
    bytes: Vec<u8>,
    consumed: tokio::sync::oneshot::Sender<()>,
}

pub(crate) struct HttpMetadata {
    pub(crate) reasoning_included: bool,
    pub(crate) turn_state: Option<String>,
}

impl ResponsesHttp {
    pub(crate) const fn new(client: reqwest::Client, stream_idle_timeout: Duration) -> Self {
        Self {
            client,
            stream_idle_timeout,
        }
    }

    pub(crate) async fn send(
        &self,
        api_base_url: &str,
        auth: &OpenAiAuthSnapshot,
        session_id: &str,
        turn_state: Option<&str>,
        request: &EncodedRequest,
    ) -> Result<(ResponsesHttpStream, HttpMetadata), ResponsesError> {
        let endpoint = format!("{}/responses", api_base_url.trim_end_matches('/'));
        let mut builder = self
            .client
            .post(endpoint)
            .bearer_auth(auth.bearer())
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "text/event-stream")
            .header(RESPONSES_LITE_HEADER, "true")
            .header("session-id", session_id)
            .header("thread-id", session_id)
            .header("x-client-request-id", session_id)
            .header(
                header::USER_AGENT,
                concat!("nanocodex/", env!("CARGO_PKG_VERSION")),
            )
            .body(request.raw().get().to_owned());
        if let Some(account_id) = auth.account_id() {
            builder = builder.header("ChatGPT-Account-ID", account_id);
        }
        if auth.is_fedramp() {
            builder = builder.header("X-OpenAI-Fedramp", "true");
        }
        if let Some(turn_state) =
            turn_state.and_then(|value| header::HeaderValue::from_bytes(value.as_bytes()).ok())
        {
            builder = builder.header(TURN_STATE_HEADER, turn_state);
        }
        let response = builder.send().await.map_err(map_http_error)?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = retry_after(response.headers());
            let body = response.text().await.unwrap_or_default();
            return Err(ResponsesError::HttpRejected {
                status: status.as_u16(),
                body,
                retry_after,
            });
        }
        let metadata = HttpMetadata {
            reasoning_included: response.headers().contains_key("x-reasoning-included"),
            turn_state: response
                .headers()
                .get(TURN_STATE_HEADER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        };
        Ok((
            ResponsesHttpStream {
                body: HttpBody::Network(response),
                decoder: SseDecoder::default(),
                ended: false,
                stream_idle_timeout: self.stream_idle_timeout,
            },
            metadata,
        ))
    }
}

impl ResponsesHttpStream {
    pub(crate) async fn next_text_or_idle_timeout(
        &mut self,
    ) -> Result<ReceivedText, ResponsesError> {
        let mut idle_deadline = Instant::now() + self.stream_idle_timeout;
        loop {
            if let Some(text) = self.decoder.next()? {
                return Ok(ReceivedText {
                    text: Utf8Bytes::from(text),
                    received_ns: monotonic_now_ns(),
                });
            }
            if self.ended {
                return Err(ResponsesError::UnexpectedEnd);
            }
            if Instant::now() >= idle_deadline {
                return Err(self.idle_timeout());
            }
            match timeout_at(idle_deadline, self.body.read_into(&mut self.decoder)).await {
                Ok(Ok(BodyRead::Activity)) => {
                    idle_deadline = Instant::now() + self.stream_idle_timeout;
                }
                Ok(Ok(BodyRead::Empty)) => {}
                Ok(Ok(BodyRead::End)) => {
                    self.ended = true;
                    self.decoder.finish();
                }
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(self.idle_timeout()),
            }
        }
    }

    const fn idle_timeout(&self) -> ResponsesError {
        ResponsesError::IdleTimeout {
            seconds: self.stream_idle_timeout.as_secs(),
        }
    }
}

impl HttpBody {
    async fn read_into(&mut self, decoder: &mut SseDecoder) -> Result<BodyRead, ResponsesError> {
        match self {
            Self::Network(response) => match response.chunk().await.map_err(map_http_error)? {
                Some(chunk) if chunk.is_empty() => Ok(BodyRead::Empty),
                Some(chunk) => {
                    decoder.push(&chunk);
                    Ok(BodyRead::Activity)
                }
                None => Ok(BodyRead::End),
            },
            #[cfg(test)]
            Self::Test(chunks) => match chunks.recv().await {
                Some(chunk) if chunk.bytes.is_empty() => {
                    let _ = chunk.consumed.send(());
                    Ok(BodyRead::Empty)
                }
                Some(chunk) => {
                    decoder.push(&chunk.bytes);
                    let _ = chunk.consumed.send(());
                    Ok(BodyRead::Activity)
                }
                None => Ok(BodyRead::End),
            },
        }
    }
}

#[derive(Default)]
struct SseDecoder {
    bytes: Vec<u8>,
    cursor: usize,
    data: Vec<String>,
    finished: bool,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) {
        self.compact();
        self.bytes.extend_from_slice(chunk);
    }

    fn finish(&mut self) {
        self.finished = true;
        self.compact();
        if !self.bytes.is_empty() {
            self.bytes.push(b'\n');
        }
        self.bytes.push(b'\n');
    }

    fn next(&mut self) -> Result<Option<String>, ResponsesError> {
        loop {
            let Some(relative_newline) = self.bytes[self.cursor..]
                .iter()
                .position(|byte| *byte == b'\n')
            else {
                return Ok(None);
            };
            let line_start = self.cursor;
            let newline = line_start + relative_newline;
            self.cursor = newline + 1;
            let line_end = if newline > line_start && self.bytes.get(newline - 1) == Some(&b'\r') {
                newline - 1
            } else {
                newline
            };
            let line = std::str::from_utf8(&self.bytes[line_start..line_end]).map_err(|error| {
                ResponsesError::InvalidSseUtf8 {
                    detail: error.to_string(),
                }
            })?;
            if line.is_empty() {
                if self.data.is_empty() {
                    if self.finished && self.cursor == self.bytes.len() {
                        return Ok(None);
                    }
                    continue;
                }
                let event = self.data.join("\n");
                self.data.clear();
                if event == "[DONE]" {
                    continue;
                }
                return Ok(Some(event));
            }
            if let Some(data) = line.strip_prefix("data:") {
                self.data
                    .push(data.strip_prefix(' ').unwrap_or(data).to_owned());
            }
        }
    }

    fn compact(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let remaining = self.bytes.len() - self.cursor;
        self.bytes.copy_within(self.cursor.., 0);
        self.bytes.truncate(remaining);
        self.cursor = 0;
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn map_http_error(error: reqwest::Error) -> ResponsesError {
    ResponsesError::HttpRequest {
        retryable: error.is_connect() || error.is_body(),
        timeout: error.is_timeout(),
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{
        sync::{mpsc, oneshot},
        time::advance,
    };

    use crate::ResponsesError;

    use super::{HttpBody, ResponsesHttpStream, SseDecoder, TestChunk};

    #[test]
    fn decodes_fragmented_and_multiline_sse_events() {
        let mut decoder = SseDecoder::default();
        decoder.push(b": keepalive\n\ndata: {\"type\":\"response.");
        assert_eq!(decoder.next().unwrap(), None);
        decoder.push(b"created\"}\r\n\r\ndata: first\ndata: second\n\n");
        assert_eq!(
            decoder.next().unwrap().as_deref(),
            Some("{\"type\":\"response.created\"}")
        );
        assert_eq!(decoder.next().unwrap().as_deref(), Some("first\nsecond"));
        assert_eq!(decoder.next().unwrap(), None);
    }

    #[test]
    fn skips_done_and_flushes_an_unterminated_final_event() {
        let mut decoder = SseDecoder::default();
        decoder.push(b"data: [DONE]\n\ndata: final");
        decoder.finish();
        assert_eq!(decoder.next().unwrap().as_deref(), Some("final"));
        assert_eq!(decoder.next().unwrap(), None);
    }

    #[test]
    fn decodes_many_events_from_one_chunk_without_repacking_each_line() {
        let mut body = String::new();
        for index in 0..4_096 {
            body.push_str("data: event-");
            body.push_str(&index.to_string());
            body.push_str("\n\n");
        }

        let mut decoder = SseDecoder::default();
        decoder.push(body.as_bytes());
        for index in 0..4_096 {
            assert_eq!(
                decoder.next().unwrap().as_deref(),
                Some(format!("event-{index}").as_str())
            );
        }
        assert_eq!(decoder.next().unwrap(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn comments_and_partial_chunks_reset_idle_without_becoming_events() {
        let (mut stream, chunks) = test_stream(Duration::from_millis(100));
        let received = tokio::spawn(async move { stream.next_text_or_idle_timeout().await });
        tokio::task::yield_now().await;

        advance(Duration::from_millis(90)).await;
        send_chunk(&chunks, b": keepalive\n\n").await;
        assert!(
            !received.is_finished(),
            "an SSE comment must not surface as a model event"
        );

        advance(Duration::from_millis(90)).await;
        send_chunk(&chunks, b"data: {\"type\":\"response.").await;
        assert!(
            !received.is_finished(),
            "a partial SSE record must not surface as a model event"
        );

        advance(Duration::from_millis(90)).await;
        send_chunk(&chunks, b"completed\"}\n\n").await;
        let received = received
            .await
            .expect("test receive task should finish")
            .expect("raw SSE activity should keep the receive alive");
        assert_eq!(received.text.as_str(), r#"{"type":"response.completed"}"#);
    }

    #[tokio::test(start_paused = true)]
    async fn no_sse_bytes_reaches_the_configured_idle_deadline() {
        let (mut stream, _chunks) = test_stream(Duration::from_millis(100));
        let received = tokio::spawn(async move { stream.next_text_or_idle_timeout().await });
        tokio::task::yield_now().await;

        advance(Duration::from_millis(99)).await;
        assert!(!received.is_finished());
        advance(Duration::from_millis(2)).await;
        tokio::task::yield_now().await;
        assert!(matches!(
            received.await,
            Ok(Err(ResponsesError::IdleTimeout { seconds: 0 }))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn sustained_empty_chunks_cannot_starve_the_absolute_idle_deadline() {
        let (mut stream, chunks) = test_stream(Duration::from_millis(100));
        let received = tokio::spawn(async move { stream.next_text_or_idle_timeout().await });
        tokio::task::yield_now().await;

        for _ in 0..4 {
            advance(Duration::from_millis(20)).await;
            send_chunk(&chunks, b"").await;
            assert!(
                !received.is_finished(),
                "an empty HTTP chunk must not reset or surface the idle timeout"
            );
        }

        advance(Duration::from_millis(21)).await;
        tokio::task::yield_now().await;
        assert!(
            received.is_finished(),
            "ready empty chunks must not starve the absolute deadline"
        );
        assert!(matches!(
            received.await,
            Ok(Err(ResponsesError::IdleTimeout { seconds: 0 }))
        ));
    }

    fn test_stream(
        stream_idle_timeout: Duration,
    ) -> (ResponsesHttpStream, mpsc::UnboundedSender<TestChunk>) {
        let (chunks, body) = mpsc::unbounded_channel();
        (
            ResponsesHttpStream {
                body: HttpBody::Test(body),
                decoder: SseDecoder::default(),
                ended: false,
                stream_idle_timeout,
            },
            chunks,
        )
    }

    async fn send_chunk(chunks: &mpsc::UnboundedSender<TestChunk>, bytes: &[u8]) {
        let (consumed, consumption) = oneshot::channel();
        chunks
            .send(TestChunk {
                bytes: bytes.to_vec(),
                consumed,
            })
            .expect("test SSE stream should still be receiving chunks");
        consumption
            .await
            .expect("test SSE stream should acknowledge the chunk");
        tokio::task::yield_now().await;
    }
}
