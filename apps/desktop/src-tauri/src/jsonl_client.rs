//! Serial JSON-lines request/response channel shared by the worker and quill clients.
//!
//! One JSON object per line in each direction. A dedicated reader task owns the child's
//! stdout, reads whole lines and forwards them over an mpsc channel. Requests only await
//! `recv()`, which is cancel-safe, so a timeout that drops a request mid-wait never splits
//! a line and the next request frames whatever the child sends next.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;
use std::marker::PhantomData;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Lines buffered between the reader task and the requester before the reader waits.
const LINE_QUEUE: usize = 64;

#[derive(Debug, Serialize)]
pub struct WorkerRequest {
    pub id: String,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Deserialize)]
pub struct WorkerResponse {
    pub id: String,
    pub ok: bool,
    pub result: Option<Value>,
    pub error: Option<WorkerError>,
}

#[derive(Debug, Deserialize)]
pub struct WorkerError {
    pub code: String,
    pub message: String,
}

/// Just the id of a reply line, for callers that parse the rest themselves.
#[derive(Deserialize)]
struct LineId {
    id: String,
}

#[derive(Debug)]
pub enum RequestError {
    /// The worker answered with an error; the connection is still healthy.
    Worker(String),
    /// The worker timed out, died or desynchronised; the connection cannot be reused.
    Transport(String),
}

impl RequestError {
    pub fn is_transport(&self) -> bool {
        matches!(self, Self::Transport(_))
    }
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Worker(message) | Self::Transport(message) => f.write_str(message),
        }
    }
}

fn transport(error: &dyn std::fmt::Display) -> RequestError {
    RequestError::Transport(error.to_string())
}

/// A serial request channel over a child's stdin (`W`) and stdout (`R`).
///
/// Dropping a request future while it waits for a reply is safe: the reply stays queued
/// as a whole line, and the caller passes its id as `skip_id` to the next request.
/// Dropping one while the request line is being written is not, since the child would
/// see a partial line; the channel then refuses every later request as a transport error.
pub struct JsonlChannel<W, R> {
    writer: W,
    lines: mpsc::Receiver<io::Result<String>>,
    reader_task: JoinHandle<()>,
    /// Stderr captured from the child, appended to the exit message.
    stderr: Arc<Mutex<String>>,
    last_id: Option<String>,
    write_interrupted: bool,
    _reader: PhantomData<fn() -> R>,
}

impl<W, R> JsonlChannel<W, R>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin + Send + 'static,
{
    /// Takes ownership of both pipe ends and starts the reader task, so it must run
    /// inside a tokio runtime.
    pub fn new(writer: W, reader: R, stderr: Arc<Mutex<String>>) -> Self {
        let (tx, lines) = mpsc::channel(LINE_QUEUE);
        let reader_task = tokio::spawn(read_lines(reader, tx));
        Self {
            writer,
            lines,
            reader_task,
            stderr,
            last_id: None,
            write_interrupted: false,
            _reader: PhantomData,
        }
    }

    /// Id of the most recent request line this channel started writing. A caller whose
    /// wait was cancelled by a timeout reads it to pass as `skip_id` on the next request.
    pub fn last_request_id(&self) -> Option<&str> {
        self.last_id.as_deref()
    }

    /// Sends a request and returns its result, rendering an error reply as
    /// `"<code>: <message>"`. Never skips an id: the worker has no abandoned requests.
    pub async fn exchange(&mut self, method: &str, params: Value) -> Result<Value, RequestError> {
        let response = self.exchange_raw(method, params, None).await?;
        if response.ok {
            Ok(response.result.unwrap_or(Value::Null))
        } else {
            let err = response
                .error
                .ok_or_else(|| RequestError::Worker("Unknown worker error".to_string()))?;
            Err(RequestError::Worker(format!("{}: {}", err.code, err.message)))
        }
    }

