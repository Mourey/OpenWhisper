//! Quill client: the validated spawn of `quill serve`, the typed engine protocol, the
//! `busy` / timeout / respawn rules and the command-facing error and result shapes.
//!
//! The engine types mirror the vendored schema in `contracts/quill` and reject unknown
//! fields, so a drifting engine fails loudly instead of being half understood.

// The Tauri commands that consume these items are wired in a later build step.
#![allow(dead_code)]

use crate::jsonl_client::{JsonlChannel, RequestError};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsString;
use std::io;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, MutexGuard};

/// Client-side limit for every quill request. The engine bounds its own upstream call at
/// 7 s, so this fires only when the engine is wedged.
pub const QUILL_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// The only message a typed parse failure may carry (spec N11).
const PROTOCOL_MISMATCH: &str = "quill protocol mismatch: reinstall quill or re-vendor the contract";

/// Stderr kept from the child, used only in transport error messages.
const STDERR_RING_BYTES: usize = 8 * 1024;

// ---- engine protocol (vendored schema, R9.5) -----------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuillMethod {
    Ping,
    ListModes,
    Rewrite,
    Outcome,
    Revert,
}

impl QuillMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::ListModes => "list_modes",
            Self::Rewrite => "rewrite",
            Self::Outcome => "outcome",
            Self::Revert => "revert",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuillRequest {
    pub id: String,
    pub method: QuillMethod,
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, bound(deserialize = "R: Deserialize<'de>"))]
pub struct QuillResponse<R> {
    pub id: String,
    pub ok: bool,
    #[serde(deserialize_with = "required")]
    pub result: Option<R>,
    #[serde(deserialize_with = "required")]
    pub error: Option<ErrorBody>,
}

