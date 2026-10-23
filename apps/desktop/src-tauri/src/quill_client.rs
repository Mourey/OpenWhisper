//! Quill client: the validated spawn of `quill serve`, the typed engine protocol, the
//! `busy` / timeout / respawn rules and the command-facing error and result shapes.
//!
//! The engine types mirror the vendored schema in `contracts/quill` and reject unknown
//! fields, so a drifting engine fails loudly instead of being half understood.

use crate::jsonl_client::{JsonlChannel, RequestError};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::future::Future;
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

/// The request envelope as the schema defines it. The client writes requests through
/// `WorkerRequest`; this typed form exists to check the vendored fixtures.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuillRequest {
    pub id: String,
    pub method: QuillMethod,
    pub params: serde_json::Value,
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
    /// Every code, for the shape tests.
    #[cfg(test)]
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

        // Whatever goes wrong while starting, the caller sees a spawn failure: `transport`.
        let ping: PingResult = client
            .request_typed(QuillMethod::Ping, &PingParams {})
            .await
            .map_err(|error| match error.code {
                QuillErrorCode::Transport => error,
                code => QuillCommandError::transport(format!(
                    "quill failed its startup ping ({}): {}",
                    code.as_str(),
                    error.message
                )),
            })?;
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
            Err(_elapsed) if self.channel.write_interrupted() => {
                // A partial line misframes the engine's input; there is no reply to wait for.
                return Err(QuillCommandError::transport(format!(
                    "quill did not accept the {} request within {:?}; restarting it.",
                    method.as_str(),
                    self.timeout
                )));
            }
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
    lease_claimed(claim_slot(slot)?, raw_command, env).await
}

/// Takes the slot without waiting; `busy` when another call holds it. A caller that must
/// look something up before starting the engine claims first, so a busy slot is reported
/// at once, and then finishes with `lease_claimed`.
pub fn claim_slot(
    slot: &Mutex<Option<QuillClient>>,
) -> Result<MutexGuard<'_, Option<QuillClient>>, QuillCommandError> {
    slot.try_lock().map_err(|_| QuillCommandError::busy())
}

/// Revalidates the command and fills the claimed slot, as `lease_quill` does after its
/// `try_lock`.
pub async fn lease_claimed<'a>(
    mut guard: MutexGuard<'a, Option<QuillClient>>,
    raw_command: &str,
    env: &QuillEnv<'_>,
) -> Result<QuillLease<'a>, QuillCommandError> {
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
        .is_some_and(|client| client.command() == configured && client.canonical == canonical);
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

// ---- command flows (R9.4, R19.1) -----------------------------------------------------

/// Everything the flows need from the machine around them: the selection and the
/// clipboard, the clock and the debug log. The real one is `NativeHost`; tests use a fake.
/// Nothing a host logs may carry selection, rewrite or engine text.
pub trait QuillHost: Send + Sync {
    /// Captures the focused element and reads its selection, or says why it cannot.
    fn read_selection(&mut self) -> impl Future<Output = Result<String, QuillErrorCode>> + Send;
    /// The application name captured with the selection.
    fn source_app(&mut self) -> Option<String>;
    /// Writes `text` over the captured selection.
    fn replace_selection(&mut self, text: &str) -> impl Future<Output = HostReplace> + Send;
    /// Remembers the rewrite so a later revert can act on it.
    fn record_rewrite(&mut self, rewrite_id: &str, text: &str, delivered: Delivered);
    /// How `rewrite_id` was delivered, if it is the rewrite the host still holds.
    fn slot_delivery(&mut self, rewrite_id: &str) -> Option<Delivered>;
    /// Writes `original` over the rewrite in place; false when it could not.
    fn revert_at_anchor(&mut self, rewrite_id: &str, original: &str) -> impl Future<Output = bool> + Send;
    fn copy_to_clipboard(&mut self, text: &str) -> bool;
    /// Milliseconds since the Unix epoch, the unit of `pressed_at_ms`.
    fn now_ms(&self) -> u64;
    /// Writes one line to `debug.log`.
    fn log(&self, message: &str);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostReplace {
    pub inserted: bool,
    pub anchored: bool,
    pub clipboard_replaced: bool,
}

/// How a hotkey rewrite ended, for the `[quill]` timing line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingResult {
    Inserted,
    Copied,
    Failed,
    Refused,
}

impl TimingResult {
    fn as_str(self) -> &'static str {
        match self {
            Self::Inserted => "inserted",
            Self::Copied => "copied",
            Self::Failed => "failed",
            Self::Refused => "refused",
        }
    }

    fn of(result: &Result<QuillRewriteSelectionResult, QuillCommandError>) -> Self {
        match result {
            Ok(_) => Self::Inserted,
            Err(error) => match error.code {
                QuillErrorCode::TargetChanged => Self::Copied,
                QuillErrorCode::NoSelection
                | QuillErrorCode::SecureField
                | QuillErrorCode::TooLong
                | QuillErrorCode::AccessibilityPermission
                | QuillErrorCode::ModifiersHeld => Self::Refused,
                _ => Self::Failed,
            },
        }
    }
}

/// The text of the `[quill]` debug-log line. `end_to_end_ms` counts from the first press
/// (the frontend's clock) to the inserter's return and saturates at 0. The mode id is the
/// only caller-supplied text, so it is cut to one safe token.
pub fn quill_timing_line(
    mode: &str,
    pressed_at_ms: u64,
    inserted_at_ms: u64,
    engine_ms: u64,
    result: TimingResult,
) -> String {
    let mode: String = mode
        .chars()
        .take(64)
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    format!(
        "surface=hotkey mode={mode} end_to_end_ms={} engine_ms={engine_ms} result={}",
        inserted_at_ms.saturating_sub(pressed_at_ms),
        result.as_str()
    )
}

/// A diagnostic line that names an event and an error code and nothing else.
pub fn quill_event_line(event: &str, code: QuillErrorCode) -> String {
    format!("quill {event} code={}", code.as_str())
}

/// True only when the hook is enabled and its command passed validation.
pub fn quill_should_spawn_at_setup(enabled: bool, validation: &Result<PathBuf, String>) -> bool {
    enabled && validation.is_ok()
}

/// Starts the client into the slot ahead of the first command, without waiting if the
/// slot is taken.
pub async fn spawn_into_slot(
    slot: &Mutex<Option<QuillClient>>,
    raw_command: &str,
    env: &QuillEnv<'_>,
) -> Result<(), QuillCommandError> {
    lease_quill(slot, raw_command, env).await.map(drop)
}