    /// Writes one request line, then reads lines until the one carrying its id. A line
    /// whose id equals `skip_id` is discarded; any other id is a transport error. Error
    /// replies are returned as the envelope so the caller can branch on `error.code`.
    pub async fn exchange_raw(
        &mut self,
        method: &str,
        params: Value,
        skip_id: Option<&str>,
    ) -> Result<WorkerResponse, RequestError> {
        self.exchange_matching(method, params, skip_id, |line| {
            let response: WorkerResponse = serde_json::from_str(line).map_err(|e| transport(&e))?;
            Ok((response.id.clone(), response))
        })
        .await
    }

    /// Same framing, `skip_id` discard and id-mismatch rule as `exchange_raw`, but returns
    /// the matching line untouched. Only its `id` is read here, so the caller can parse the
    /// whole envelope as strictly as it likes.
    pub async fn exchange_line(
        &mut self,
        method: &str,
        params: Value,
        skip_id: Option<&str>,
    ) -> Result<String, RequestError> {
        self.exchange_matching(method, params, skip_id, |line| {
            let LineId { id } = serde_json::from_str(line).map_err(|e| transport(&e))?;
            Ok((id, line.to_string()))
        })
        .await
    }

    async fn exchange_matching<T>(
        &mut self,
        method: &str,
        params: Value,
        skip_id: Option<&str>,
        read: impl Fn(&str) -> Result<(String, T), RequestError>,
    ) -> Result<T, RequestError> {
        if self.write_interrupted {
            return Err(RequestError::Transport(
                "Previous request was interrupted while writing".to_string(),
            ));
        }
        let req = WorkerRequest {
            id: Uuid::new_v4().to_string(),
            method: method.to_string(),
            params,
        };
        let mut line = serde_json::to_string(&req).map_err(|e| transport(&e))?;
        line.push('\n');

        self.last_id = Some(req.id.clone());
        self.write_interrupted = true;
        self.writer.write_all(line.as_bytes()).await.map_err(|e| transport(&e))?;
        self.writer.flush().await.map_err(|e| transport(&e))?;
        self.write_interrupted = false;

        loop {
            let line = self.next_line().await?;
            let (id, reply) = read(&line)?;
            if id == req.id {
                return Ok(reply);
            }
            if skip_id == Some(id.as_str()) {
                continue;
            }
            return Err(RequestError::Transport("Worker response id mismatch".to_string()));
        }
    }

    /// Next whole non-empty line. End of stream and an empty line both mean the child
    /// exited; the message carries its captured stderr.
    async fn next_line(&mut self) -> Result<String, RequestError> {
        match self.lines.recv().await {
            Some(Ok(line)) if !line.trim().is_empty() => Ok(line),
            Some(Err(error)) => Err(transport(&error)),
            Some(Ok(_)) | None => {
                let stderr = self.stderr.lock().await;
                let msg = if stderr.trim().is_empty() {
                    "Worker process exited unexpectedly".to_string()
                } else {
                    format!("Worker process exited:\n{}", stderr.trim())
                };
                Err(RequestError::Transport(msg))
            }
        }
    }
}