/// An `Option` the schema marks required: it may be null but must be present.
fn required<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineErrorCode {
    NotFound,
    BadRequest,
    InternalError,
    Timeout,
    UpstreamError,
    NoKey,
    UnknownMode,
    RevertExpired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorBody {
    pub code: EngineErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Hotkey,
    Pill,
    Cli,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetModel {
    Claude,
    Gpt,
    Generic,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeForm {
    Task,
    System,
    Skill,
    Dictation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivered {
    Inserted,
    Copied,
    Printed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevertedVia {
    Inserted,
    Copied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PingParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListModesParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewriteParams {
    pub text: String,
    pub mode: String,
    pub surface: Surface,
    pub source_app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_model: Option<TargetModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeParams {
    pub rewrite_id: String,
    pub delivered: Delivered,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverted_via: Option<RevertedVia>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevertParams {
    pub rewrite_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PingResult {
    pub protocol: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub form: ModeForm,
    pub target_model: TargetModel,
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListModesResult {
    pub modes: Vec<ModeInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewriteResult {
    pub rewrite_id: String,
    pub text: String,
    pub mode: String,
    pub model: String,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeResult {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevertResult {
    pub text: String,
}

// ---- command boundary (R9.4) ---------------------------------------------------------

/// Every error a quill command returns carries one of these codes: the eight engine codes,
/// then the client-local ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuillErrorCode {
    NotFound,
    BadRequest,
    InternalError,
    Timeout,
    UpstreamError,
    NoKey,
    UnknownMode,
    RevertExpired,
    Busy,
    Transport,
    NoSelection,
    SecureField,
    TooLong,
    AccessibilityPermission,
    ModifiersHeld,
    TargetChanged,
    Disabled,
}

impl QuillErrorCode {
    pub const ALL: [Self; 17] = [
        Self::NotFound,
        Self::BadRequest,
        Self::InternalError,
        Self::Timeout,
        Self::UpstreamError,
        Self::NoKey,
        Self::UnknownMode,
        Self::RevertExpired,
        Self::Busy,
        Self::Transport,
        Self::NoSelection,
        Self::SecureField,
        Self::TooLong,
        Self::AccessibilityPermission,
        Self::ModifiersHeld,
        Self::TargetChanged,
        Self::Disabled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::BadRequest => "bad_request",
            Self::InternalError => "internal_error",
            Self::Timeout => "timeout",
            Self::UpstreamError => "upstream_error",
            Self::NoKey => "no_key",
            Self::UnknownMode => "unknown_mode",
            Self::RevertExpired => "revert_expired",
            Self::Busy => "busy",
            Self::Transport => "transport",
            Self::NoSelection => "no_selection",
            Self::SecureField => "secure_field",
            Self::TooLong => "too_long",
            Self::AccessibilityPermission => "accessibility_permission",
            Self::ModifiersHeld => "modifiers_held",
            Self::TargetChanged => "target_changed",
            Self::Disabled => "disabled",
        }
    }
}

impl From<EngineErrorCode> for QuillErrorCode {
    fn from(code: EngineErrorCode) -> Self {
        match code {
            EngineErrorCode::NotFound => Self::NotFound,
            EngineErrorCode::BadRequest => Self::BadRequest,
            EngineErrorCode::InternalError => Self::InternalError,
            EngineErrorCode::Timeout => Self::Timeout,
            EngineErrorCode::UpstreamError => Self::UpstreamError,
            EngineErrorCode::NoKey => Self::NoKey,
            EngineErrorCode::UnknownMode => Self::UnknownMode,
            EngineErrorCode::RevertExpired => Self::RevertExpired,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuillCommandError {
    pub code: QuillErrorCode,
    pub message: String,
}

impl QuillCommandError {
    pub fn new(code: QuillErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(QuillErrorCode::Transport, message)
    }

    fn protocol_mismatch() -> Self {
        Self::transport(PROTOCOL_MISMATCH)
    }

    fn busy() -> Self {
        Self::new(QuillErrorCode::Busy, "Another quill request is already running.")
    }
}

impl std::fmt::Display for QuillCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for QuillCommandError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuillRewriteSelectionResult {
    pub rewrite_id: String,
    pub mode: String,
    pub model: String,
    pub latency_ms: u64,
    pub end_to_end_ms: u64,
    pub anchored: bool,
    pub clipboard_replaced: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuillRewriteTextResult {
    pub rewrite_id: String,
    pub text: String,
    pub mode: String,
    pub model: String,
    pub latency_ms: u64,
}

/// How the original text reached the user after a revert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevertDelivered {
    Inserted,
    Copied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuillRevertResult {
    pub delivered: RevertDelivered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuillReportOutcomeResult {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuillModeInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub form: ModeForm,
    pub target_model: TargetModel,
    pub default: bool,
}

impl From<ModeInfo> for QuillModeInfo {
    fn from(mode: ModeInfo) -> Self {
        Self {
            id: mode.id,
            name: mode.name,
            description: mode.description,
            form: mode.form,
            target_model: mode.target_model,
            default: mode.default,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuillListModesResult {
    pub modes: Vec<QuillModeInfo>,
}

/// Arguments of `quill_rewrite_text`. `surface` stays a string so the command can refuse
/// anything but `pill` with a `bad_request` instead of a deserialisation failure.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuillRewriteTextArgs {
    pub text: String,
    pub mode: String,
    pub surface: String,
    #[serde(default)]
    pub source_app: Option<String>,
}

// ---- command validation (R10.1) ------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandMetadata {
    pub is_regular_file: bool,
    pub uid: u32,
    pub mode: u32,
}

/// The file-system facts command validation needs, behind a seam for tests.
pub trait CommandFs: Sync {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    /// Metadata of `path` after following symlinks.
    fn metadata(&self, path: &Path) -> io::Result<CommandMetadata>;
    fn current_uid(&self) -> u32;
}

/// The real file system.
pub struct NativeFs;

#[cfg(unix)]
mod native {
    extern "C" {
        pub fn getuid() -> u32;
    }
}

#[cfg(unix)]
impl CommandFs for NativeFs {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<CommandMetadata> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path)?;
        Ok(CommandMetadata {
            is_regular_file: metadata.is_file(),
            uid: metadata.uid(),
            mode: metadata.mode() & 0o7777,
        })
    }

    fn current_uid(&self) -> u32 {
        // SAFETY: getuid takes no arguments, cannot fail and touches no memory.
        unsafe { native::getuid() }
    }
}

#[cfg(not(unix))]
impl CommandFs for NativeFs {
    fn canonicalize(&self, _path: &Path) -> io::Result<PathBuf> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn metadata(&self, _path: &Path) -> io::Result<CommandMetadata> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn current_uid(&self) -> u32 {
        0
    }
}

/// Checks the configured quill command and returns the canonical path to execute. The
/// command must be absolute and resolve to a regular file the current user owns, with the
/// owner execute bit set and no group or world write bit. Nothing is expanded, trimmed or
/// searched on `PATH`, and no environment variable is read.
pub fn validate_quill_command(raw: &str, fs: &dyn CommandFs) -> Result<PathBuf, String> {
    let configured = Path::new(raw);
    if !configured.is_absolute() {
        return Err("Quill command must be an absolute path.".to_string());
    }
    let canonical = fs
        .canonicalize(configured)
        .map_err(|_| format!("Quill command not found: {}", configured.display()))?;
    let metadata = fs
        .metadata(&canonical)
        .map_err(|_| format!("Quill command not found: {}", canonical.display()))?;
    let shown = canonical.display();
    if !metadata.is_regular_file {
        return Err(format!("Quill command is not a regular file: {shown}"));
    }
    if metadata.uid != fs.current_uid() {
        return Err(format!("Quill command is not owned by you: {shown}"));
    }
    if metadata.mode & 0o020 != 0 {
        return Err(format!("Quill command is group-writable: {shown}"));
    }
    if metadata.mode & 0o002 != 0 {
        return Err(format!("Quill command is world-writable: {shown}"));
    }
    if metadata.mode & 0o100 == 0 {
        return Err(format!("Quill command is not executable: {shown}"));
    }
    Ok(canonical)
}

// ---- the client (R9, R9.2, R9.3) -----------------------------------------------------

pub struct QuillClient {
    channel: JsonlChannel<ChildStdin, ChildStdout>,
    child: Child,
    /// The command as configured, compared on every call to notice a settings change.
    command: PathBuf,
    /// The canonical target that was executed.
    canonical: PathBuf,
    /// Id of a request that timed out; its late reply is discarded.
    abandoned: Option<String>,
    consecutive_timeouts: u8,
    timeout: Duration,
    /// Set by a transport error: the client must not be reused.
    broken: bool,
}

impl QuillClient {
    /// Starts `<canonical> serve` with an environment of `HOME` and
    /// `PATH=/usr/bin:/bin:<parent of the configured path>` only, then checks that `ping`
    /// answers exactly protocol `quill` version `1`. `home` is injected; the caller reads it.
    pub async fn spawn(
        configured: &Path,
        canonical: &Path,
        home: Option<&Path>,
        timeout: Duration,
    ) -> Result<Self, QuillCommandError> {
        let home = home.ok_or_else(|| QuillCommandError::transport("HOME is not set, so quill cannot be started."))?;
        let parent = configured.parent().ok_or_else(|| {
            QuillCommandError::transport(format!("Quill command has no parent directory: {}", configured.display()))
        })?;
        let mut path = OsString::from("/usr/bin:/bin:");
        path.push(parent);

        let mut child = Command::new(canonical)
            .arg("serve")
            .env_clear()
            .env("HOME", home)
            .env("PATH", path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| QuillCommandError::transport(format!("Failed to start quill: {error}")))?;

        let pipe_missing = |which: &str| QuillCommandError::transport(format!("Failed to open quill {which}."));
        let stdin = child.stdin.take().ok_or_else(|| pipe_missing("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| pipe_missing("stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| pipe_missing("stderr"))?;

        let stderr_ring = Arc::new(Mutex::new(String::new()));
        tokio::spawn(capture_stderr(stderr, stderr_ring.clone()));

        let mut client = Self {
            channel: JsonlChannel::new(stdin, stdout, stderr_ring),
            child,
            command: configured.to_path_buf(),
            canonical: canonical.to_path_buf(),
            abandoned: None,
            consecutive_timeouts: 0,
            timeout,
            broken: false,
        };

        let ping: PingResult = client.request_typed(QuillMethod::Ping, &PingParams {}).await?;
        if ping.protocol != "quill" || ping.version != "1" {
            return Err(QuillCommandError::transport(format!(
                "quill speaks protocol {} version {}; expected quill version 1.",
                ping.protocol, ping.version
            )));
        }
        Ok(client)
    }

    /// The command path this client was started from, as configured.
    pub fn command(&self) -> &Path {
        &self.command
    }

    /// True after a transport error; the owner must drop the client.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// Sends one request and parses the answer through the typed contract.
    ///
    /// An error reply maps to its code with the engine's message unchanged. A client-side
    /// expiry returns `timeout` and keeps the connection, recording the request id so the
    /// late reply is discarded; a second consecutive expiry, a dead child, an id mismatch
    /// or a reply that does not fit the contract is `transport` and marks the client broken.
    pub async fn request_typed<P, R>(&mut self, method: QuillMethod, params: &P) -> Result<R, QuillCommandError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let result = self.exchange_typed(method, params).await;
        if matches!(&result, Err(error) if error.code == QuillErrorCode::Transport) {
            self.broken = true;
        }
        result
    }

    async fn exchange_typed<P, R>(&mut self, method: QuillMethod, params: &P) -> Result<R, QuillCommandError>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let params = serde_json::to_value(params)
            .map_err(|error| QuillCommandError::transport(format!("Failed to encode the quill request: {error}")))?;
        let skip_id = self.abandoned.clone();
        let outcome = tokio::time::timeout(
            self.timeout,
            self.channel.exchange_line(method.as_str(), params, skip_id.as_deref()),
        )
        .await;

        let line = match outcome {
            Err(_elapsed) => {
                self.abandoned = self.channel.last_request_id().map(str::to_owned);
                self.consecutive_timeouts += 1;
                return Err(if self.consecutive_timeouts >= 2 {
                    QuillCommandError::transport(format!(
                        "quill did not answer {} twice in a row; restarting it.",
                        method.as_str()
                    ))
                } else {
                    QuillCommandError::new(
                        QuillErrorCode::Timeout,
                        format!("quill did not answer {} within {:?}.", method.as_str(), self.timeout),
                    )
                });
            }
            Ok(Err(RequestError::Transport(message) | RequestError::Worker(message))) => {
                return Err(QuillCommandError::transport(message));
            }
            Ok(Ok(line)) => line,
        };
        self.abandoned = None;
        self.consecutive_timeouts = 0;

        // The whole envelope goes through the strict typed contract.
        let response: QuillResponse<R> =
            serde_json::from_str(&line).map_err(|_| QuillCommandError::protocol_mismatch())?;
        match (response.ok, response.result, response.error) {
            (true, Some(result), None) => Ok(result),
            (false, None, Some(body)) => Err(QuillCommandError::new(body.code.into(), body.message)),
            _ => Err(QuillCommandError::protocol_mismatch()),
        }
    }
}

impl Drop for QuillClient {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Keeps the last 8 KiB the child wrote to stderr. Nothing is printed or logged.
async fn capture_stderr(mut stderr: ChildStderr, ring: Arc<Mutex<String>>) {
    let mut chunk = [0u8; 1024];
    loop {
        let read = match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let mut text = ring.lock().await;
        text.push_str(&String::from_utf8_lossy(&chunk[..read]));
        if text.len() > STDERR_RING_BYTES {
            let mut cut = text.len() - STDERR_RING_BYTES;
            while !text.is_char_boundary(cut) {
                cut += 1;
            }
            text.drain(..cut);
        }
    }
}

// ---- the slot (R9.3) -----------------------------------------------------------------

/// What a call needs to find, validate and start the engine.
pub struct QuillEnv<'a> {
    pub fs: &'a dyn CommandFs,
    pub home: Option<&'a Path>,
    /// Limit for each request on a running client.
    pub timeout: Duration,
    /// Limit for the `ping` that starts a client. Production passes the same value as
    /// `timeout`; tests shorten `timeout` alone so a slow process start is not an expiry.
    pub spawn_timeout: Duration,
}

/// Exclusive use of the slot's client. Dropping it puts the client back, or discards it
/// when a transport error broke it, so the next call respawns.
pub struct QuillLease<'a> {
    slot: MutexGuard<'a, Option<QuillClient>>,
    client: Option<QuillClient>,
}

impl QuillLease<'_> {
    pub fn client(&mut self) -> &mut QuillClient {
        self.client
            .as_mut()
            .expect("a lease holds its client until it is dropped")
    }
}

impl Deref for QuillLease<'_> {
    type Target = QuillClient;

    fn deref(&self) -> &QuillClient {
        self.client
            .as_ref()
            .expect("a lease holds its client until it is dropped")
    }
}

impl DerefMut for QuillLease<'_> {
    fn deref_mut(&mut self) -> &mut QuillClient {
        self.client()
    }
}

impl Drop for QuillLease<'_> {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            if !client.is_broken() {
                *self.slot = Some(client);
            }
        }
    }
}

/// Takes the slot without waiting (`busy` when another call holds it), revalidates the
/// configured command on every call, drops a client started from a different command and
/// spawns one when the slot is empty.
pub async fn lease_quill<'a>(
    slot: &'a Mutex<Option<QuillClient>>,
    raw_command: &str,
    env: &QuillEnv<'_>,
) -> Result<QuillLease<'a>, QuillCommandError> {
    let mut guard = slot.try_lock().map_err(|_| QuillCommandError::busy())?;
    let canonical = match validate_quill_command(raw_command, env.fs) {
        Ok(canonical) => canonical,
        Err(message) => {
            *guard = None;
            return Err(QuillCommandError::new(QuillErrorCode::Disabled, message));
        }
    };
    let configured = Path::new(raw_command);
    let reusable = guard
        .as_ref()
        .is_some_and(|client| client.command == configured && client.canonical == canonical);
    let client = if reusable {
        guard.take()
    } else {
        *guard = None;
        let mut client = QuillClient::spawn(configured, &canonical, env.home, env.spawn_timeout).await?;
        client.timeout = env.timeout;
        Some(client)
    };
    Ok(QuillLease { slot: guard, client })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;
    use serde::Serialize;
    use serde_json::{json, Value};
    use std::collections::{BTreeSet, HashMap};
    use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use tokio::sync::Mutex;
    use uuid::Uuid;

    // ---- helpers -------------------------------------------------------------------

    const MISMATCH: &str = "quill protocol mismatch: reinstall quill or re-vendor the contract";
    const PING_OK: &str = r#"{"protocol":"quill","version":"1"}"#;

    /// A temporary directory removed when the test ends, even on a panic.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(Uuid::new_v4().to_string());
            std::fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A `/bin/sh` script standing in for `quill`: it records its argv, environment, spawn
    /// count and every request line next to itself, answers `ping` with `__PING__` and runs
    /// `__BODY__` for every other request, with `$id`, `$method`, `$n` (the count of
    /// non-ping requests), `$dir`, `reply`, `fail` and `modes` in scope.
    const SCRIPT: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo start >> "$dir/spawns"
printf '%s\n' "$@" > "$dir/argv"
/usr/bin/env > "$dir/env"
reply() { printf '{"id":"%s","ok":true,"result":%s,"error":null}\n' "$id" "$1"; }
fail() { printf '{"id":"%s","ok":false,"result":null,"error":{"code":"%s","message":"%s"}}\n' "$id" "$1" "$2"; }
modes() { printf '{"modes":[{"id":"%s","name":"n","description":"d","form":"task","target_model":"claude","default":true}]}' "$1"; }
n=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$dir/requests"
  id=$(printf '%s' "$line" | sed 's/^{"id":"\([^"]*\)".*/\1/')
  method=$(printf '%s' "$line" | sed 's/^{"id":"[^"]*","method":"\([^"]*\)".*/\1/')
  if [ "$method" = ping ]; then reply '__PING__'; continue; fi
  n=$((n+1))
  __BODY__
done
"#;

    const ECHO_MODES: &str = r#"reply "$(modes only)""#;

    struct FakeQuill {
        dir: TempDir,
        command: PathBuf,
    }

    impl FakeQuill {
        fn with_ping(ping_result: &str, body: &str) -> Self {
            let dir = TempDir::new();
            let command = dir.0.join("quill");
            let script = SCRIPT.replace("__PING__", ping_result).replace("__BODY__", body);
            std::fs::write(&command, script).unwrap();
            std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self { dir, command }
        }

        fn new(body: &str) -> Self {
            Self::with_ping(PING_OK, body)
        }

        fn command(&self) -> &str {
            self.command.to_str().unwrap()
        }

        fn file(&self, name: &str) -> PathBuf {
            self.dir.0.join(name)
        }

        fn lines(&self, name: &str) -> Vec<String> {
            std::fs::read_to_string(self.file(name))
                .map(|text| text.lines().map(str::to_string).collect())
                .unwrap_or_default()
        }

        fn spawn_count(&self) -> usize {
            self.lines("spawns").len()
        }
    }

    async fn spawn_fake(fake: &FakeQuill, timeout: Duration) -> Result<QuillClient, QuillCommandError> {
        let canonical = std::fs::canonicalize(&fake.command).unwrap();
        QuillClient::spawn(&fake.command, &canonical, Some(fake.dir.0.as_path()), timeout).await
    }

    fn env_for<'a>(fake: &'a FakeQuill, fs: &'a NativeFs, timeout: Duration) -> QuillEnv<'a> {
        QuillEnv {
            fs,
            home: Some(fake.dir.0.as_path()),
            timeout,
            spawn_timeout: Duration::from_secs(5),
        }
    }

    async fn list_modes(
        slot: &Mutex<Option<QuillClient>>,
        command: &str,
        env: &QuillEnv<'_>,
    ) -> Result<ListModesResult, QuillCommandError> {
        let mut lease = lease_quill(slot, command, env).await?;
        lease
            .client()
            .request_typed(QuillMethod::ListModes, &ListModesParams {})
            .await
    }

    fn only_mode_id(result: ListModesResult) -> String {
        assert_eq!(result.modes.len(), 1);
        result.modes.into_iter().next().unwrap().id
    }

    async fn wait_for(path: &Path) {
        for _ in 0..500 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {}", path.display());
    }

    fn set(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|key| key.to_string()).collect()
    }

    fn key_set(value: &impl Serialize) -> BTreeSet<String> {
        serde_json::to_value(value)
            .unwrap()
            .as_object()
            .expect("a JSON object")
            .keys()
            .cloned()
            .collect()
    }

    // ---- quill_contract_ -----------------------------------------------------------

    fn contract_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("contracts/quill")
    }

    fn schema() -> Value {
        serde_json::from_str(&std::fs::read_to_string(contract_dir().join("quill-protocol.v1.json")).unwrap())
            .unwrap()
    }

    /// `properties` (or `required`) of a `$defs` entry, as a key set.
    fn schema_keys(def: &str, field: &str) -> BTreeSet<String> {
        let schema = schema();
        let entry = &schema["$defs"][def][field];
        if let Some(object) = entry.as_object() {
            object.keys().cloned().collect()
        } else {
            entry
                .as_array()
                .unwrap_or_else(|| panic!("{def}.{field} is neither an object nor an array"))
                .iter()
                .map(|key| key.as_str().unwrap().to_string())
                .collect()
        }
    }

    fn mode_item_keys() -> BTreeSet<String> {
        schema()["$defs"]["list_modes_result"]["properties"]["modes"]["items"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    /// Deserialises into `T` and checks serialising it again gives back the same JSON.
    fn round_trip<T: Serialize + DeserializeOwned>(raw: &Value) -> T {
        let typed: T = serde_json::from_value(raw.clone())
            .unwrap_or_else(|error| panic!("{} does not fit {}: {error}", raw, std::any::type_name::<T>()));
        assert_eq!(&serde_json::to_value(&typed).unwrap(), raw);
        typed
    }

    fn fixtures() -> Vec<(String, Value)> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(contract_dir().join("fixtures")).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            found.push((name, serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()));
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found
    }

    #[test]
    fn quill_contract_request_response_fixtures_deserialise_into_typed_structs() {
        let mut checked = 0;
        for (name, fixture) in fixtures() {
            let Some(request) = fixture.get("request") else { continue };
            let typed: QuillRequest = round_trip(request);
            let response = &fixture["response"];
            match typed.method {
                QuillMethod::Ping => {
                    round_trip::<PingParams>(&typed.params);
                    round_trip::<QuillResponse<PingResult>>(response);
                }
                QuillMethod::ListModes => {
                    round_trip::<ListModesParams>(&typed.params);
                    round_trip::<QuillResponse<ListModesResult>>(response);
                }
                QuillMethod::Rewrite => {
                    round_trip::<RewriteParams>(&typed.params);
                    round_trip::<QuillResponse<RewriteResult>>(response);
                }
                QuillMethod::Outcome => {
                    round_trip::<OutcomeParams>(&typed.params);
                    round_trip::<QuillResponse<OutcomeResult>>(response);
                }
                QuillMethod::Revert => {
                    round_trip::<RevertParams>(&typed.params);
                    round_trip::<QuillResponse<RevertResult>>(response);
                }
            }
            assert_eq!(typed.id, response["id"], "{name}: request and response ids differ");
            checked += 1;
        }
        assert_eq!(checked, 12, "twelve fixtures carry a typed request");
    }

    #[test]
    fn quill_contract_request_line_fixtures_keep_the_line_opaque_and_type_only_the_response() {
        let mut checked = 0;
        for (name, fixture) in fixtures() {
            let Some(line) = fixture.get("request_line") else { continue };
            let line = line.as_str().expect("request_line is a string");
            // The line is written verbatim; it is deliberately not a valid typed request.
            assert!(
                serde_json::from_str::<QuillRequest>(line).is_err(),
                "{name}: the raw line must not fit the typed request"
            );
            let response: QuillResponse<OutcomeResult> = round_trip(&fixture["response"]);
            assert!(!response.ok, "{name}: a raw-line fixture answers with an error");
            checked += 1;
        }
        assert_eq!(checked, 2, "the unparseable line and the unknown method");
    }

    #[test]
    fn quill_contract_every_error_fixture_code_is_a_typed_engine_code() {
        let mut codes = BTreeSet::new();
        for (_, fixture) in fixtures() {
            let response: QuillResponse<Value> = serde_json::from_value(fixture["response"].clone()).unwrap();
            if let Some(error) = response.error {
                codes.insert(serde_json::to_value(error.code).unwrap().as_str().unwrap().to_string());
            }
        }
        assert_eq!(
            codes,
            set(&[
                "not_found",
                "bad_request",
                "internal_error",
                "timeout",
                "upstream_error",
                "no_key",
                "unknown_mode",
                "revert_expired"
            ])
        );
    }

    #[test]
    fn quill_contract_vendored_file_is_the_two_pinned_lines() {
        let vendored = std::fs::read_to_string(contract_dir().join("VENDORED")).unwrap();
        let id = schema()["$id"].as_str().unwrap().to_string();
        assert_eq!(id, "https://quill.local/contracts/quill-protocol/v1");
        let lines: Vec<&str> = vendored.split_inclusive('\n').collect();
        assert_eq!(lines.len(), 2, "VENDORED is exactly two lines: {vendored:?}");
        let commit = lines[0].strip_prefix("quill_commit=").expect("first line pins the commit");
        let commit = commit.strip_suffix('\n').expect("first line ends with a newline");
        assert_eq!(commit.len(), 40);
        assert!(commit.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(lines[1], format!("schema_id={id}\n"));
    }

    #[test]
    fn quill_contract_serialised_key_sets_equal_the_schema_properties() {
        let request = QuillRequest {
            id: "i".into(),
            method: QuillMethod::Rewrite,
            params: json!({}),
        };
        assert_eq!(key_set(&request), schema_keys("request", "properties"));

        let response = QuillResponse {
            id: "i".into(),
            ok: false,
            result: Some(OutcomeResult {}),
            error: Some(ErrorBody {
                code: EngineErrorCode::NotFound,
                message: "m".into(),
            }),
        };
        assert_eq!(key_set(&response), schema_keys("response", "properties"));

        let error = ErrorBody {
            code: EngineErrorCode::Timeout,
            message: "m".into(),
        };
        assert_eq!(key_set(&error), schema_keys("error", "properties"));

        assert_eq!(key_set(&PingParams {}), schema_keys("ping_params", "properties"));
        assert_eq!(key_set(&ListModesParams {}), schema_keys("list_modes_params", "properties"));

        let rewrite_params = RewriteParams {
            text: "t".into(),
            mode: "m".into(),
            surface: Surface::Hotkey,
            source_app: Some("app".into()),
            target_model: Some(TargetModel::Claude),
        };
        assert_eq!(key_set(&rewrite_params), schema_keys("rewrite_params", "properties"));
        let minimal = RewriteParams {
            source_app: None,
            target_model: None,
            ..rewrite_params
        };
        assert_eq!(key_set(&minimal), schema_keys("rewrite_params", "required"));

        let outcome_params = OutcomeParams {
            rewrite_id: "r".into(),
            delivered: Delivered::Inserted,
            reverted_via: Some(RevertedVia::Copied),
        };
        assert_eq!(key_set(&outcome_params), schema_keys("outcome_params", "properties"));
        let minimal = OutcomeParams {
            reverted_via: None,
            ..outcome_params
        };
        assert_eq!(key_set(&minimal), schema_keys("outcome_params", "required"));

        let revert_params = RevertParams { rewrite_id: "r".into() };
        assert_eq!(key_set(&revert_params), schema_keys("revert_params", "properties"));

        let ping_result = PingResult {
            protocol: "quill".into(),
            version: "1".into(),
        };
        assert_eq!(key_set(&ping_result), schema_keys("ping_result", "properties"));

        let mode = ModeInfo {
            id: "i".into(),
            name: "n".into(),
            description: "d".into(),
            form: ModeForm::Task,
            target_model: TargetModel::Any,
            default: true,
        };
        assert_eq!(key_set(&mode), mode_item_keys());
        let modes = ListModesResult { modes: vec![mode] };
        assert_eq!(key_set(&modes), schema_keys("list_modes_result", "properties"));

        let rewrite_result = RewriteResult {
            rewrite_id: "r".into(),
            text: "t".into(),
            mode: "m".into(),
            model: "x".into(),
            latency_ms: 1,
        };
        assert_eq!(key_set(&rewrite_result), schema_keys("rewrite_result", "properties"));

        assert_eq!(key_set(&OutcomeResult {}), schema_keys("outcome_result", "properties"));
        assert_eq!(
            key_set(&RevertResult { text: "t".into() }),
            schema_keys("revert_result", "properties")
        );
    }

    /// The closed `fixture` definition is checked with a local type, since only the tests
    /// read fixtures.
    #[test]
    fn quill_contract_fixture_definition_key_set_is_request_request_line_response() {
        #[derive(Serialize)]
        struct Fixture {
            request: Option<QuillRequest>,
            request_line: Option<String>,
            response: QuillResponse<OutcomeResult>,
        }
        let fixture = Fixture {
            request: Some(QuillRequest {
                id: "i".into(),
                method: QuillMethod::Ping,
                params: json!({}),
            }),
            request_line: Some("not json".into()),
            response: QuillResponse {
                id: "i".into(),
                ok: true,
                result: Some(OutcomeResult {}),
                error: None,
            },
        };
        assert_eq!(key_set(&fixture), schema_keys("fixture", "properties"));
        assert_eq!(schema_keys("fixture", "properties"), set(&["request", "request_line", "response"]));
    }

    #[test]
    fn quill_contract_engine_types_reject_unknown_fields() {
        fn rejects_extra<T: DeserializeOwned>(valid: Value) {
            serde_json::from_value::<T>(valid.clone()).expect("the valid form must parse");
            let mut extended = valid;
            extended.as_object_mut().unwrap().insert("unexpected".into(), json!(1));
            assert!(
                serde_json::from_value::<T>(extended).is_err(),
                "{} accepted an unknown field",
                std::any::type_name::<T>()
            );
        }
        rejects_extra::<QuillRequest>(json!({"id": "i", "method": "ping", "params": {}}));
        rejects_extra::<QuillResponse<PingResult>>(json!({"id": "i", "ok": true, "result": null, "error": null}));
        rejects_extra::<ErrorBody>(json!({"code": "timeout", "message": "m"}));
        rejects_extra::<PingParams>(json!({}));
        rejects_extra::<ListModesParams>(json!({}));
        rejects_extra::<RewriteParams>(json!({"text": "t", "mode": "m", "surface": "pill", "source_app": null}));
        rejects_extra::<OutcomeParams>(json!({"rewrite_id": "r", "delivered": "copied"}));
        rejects_extra::<RevertParams>(json!({"rewrite_id": "r"}));
        rejects_extra::<PingResult>(json!({"protocol": "quill", "version": "1"}));
        rejects_extra::<ListModesResult>(json!({"modes": []}));
        rejects_extra::<ModeInfo>(json!({
            "id": "i", "name": "n", "description": "d", "form": "skill", "target_model": "gpt", "default": false
        }));
        rejects_extra::<RewriteResult>(json!({
            "rewrite_id": "r", "text": "t", "mode": "m", "model": "x", "latency_ms": 3
        }));
        rejects_extra::<OutcomeResult>(json!({}));
        rejects_extra::<RevertResult>(json!({"text": "t"}));
    }

    // ---- quill_command_validation_ -------------------------------------------------

    struct FakeFs {
        uid: u32,
        /// configured path -> canonical path
        canonical: HashMap<PathBuf, PathBuf>,
        /// canonical path -> metadata
        metadata: HashMap<PathBuf, CommandMetadata>,
    }

    impl FakeFs {
        fn with(path: &str, meta: CommandMetadata) -> Self {
            Self::linked(path, path, meta)
        }

        fn linked(link: &str, target: &str, meta: CommandMetadata) -> Self {
            Self {
                uid: 501,
                canonical: HashMap::from([(PathBuf::from(link), PathBuf::from(target))]),
                metadata: HashMap::from([(PathBuf::from(target), meta)]),
            }
        }
    }

    impl CommandFs for FakeFs {
        fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
            self.canonical
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        }

        fn metadata(&self, path: &Path) -> std::io::Result<CommandMetadata> {
            self.metadata
                .get(path)
                .copied()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        }

        fn current_uid(&self) -> u32 {
            self.uid
        }
    }

    fn meta(is_regular_file: bool, uid: u32, mode: u32) -> CommandMetadata {
        CommandMetadata {
            is_regular_file,
            uid,
            mode,
        }
    }

    const QUILL: &str = "/Users/me/.local/bin/quill";

    #[test]
    fn quill_command_validation_relative_path_is_refused() {
        let fs = FakeFs::with(QUILL, meta(true, 501, 0o700));
        for raw in ["quill", "./quill", "~/.local/bin/quill", "../bin/quill", ""] {
            assert_eq!(
                validate_quill_command(raw, &fs).unwrap_err(),
                "Quill command must be an absolute path.",
                "{raw:?}"
            );
        }
    }

    #[test]
    fn quill_command_validation_missing_file_is_refused_with_the_path() {
        let fs = FakeFs::with(QUILL, meta(true, 501, 0o700));
        assert_eq!(
            validate_quill_command("/Users/me/missing", &fs).unwrap_err(),
            "Quill command not found: /Users/me/missing"
        );
    }

    #[test]
    fn quill_command_validation_non_regular_file_is_refused() {
        let fs = FakeFs::with(QUILL, meta(false, 501, 0o700));
        assert_eq!(
            validate_quill_command(QUILL, &fs).unwrap_err(),
            format!("Quill command is not a regular file: {QUILL}")
        );
    }

    #[test]
    fn quill_command_validation_foreign_owner_is_refused() {
        let fs = FakeFs::with(QUILL, meta(true, 0, 0o700));
        assert_eq!(
            validate_quill_command(QUILL, &fs).unwrap_err(),
            format!("Quill command is not owned by you: {QUILL}")
        );
    }

    #[test]
    fn quill_command_validation_group_writable_is_refused() {
        let fs = FakeFs::with(QUILL, meta(true, 501, 0o770));
        assert_eq!(
            validate_quill_command(QUILL, &fs).unwrap_err(),
            format!("Quill command is group-writable: {QUILL}")
        );
    }

    #[test]
    fn quill_command_validation_world_writable_is_refused() {
        let fs = FakeFs::with(QUILL, meta(true, 501, 0o707));
        assert_eq!(
            validate_quill_command(QUILL, &fs).unwrap_err(),
            format!("Quill command is world-writable: {QUILL}")
        );
    }

    #[test]
    fn quill_command_validation_non_executable_is_refused() {
        let fs = FakeFs::with(QUILL, meta(true, 501, 0o600));
        assert_eq!(
            validate_quill_command(QUILL, &fs).unwrap_err(),
            format!("Quill command is not executable: {QUILL}")
        );
        // Only the owner execute bit counts.
        let fs = FakeFs::with(QUILL, meta(true, 501, 0o611));
        assert_eq!(
            validate_quill_command(QUILL, &fs).unwrap_err(),
            format!("Quill command is not executable: {QUILL}")
        );
    }

    #[test]
    fn quill_command_validation_symlink_to_a_failing_target_is_refused_with_the_target_path() {
        let target = "/Users/me/.local/share/uv/tools/quill/bin/quill";
        let fs = FakeFs::linked(QUILL, target, meta(true, 501, 0o770));
        assert_eq!(
            validate_quill_command(QUILL, &fs).unwrap_err(),
            format!("Quill command is group-writable: {target}")
        );
    }

    #[test]
    fn quill_command_validation_valid_symlink_returns_the_canonical_path() {
        let target = "/Users/me/.local/share/uv/tools/quill/bin/quill";
        for mode in [0o700, 0o755, 0o500] {
            let fs = FakeFs::linked(QUILL, target, meta(true, 501, mode));
            assert_eq!(validate_quill_command(QUILL, &fs), Ok(PathBuf::from(target)), "{mode:o}");
        }
    }

    #[test]
    fn quill_command_validation_native_file_is_checked_for_real() {
        let dir = TempDir::new();
        let file = dir.0.join("quill");
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        let canonical = std::fs::canonicalize(&file).unwrap();
        let raw = file.to_str().unwrap();

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(validate_quill_command(raw, &NativeFs), Ok(canonical.clone()));

        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert_eq!(
            validate_quill_command(raw, &NativeFs).unwrap_err(),
            format!("Quill command is group-writable: {}", canonical.display())
        );

        let link = dir.0.join("link");
        symlink(&file, &link).unwrap();
        assert_eq!(
            validate_quill_command(link.to_str().unwrap(), &NativeFs).unwrap_err(),
            format!("Quill command is group-writable: {}", canonical.display()),
            "a symlink is judged by its target"
        );

        assert_eq!(
            validate_quill_command(dir.0.to_str().unwrap(), &NativeFs).unwrap_err(),
            format!(
                "Quill command is not a regular file: {}",
                std::fs::canonicalize(&dir.0).unwrap().display()
            )
        );
    }

    // ---- quill_spawn_ --------------------------------------------------------------

    #[tokio::test]
    async fn quill_spawn_accepts_a_matching_ping() {
        let fake = FakeQuill::new(ECHO_MODES);
        let client = spawn_fake(&fake, Duration::from_secs(5)).await.unwrap();
        assert_eq!(client.command(), fake.command.as_path());
        assert_eq!(fake.spawn_count(), 1);
    }

    #[tokio::test]
    async fn quill_spawn_refuses_version_two() {
        let fake = FakeQuill::with_ping(r#"{"protocol":"quill","version":"2"}"#, ECHO_MODES);
        let error = spawn_fake(&fake, Duration::from_secs(5)).await.err().unwrap();
        assert_eq!(error.code, QuillErrorCode::Transport);
    }

    #[tokio::test]
    async fn quill_spawn_refuses_another_protocol() {
        let fake = FakeQuill::with_ping(r#"{"protocol":"other","version":"1"}"#, ECHO_MODES);
        let error = spawn_fake(&fake, Duration::from_secs(5)).await.err().unwrap();
        assert_eq!(error.code, QuillErrorCode::Transport);
    }

    #[tokio::test]
    async fn quill_spawn_refuses_a_ping_with_an_unknown_field() {
        let fake = FakeQuill::with_ping(r#"{"protocol":"quill","version":"1","build":"x"}"#, ECHO_MODES);
        let error = spawn_fake(&fake, Duration::from_secs(5)).await.err().unwrap();
        assert_eq!(error.code, QuillErrorCode::Transport);
        assert_eq!(error.message, MISMATCH);
    }

    #[tokio::test]
    async fn quill_spawn_environment_is_home_and_path_only() {
        let fake = FakeQuill::new(ECHO_MODES);
        let _client = spawn_fake(&fake, Duration::from_secs(5)).await.unwrap();

        let env_text = std::fs::read_to_string(fake.file("env")).unwrap();
        let vars: HashMap<&str, &str> = env_text.lines().filter_map(|line| line.split_once('=')).collect();
        let allowed: BTreeSet<&str> = ["HOME", "PATH", "PWD", "SHLVL", "_"].into_iter().collect();
        let names: BTreeSet<&str> = vars.keys().copied().collect();
        assert!(names.is_subset(&allowed), "unexpected variables in the child: {names:?}");
        assert!(!vars.contains_key("OPENAI_API_KEY"));
        assert!(!vars.contains_key("OPENROUTER_API_KEY"));
        assert_eq!(vars["HOME"], fake.dir.0.to_str().unwrap());
        assert_eq!(
            vars["PATH"],
            format!("/usr/bin:/bin:{}", fake.dir.0.to_str().unwrap()),
            "PATH ends with the parent of the configured path"
        );
    }

    #[tokio::test]
    async fn quill_spawn_runs_serve_as_the_only_argument() {
        let fake = FakeQuill::new(ECHO_MODES);
        let _client = spawn_fake(&fake, Duration::from_secs(5)).await.unwrap();
        assert_eq!(std::fs::read_to_string(fake.file("argv")).unwrap(), "serve\n");
    }

    #[tokio::test]
    async fn quill_spawn_is_refused_without_home() {
        let fake = FakeQuill::new(ECHO_MODES);
        let canonical = std::fs::canonicalize(&fake.command).unwrap();
        let error = QuillClient::spawn(&fake.command, &canonical, None, Duration::from_secs(5))
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, QuillErrorCode::Transport);
        assert_eq!(fake.spawn_count(), 0, "no child may start without HOME");
    }

    #[tokio::test]
    async fn quill_spawn_failure_is_a_transport_error() {
        let dir = TempDir::new();
        let missing = dir.0.join("missing");
        let error = QuillClient::spawn(&missing, &missing, Some(dir.0.as_path()), Duration::from_secs(5))
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, QuillErrorCode::Transport);
    }

    // ---- quill_transport_ ----------------------------------------------------------

    #[tokio::test]
    async fn quill_transport_reply_with_another_id_is_transport_and_empties_the_slot() {
        let fake = FakeQuill::new(r#"id=someone-else; reply "$(modes only)""#);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        let error = list_modes(&slot, fake.command(), &env).await.unwrap_err();

        assert_eq!(error.code, QuillErrorCode::Transport);
        assert!(slot.try_lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn quill_transport_typed_parse_failure_uses_the_mismatch_message() {
        let fake = FakeQuill::new(r#"reply "$(modes only | sed 's/}$/,"extra":1}/')""#);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        let error = list_modes(&slot, fake.command(), &env).await.unwrap_err();

        assert_eq!(error.code, QuillErrorCode::Transport);
        assert_eq!(error.message, MISMATCH);
        assert!(slot.try_lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn quill_transport_engine_code_outside_the_contract_uses_the_mismatch_message() {
        let fake = FakeQuill::new(r#"fail martian "whatever""#);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        let error = list_modes(&slot, fake.command(), &env).await.unwrap_err();

        assert_eq!(error.code, QuillErrorCode::Transport);
        assert_eq!(error.message, MISMATCH);
    }

    #[tokio::test]
    async fn quill_transport_envelope_with_an_unknown_key_uses_the_mismatch_message() {
        let body = r#"printf '{"id":"%s","ok":true,"result":%s,"error":null,"extra":1}\n' "$id" "$(modes only)""#;
        let fake = FakeQuill::new(body);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        let error = list_modes(&slot, fake.command(), &env).await.unwrap_err();

        assert_eq!(error.code, QuillErrorCode::Transport);
        assert_eq!(error.message, MISMATCH);
        assert!(slot.try_lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn quill_transport_envelope_missing_a_required_key_uses_the_mismatch_message() {
        let body = r#"printf '{"id":"%s","ok":true,"result":%s}\n' "$id" "$(modes only)""#;
        let fake = FakeQuill::new(body);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        let error = list_modes(&slot, fake.command(), &env).await.unwrap_err();

        assert_eq!(error.code, QuillErrorCode::Transport);
        assert_eq!(error.message, MISMATCH);
    }

    #[tokio::test]
    async fn quill_transport_error_envelope_with_an_unknown_key_uses_the_mismatch_message() {
        let body = r#"printf '{"id":"%s","ok":false,"result":null,"error":{"code":"timeout","message":"m","hint":"h"}}\n' "$id""#;
        let fake = FakeQuill::new(body);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        let error = list_modes(&slot, fake.command(), &env).await.unwrap_err();

        assert_eq!(error.code, QuillErrorCode::Transport);
        assert_eq!(error.message, MISMATCH);
    }

    #[tokio::test]
    async fn quill_transport_exit_message_carries_the_captured_stderr() {
        let fake = FakeQuill::new("echo kaboom >&2; sleep 0.2; exit 0");
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        let error = list_modes(&slot, fake.command(), &env).await.unwrap_err();

        assert_eq!(error.code, QuillErrorCode::Transport);
        assert!(error.message.contains("kaboom"), "{}", error.message);
    }

    #[tokio::test]
    async fn quill_transport_engine_errors_are_carried_as_received_and_keep_the_client() {
        let body = r#"case "$n" in
  1) fail not_found "text one";;
  2) fail bad_request "text two";;
  3) fail internal_error "text three";;
  4) fail timeout "text four";;
  5) fail upstream_error "text five";;
  6) fail no_key "text six";;
  7) fail unknown_mode "text seven";;
  8) fail revert_expired "text eight";;
esac"#;
        let fake = FakeQuill::new(body);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let expected = [
            (QuillErrorCode::NotFound, "text one"),
            (QuillErrorCode::BadRequest, "text two"),
            (QuillErrorCode::InternalError, "text three"),
            (QuillErrorCode::Timeout, "text four"),
            (QuillErrorCode::UpstreamError, "text five"),
            (QuillErrorCode::NoKey, "text six"),
            (QuillErrorCode::UnknownMode, "text seven"),
            (QuillErrorCode::RevertExpired, "text eight"),
        ];

        for (code, message) in expected {
            let error = list_modes(&slot, fake.command(), &env).await.unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(error.message, message, "the engine's message is never rewritten or matched");
        }

        assert_eq!(fake.spawn_count(), 1, "engine errors leave the connection healthy");
        assert!(slot.try_lock().unwrap().is_some());
    }

    // ---- quill_timeout_ ------------------------------------------------------------

    #[test]
    fn quill_timeout_request_limit_is_eight_seconds_above_the_engine_deadline() {
        assert_eq!(QUILL_REQUEST_TIMEOUT, Duration::from_secs(8));
        assert!(QUILL_REQUEST_TIMEOUT > Duration::from_secs(7));
    }

    #[tokio::test]
    async fn quill_timeout_concurrent_call_is_busy_while_the_first_is_still_blocked() {
        let body = r#"printf x >> "$dir/entered"
while [ ! -f "$dir/release" ]; do sleep 0.02; done
reply "$(modes only)""#;
        let fake = FakeQuill::new(body);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(10));
        let slot = Mutex::new(None);
        let first_done = AtomicBool::new(false);

        let first = async {
            let result = list_modes(&slot, fake.command(), &env).await;
            first_done.store(true, Ordering::SeqCst);
            result
        };
        let second = async {
            wait_for(&fake.file("entered")).await;
            let started = Instant::now();
            let second = lease_quill(&slot, fake.command(), &env).await;
            let elapsed = started.elapsed();
            let first_was_done = first_done.load(Ordering::SeqCst);
            std::fs::write(fake.file("release"), "go").unwrap();
            let code = match second {
                Err(error) => error.code,
                Ok(_) => panic!("the second call must not get the client"),
            };
            (code, elapsed, first_was_done)
        };
        let (first, (code, elapsed, first_was_done)) = tokio::join!(first, second);

        assert_eq!(code, QuillErrorCode::Busy);
        assert!(!first_was_done, "busy must come before the first reply");
        assert!(elapsed < Duration::from_millis(50), "busy took {elapsed:?}");
        assert_eq!(only_mode_id(first.unwrap()), "only");
    }

    #[tokio::test]
    async fn quill_timeout_child_exiting_after_one_reply_gives_transport_and_a_respawn() {
        let fake = FakeQuill::new(r#"reply "$(modes only)"; exit 0"#);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        assert_eq!(only_mode_id(list_modes(&slot, fake.command(), &env).await.unwrap()), "only");
        assert!(slot.try_lock().unwrap().is_some());

        let error = list_modes(&slot, fake.command(), &env).await.unwrap_err();
        assert_eq!(error.code, QuillErrorCode::Transport);
        assert!(slot.try_lock().unwrap().is_none(), "a transport error empties the slot");

        assert_eq!(only_mode_id(list_modes(&slot, fake.command(), &env).await.unwrap()), "only");
        assert_eq!(fake.spawn_count(), 2, "the third call spawned a second child");
    }

    #[tokio::test]
    async fn quill_timeout_late_reply_returns_timeout_keeps_the_child_and_the_next_call_gets_its_own() {
        let body = r#"if [ "$n" = 1 ]; then sleep 0.3; reply "$(modes first)"; else reply "$(modes second)"; fi"#;
        let fake = FakeQuill::new(body);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_millis(200));
        let slot = Mutex::new(None);

        let error = list_modes(&slot, fake.command(), &env).await.unwrap_err();
        assert_eq!(error.code, QuillErrorCode::Timeout);
        assert!(slot.try_lock().unwrap().is_some(), "the first expiry keeps the client");

        let next = list_modes(&slot, fake.command(), &env).await.unwrap();
        assert_eq!(only_mode_id(next), "second", "the late reply is discarded by its id");
        assert_eq!(fake.spawn_count(), 1);
    }

    #[tokio::test]
    async fn quill_timeout_half_written_reply_is_framed_whole_and_discarded() {
        let body = r#"if [ "$n" = 1 ]; then
  printf '{"id":"%s","ok":true,"result":' "$id"; sleep 0.3; printf '%s,"error":null}\n' "$(modes first)"
else
  reply "$(modes second)"
fi"#;
        let fake = FakeQuill::new(body);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_millis(200));
        let slot = Mutex::new(None);

        let error = list_modes(&slot, fake.command(), &env).await.unwrap_err();
        assert_eq!(error.code, QuillErrorCode::Timeout);

        let next = list_modes(&slot, fake.command(), &env).await.unwrap();
        assert_eq!(only_mode_id(next), "second");
        assert_eq!(fake.spawn_count(), 1);
    }

    #[tokio::test]
    async fn quill_timeout_a_success_between_expiries_resets_the_count() {
        let body = r#"case "$n" in
  1) sleep 0.3; reply "$(modes first)";;
  2) reply "$(modes second)";;
  3) sleep 0.3; reply "$(modes third)";;
  *) reply "$(modes later)";;
esac"#;
        let fake = FakeQuill::new(body);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_millis(200));
        let slot = Mutex::new(None);

        assert_eq!(list_modes(&slot, fake.command(), &env).await.unwrap_err().code, QuillErrorCode::Timeout);
        assert_eq!(only_mode_id(list_modes(&slot, fake.command(), &env).await.unwrap()), "second");
        assert_eq!(list_modes(&slot, fake.command(), &env).await.unwrap_err().code, QuillErrorCode::Timeout);

        assert!(slot.try_lock().unwrap().is_some(), "non-consecutive expiries keep the client");
        assert_eq!(fake.spawn_count(), 1);
    }

    #[tokio::test]
    async fn quill_timeout_two_consecutive_expiries_empty_the_slot() {
        let fake = FakeQuill::new("sleep 1");
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_millis(200));
        let slot = Mutex::new(None);

        assert_eq!(list_modes(&slot, fake.command(), &env).await.unwrap_err().code, QuillErrorCode::Timeout);
        assert!(slot.try_lock().unwrap().is_some());

        let second = list_modes(&slot, fake.command(), &env).await.unwrap_err();
        assert_eq!(second.code, QuillErrorCode::Transport, "the second expiry is a transport error");
        assert!(slot.try_lock().unwrap().is_none());
    }

    // ---- quill_lease_ (R9.3 revalidation) -------------------------------------------

    #[tokio::test]
    async fn quill_lease_same_command_reuses_the_client() {
        let fake = FakeQuill::new(ECHO_MODES);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        list_modes(&slot, fake.command(), &env).await.unwrap();
        list_modes(&slot, fake.command(), &env).await.unwrap();

        assert_eq!(fake.spawn_count(), 1);
        assert_eq!(fake.lines("requests").len(), 3, "one ping and two list_modes on one child");
    }

    #[tokio::test]
    async fn quill_lease_changed_command_drops_the_client_and_spawns_the_new_one() {
        let first = FakeQuill::new(ECHO_MODES);
        let second = FakeQuill::new(ECHO_MODES);
        let fs = NativeFs;
        let env = env_for(&first, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        list_modes(&slot, first.command(), &env).await.unwrap();
        list_modes(&slot, second.command(), &env).await.unwrap();
        list_modes(&slot, second.command(), &env).await.unwrap();

        assert_eq!(first.spawn_count(), 1);
        assert_eq!(second.spawn_count(), 1);
        assert_eq!(second.lines("requests").len(), 3);
    }

    #[tokio::test]
    async fn quill_lease_failing_command_is_disabled_and_empties_the_slot() {
        let fake = FakeQuill::new(ECHO_MODES);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        list_modes(&slot, fake.command(), &env).await.unwrap();
        assert!(slot.try_lock().unwrap().is_some());

        std::fs::set_permissions(&fake.command, std::fs::Permissions::from_mode(0o770)).unwrap();
        let error = list_modes(&slot, fake.command(), &env).await.unwrap_err();

        assert_eq!(error.code, QuillErrorCode::Disabled);
        let canonical = std::fs::canonicalize(&fake.command).unwrap();
        assert_eq!(error.message, format!("Quill command is group-writable: {}", canonical.display()));
        assert!(slot.try_lock().unwrap().is_none());
        assert_eq!(fake.spawn_count(), 1, "an invalid command is never spawned");
    }

    #[tokio::test]
    async fn quill_lease_invalid_command_never_spawns() {
        let fake = FakeQuill::new(ECHO_MODES);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        let error = list_modes(&slot, "quill", &env).await.unwrap_err();

        assert_eq!(error.code, QuillErrorCode::Disabled);
        assert_eq!(error.message, "Quill command must be an absolute path.");
        assert_eq!(fake.spawn_count(), 0);
    }

    // ---- quill_command_shapes_ -----------------------------------------------------

    #[test]
    fn quill_command_shapes_results_serialise_to_exactly_their_key_sets() {
        let selection = QuillRewriteSelectionResult {
            rewrite_id: "r".into(),
            mode: "m".into(),
            model: "x".into(),
            latency_ms: 1,
            end_to_end_ms: 2,
            anchored: true,
            clipboard_replaced: false,
        };
        assert_eq!(
            key_set(&selection),
            set(&["rewriteId", "mode", "model", "latencyMs", "endToEndMs", "anchored", "clipboardReplaced"])
        );

        let text = QuillRewriteTextResult {
            rewrite_id: "r".into(),
            text: "t".into(),
            mode: "m".into(),
            model: "x".into(),
            latency_ms: 1,
        };
        assert_eq!(key_set(&text), set(&["rewriteId", "text", "mode", "model", "latencyMs"]));

        let revert = QuillRevertResult {
            delivered: RevertDelivered::Inserted,
        };
        assert_eq!(key_set(&revert), set(&["delivered"]));

        assert_eq!(key_set(&QuillReportOutcomeResult {}), set(&[]));
        assert_eq!(serde_json::to_value(QuillReportOutcomeResult {}).unwrap(), json!({}));

        let modes = QuillListModesResult {
            modes: vec![QuillModeInfo {
                id: "i".into(),
                name: "n".into(),
                description: "d".into(),
                form: ModeForm::Dictation,
                target_model: TargetModel::Generic,
                default: true,
            }],
        };
        assert_eq!(key_set(&modes), set(&["modes"]));
        let value = serde_json::to_value(&modes).unwrap();
        let mode_keys: BTreeSet<String> = value["modes"][0].as_object().unwrap().keys().cloned().collect();
        assert_eq!(mode_keys, set(&["id", "name", "description", "form", "targetModel", "default"]));
    }

    #[test]
    fn quill_command_shapes_error_serialises_to_code_and_message() {
        let error = QuillCommandError::new(QuillErrorCode::Busy, "another request is running");
        assert_eq!(key_set(&error), set(&["code", "message"]));
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            json!({"code": "busy", "message": "another request is running"})
        );
    }

    #[test]
    fn quill_command_shapes_revert_delivered_is_inserted_or_copied_only() {
        assert_eq!(serde_json::to_value(RevertDelivered::Inserted).unwrap(), json!("inserted"));
        assert_eq!(serde_json::to_value(RevertDelivered::Copied).unwrap(), json!("copied"));
        for other in ["printed", "failed", "Inserted", ""] {
            assert!(serde_json::from_value::<RevertDelivered>(json!(other)).is_err(), "{other:?}");
        }
    }

    #[test]
    fn quill_command_shapes_rewrite_text_arguments_accept_source_app_absent_or_null() {
        let absent: QuillRewriteTextArgs =
            serde_json::from_value(json!({"text": "t", "mode": "m", "surface": "pill"})).unwrap();
        assert_eq!(absent.source_app, None);

        let null: QuillRewriteTextArgs =
            serde_json::from_value(json!({"text": "t", "mode": "m", "surface": "pill", "sourceApp": null})).unwrap();
        assert_eq!(null.source_app, None);

        let named: QuillRewriteTextArgs = serde_json::from_value(
            json!({"text": "t", "mode": "m", "surface": "pill", "sourceApp": "Notes"}),
        )
        .unwrap();
        assert_eq!(named.source_app.as_deref(), Some("Notes"));
        assert_eq!((named.text.as_str(), named.mode.as_str(), named.surface.as_str()), ("t", "m", "pill"));
    }

    #[test]
    fn quill_command_shapes_error_codes_are_exactly_the_seventeen_snake_case_strings() {
        let expected = [
            "not_found",
            "bad_request",
            "internal_error",
            "timeout",
            "upstream_error",
            "no_key",
            "unknown_mode",
            "revert_expired",
            "busy",
            "transport",
            "no_selection",
            "secure_field",
            "too_long",
            "accessibility_permission",
            "modifiers_held",
            "target_changed",
            "disabled",
        ];
        assert_eq!(QuillErrorCode::ALL.len(), 17);
        let serialised: Vec<Value> = QuillErrorCode::ALL
            .iter()
            .map(|code| serde_json::to_value(code).unwrap())
            .collect();
        assert_eq!(serialised, expected.iter().map(|code| json!(code)).collect::<Vec<_>>());
        for (code, text) in QuillErrorCode::ALL.iter().zip(expected) {
            assert_eq!(code.as_str(), text);
            assert_eq!(serde_json::from_value::<QuillErrorCode>(json!(text)).unwrap(), *code);
        }
        for other in ["printed", "Busy", "BUSY", "", "ok", "invalid_command"] {
            assert!(serde_json::from_value::<QuillErrorCode>(json!(other)).is_err(), "{other:?}");
        }
    }
}