fn refusal_message(code: QuillErrorCode) -> &'static str {
    match code {
        QuillErrorCode::NoSelection => "No text is selected.",
        QuillErrorCode::SecureField => "The focused field is a secure field.",
        QuillErrorCode::TooLong => "The selection is longer than 8,000 characters.",
        QuillErrorCode::AccessibilityPermission => "Accessibility permission is required.",
        QuillErrorCode::ModifiersHeld => "Release the modifier keys and try again.",
        _ => "The selection could not be read.",
    }
}

fn bad_request(message: &str) -> QuillCommandError {
    QuillCommandError::new(QuillErrorCode::BadRequest, message)
}

fn parse_delivered(value: &str) -> Result<Delivered, QuillCommandError> {
    match value {
        "inserted" => Ok(Delivered::Inserted),
        "copied" => Ok(Delivered::Copied),
        "failed" => Ok(Delivered::Failed),
        _ => Err(bad_request("delivered must be inserted, copied or failed.")),
    }
}

fn parse_reverted_via(value: &str) -> Result<RevertedVia, QuillCommandError> {
    match value {
        "inserted" => Ok(RevertedVia::Inserted),
        "copied" => Ok(RevertedVia::Copied),
        _ => Err(bad_request("revertedVia must be inserted or copied.")),
    }
}

/// Tells quill how a rewrite was delivered. The text is already with the user, so a
/// failure is logged by code and otherwise ignored.
async fn send_outcome<H: QuillHost>(
    client: &mut QuillClient,
    host: &H,
    rewrite_id: &str,
    delivered: Delivered,
    reverted_via: Option<RevertedVia>,
) {
    let params = OutcomeParams {
        rewrite_id: rewrite_id.to_string(),
        delivered,
        reverted_via,
    };
    if let Err(error) = client.request_typed::<_, OutcomeResult>(QuillMethod::Outcome, &params).await {
        host.log(&quill_event_line("outcome_failed", error.code));
    }
}

#[derive(Default)]
struct SelectionTiming {
    engine_ms: u64,
    inserted_at_ms: Option<u64>,
}

/// The hotkey rewrite: read the selection, rewrite it, write it back, report `outcome`.
/// Holds the slot from before the read until `outcome` returns, and writes exactly one
/// timing line however the call ends.
pub async fn run_rewrite_selection<H: QuillHost>(
    slot: &Mutex<Option<QuillClient>>,
    raw_command: &str,
    env: &QuillEnv<'_>,
    host: &mut H,
    mode: &str,
    pressed_at_ms: u64,
) -> Result<QuillRewriteSelectionResult, QuillCommandError> {
    let mut timing = SelectionTiming::default();
    let result = match lease_quill(slot, raw_command, env).await {
        Ok(mut lease) => rewrite_selection(lease.client(), host, mode, pressed_at_ms, &mut timing).await,
        Err(error) => Err(error),
    };
    let inserted_at_ms = timing.inserted_at_ms.unwrap_or_else(|| host.now_ms());
    host.log(&quill_timing_line(
        mode,
        pressed_at_ms,
        inserted_at_ms,
        timing.engine_ms,
        TimingResult::of(&result),
    ));
    result
}

async fn rewrite_selection<H: QuillHost>(
    client: &mut QuillClient,
    host: &mut H,
    mode: &str,
    pressed_at_ms: u64,
    timing: &mut SelectionTiming,
) -> Result<QuillRewriteSelectionResult, QuillCommandError> {
    let text = host
        .read_selection()
        .await
        .map_err(|code| QuillCommandError::new(code, refusal_message(code)))?;
    let params = RewriteParams {
        text,
        mode: mode.to_string(),
        surface: Surface::Hotkey,
        source_app: host.source_app(),
        target_model: None,
    };

    let started_ms = host.now_ms();
    let rewritten = client.request_typed::<_, RewriteResult>(QuillMethod::Rewrite, &params).await;
    timing.engine_ms = host.now_ms().saturating_sub(started_ms);
    let rewritten = rewritten?;

    let replaced = host.replace_selection(&rewritten.text).await;
    let inserted_at_ms = host.now_ms();
    timing.inserted_at_ms = Some(inserted_at_ms);

    let delivered = if replaced.inserted {
        Delivered::Inserted
    } else if host.copy_to_clipboard(&rewritten.text) {
        Delivered::Copied
    } else {
        host.log(&quill_event_line("clipboard_copy_failed", QuillErrorCode::InternalError));
        Delivered::Failed
    };
    host.record_rewrite(&rewritten.rewrite_id, &rewritten.text, delivered);
    send_outcome(client, host, &rewritten.rewrite_id, delivered, None).await;

    if !replaced.inserted {
        let message = if delivered == Delivered::Copied {
            "The text field changed before the rewrite could be inserted; it was copied to the clipboard."
        } else {
            "The text field changed before the rewrite could be inserted, and it could not be copied."
        };
        return Err(QuillCommandError::new(QuillErrorCode::TargetChanged, message));
    }
    Ok(QuillRewriteSelectionResult {
        rewrite_id: rewritten.rewrite_id,
        mode: rewritten.mode,
        model: rewritten.model,
        latency_ms: rewritten.latency_ms,
        end_to_end_ms: inserted_at_ms.saturating_sub(pressed_at_ms),
        anchored: replaced.anchored,
        clipboard_replaced: replaced.clipboard_replaced,
    })
}

/// A rewrite for the dictation pill: nothing is inserted, the caller delivers the text.
pub async fn run_rewrite_text<H: QuillHost>(
    slot: &Mutex<Option<QuillClient>>,
    raw_command: &str,
    env: &QuillEnv<'_>,
    host: &H,
    args: QuillRewriteTextArgs,
) -> Result<QuillRewriteTextResult, QuillCommandError> {
    if args.surface != "pill" {
        return Err(bad_request("quill_rewrite_text only accepts the pill surface."));
    }
    let params = RewriteParams {
        text: args.text,
        mode: args.mode,
        surface: Surface::Pill,
        source_app: args.source_app,
        target_model: None,
    };
    let mut lease = lease_quill(slot, raw_command, env).await?;
    let result = lease
        .client()
        .request_typed::<_, RewriteResult>(QuillMethod::Rewrite, &params)
        .await;
    match result {
        Ok(rewritten) => Ok(QuillRewriteTextResult {
            rewrite_id: rewritten.rewrite_id,
            text: rewritten.text,
            mode: rewritten.mode,
            model: rewritten.model,
            latency_ms: rewritten.latency_ms,
        }),
        Err(error) => {
            host.log(&quill_event_line("rewrite_text_failed", error.code));
            Err(error)
        }
    }
}