impl<W, R> Drop for JsonlChannel<W, R> {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

/// Forwards each complete line, then closes the channel at end of stream or on a read
/// error (which is forwarded first). A final line without a newline is still forwarded.
async fn read_lines<R: AsyncRead + Unpin>(reader: R, tx: mpsc::Sender<io::Result<String>>) {
    let mut reader = BufReader::new(reader);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                if tx.send(Ok(line)).await.is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = tx.send(Err(error)).await;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{
        duplex, split, AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf,
    };
    use tokio::sync::Mutex;

    type TestChannel = JsonlChannel<WriteHalf<DuplexStream>, ReadHalf<DuplexStream>>;

    /// The far end of the channel, standing in for the child process.
    struct Peer {
        reader: BufReader<ReadHalf<DuplexStream>>,
        writer: WriteHalf<DuplexStream>,
    }

    impl Peer {
        async fn read_request(&mut self) -> (String, Value) {
            let mut line = String::new();
            self.reader.read_line(&mut line).await.unwrap();
            let value = serde_json::from_str(&line).unwrap();
            (line, value)
        }

        async fn write(&mut self, text: &str) {
            self.writer.write_all(text.as_bytes()).await.unwrap();
            self.writer.flush().await.unwrap();
        }
    }

    fn pair_with_buffer(buffer: usize) -> (TestChannel, Peer, Arc<Mutex<String>>) {
        let (near, far) = duplex(buffer);
        let (near_read, near_write) = split(near);
        let (far_read, far_write) = split(far);
        let stderr = Arc::new(Mutex::new(String::new()));
        let channel = JsonlChannel::new(near_write, near_read, stderr.clone());
        let peer = Peer {
            reader: BufReader::new(far_read),
            writer: far_write,
        };
        (channel, peer, stderr)
    }

    fn pair() -> (TestChannel, Peer, Arc<Mutex<String>>) {
        pair_with_buffer(4096)
    }

    fn id_of(request: &Value) -> String {
        request["id"].as_str().unwrap().to_string()
    }

    fn reply(id: &str, result: Value) -> String {
        format!("{}\n", json!({"id": id, "ok": true, "result": result, "error": null}))
    }

    fn transport_message(error: RequestError) -> String {
        match error {
            RequestError::Transport(message) => message,
            other => panic!("expected a transport error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_line_carries_id_method_and_params() {
        let (mut channel, mut peer, _) = pair();
        let client = channel.exchange("list_models", json!({"limit": 3}));
        let server = async {
            let (line, request) = peer.read_request().await;
            peer.write(&reply(&id_of(&request), json!({"ok": 1}))).await;
            (line, request)
        };
        let (result, (line, request)) = tokio::join!(client, server);

        assert_eq!(result.unwrap(), json!({"ok": 1}));
        let keys: BTreeSet<&str> = request.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, BTreeSet::from(["id", "method", "params"]));
        assert!(!id_of(&request).is_empty());
        assert_eq!(request["method"], "list_models");
        assert_eq!(request["params"], json!({"limit": 3}));
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
    }

    #[tokio::test]
    async fn each_request_gets_a_fresh_id() {
        let (mut channel, mut peer, _) = pair();
        let mut ids = BTreeSet::new();
        for _ in 0..2 {
            let client = channel.exchange("ping", json!({}));
            let server = async {
                let (_, request) = peer.read_request().await;
                peer.write(&reply(&id_of(&request), json!(null))).await;
                id_of(&request)
            };
            let (result, id) = tokio::join!(client, server);
            assert_eq!(result.unwrap(), Value::Null);
            ids.insert(id);
        }
        assert_eq!(ids.len(), 2);
    }

    #[tokio::test]
    async fn closed_channel_is_a_transport_error_with_the_exit_text() {
        let (mut channel, mut peer, _) = pair();
        let client = channel.exchange("ping", json!({}));
        let server = async move {
            peer.read_request().await;
            drop(peer);
        };
        let (result, ()) = tokio::join!(client, server);

        assert_eq!(transport_message(result.unwrap_err()), "Worker process exited unexpectedly");
    }

    #[tokio::test]
    async fn closed_channel_includes_trimmed_stderr_in_the_exit_text() {
        let (mut channel, mut peer, stderr) = pair();
        *stderr.lock().await = "Traceback: boom\n".to_string();
        let client = channel.exchange("ping", json!({}));
        let server = async move {
            peer.read_request().await;
            drop(peer);
        };
        let (result, ()) = tokio::join!(client, server);

        assert_eq!(
            transport_message(result.unwrap_err()),
            "Worker process exited:\nTraceback: boom"
        );
    }

    #[tokio::test]
    async fn empty_line_is_a_transport_error_with_the_exit_text() {
        let (mut channel, mut peer, _) = pair();
        let client = channel.exchange("ping", json!({}));
        let server = async {
            peer.read_request().await;
            peer.write("\n").await;
        };
        let (result, ()) = tokio::join!(client, server);

        assert_eq!(transport_message(result.unwrap_err()), "Worker process exited unexpectedly");
    }

    #[tokio::test]
    async fn unparseable_line_is_a_transport_error() {
        let (mut channel, mut peer, _) = pair();
        let client = channel.exchange("ping", json!({}));
        let server = async {
            peer.read_request().await;
            peer.write("this is not json\n").await;
        };
        let (result, ()) = tokio::join!(client, server);

        assert!(result.unwrap_err().is_transport());
    }

    #[tokio::test]
    async fn id_mismatch_is_a_transport_error() {
        let (mut channel, mut peer, _) = pair();
        let client = channel.exchange("ping", json!({}));
        let server = async {
            peer.read_request().await;
            peer.write(&reply("someone-else", json!(null))).await;
        };
        let (result, ()) = tokio::join!(client, server);

        assert_eq!(transport_message(result.unwrap_err()), "Worker response id mismatch");
    }

    #[tokio::test]
    async fn id_mismatch_stays_transport_when_it_is_not_the_skipped_id() {
        let (mut channel, mut peer, _) = pair();
        let client = channel.exchange_raw("ping", json!({}), Some("abandoned"));
        let server = async {
            peer.read_request().await;
            peer.write(&reply("someone-else", json!(null))).await;
        };
        let (result, ()) = tokio::join!(client, server);

        assert_eq!(transport_message(result.unwrap_err()), "Worker response id mismatch");
    }

    fn error_reply(id: &str, code: &str, message: &str) -> String {
        format!(
            "{}\n",
            json!({"id": id, "ok": false, "result": null, "error": {"code": code, "message": message}})
        )
    }

    #[tokio::test]
    async fn error_reply_is_rendered_as_code_and_message_by_exchange() {
        let (mut channel, mut peer, _) = pair();
        let client = channel.exchange("revert", json!({}));
        let server = async {
            let (_, request) = peer.read_request().await;
            peer.write(&error_reply(&id_of(&request), "not_found", "rewrite not found")).await;
        };
        let (result, ()) = tokio::join!(client, server);

        match result.unwrap_err() {
            RequestError::Worker(message) => assert_eq!(message, "not_found: rewrite not found"),
            other => panic!("expected a worker error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn error_reply_returns_code_and_message_from_exchange_raw() {
        let (mut channel, mut peer, _) = pair();
        let client = channel.exchange_raw("revert", json!({}), None);
        let server = async {
            let (_, request) = peer.read_request().await;
            peer.write(&error_reply(&id_of(&request), "revert_expired", "revert window expired"))
                .await;
        };
        let (result, ()) = tokio::join!(client, server);

        let response = result.unwrap();
        assert!(!response.ok);
        let error = response.error.unwrap();
        assert_eq!(error.code, "revert_expired");
        assert_eq!(error.message, "revert window expired");
    }

    #[tokio::test]
    async fn error_reply_without_a_body_is_an_unknown_worker_error() {
        let (mut channel, mut peer, _) = pair();
        let client = channel.exchange("ping", json!({}));
        let server = async {
            let (_, request) = peer.read_request().await;
            let line = format!("{}\n", json!({"id": id_of(&request), "ok": false}));
            peer.write(&line).await;
        };
        let (result, ()) = tokio::join!(client, server);

        match result.unwrap_err() {
            RequestError::Worker(message) => assert_eq!(message, "Unknown worker error"),
            other => panic!("expected a worker error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn line_carrying_the_skip_id_is_discarded() {
        let (mut channel, mut peer, _) = pair();
        let client = channel.exchange_raw("ping", json!({}), Some("abandoned"));
        let server = async {
            let (_, request) = peer.read_request().await;
            peer.write(&reply("abandoned", json!({"late": true}))).await;
            peer.write(&reply(&id_of(&request), json!({"own": true}))).await;
        };
        let (result, ()) = tokio::join!(client, server);

        let response = result.unwrap();
        assert_eq!(response.result, Some(json!({"own": true})));
    }

    #[tokio::test]
    async fn reply_split_by_a_timeout_is_framed_whole_on_the_next_request() {
        let (mut channel, mut peer, _) = pair();
        let first = tokio::time::timeout(
            Duration::from_millis(50),
            channel.exchange_raw("rewrite", json!({}), None),
        );
        let server = async {
            let (_, request) = peer.read_request().await;
            let full = reply(&id_of(&request), json!({"late": true}));
            let (head, tail) = full.split_at(full.len() / 2);
            peer.write(head).await;
            (id_of(&request), tail.to_string())
        };
        let (first, (abandoned, tail)) = tokio::join!(first, server);

        assert!(first.is_err(), "the first request must time out between the halves");
        assert_eq!(channel.last_request_id(), Some(abandoned.as_str()));

        let second = channel.exchange_raw("ping", json!({}), Some(&abandoned));
        let server = async {
            let (_, request) = peer.read_request().await;
            peer.write(&tail).await;
            peer.write(&reply(&id_of(&request), json!({"own": true}))).await;
        };
        let (second, ()) = tokio::join!(second, server);

        assert_eq!(second.unwrap().result, Some(json!({"own": true})));
    }

    #[tokio::test]
    async fn write_cut_short_by_a_timeout_poisons_the_channel() {
        let (mut channel, _peer, _) = pair_with_buffer(16);
        let big = "x".repeat(1024);
        let stalled = tokio::time::timeout(
            Duration::from_millis(50),
            channel.exchange_raw("rewrite", json!({"text": big}), None),
        )
        .await;
        assert!(stalled.is_err(), "the write must stall on the full pipe");

        let error = channel.exchange("ping", json!({})).await.unwrap_err();

        assert_eq!(
            transport_message(error),
            "Previous request was interrupted while writing"
        );
    }

    #[tokio::test]
    async fn exchange_line_returns_the_matching_line_verbatim() {
        let (mut channel, mut peer, _) = pair();
        let client = channel.exchange_line("ping", json!({}), None);
        let server = async {
            let (_, request) = peer.read_request().await;
            let line = format!("{{\"id\":\"{}\",\"ok\":true,\"extra\":[1, 2]}}\n", id_of(&request));
            peer.write(&line).await;
            line
        };
        let (result, sent) = tokio::join!(client, server);

        assert_eq!(result.unwrap(), sent, "unknown keys must survive for the caller's strict parse");
    }

    #[tokio::test]
    async fn exchange_line_discards_the_skip_id_and_frames_a_split_reply_whole() {
        let (mut channel, mut peer, _) = pair();
        let first = tokio::time::timeout(
            Duration::from_millis(50),
            channel.exchange_line("rewrite", json!({}), None),
        );
        let server = async {
            let (_, request) = peer.read_request().await;
            let full = reply(&id_of(&request), json!({"late": true}));
            let (head, tail) = full.split_at(full.len() / 2);
            peer.write(head).await;
            (id_of(&request), tail.to_string())
        };
        let (first, (abandoned, tail)) = tokio::join!(first, server);
        assert!(first.is_err());
        assert_eq!(channel.last_request_id(), Some(abandoned.as_str()));

        let second = channel.exchange_line("ping", json!({}), Some(&abandoned));
        let server = async {
            let (_, request) = peer.read_request().await;
            peer.write(&tail).await;
            let own = reply(&id_of(&request), json!({"own": true}));
            peer.write(&own).await;
            own
        };
        let (second, own) = tokio::join!(second, server);

        assert_eq!(second.unwrap(), own);
    }

    #[tokio::test]
    async fn exchange_line_id_mismatch_and_unreadable_lines_are_transport_errors() {
        let (mut channel, mut peer, _) = pair();
        let client = channel.exchange_line("ping", json!({}), Some("abandoned"));
        let server = async {
            peer.read_request().await;
            peer.write(&reply("someone-else", json!(null))).await;
        };
        let (result, ()) = tokio::join!(client, server);
        assert_eq!(transport_message(result.unwrap_err()), "Worker response id mismatch");

        let (mut channel, mut peer, _) = pair();
        let client = channel.exchange_line("ping", json!({}), None);
        let server = async {
            peer.read_request().await;
            peer.write("{\"no_id\":true}\n").await;
        };
        let (result, ()) = tokio::join!(client, server);
        assert!(result.unwrap_err().is_transport());
    }

    #[tokio::test]
    async fn exchange_line_closed_channel_is_a_transport_error_with_the_exit_text() {
        let (mut channel, mut peer, _) = pair();
        let client = channel.exchange_line("ping", json!({}), None);
        let server = async move {
            peer.read_request().await;
            drop(peer);
        };
        let (result, ()) = tokio::join!(client, server);

        assert_eq!(transport_message(result.unwrap_err()), "Worker process exited unexpectedly");
    }
}