/// Reverts a rewrite: quill gives the original back, the host writes it over the rewrite
/// when it still can and copies it otherwise, then `outcome` records how. `delivered` is
/// how the rewrite itself was delivered; the host knows it for the rewrite it holds,
/// anything else needs the caller's value.
pub async fn run_revert<H: QuillHost>(
    slot: &Mutex<Option<QuillClient>>,
    raw_command: &str,
    env: &QuillEnv<'_>,
    host: &mut H,
    rewrite_id: &str,
    delivered: Option<&str>,
) -> Result<QuillRevertResult, QuillCommandError> {
    let supplied = delivered.map(parse_delivered).transpose()?;

    // Claim the slot first: it answers `busy` at once, while the host's slot lookup may
    // wait on a read or replace that holds the target slot. The lookup still precedes the
    // engine, so a missing `delivered` is refused without starting it.
    let claimed = claim_slot(slot)?;
    let held = host.slot_delivery(rewrite_id);
    let delivered = held.or(supplied).ok_or_else(|| {
        bad_request("delivered is required to revert a rewrite that was not inserted by the hotkey.")
    })?;
    let mut lease = lease_claimed(claimed, raw_command, env).await?;
    let client = lease.client();

    let original: RevertResult = client
        .request_typed(
            QuillMethod::Revert,
            &RevertParams {
                rewrite_id: rewrite_id.to_string(),
            },
        )
        .await?;

    let in_place = held.is_some() && host.revert_at_anchor(rewrite_id, &original.text).await;
    let (reverted_via, how) = if in_place {
        (RevertedVia::Inserted, RevertDelivered::Inserted)
    } else if host.copy_to_clipboard(&original.text) {
        (RevertedVia::Copied, RevertDelivered::Copied)
    } else {
        host.log(&quill_event_line("clipboard_copy_failed", QuillErrorCode::InternalError));
        return Err(QuillCommandError::new(
            QuillErrorCode::InternalError,
            "The original text could not be copied to the clipboard.",
        ));
    };
    send_outcome(client, host, rewrite_id, delivered, Some(reverted_via)).await;
    Ok(QuillRevertResult { delivered: how })
}

/// Reports how a dictation rewrite was delivered.
pub async fn run_report_outcome(
    slot: &Mutex<Option<QuillClient>>,
    raw_command: &str,
    env: &QuillEnv<'_>,
    rewrite_id: &str,
    delivered: &str,
    reverted_via: Option<&str>,
) -> Result<QuillReportOutcomeResult, QuillCommandError> {
    let params = OutcomeParams {
        rewrite_id: rewrite_id.to_string(),
        delivered: parse_delivered(delivered)?,
        reverted_via: reverted_via.map(parse_reverted_via).transpose()?,
    };
    let mut lease = lease_quill(slot, raw_command, env).await?;
    lease
        .client()
        .request_typed::<_, OutcomeResult>(QuillMethod::Outcome, &params)
        .await?;
    Ok(QuillReportOutcomeResult {})
}

pub async fn run_list_modes(
    slot: &Mutex<Option<QuillClient>>,
    raw_command: &str,
    env: &QuillEnv<'_>,
) -> Result<QuillListModesResult, QuillCommandError> {
    let mut lease = lease_quill(slot, raw_command, env).await?;
    let result: ListModesResult = lease
        .client()
        .request_typed(QuillMethod::ListModes, &ListModesParams {})
        .await?;
    Ok(QuillListModesResult {
        modes: result.modes.into_iter().map(QuillModeInfo::from).collect(),
    })
}

/// The real selection, clipboard, clock and debug log (macOS only; other targets answer
/// `disabled` before reaching a host).
#[cfg(target_os = "macos")]
pub struct NativeHost;

#[cfg(target_os = "macos")]
impl NativeHost {
    async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static, fallback: T) -> T {
        tokio::task::spawn_blocking(work).await.unwrap_or(fallback)
    }
}

#[cfg(target_os = "macos")]
impl QuillHost for NativeHost {
    fn read_selection(&mut self) -> impl Future<Output = Result<String, QuillErrorCode>> + Send {
        use crate::macos_text_inserter::read_selected_text;
        async {
            let read = Self::blocking(read_selected_text, Err("no_selection")).await;
            read.map_err(|reason| match reason {
                "accessibility_permission" => QuillErrorCode::AccessibilityPermission,
                "secure_field" => QuillErrorCode::SecureField,
                "modifiers_held" => QuillErrorCode::ModifiersHeld,
                "too_long" => QuillErrorCode::TooLong,
                _ => QuillErrorCode::NoSelection,
            })
        }
    }

    fn source_app(&mut self) -> Option<String> {
        crate::macos_text_inserter::quill_source_app()
    }

    fn replace_selection(&mut self, text: &str) -> impl Future<Output = HostReplace> + Send {
        let text = text.to_string();
        async move {
            let failed = HostReplace {
                inserted: false,
                anchored: false,
                clipboard_replaced: false,
            };
            let report = Self::blocking(
                move || Some(crate::macos_text_inserter::replace_selection(&text)),
                None,
            )
            .await;
            report.map_or(failed, |report| HostReplace {
                inserted: report.paste.pasted,
                anchored: report.anchored,
                clipboard_replaced: report.clipboard_replaced,
            })
        }
    }

    fn record_rewrite(&mut self, rewrite_id: &str, text: &str, delivered: Delivered) {
        use crate::macos_text_inserter::Delivery;
        let delivery = if delivered == Delivered::Inserted {
            Delivery::Inserted
        } else {
            Delivery::Copied
        };
        crate::macos_text_inserter::quill_record_rewrite(rewrite_id, text, delivery);
    }

    fn slot_delivery(&mut self, rewrite_id: &str) -> Option<Delivered> {
        use crate::macos_text_inserter::Delivery;
        crate::macos_text_inserter::quill_slot_delivery(rewrite_id).map(|delivery| match delivery {
            Delivery::Inserted => Delivered::Inserted,
            Delivery::Copied => Delivered::Copied,
        })
    }

    fn revert_at_anchor(&mut self, rewrite_id: &str, original: &str) -> impl Future<Output = bool> + Send {
        use crate::macos_text_inserter::{revert_at_anchor, RevertAtAnchor};
        let (rewrite_id, original) = (rewrite_id.to_string(), original.to_string());
        async move {
            Self::blocking(move || revert_at_anchor(&rewrite_id, &original), RevertAtAnchor::Copied).await
                == RevertAtAnchor::Inserted
        }
    }

    fn copy_to_clipboard(&mut self, text: &str) -> bool {
        arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(text.to_string()))
            .is_ok()
    }

    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
    }

    fn log(&self, message: &str) {
        crate::debug_log_line("quill", message);
    }
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
    /// count and every request line next to itself, answers `ping` with `__PING_STEP__` and runs
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
  if [ "$method" = ping ]; then __PING_STEP__; continue; fi
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
            Self::with_ping_step(&format!("reply '{ping_result}'"), body)
        }

        /// `ping_step` is the shell that answers `ping`, with `reply` and `fail` in scope.
        fn with_ping_step(ping_step: &str, body: &str) -> Self {
            let dir = TempDir::new();
            let command = dir.0.join("quill");
            let script = SCRIPT.replace("__PING_STEP__", ping_step).replace("__BODY__", body);
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

    /// Fills an empty slot through `lease_quill`, which must fail; reports the error and
    /// whether the slot is still empty. `spawn_timeout` bounds the startup `ping`.
    async fn failed_lease(fake: &FakeQuill, spawn_timeout: Duration) -> (QuillCommandError, bool) {
        let fs = NativeFs;
        let env = QuillEnv {
            fs: &fs,
            home: Some(fake.dir.0.as_path()),
            timeout: Duration::from_secs(5),
            spawn_timeout,
        };
        let slot = Mutex::new(None);
        let error = match lease_quill(&slot, fake.command(), &env).await {
            Err(error) => error,
            Ok(_) => panic!("the spawn must fail"),
        };
        let empty = slot.try_lock().unwrap().is_none();
        (error, empty)
    }

    #[tokio::test]
    async fn quill_spawn_ping_error_reply_is_transport_and_leaves_the_slot_empty() {
        let fake = FakeQuill::with_ping_step(r#"fail internal_error "x""#, ECHO_MODES);
        let (error, empty) = failed_lease(&fake, Duration::from_secs(5)).await;
        assert_eq!(error.code, QuillErrorCode::Transport);
        assert!(empty);
    }

    #[tokio::test]
    async fn quill_spawn_unanswered_ping_is_transport_and_leaves_the_slot_empty() {
        let fake = FakeQuill::with_ping_step("sleep 2", ECHO_MODES);
        let (error, empty) = failed_lease(&fake, Duration::from_millis(200)).await;
        assert_eq!(error.code, QuillErrorCode::Transport);
        assert!(empty);
    }

    #[tokio::test]
    async fn quill_spawn_child_exiting_during_ping_is_transport_and_leaves_the_slot_empty() {
        let fake = FakeQuill::with_ping_step("exit 0", ECHO_MODES);
        let (error, empty) = failed_lease(&fake, Duration::from_secs(5)).await;
        assert_eq!(error.code, QuillErrorCode::Transport);
        assert!(empty);
    }

    #[tokio::test]
    async fn quill_spawn_wrong_ping_and_parse_failures_leave_the_slot_empty() {
        for ping in [
            r#"{"protocol":"quill","version":"2"}"#,
            r#"{"protocol":"other","version":"1"}"#,
            r#"{"protocol":"quill","version":"1","extra":1}"#,
        ] {
            let fake = FakeQuill::with_ping(ping, ECHO_MODES);
            let (error, empty) = failed_lease(&fake, Duration::from_secs(5)).await;
            assert_eq!(error.code, QuillErrorCode::Transport, "{ping}");
            assert!(empty, "{ping}");
        }
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

    #[tokio::test]
    async fn quill_timeout_expiry_during_the_request_write_is_transport_and_respawns() {
        // The first child answers the ping and then stops reading stdin for a while, so a
        // large request stalls in the pipe; the second child behaves.
        let ping_step = r#"reply '{"protocol":"quill","version":"1"}'
if [ ! -f "$dir/stalled" ]; then : > "$dir/stalled"; sleep 3; fi"#;
        let fake = FakeQuill::with_ping_step(ping_step, ECHO_MODES);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_millis(200));
        let slot = Mutex::new(None);
        let huge = RewriteParams {
            text: "x".repeat(1 << 20),
            mode: "task-claude".into(),
            surface: Surface::Hotkey,
            source_app: None,
            target_model: None,
        };

        let mut lease = lease_quill(&slot, fake.command(), &env).await.unwrap();
        let error = lease
            .client()
            .request_typed::<_, RewriteResult>(QuillMethod::Rewrite, &huge)
            .await
            .unwrap_err();
        drop(lease);

        assert_eq!(error.code, QuillErrorCode::Transport, "{}", error.message);
        assert!(slot.try_lock().unwrap().is_none(), "a cut-short write empties the slot");

        // The next call starts a fresh child instead of writing after a partial line.
        assert_eq!(only_mode_id(list_modes(&slot, fake.command(), &env).await.unwrap()), "only");
        assert_eq!(fake.spawn_count(), 2);
    }

    #[tokio::test]
    async fn quill_timeout_expiry_while_waiting_for_the_reply_is_still_a_timeout() {
        let body = r#"if [ "$n" = 1 ]; then sleep 0.3; fi; reply "$(modes first)""#;
        let fake = FakeQuill::new(body);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_millis(200));
        let slot = Mutex::new(None);

        let error = list_modes(&slot, fake.command(), &env).await.unwrap_err();

        assert_eq!(error.code, QuillErrorCode::Timeout);
        assert!(slot.try_lock().unwrap().is_some(), "the abandoned-id path is unchanged");
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

    // ---- quill_flow_ / quill_timing_line_ / quill_should_spawn_at_setup_ ------------

    /// Stands in for the selection port, the clipboard, the clock and the log.
    struct FakeHost {
        selection: Result<String, QuillErrorCode>,
        source_app: Option<String>,
        replace: HostReplace,
        slot_delivery: Option<Delivered>,
        /// Stands for the target-slot lock being busy: the lookup blocks this long.
        slot_delivery_delay: Duration,
        slot_delivery_calls: usize,
        revert_inserts: bool,
        copy_ok: bool,
        clock: std::sync::Mutex<std::collections::VecDeque<u64>>,
        calls: Vec<String>,
        copies: Vec<String>,
        records: Vec<(String, String, Delivered)>,
        logs: std::sync::Mutex<Vec<String>>,
    }

    impl FakeHost {
        fn new() -> Self {
            Self {
                selection: Ok("selected".to_string()),
                source_app: Some("Notes".to_string()),
                replace: HostReplace {
                    inserted: true,
                    anchored: true,
                    clipboard_replaced: false,
                },
                slot_delivery: None,
                slot_delivery_delay: Duration::ZERO,
                slot_delivery_calls: 0,
                revert_inserts: true,
                copy_ok: true,
                clock: std::sync::Mutex::new(std::collections::VecDeque::new()),
                calls: Vec::new(),
                copies: Vec::new(),
                records: Vec::new(),
                logs: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn with_clock(mut self, values: &[u64]) -> Self {
            self.clock = std::sync::Mutex::new(values.iter().copied().collect());
            self
        }

        fn logs(&self) -> Vec<String> {
            self.logs.lock().unwrap().clone()
        }

        fn timing_lines(&self) -> Vec<String> {
            self.logs().into_iter().filter(|line| line.starts_with("surface=hotkey ")).collect()
        }
    }

    impl QuillHost for FakeHost {
        fn read_selection(&mut self) -> impl std::future::Future<Output = Result<String, QuillErrorCode>> + Send {
            self.calls.push("read".to_string());
            let result = self.selection.clone();
            async move { result }
        }

        fn source_app(&mut self) -> Option<String> {
            self.source_app.clone()
        }

        fn replace_selection(&mut self, text: &str) -> impl std::future::Future<Output = HostReplace> + Send {
            self.calls.push(format!("replace:{text}"));
            let report = self.replace;
            async move { report }
        }

        fn record_rewrite(&mut self, rewrite_id: &str, text: &str, delivered: Delivered) {
            self.records.push((rewrite_id.to_string(), text.to_string(), delivered));
        }

        fn slot_delivery(&mut self, _rewrite_id: &str) -> Option<Delivered> {
            self.slot_delivery_calls += 1;
            std::thread::sleep(self.slot_delivery_delay);
            self.slot_delivery
        }

        fn revert_at_anchor(
            &mut self,
            rewrite_id: &str,
            original: &str,
        ) -> impl std::future::Future<Output = bool> + Send {
            self.calls.push(format!("revert:{rewrite_id}:{original}"));
            let inserted = self.revert_inserts;
            async move { inserted }
        }

        fn copy_to_clipboard(&mut self, text: &str) -> bool {
            self.copies.push(text.to_string());
            self.copy_ok
        }

        fn now_ms(&self) -> u64 {
            self.clock.lock().unwrap().pop_front().unwrap_or(5_000_000)
        }

        fn log(&self, message: &str) {
            self.logs.lock().unwrap().push(message.to_string());
        }
    }

    const FLOW_BODY: &str = r#"case "$method" in
  rewrite) reply '{"rewrite_id":"rw-1","text":"REWRITTEN","mode":"task-claude","model":"test/model","latency_ms":12}';;
  outcome) reply '{}';;
  revert) reply '{"text":"ORIGINAL"}';;
  list_modes) reply "$(modes only)";;
esac"#;

    fn sent(fake: &FakeQuill) -> Vec<Value> {
        fake.lines("requests")
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn methods(fake: &FakeQuill) -> Vec<String> {
        sent(fake)
            .iter()
            .map(|request| request["method"].as_str().unwrap().to_string())
            .collect()
    }

    fn params_of(fake: &FakeQuill, method: &str) -> Vec<Value> {
        sent(fake)
            .into_iter()
            .filter(|request| request["method"] == method)
            .map(|request| request["params"].clone())
            .collect()
    }

    #[tokio::test]
    async fn quill_timeout_every_command_returns_busy_at_once_before_touching_the_target_slot() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let _held = slot.lock().await;
        // A read or replace in progress holds the target slot, so any lookup there blocks.
        let mut host = FakeHost::new();
        host.slot_delivery = Some(Delivered::Inserted);
        host.slot_delivery_delay = Duration::from_millis(300);
        let busy = |error: QuillCommandError, started: Instant| {
            assert_eq!(error.code, QuillErrorCode::Busy);
            let elapsed = started.elapsed();
            assert!(elapsed < Duration::from_millis(50), "busy took {elapsed:?}");
        };

        let started = Instant::now();
        busy(
            run_revert(&slot, fake.command(), &env, &mut host, "rw-1", Some("inserted"))
                .await
                .unwrap_err(),
            started,
        );
        assert_eq!(host.slot_delivery_calls, 0, "the target slot is not touched before the lease");

        let started = Instant::now();
        busy(
            run_rewrite_selection(&slot, fake.command(), &env, &mut host, "task-claude", 0)
                .await
                .unwrap_err(),
            started,
        );
        assert!(host.calls.is_empty());
        let started = Instant::now();
        busy(
            run_rewrite_text(&slot, fake.command(), &env, &host, text_args("pill")).await.unwrap_err(),
            started,
        );
        let started = Instant::now();
        busy(
            run_report_outcome(&slot, fake.command(), &env, "rw-1", "inserted", None)
                .await
                .unwrap_err(),
            started,
        );
        let started = Instant::now();
        busy(run_list_modes(&slot, fake.command(), &env).await.unwrap_err(), started);
        assert_eq!(fake.spawn_count(), 0);
    }

    // quill_flow_: rewrite_selection

    #[tokio::test]
    async fn quill_flow_read_refusals_return_their_code_and_send_no_rewrite() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        for code in [
            QuillErrorCode::NoSelection,
            QuillErrorCode::SecureField,
            QuillErrorCode::TooLong,
            QuillErrorCode::AccessibilityPermission,
            QuillErrorCode::ModifiersHeld,
        ] {
            let mut host = FakeHost::new();
            host.selection = Err(code);
            let error = run_rewrite_selection(&slot, fake.command(), &env, &mut host, "task-claude", 0)
                .await
                .unwrap_err();
            assert_eq!(error.code, code);
            assert!(!error.message.is_empty());
            assert_eq!(host.calls, ["read"], "nothing is replaced after a refusal");
            assert!(host.copies.is_empty() && host.records.is_empty());
        }
        assert_eq!(methods(&fake), ["ping"], "no rewrite request for a refused selection");
    }

    #[tokio::test]
    async fn quill_flow_success_sends_rewrite_then_outcome_inserted() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let mut host = FakeHost::new().with_clock(&[10_600, 10_900, 10_950]);
        host.replace.clipboard_replaced = true;

        let result = run_rewrite_selection(&slot, fake.command(), &env, &mut host, "task-claude", 10_000)
            .await
            .unwrap();

        assert_eq!(
            result,
            QuillRewriteSelectionResult {
                rewrite_id: "rw-1".into(),
                mode: "task-claude".into(),
                model: "test/model".into(),
                latency_ms: 12,
                end_to_end_ms: 950,
                anchored: true,
                clipboard_replaced: true,
            }
        );
        assert_eq!(methods(&fake), ["ping", "rewrite", "outcome"]);
        assert_eq!(
            params_of(&fake, "rewrite"),
            [json!({"text": "selected", "mode": "task-claude", "surface": "hotkey", "source_app": "Notes"})]
        );
        assert_eq!(params_of(&fake, "outcome"), [json!({"rewrite_id": "rw-1", "delivered": "inserted"})]);
        assert_eq!(host.calls, ["read", "replace:REWRITTEN"]);
        assert_eq!(host.records, [("rw-1".to_string(), "REWRITTEN".to_string(), Delivered::Inserted)]);
        assert!(host.copies.is_empty());
    }

    #[tokio::test]
    async fn quill_flow_target_change_copies_the_rewrite_reports_copied_and_returns_target_changed() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let mut host = FakeHost::new();
        host.replace = HostReplace {
            inserted: false,
            anchored: false,
            clipboard_replaced: false,
        };

        let error = run_rewrite_selection(&slot, fake.command(), &env, &mut host, "task-claude", 0)
            .await
            .unwrap_err();

        assert_eq!(error.code, QuillErrorCode::TargetChanged);
        assert_eq!(host.copies, ["REWRITTEN"]);
        assert_eq!(methods(&fake), ["ping", "rewrite", "outcome"]);
        assert_eq!(params_of(&fake, "outcome"), [json!({"rewrite_id": "rw-1", "delivered": "copied"})]);
        assert_eq!(host.records, [("rw-1".to_string(), "REWRITTEN".to_string(), Delivered::Copied)]);
    }

    #[tokio::test]
    async fn quill_flow_invalid_command_is_disabled_with_no_spawn_and_no_read() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let mut host = FakeHost::new();

        let error = run_rewrite_selection(&slot, "quill", &env, &mut host, "task-claude", 0)
            .await
            .unwrap_err();

        assert_eq!(error.code, QuillErrorCode::Disabled);
        assert_eq!(error.message, "Quill command must be an absolute path.");
        assert_eq!(fake.spawn_count(), 0);
        assert!(host.calls.is_empty(), "the selection is not read when the hook is disabled");
    }

    #[tokio::test]
    async fn quill_flow_engine_error_inserts_nothing_and_sends_no_outcome() {
        let body = r#"case "$method" in rewrite) fail upstream_error "engine said no";; esac"#;
        let fake = FakeQuill::new(body);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let mut host = FakeHost::new();

        let error = run_rewrite_selection(&slot, fake.command(), &env, &mut host, "task-claude", 0)
            .await
            .unwrap_err();

        assert_eq!(error.code, QuillErrorCode::UpstreamError);
        assert_eq!(error.message, "engine said no");
        assert_eq!(host.calls, ["read"]);
        assert!(host.copies.is_empty() && host.records.is_empty());
        assert_eq!(methods(&fake), ["ping", "rewrite"]);
    }

    #[tokio::test]
    async fn quill_flow_outcome_failure_after_insertion_is_logged_by_code_only() {
        let body = r#"case "$method" in
  rewrite) reply '{"rewrite_id":"rw-1","text":"REWRITTEN","mode":"task-claude","model":"m","latency_ms":1}';;
  outcome) fail internal_error "secret engine text";;
esac"#;
        let fake = FakeQuill::new(body);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let mut host = FakeHost::new();

        let result = run_rewrite_selection(&slot, fake.command(), &env, &mut host, "task-claude", 0).await;

        assert!(result.is_ok(), "the insertion already happened");
        assert!(host.logs().contains(&"quill outcome_failed code=internal_error".to_string()));
        assert!(host.logs().iter().all(|line| !line.contains("secret engine text")));
    }

    #[tokio::test]
    async fn quill_flow_busy_slot_returns_busy_without_reading_the_selection() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let _held = slot.lock().await;
        let mut host = FakeHost::new();

        let error = run_rewrite_selection(&slot, fake.command(), &env, &mut host, "task-claude", 0)
            .await
            .unwrap_err();

        assert_eq!(error.code, QuillErrorCode::Busy);
        assert!(host.calls.is_empty());
    }

    // quill_flow_: rewrite_text

    fn text_args(surface: &str) -> QuillRewriteTextArgs {
        QuillRewriteTextArgs {
            text: "dictated words".into(),
            mode: "dictation-clean".into(),
            surface: surface.into(),
            source_app: Some("Slack".into()),
        }
    }

    #[tokio::test]
    async fn quill_flow_rewrite_text_accepts_only_the_pill_surface_and_inserts_nothing() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let host = FakeHost::new();

        for surface in ["hotkey", "cli", "", "PILL"] {
            let error = run_rewrite_text(&slot, fake.command(), &env, &host, text_args(surface))
                .await
                .unwrap_err();
            assert_eq!(error.code, QuillErrorCode::BadRequest, "{surface:?}");
        }
        assert_eq!(fake.spawn_count(), 0, "a refused surface makes no request and no spawn");

        let result = run_rewrite_text(&slot, fake.command(), &env, &host, text_args("pill"))
            .await
            .unwrap();
        assert_eq!(
            result,
            QuillRewriteTextResult {
                rewrite_id: "rw-1".into(),
                text: "REWRITTEN".into(),
                mode: "task-claude".into(),
                model: "test/model".into(),
                latency_ms: 12,
            }
        );
        assert_eq!(
            params_of(&fake, "rewrite"),
            [json!({"text": "dictated words", "mode": "dictation-clean", "surface": "pill", "source_app": "Slack"})]
        );
        let mut absent = text_args("pill");
        absent.source_app = None;
        run_rewrite_text(&slot, fake.command(), &env, &host, absent).await.unwrap();
        assert_eq!(params_of(&fake, "rewrite")[1]["source_app"], Value::Null);
    }

    // quill_flow_: revert

    #[tokio::test]
    async fn quill_flow_revert_with_an_anchor_reports_reverted_via_inserted() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let mut host = FakeHost::new();
        host.slot_delivery = Some(Delivered::Inserted);

        let result = run_revert(&slot, fake.command(), &env, &mut host, "rw-1", None).await.unwrap();

        assert_eq!(result, QuillRevertResult { delivered: RevertDelivered::Inserted });
        assert_eq!(methods(&fake), ["ping", "revert", "outcome"]);
        assert_eq!(params_of(&fake, "revert"), [json!({"rewrite_id": "rw-1"})]);
        assert_eq!(
            params_of(&fake, "outcome"),
            [json!({"rewrite_id": "rw-1", "delivered": "inserted", "reverted_via": "inserted"})]
        );
        assert_eq!(host.calls, ["revert:rw-1:ORIGINAL"]);
        assert!(host.copies.is_empty());
    }

    #[tokio::test]
    async fn quill_flow_revert_that_cannot_act_in_place_copies_the_original() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        // The slot's own rewrite, but the anchor no longer holds.
        let mut host = FakeHost::new();
        host.slot_delivery = Some(Delivered::Copied);
        host.revert_inserts = false;
        let result = run_revert(&slot, fake.command(), &env, &mut host, "rw-1", None).await.unwrap();
        assert_eq!(result.delivered, RevertDelivered::Copied);
        assert_eq!(host.copies, ["ORIGINAL"]);

        // A rewrite the slot does not hold (a pill rewrite): the caller says how it was delivered.
        let mut host = FakeHost::new();
        let result = run_revert(&slot, fake.command(), &env, &mut host, "rw-9", Some("inserted"))
            .await
            .unwrap();
        assert_eq!(result.delivered, RevertDelivered::Copied);
        assert_eq!(host.copies, ["ORIGINAL"]);
        assert!(host.calls.is_empty(), "no in-place revert for a rewrite the slot does not hold");
        assert_eq!(
            params_of(&fake, "outcome")[1],
            json!({"rewrite_id": "rw-9", "delivered": "inserted", "reverted_via": "copied"})
        );
    }

    #[tokio::test]
    async fn quill_flow_revert_without_delivered_for_a_foreign_rewrite_is_bad_request_with_no_request() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let mut host = FakeHost::new();

        let missing = run_revert(&slot, fake.command(), &env, &mut host, "rw-9", None).await.unwrap_err();
        let invalid = run_revert(&slot, fake.command(), &env, &mut host, "rw-9", Some("printed"))
            .await
            .unwrap_err();

        assert_eq!(missing.code, QuillErrorCode::BadRequest);
        assert_eq!(invalid.code, QuillErrorCode::BadRequest);
        assert_eq!(fake.spawn_count(), 0);
    }

    #[tokio::test]
    async fn quill_flow_revert_engine_error_touches_no_text() {
        let body = r#"case "$method" in revert) fail revert_expired "too late";; esac"#;
        let fake = FakeQuill::new(body);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let mut host = FakeHost::new();
        host.slot_delivery = Some(Delivered::Inserted);

        let error = run_revert(&slot, fake.command(), &env, &mut host, "rw-1", None).await.unwrap_err();

        assert_eq!(error.code, QuillErrorCode::RevertExpired);
        assert_eq!(error.message, "too late");
        assert!(host.calls.is_empty() && host.copies.is_empty());
        assert_eq!(methods(&fake), ["ping", "revert"]);
    }

    // quill_flow_: report_outcome, list_modes

    #[tokio::test]
    async fn quill_flow_report_outcome_sends_one_outcome_line_with_exactly_the_given_fields() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        let result = run_report_outcome(&slot, fake.command(), &env, "rw-1", "failed", None).await;
        assert_eq!(result, Ok(QuillReportOutcomeResult {}));
        run_report_outcome(&slot, fake.command(), &env, "rw-2", "inserted", Some("copied"))
            .await
            .unwrap();

        assert_eq!(methods(&fake), ["ping", "outcome", "outcome"]);
        assert_eq!(
            params_of(&fake, "outcome"),
            [
                json!({"rewrite_id": "rw-1", "delivered": "failed"}),
                json!({"rewrite_id": "rw-2", "delivered": "inserted", "reverted_via": "copied"}),
            ]
        );
    }

    #[tokio::test]
    async fn quill_flow_report_outcome_refuses_values_outside_the_sets_without_a_request() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        for (delivered, reverted_via) in [
            ("printed", None),
            ("", None),
            ("Inserted", None),
            ("inserted", Some("failed")),
            ("inserted", Some("printed")),
            ("copied", Some("")),
        ] {
            let error = run_report_outcome(&slot, fake.command(), &env, "rw-1", delivered, reverted_via)
                .await
                .unwrap_err();
            assert_eq!(error.code, QuillErrorCode::BadRequest, "{delivered:?} {reverted_via:?}");
        }
        assert_eq!(fake.spawn_count(), 0);
    }

    #[tokio::test]
    async fn quill_flow_list_modes_returns_camel_case_modes() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        let result = run_list_modes(&slot, fake.command(), &env).await.unwrap();

        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(
            value,
            json!({"modes": [{
                "id": "only", "name": "n", "description": "d",
                "form": "task", "targetModel": "claude", "default": true
            }]})
        );
    }

    // quill_timing_line_

    #[test]
    fn quill_timing_line_has_the_exact_format() {
        assert_eq!(
            quill_timing_line("task-claude", 10_000, 10_950, 300, TimingResult::Inserted),
            "surface=hotkey mode=task-claude end_to_end_ms=950 engine_ms=300 result=inserted"
        );
        for (result, text) in [
            (TimingResult::Inserted, "inserted"),
            (TimingResult::Copied, "copied"),
            (TimingResult::Failed, "failed"),
            (TimingResult::Refused, "refused"),
        ] {
            assert!(quill_timing_line("m", 0, 0, 0, result).ends_with(&format!(" result={text}")));
        }
    }

    #[test]
    fn quill_timing_line_counts_from_the_press_and_saturates_at_zero() {
        // Pressed 600 ms before the call was dispatched: the hold window is counted.
        assert!(quill_timing_line("m", 9_400, 10_100, 0, TimingResult::Inserted)
            .contains("end_to_end_ms=700 "));
        // A frontend clock ahead of ours cannot produce a negative or wrapped value.
        assert!(quill_timing_line("m", u64::MAX, 10, 0, TimingResult::Inserted).contains("end_to_end_ms=0 "));
    }

    #[test]
    fn quill_timing_line_keeps_the_mode_to_one_safe_token() {
        let line = quill_timing_line("evil mode\nresult=inserted\r[x]", 0, 0, 0, TimingResult::Failed);
        assert_eq!(line.lines().count(), 1);
        assert!(line.starts_with("surface=hotkey mode=evil_mode_result_inserted__x_ end_to_end_ms=0"));
        let long = quill_timing_line(&"a".repeat(500), 0, 0, 0, TimingResult::Failed);
        assert_eq!(long.split(' ').nth(1).unwrap().len(), "mode=".len() + 64);
    }

    #[test]
    fn quill_timing_line_event_lines_carry_only_the_event_and_the_code() {
        assert_eq!(
            quill_event_line("outcome_failed", QuillErrorCode::Transport),
            "quill outcome_failed code=transport"
        );
    }

    #[tokio::test]
    async fn quill_timing_line_from_the_flow_uses_the_injected_clock() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        // engine start, engine end, inserter return
        let mut host = FakeHost::new().with_clock(&[10_600, 10_900, 10_950]);

        run_rewrite_selection(&slot, fake.command(), &env, &mut host, "task-claude", 10_000)
            .await
            .unwrap();

        assert_eq!(
            host.logs(),
            ["surface=hotkey mode=task-claude end_to_end_ms=950 engine_ms=300 result=inserted"]
        );
    }

    #[tokio::test]
    async fn quill_timing_line_future_press_time_yields_zero() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let mut host = FakeHost::new().with_clock(&[10_600, 10_900, 10_950]);

        run_rewrite_selection(&slot, fake.command(), &env, &mut host, "task-claude", 99_999_999)
            .await
            .unwrap();

        assert!(host.timing_lines()[0].contains("end_to_end_ms=0 "), "{:?}", host.logs());
    }

    #[tokio::test]
    async fn quill_timing_line_each_code_maps_to_its_result_and_writes_exactly_one_line() {
        let fs = NativeFs;
        let flow = FakeQuill::new(FLOW_BODY);
        let failing = FakeQuill::new(r#"case "$method" in rewrite) fail upstream_error "x";; esac"#);
        let dying = FakeQuill::new("exit 0");

        // (label, command, host tweak, expected result)
        type Tweak = fn(&mut FakeHost);
        let cases: Vec<(&str, &FakeQuill, Tweak, &str)> = vec![
            ("inserted", &flow, |_| {}, "inserted"),
            ("target_changed", &flow, |host| host.replace.inserted = false, "copied"),
            ("no_selection", &flow, |host| host.selection = Err(QuillErrorCode::NoSelection), "refused"),
            ("secure_field", &flow, |host| host.selection = Err(QuillErrorCode::SecureField), "refused"),
            ("too_long", &flow, |host| host.selection = Err(QuillErrorCode::TooLong), "refused"),
            (
                "accessibility_permission",
                &flow,
                |host| host.selection = Err(QuillErrorCode::AccessibilityPermission),
                "refused",
            ),
            ("modifiers_held", &flow, |host| host.selection = Err(QuillErrorCode::ModifiersHeld), "refused"),
            ("upstream_error", &failing, |_| {}, "failed"),
            ("transport", &dying, |_| {}, "failed"),
        ];
        for (label, fake, tweak, expected) in cases {
            let env = env_for(fake, &fs, Duration::from_secs(5));
            let slot = Mutex::new(None);
            let mut host = FakeHost::new();
            tweak(&mut host);
            let _ = run_rewrite_selection(&slot, fake.command(), &env, &mut host, "task-claude", 0).await;
            let lines = host.timing_lines();
            assert_eq!(lines.len(), 1, "{label}: {:?}", host.logs());
            assert!(lines[0].ends_with(&format!(" result={expected}")), "{label}: {}", lines[0]);
        }

        // busy and disabled never reach the engine and are failures too.
        let env = env_for(&flow, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let held = slot.lock().await;
        let mut host = FakeHost::new();
        let _ = run_rewrite_selection(&slot, flow.command(), &env, &mut host, "task-claude", 0).await;
        assert_eq!(host.timing_lines().len(), 1);
        assert!(host.timing_lines()[0].ends_with(" result=failed"));
        drop(held);
        let mut host = FakeHost::new();
        let _ = run_rewrite_selection(&slot, "quill", &env, &mut host, "task-claude", 0).await;
        assert_eq!(host.timing_lines().len(), 1);
        assert!(host.timing_lines()[0].ends_with(" result=failed"));
    }

    #[tokio::test]
    async fn quill_timing_line_never_carries_selection_rewrite_or_engine_text() {
        const SELECTION: &str = "SENTINEL-SELECTION-TEXT";
        let body = r#"case "$method" in
  rewrite) reply '{"rewrite_id":"rw-1","text":"SENTINEL-REWRITE-TEXT","mode":"task-claude","model":"m","latency_ms":1}';;
  outcome) fail internal_error "SENTINEL-ENGINE-TEXT";;
esac"#;
        let fake = FakeQuill::new(body);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let mut host = FakeHost::new();
        host.selection = Ok(SELECTION.to_string());
        host.source_app = Some("SENTINEL-APP".to_string());

        run_rewrite_selection(&slot, fake.command(), &env, &mut host, "task-claude", 0)
            .await
            .unwrap();
        host.selection = Err(QuillErrorCode::TooLong);
        let _ = run_rewrite_selection(&slot, fake.command(), &env, &mut host, "task-claude", 0).await;

        let logs = host.logs();
        assert!(!logs.is_empty());
        for line in &logs {
            assert!(!line.contains("SENTINEL"), "{line}");
            assert!(
                line.starts_with("surface=hotkey mode=task-claude ")
                    || (line.starts_with("quill ") && line.contains(" code=")),
                "unexpected log line: {line}"
            );
        }
    }

    #[tokio::test]
    async fn quill_timing_line_is_not_written_for_rewrite_text() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);
        let host = FakeHost::new();

        run_rewrite_text(&slot, fake.command(), &env, &host, text_args("pill")).await.unwrap();
        let _ = run_rewrite_text(&slot, fake.command(), &env, &host, text_args("hotkey")).await;

        assert!(host.timing_lines().is_empty(), "{:?}", host.logs());
    }

    // quill_should_spawn_at_setup_

    #[test]
    fn quill_should_spawn_at_setup_only_when_enabled_and_the_command_validated() {
        let valid: Result<PathBuf, String> = Ok(PathBuf::from("/Users/me/.local/bin/quill"));
        let invalid: Result<PathBuf, String> = Err("Quill command must be an absolute path.".to_string());
        assert!(quill_should_spawn_at_setup(true, &valid));
        assert!(!quill_should_spawn_at_setup(true, &invalid));
        assert!(!quill_should_spawn_at_setup(false, &valid));
        assert!(!quill_should_spawn_at_setup(false, &invalid));
    }

    #[tokio::test]
    async fn quill_should_spawn_at_setup_spawn_fills_the_slot_without_a_request() {
        let fake = FakeQuill::new(FLOW_BODY);
        let fs = NativeFs;
        let env = env_for(&fake, &fs, Duration::from_secs(5));
        let slot = Mutex::new(None);

        spawn_into_slot(&slot, fake.command(), &env).await.unwrap();

        assert!(slot.try_lock().unwrap().is_some());
        assert_eq!(fake.spawn_count(), 1);
        assert_eq!(methods(&fake), ["ping"]);
        // A setup spawn never waits: a held slot is skipped as busy.
        let _held = slot.lock().await;
        let error = spawn_into_slot(&slot, fake.command(), &env).await.unwrap_err();
        assert_eq!(error.code, QuillErrorCode::Busy);
    }
}
