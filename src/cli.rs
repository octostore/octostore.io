use crate::{
    elections::{CampaignResponse, CreateElectionResponse, ElectionStatusResponse, ResignResponse},
    models::{
        AcquireLockResponse, CreateSessionResponse, KeepAliveResponse, LockStatusResponse,
        RenewLockResponse,
    },
};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand};
use futures::{Future, StreamExt};
use rand::Rng;
use reqwest::{Client, Response, StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::json;
use std::{
    env,
    io::{self, Read},
    path::Path,
    pin::Pin,
    time::{Duration, Instant},
};
use uuid::Uuid;

pub const EXIT_OK: i32 = 0;
pub const EXIT_ACQUIRE_TIMEOUT: i32 = 11;
pub const EXIT_LOST: i32 = 20;
pub const EXIT_USAGE: i32 = 64;
pub const EXIT_SOFTWARE: i32 = 70;
pub const EXIT_SIGINT: i32 = 130;
pub const EXIT_SIGTERM: i32 = 143;

const DEFAULT_PUBLIC_SERVER: &str = "https://api.octostore.io";
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const MIN_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_CONFIGURED_DURATION: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_API_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_SERVER_RETRY_DELAY_MS: u64 = 5 * 60 * 1_000;
const AUTHORITY_CLOCK_POLL: Duration = Duration::from_millis(100);
const LOCK_SESSION_TTL_SECONDS: u32 = 30;
const AUTHORITY_SAFETY_FRACTION: f64 = 0.8;
const MAX_SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);
const MAX_SSE_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "octostore",
    version,
    about = "Stop two agents from doing the same work",
    long_about = "OctoStore gives independent processes one temporary coordinator or one temporary task owner. Run without a subcommand to start the server.",
    after_help = "Use `election` when one agent coordinates a group. Use `lock` when one agent owns one exact item. A lease is not permission to perform irreversible work, and your supervisor must stop or fence work when authority is lost."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the OctoStore API server (also the default with no subcommand).
    Serve,
    /// Choose one live coordinator; hosted elections need no login.
    Election(ElectionArgs),
    /// Give one process temporary ownership of one exact item.
    Lock(LockArgs),
}

#[derive(Debug, Args)]
pub struct ElectionArgs {
    #[command(subcommand)]
    pub command: ElectionCommand,
}

#[derive(Debug, Subcommand)]
pub enum ElectionCommand {
    /// Create one room, then pass the same room ID to every candidate.
    Create(PublicReadArgs),
    /// Wait for leadership, renew it, and report loss without reacquiring.
    Hold(ElectionHoldArgs),
    /// Read the current leader or vacant state.
    Status(PublicTargetArgs),
    /// Reconcile current state after each best-effort watch signal.
    Watch(PublicTargetArgs),
}

#[derive(Debug, Args)]
pub struct LockArgs {
    #[command(subcommand)]
    pub command: LockCommand,
}

#[derive(Debug, Subcommand)]
pub enum LockCommand {
    /// Wait for ownership, renew it, and report loss without reacquiring.
    Hold(LockHoldArgs),
    /// Read whether this exact item is free or held.
    Status(LockTargetArgs),
    /// Reconcile current state after each best-effort watch signal.
    Watch(LockTargetArgs),
}

#[derive(Debug, Clone, Args)]
pub struct PublicReadArgs {
    /// OctoStore authority. Overrides OCTOSTORE_URL.
    #[arg(long)]
    pub server: Option<String>,
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub timeouts: TimeoutArgs,
}

#[derive(Debug, Clone, Args)]
pub struct PublicTargetArgs {
    /// Shared election room ID.
    pub election_id: String,
    /// OctoStore authority. Overrides OCTOSTORE_URL.
    #[arg(long)]
    pub server: Option<String>,
    /// Emit machine-readable JSON or JSONL.
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub timeouts: TimeoutArgs,
}

#[derive(Debug, Clone, Args)]
pub struct ElectionHoldArgs {
    /// Shared election room ID created once outside the candidates.
    pub election_id: String,
    /// Stable identifier unique to this candidate process.
    #[arg(long)]
    pub candidate: String,
    /// Lease duration in seconds.
    #[arg(long, default_value_t = 30)]
    pub ttl: u32,
    /// Stop waiting after this duration; bare integers are seconds; wait indefinitely when omitted.
    #[arg(long)]
    pub acquire_timeout: Option<String>,
    /// OctoStore authority. Overrides OCTOSTORE_URL.
    #[arg(long)]
    pub server: Option<String>,
    /// Permit the leader capability over cleartext HTTP to a non-loopback development server.
    #[arg(long)]
    pub allow_insecure_http: bool,
    /// Emit versioned newline-delimited lifecycle events.
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub timeouts: TimeoutArgs,
}

#[derive(Debug, Clone, Args)]
pub struct LockTargetArgs {
    /// Stable key derived from the durable work identity.
    pub name: String,
    /// Required OctoStore authority. Overrides OCTOSTORE_URL.
    #[arg(long)]
    pub server: Option<String>,
    /// Permit credentials over cleartext HTTP to a non-loopback development server.
    #[arg(long)]
    pub allow_insecure_http: bool,
    /// Emit machine-readable JSON or JSONL.
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub timeouts: TimeoutArgs,
}

#[derive(Debug, Clone, Args)]
pub struct LockHoldArgs {
    /// Stable key derived from the durable work identity.
    pub name: String,
    /// Lease duration in seconds.
    #[arg(long, default_value_t = 120)]
    pub ttl: u32,
    /// Stop waiting after this duration; bare integers are seconds; wait indefinitely when omitted.
    #[arg(long)]
    pub acquire_timeout: Option<String>,
    /// Required OctoStore authority. Overrides OCTOSTORE_URL.
    #[arg(long)]
    pub server: Option<String>,
    /// Permit credentials over cleartext HTTP to a non-loopback development server.
    #[arg(long)]
    pub allow_insecure_http: bool,
    /// Emit versioned newline-delimited lifecycle events.
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub timeouts: TimeoutArgs,
}

#[derive(Debug, Clone, Args)]
pub struct TimeoutArgs {
    /// TCP connection timeout.
    #[arg(long, default_value = "5s")]
    pub connect_timeout: String,
    /// Per-request timeout. Renewal safety deadlines may shorten it.
    #[arg(long, default_value = "10s")]
    pub request_timeout: String,
    /// Maximum clean release attempt after a signal.
    #[arg(long, default_value = "2s")]
    pub shutdown_timeout: String,
}

#[derive(Debug, Clone)]
struct RuntimeOptions {
    server: String,
    client: Client,
    request_timeout: Duration,
    shutdown_timeout: Duration,
    json: bool,
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    code: Option<String>,
    details: Option<String>,
    retry_after_ms: Option<u64>,
    request_id: Option<String>,
}

#[derive(Debug)]
struct ApiFailure {
    status: Option<StatusCode>,
    code: String,
    details: String,
    retry_after_ms: Option<u64>,
    request_id: Option<String>,
    transport: bool,
}

impl ApiFailure {
    fn retryable_before_acquire(&self) -> bool {
        self.transport
            || self.status.is_some_and(|status| status.is_server_error())
            || matches!(
                self.code.as_str(),
                "rate_limited" | "capacity_exceeded" | "upstream_unavailable" | "internal_error"
            )
    }

    fn proves_loss(&self) -> bool {
        matches!(
            self.code.as_str(),
            "lease_not_current"
                | "not_found"
                | "session_expired"
                | "authentication_failed"
                | "authentication_required"
                | "forbidden"
                | "session_identity_mismatch"
                | "invalid_input"
                | "invalid_ttl"
                | "invalid_lock_name"
        )
    }

    fn diagnostic(&self) -> String {
        match &self.request_id {
            Some(request_id) => format!("{} (request {})", self.details, request_id),
            None => self.details.clone(),
        }
    }

    fn diagnostic_redacting(&self, secret: Option<&str>) -> String {
        redact_text(&self.diagnostic(), secret)
    }
}

#[derive(Debug, Clone, Copy)]
enum Signal {
    Interrupt,
    Terminate,
}

impl Signal {
    fn exit_code(self) -> i32 {
        match self {
            Self::Interrupt => EXIT_SIGINT,
            Self::Terminate => EXIT_SIGTERM,
        }
    }
}

type SignalFuture = Pin<Box<dyn Future<Output = Signal> + Send>>;

#[derive(Debug, Serialize)]
struct LifecycleEvent<'a> {
    schema_version: u8,
    sequence: u64,
    event: &'a str,
    kind: &'a str,
    name: &'a str,
    server: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    term: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason_code: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    authority_remaining_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    authority_observed_unix_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    authority_observed_continuous_ms: Option<u64>,
    observed_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct WatchSnapshot<'a> {
    schema_version: u8,
    sequence: u64,
    event: &'static str,
    kind: &'a str,
    name: &'a str,
    server: &'a str,
    snapshot: &'a serde_json::Value,
    observed_at: DateTime<Utc>,
}

struct Emitter {
    json: bool,
    sequence: u64,
}

#[derive(Debug, Clone, Copy)]
struct AuthorityInstant {
    scheduler: Instant,
    continuous_ms: u64,
}

impl AuthorityInstant {
    fn now() -> Result<Self, i32> {
        // Sample the suspend-inclusive clock first. A pause before the
        // scheduler sample can only shorten the resulting authority budget.
        let continuous_ms = continuous_observation_ms()?;
        Ok(Self {
            scheduler: Instant::now(),
            continuous_ms,
        })
    }

    fn deadline_after(self, duration: Duration) -> Result<AuthorityDeadline, i32> {
        let duration_ms = u64::try_from(duration.as_millis()).map_err(|_| EXIT_SOFTWARE)?;
        Ok(AuthorityDeadline {
            scheduler: self.scheduler.checked_add(duration).ok_or(EXIT_SOFTWARE)?,
            continuous_ms: self
                .continuous_ms
                .checked_add(duration_ms)
                .ok_or(EXIT_SOFTWARE)?,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct AuthorityDeadline {
    scheduler: Instant,
    continuous_ms: u64,
}

impl AuthorityDeadline {
    fn remaining(self) -> Result<Duration, i32> {
        self.remaining_at(Instant::now(), continuous_observation_ms()?)
    }

    fn remaining_at(self, scheduler: Instant, continuous_ms: u64) -> Result<Duration, i32> {
        let scheduler_remaining = self.scheduler.saturating_duration_since(scheduler);
        let continuous_remaining =
            Duration::from_millis(self.continuous_ms.saturating_sub(continuous_ms));
        let remaining = scheduler_remaining.min(continuous_remaining);
        if remaining.is_zero() {
            Err(EXIT_LOST)
        } else {
            Ok(remaining)
        }
    }

    fn reached(self) -> bool {
        self.remaining().is_err()
    }

    fn min(self, other: Self) -> Self {
        Self {
            scheduler: self.scheduler.min(other.scheduler),
            continuous_ms: self.continuous_ms.min(other.continuous_ms),
        }
    }

    fn no_later_than(self, other: Self) -> bool {
        self.continuous_ms <= other.continuous_ms
    }
}

struct EventData<'a> {
    event: &'a str,
    kind: &'a str,
    name: &'a str,
    server: &'a str,
    candidate_id: Option<&'a str>,
    term: Option<u64>,
    expires_at: Option<DateTime<Utc>>,
    retry_after_ms: Option<u64>,
    reason_code: Option<&'a str>,
}

impl Emitter {
    fn new(json: bool) -> Self {
        Self { json, sequence: 0 }
    }

    fn emit(&mut self, data: EventData<'_>) {
        self.emit_inner(data, None, None, None, Utc::now());
    }

    fn emit_authority(
        &mut self,
        data: EventData<'_>,
        deadline: AuthorityDeadline,
    ) -> Result<(), i32> {
        let (remaining_ms, observed_at, observed_continuous_ms) = authority_observation_with(
            deadline,
            Utc::now,
            continuous_observation_ms,
            Instant::now,
            continuous_observation_ms,
        )?;
        self.emit_inner(
            data,
            Some(remaining_ms),
            Some(observed_at.timestamp_millis()),
            Some(observed_continuous_ms),
            observed_at,
        );
        Ok(())
    }

    fn emit_inner(
        &mut self,
        data: EventData<'_>,
        authority_remaining_ms: Option<u64>,
        authority_observed_unix_ms: Option<i64>,
        authority_observed_continuous_ms: Option<u64>,
        observed_at: DateTime<Utc>,
    ) {
        self.sequence += 1;
        let event = LifecycleEvent {
            schema_version: 1,
            sequence: self.sequence,
            event: data.event,
            kind: data.kind,
            name: data.name,
            server: data.server,
            candidate_id: data.candidate_id,
            term: data.term,
            expires_at: data.expires_at,
            retry_after_ms: data.retry_after_ms,
            reason_code: data.reason_code,
            authority_remaining_ms,
            authority_observed_unix_ms,
            authority_observed_continuous_ms,
            observed_at,
        };
        if self.json {
            println!(
                "{}",
                serde_json::to_string(&event).expect("lifecycle event must serialize")
            );
        } else {
            let detail = match data.event {
                "waiting" => format!(
                    "waiting; retry in {}ms",
                    data.retry_after_ms.unwrap_or_default()
                ),
                "leader" | "acquired" | "renewed" => {
                    format!("term {}", data.term.unwrap_or_default())
                }
                "released" => "released cleanly".to_string(),
                "lost" | "uncertain" | "error" => data.reason_code.unwrap_or("unknown").to_string(),
                _ => data.event.to_string(),
            };
            println!(
                "{} {} · {} · {}",
                terminal_safe(data.kind),
                terminal_safe(data.name),
                terminal_safe(data.event),
                terminal_safe(&detail)
            );
        }
    }
}

fn authority_observation_with<WallClock, TransferClock, SchedulerClock, ContinuousClock>(
    deadline: AuthorityDeadline,
    wall_clock: WallClock,
    transfer_clock: TransferClock,
    scheduler_clock: SchedulerClock,
    continuous_clock: ContinuousClock,
) -> Result<(u64, DateTime<Utc>, u64), i32>
where
    WallClock: FnOnce() -> DateTime<Utc>,
    TransferClock: FnOnce() -> Result<u64, i32>,
    SchedulerClock: FnOnce() -> Instant,
    ContinuousClock: FnOnce() -> Result<u64, i32>,
{
    // Take both transferable observations before the local deadline sample. If
    // this process is paused between samples, the later local continuous sample
    // shortens the emitted budget while the supervisor also subtracts the full
    // same-host suspend-inclusive age. Reversing the order could hide that pause.
    let observed_at = wall_clock();
    let observed_continuous_ms = transfer_clock()?;
    let remaining_ms = authority_remaining_ms(deadline, scheduler_clock(), continuous_clock()?)?;
    Ok((remaining_ms, observed_at, observed_continuous_ms))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn continuous_observation_ms() -> Result<u64, i32> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `value` is valid writable storage for one timespec, and
    // CLOCK_BOOTTIME does not retain the pointer and includes system suspend.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut value) } != 0 {
        return Err(EXIT_SOFTWARE);
    }
    let seconds = u64::try_from(value.tv_sec).map_err(|_| EXIT_SOFTWARE)?;
    let nanoseconds = u64::try_from(value.tv_nsec).map_err(|_| EXIT_SOFTWARE)?;
    seconds
        .checked_mul(1_000)
        .and_then(|milliseconds| milliseconds.checked_add(nanoseconds / 1_000_000))
        .ok_or(EXIT_SOFTWARE)
}

#[cfg(target_os = "macos")]
fn continuous_observation_ms() -> Result<u64, i32> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `value` is writable storage for one timespec. On macOS,
    // CLOCK_MONOTONIC_RAW is mach_continuous_time and includes system sleep.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut value) } != 0 {
        return Err(EXIT_SOFTWARE);
    }
    let seconds = u64::try_from(value.tv_sec).map_err(|_| EXIT_SOFTWARE)?;
    let nanoseconds = u64::try_from(value.tv_nsec).map_err(|_| EXIT_SOFTWARE)?;
    seconds
        .checked_mul(1_000)
        .and_then(|milliseconds| milliseconds.checked_add(nanoseconds / 1_000_000))
        .ok_or(EXIT_SOFTWARE)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn continuous_observation_ms() -> Result<u64, i32> {
    Err(EXIT_SOFTWARE)
}

fn authority_remaining_ms(
    deadline: AuthorityDeadline,
    scheduler: Instant,
    continuous_ms: u64,
) -> Result<u64, i32> {
    let remaining_ms = u64::try_from(deadline.remaining_at(scheduler, continuous_ms)?.as_millis())
        .unwrap_or(u64::MAX);
    if remaining_ms == 0 {
        Err(EXIT_LOST)
    } else {
        Ok(remaining_ms)
    }
}

pub async fn run(command: Command) -> i32 {
    match command {
        Command::Serve => EXIT_OK,
        Command::Election(args) => match args.command {
            ElectionCommand::Create(args) => election_create(args).await,
            ElectionCommand::Hold(args) => election_hold(args).await,
            ElectionCommand::Status(args) => election_status(args).await,
            ElectionCommand::Watch(args) => election_watch(args).await,
        },
        Command::Lock(args) => match args.command {
            LockCommand::Hold(args) => lock_hold(args).await,
            LockCommand::Status(args) => lock_status(args).await,
            LockCommand::Watch(args) => lock_watch(args).await,
        },
    }
}

fn runtime_options(
    server: Option<&str>,
    timeouts: &TimeoutArgs,
    require_explicit_server: bool,
    capability_bearing: bool,
    allow_insecure_http: bool,
    json: bool,
) -> Result<RuntimeOptions, String> {
    let server = resolve_server(server, require_explicit_server)?;
    let server = validate_server(&server, capability_bearing, allow_insecure_http)?;
    let connect_timeout = parse_duration(&timeouts.connect_timeout)?;
    let request_timeout = parse_duration(&timeouts.request_timeout)?;
    let shutdown_timeout = parse_duration(&timeouts.shutdown_timeout)?;
    if connect_timeout.is_zero() || request_timeout.is_zero() || shutdown_timeout.is_zero() {
        return Err("timeouts must be greater than zero".to_string());
    }
    let mut client_builder = client_builder_with_trust()?;
    if capability_bearing
        && Url::parse(&server)
            .ok()
            .as_ref()
            .is_some_and(is_loopback_url)
    {
        // Loopback HTTP is accepted without the explicit insecure override,
        // so it must remain loopback in the actual transport too. Reqwest
        // otherwise inherits ambient proxy variables and can forward the
        // bearer header to an unrelated local or network proxy.
        client_builder = client_builder.no_proxy();
    }
    let client = client_builder
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .user_agent(format!("octostore-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("could not create HTTP client: {error}"))?;
    Ok(RuntimeOptions {
        server,
        client,
        request_timeout,
        shutdown_timeout,
        json,
    })
}

fn client_builder_with_trust() -> Result<reqwest::ClientBuilder, String> {
    // API redirects are never part of the CLI contract. Refusing them also
    // ensures a bearer credential cannot cross a transport boundary that was
    // not validated by `validate_server` (including HTTPS-to-HTTP downgrades).
    let mut builder = Client::builder()
        .use_rustls_tls()
        .redirect(reqwest::redirect::Policy::none());
    let Some(path) = env::var_os("OCTOSTORE_CA_BUNDLE") else {
        return Ok(builder);
    };
    let certificates = std::fs::read(&path).map_err(|error| {
        format!(
            "could not read OCTOSTORE_CA_BUNDLE {}: {error}",
            Path::new(&path).display()
        )
    })?;
    let certificates = reqwest::Certificate::from_pem_bundle(&certificates)
        .map_err(|error| format!("could not parse OCTOSTORE_CA_BUNDLE: {error}"))?;
    if certificates.is_empty() {
        return Err("OCTOSTORE_CA_BUNDLE contains no certificates".to_string());
    }
    for certificate in certificates {
        builder = builder.add_root_certificate(certificate);
    }
    Ok(builder)
}

fn resolve_server(explicit: Option<&str>, require_explicit_server: bool) -> Result<String, String> {
    if let Some(server) = explicit {
        return Ok(server.to_string());
    }
    if let Ok(server) = env::var("OCTOSTORE_URL") {
        if !server.trim().is_empty() {
            return Ok(server);
        }
    }
    if require_explicit_server {
        Err("lock commands require --server or OCTOSTORE_URL".to_string())
    } else {
        Ok(DEFAULT_PUBLIC_SERVER.to_string())
    }
}

fn validate_server(
    server: &str,
    capability_bearing: bool,
    allow_insecure_http: bool,
) -> Result<String, String> {
    let mut url = Url::parse(server).map_err(|error| format!("invalid server URL: {error}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("credentials are not allowed in the server URL".to_string());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("server URL must not contain a query or fragment".to_string());
    }
    if !matches!(url.scheme(), "http" | "https") {
        return Err("server URL must use http or https".to_string());
    }
    if capability_bearing
        && url.scheme() == "http"
        && !allow_insecure_http
        && !is_loopback_url(&url)
    {
        return Err(
            "refusing to send a secret capability over cleartext HTTP; use HTTPS, loopback, or --allow-insecure-http for development"
                .to_string(),
        );
    }
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(if path.is_empty() { "/" } else { &path });
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn is_loopback_url(url: &Url) -> bool {
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback()),
        None => false,
    }
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1u64)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3_600_000)
    } else {
        (value, 1_000)
    };
    let number = number
        .parse::<u64>()
        .map_err(|_| format!("invalid duration '{value}'"))?;
    let milliseconds = number
        .checked_mul(multiplier)
        .ok_or_else(|| format!("duration '{value}' is too large"))?;
    let duration = Duration::from_millis(milliseconds);
    if duration > MAX_CONFIGURED_DURATION {
        return Err(format!("duration '{value}' exceeds the 24h maximum"));
    }
    Ok(duration)
}

fn endpoint(server: &str, path: &str) -> String {
    format!("{}{}", server.trim_end_matches('/'), path)
}

fn encoded(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

fn validated_request_id(value: &str) -> Option<String> {
    let suffix = value.strip_prefix("req_")?;
    (suffix.len() == 32
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then(|| value.to_string())
}

fn normalized_error_code(code: Option<String>, status: StatusCode) -> String {
    let code = code.filter(|code| {
        matches!(
            code.as_str(),
            "authentication_required"
                | "authentication_failed"
                | "forbidden"
                | "invalid_input"
                | "invalid_ttl"
                | "invalid_lock_name"
                | "not_found"
                | "session_expired"
                | "lease_not_current"
                | "conflict"
                | "capacity_exceeded"
                | "lock_limit_exceeded"
                | "rate_limited"
                | "upstream_unavailable"
                | "internal_error"
        )
    });
    code.unwrap_or_else(|| {
        if status.is_server_error() {
            "internal_error".to_string()
        } else {
            "request_failed".to_string()
        }
    })
}

fn redact_text(value: &str, secret: Option<&str>) -> String {
    match secret.filter(|secret| !secret.is_empty()) {
        Some(secret) => value.replace(secret, "[REDACTED]"),
        None => value.to_string(),
    }
}

fn redact_json_strings(value: &mut serde_json::Value, secret: Option<&str>) {
    match value {
        serde_json::Value::String(text) => *text = redact_text(text, secret),
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_strings(value, secret);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                redact_json_strings(value, secret);
            }
        }
        _ => {}
    }
}

async fn parse_response<T: DeserializeOwned>(
    response: Result<Response, reqwest::Error>,
    accepted: &[StatusCode],
) -> Result<T, ApiFailure> {
    let mut response = response.map_err(transport_failure)?;
    let status = response.status();
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .and_then(validated_request_id);
    if response.content_length().is_some_and(|length| {
        length > u64::try_from(MAX_API_RESPONSE_BYTES).expect("response limit fits u64")
    }) {
        return Err(ApiFailure {
            status: Some(status),
            code: "response_too_large".to_string(),
            details: format!("server response exceeded {MAX_API_RESPONSE_BYTES} bytes"),
            retry_after_ms: None,
            request_id,
            transport: true,
        });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_failure)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_API_RESPONSE_BYTES {
            return Err(ApiFailure {
                status: Some(status),
                code: "response_too_large".to_string(),
                details: format!("server response exceeded {MAX_API_RESPONSE_BYTES} bytes"),
                retry_after_ms: None,
                request_id,
                transport: true,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    if accepted.contains(&status) {
        return serde_json::from_slice(&bytes).map_err(|error| ApiFailure {
            status: Some(status),
            code: "invalid_response".to_string(),
            details: format!("server returned malformed JSON: {error}"),
            retry_after_ms: None,
            request_id,
            transport: true,
        });
    }
    let body = serde_json::from_slice::<ApiErrorBody>(&bytes).unwrap_or(ApiErrorBody {
        code: None,
        details: None,
        retry_after_ms: None,
        request_id: None,
    });
    let retry_after_ms = body
        .retry_after_ms
        .filter(|delay| (1..=MAX_SERVER_RETRY_DELAY_MS).contains(delay));
    let invalid_retry_guidance = body.retry_after_ms.is_some() && retry_after_ms.is_none();
    Err(ApiFailure {
        status: Some(status),
        code: if invalid_retry_guidance {
            "invalid_response".to_string()
        } else {
            normalized_error_code(body.code, status)
        },
        details: if invalid_retry_guidance {
            "server returned invalid retry_after_ms guidance".to_string()
        } else {
            body.details
                .unwrap_or_else(|| format!("server returned HTTP {status}"))
        },
        retry_after_ms,
        request_id: body
            .request_id
            .as_deref()
            .and_then(validated_request_id)
            .or(request_id),
        transport: false,
    })
}

fn transport_failure(error: reqwest::Error) -> ApiFailure {
    ApiFailure {
        status: error.status(),
        code: "transport_error".to_string(),
        details: "could not confirm the server response".to_string(),
        retry_after_ms: None,
        request_id: None,
        transport: true,
    }
}

fn usage_error(message: impl AsRef<str>) -> i32 {
    eprintln!("octostore: {}", terminal_safe(message.as_ref()));
    EXIT_USAGE
}

fn software_error(message: impl AsRef<str>) -> i32 {
    eprintln!("octostore: {}", terminal_safe(message.as_ref()));
    EXIT_SOFTWARE
}

fn print_value<T: Serialize>(value: &T, json_output: bool, human: impl FnOnce()) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string(value).expect("API response must serialize")
        );
    } else {
        human();
    }
}

fn random_wait_delay(guidance_ms: Option<u64>, attempt: u32) -> Duration {
    let jitter_basis_points = rand::thread_rng().gen_range(0..=1_000);
    if let Some(guidance_ms) = guidance_ms {
        let base = Duration::from_millis(guidance_ms.clamp(1, MAX_SERVER_RETRY_DELAY_MS))
            .max(MIN_RETRY_DELAY);
        return apply_later_jitter(base, jitter_basis_points);
    }

    let base = Duration::from_millis(500u64.saturating_mul(1u64 << attempt.min(5)))
        .clamp(MIN_RETRY_DELAY, MAX_RETRY_DELAY);
    apply_later_jitter(base, jitter_basis_points).min(MAX_RETRY_DELAY)
}

fn apply_later_jitter(base: Duration, basis_points: u32) -> Duration {
    base.saturating_add(base.mul_f64(f64::from(basis_points.min(1_000)) / 10_000.0))
}

fn apply_earlier_jitter(base: Duration, basis_points: u32) -> Duration {
    base.saturating_sub(base.mul_f64(f64::from(basis_points.min(1_000)) / 10_000.0))
}

fn terminal_safe(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| {
            if character.is_control() {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect()
}

fn valid_agent_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    value.len() <= 64
        && first.is_ascii_alphanumeric()
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':')
        })
}

fn renewal_schedule(
    request_started: AuthorityInstant,
    ttl: Duration,
    guidance_ms: u64,
) -> Result<(AuthorityDeadline, AuthorityDeadline), i32> {
    let guidance = Duration::from_millis(guidance_ms.clamp(1, MAX_SERVER_RETRY_DELAY_MS))
        .max(MIN_RETRY_DELAY)
        .min(ttl / 2);
    let jitter_basis_points = rand::thread_rng().gen_range(0..=1_000);
    let renew_at =
        request_started.deadline_after(apply_earlier_jitter(guidance, jitter_basis_points))?;
    let safety_deadline = request_started.deadline_after(ttl.mul_f64(AUTHORITY_SAFETY_FRACTION))?;
    Ok((renew_at, safety_deadline))
}

fn validated_session_interval(seconds: u32) -> Result<Duration, ApiFailure> {
    if (1..=LOCK_SESSION_TTL_SECONDS).contains(&seconds) {
        Ok(Duration::from_secs(u64::from(seconds)))
    } else {
        Err(ApiFailure {
            status: None,
            code: "invalid_response".to_string(),
            details: format!(
                "server returned keepalive_interval_secs outside 1..={LOCK_SESSION_TTL_SECONDS}"
            ),
            retry_after_ms: None,
            request_id: None,
            transport: true,
        })
    }
}

fn acquisition_deadline(value: Option<&str>) -> Result<Option<Instant>, String> {
    value
        .map(parse_duration)
        .transpose()
        .map(|duration| duration.map(|duration| Instant::now() + duration))
}

fn validate_ttl(ttl: u32, min: u32, max: u32) -> Result<(), String> {
    if (min..=max).contains(&ttl) {
        Ok(())
    } else {
        Err(format!("TTL must be between {min} and {max} seconds"))
    }
}

fn signal_future() -> SignalFuture {
    Box::pin(async {
        #[cfg(unix)]
        {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler must install");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => Signal::Interrupt,
                _ = terminate.recv() => Signal::Terminate,
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c()
                .await
                .expect("interrupt handler must install");
            Signal::Interrupt
        }
    })
}

async fn with_signal<T, F>(future: F, signal: &mut SignalFuture) -> Result<T, Signal>
where
    F: Future<Output = T>,
{
    tokio::select! {
        result = future => Ok(result),
        received = signal.as_mut() => Err(received),
    }
}

enum DeadlineOutcome<T> {
    Completed(T),
    Deadline,
    Signal(Signal),
}

async fn with_signal_deadline<T, F>(
    future: F,
    deadline: Option<Instant>,
    signal: &mut SignalFuture,
) -> DeadlineOutcome<T>
where
    F: Future<Output = T>,
{
    match deadline {
        Some(deadline) => {
            match with_signal(tokio::time::timeout_at(deadline.into(), future), signal).await {
                Ok(Ok(value)) => DeadlineOutcome::Completed(value),
                Ok(Err(_)) => DeadlineOutcome::Deadline,
                Err(received) => DeadlineOutcome::Signal(received),
            }
        }
        None => match with_signal(future, signal).await {
            Ok(value) => DeadlineOutcome::Completed(value),
            Err(received) => DeadlineOutcome::Signal(received),
        },
    }
}

async fn with_signal_authority_deadline<T, F>(
    future: F,
    deadline: AuthorityDeadline,
    signal: &mut SignalFuture,
) -> DeadlineOutcome<T>
where
    F: Future<Output = T>,
{
    let mut future = Box::pin(future);
    loop {
        let remaining = match deadline.remaining() {
            Ok(remaining) => remaining,
            Err(_) => return DeadlineOutcome::Deadline,
        };
        tokio::select! {
            result = future.as_mut() => return DeadlineOutcome::Completed(result),
            received = signal.as_mut() => return DeadlineOutcome::Signal(received),
            _ = tokio::time::sleep(remaining.min(AUTHORITY_CLOCK_POLL)) => {}
        }
    }
}

async fn wait_until_authority(
    deadline: AuthorityDeadline,
    signal: &mut SignalFuture,
) -> Result<(), Signal> {
    loop {
        let Ok(remaining) = deadline.remaining() else {
            return Ok(());
        };
        with_signal(
            tokio::time::sleep(remaining.min(AUTHORITY_CLOCK_POLL)),
            signal,
        )
        .await?;
    }
}

fn session_confirmation_deadline(
    request_started: AuthorityInstant,
) -> Result<AuthorityDeadline, i32> {
    request_started.deadline_after(
        Duration::from_secs(u64::from(LOCK_SESSION_TTL_SECONDS)).mul_f64(AUTHORITY_SAFETY_FRACTION),
    )
}

fn shutdown_deadline(runtime: &RuntimeOptions, authority_deadline: AuthorityDeadline) -> Instant {
    let remaining = authority_deadline.remaining().unwrap_or_default();
    Instant::now()
        + runtime
            .shutdown_timeout
            .min(MAX_SHUTDOWN_BUDGET)
            .min(remaining)
}

async fn wait_with_signal(duration: Duration, signal: &mut SignalFuture) -> Result<(), Signal> {
    with_signal(tokio::time::sleep(duration), signal).await
}

fn signal_during_wait(signal: Signal) -> i32 {
    signal.exit_code()
}

async fn election_create(args: PublicReadArgs) -> i32 {
    let runtime = match runtime_options(
        args.server.as_deref(),
        &args.timeouts,
        false,
        false,
        false,
        args.json,
    ) {
        Ok(runtime) => runtime,
        Err(error) => return usage_error(error),
    };
    let response = parse_response::<CreateElectionResponse>(
        runtime
            .client
            .post(endpoint(&runtime.server, "/elections"))
            .send()
            .await,
        &[StatusCode::CREATED],
    )
    .await;
    match response {
        Ok(room) => {
            print_value(&room, runtime.json, || {
                println!("authority: {}", terminal_safe(&runtime.server));
                println!("election: {}", terminal_safe(&room.election_id));
                println!("give every candidate this same ID; do not create one room per candidate");
            });
            EXIT_OK
        }
        Err(error) => software_error(error.diagnostic()),
    }
}

async fn election_status(args: PublicTargetArgs) -> i32 {
    let runtime = match runtime_options(
        args.server.as_deref(),
        &args.timeouts,
        false,
        false,
        false,
        args.json,
    ) {
        Ok(runtime) => runtime,
        Err(error) => return usage_error(error),
    };
    match get_election_status(&runtime, &args.election_id).await {
        Ok(status) => {
            print_election_status(&status, &runtime);
            EXIT_OK
        }
        Err(error) => software_error(error.diagnostic()),
    }
}

async fn get_election_status(
    runtime: &RuntimeOptions,
    election_id: &str,
) -> Result<ElectionStatusResponse, ApiFailure> {
    parse_response(
        runtime
            .client
            .get(endpoint(
                &runtime.server,
                &format!("/elections/{}", encoded(election_id)),
            ))
            .send()
            .await,
        &[StatusCode::OK],
    )
    .await
}

fn print_election_status(status: &ElectionStatusResponse, runtime: &RuntimeOptions) {
    print_value(status, runtime.json, || match &status.leader {
        Some(leader) => println!(
            "{} · leader {} · term {} · expires {} · authority {}",
            terminal_safe(&status.election_id),
            terminal_safe(&leader.candidate_id),
            leader.term,
            leader.expires_at.to_rfc3339(),
            terminal_safe(&runtime.server)
        ),
        None => println!(
            "{} · vacant · authority {}",
            terminal_safe(&status.election_id),
            terminal_safe(&runtime.server)
        ),
    });
}

async fn election_hold(args: ElectionHoldArgs) -> i32 {
    if let Err(error) = validate_ttl(args.ttl, 5, 300) {
        return usage_error(error);
    }
    if !valid_agent_identifier(&args.candidate) {
        return usage_error(
            "candidate must be 1-64 ASCII letters, digits, '.', '_', ':', or '-', starting with a letter or digit",
        );
    }
    let deadline = match acquisition_deadline(args.acquire_timeout.as_deref()) {
        Ok(deadline) => deadline,
        Err(error) => return usage_error(error),
    };
    let runtime = match runtime_options(
        args.server.as_deref(),
        &args.timeouts,
        false,
        true,
        args.allow_insecure_http,
        args.json,
    ) {
        Ok(runtime) => runtime,
        Err(error) => return usage_error(error),
    };
    let mut emitter = Emitter::new(runtime.json);
    let mut signal = signal_future();
    let mut attempt = 0u32;

    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            emitter.emit(EventData {
                event: "error",
                kind: "election",
                name: &args.election_id,
                server: &runtime.server,
                candidate_id: Some(&args.candidate),
                term: None,
                expires_at: None,
                retry_after_ms: None,
                reason_code: Some("acquire_timeout"),
            });
            return EXIT_ACQUIRE_TIMEOUT;
        }
        let request_started = match AuthorityInstant::now() {
            Ok(started) => started,
            Err(code) => return code,
        };
        let authority_deadline = match request_started.deadline_after(
            Duration::from_secs(u64::from(args.ttl)).mul_f64(AUTHORITY_SAFETY_FRACTION),
        ) {
            Ok(deadline) => deadline,
            Err(code) => return code,
        };
        let request = async {
            parse_response::<CampaignResponse>(
                runtime
                    .client
                    .post(endpoint(
                        &runtime.server,
                        &format!("/elections/{}/campaign", encoded(&args.election_id)),
                    ))
                    .json(&json!({"candidate_id":args.candidate,"ttl_seconds":args.ttl}))
                    .send()
                    .await,
                &[StatusCode::OK],
            )
            .await
        };
        let request_deadline = match deadline {
            Some(acquisition_deadline) => {
                let acquisition_budget = AuthorityInstant::now().and_then(|now| {
                    now.deadline_after(
                        acquisition_deadline.saturating_duration_since(Instant::now()),
                    )
                });
                match acquisition_budget {
                    Ok(acquisition_budget) => authority_deadline.min(acquisition_budget),
                    Err(code) => return code,
                }
            }
            None => authority_deadline,
        };
        let response =
            match with_signal_authority_deadline(request, request_deadline, &mut signal).await {
                DeadlineOutcome::Completed(response) => response,
                DeadlineOutcome::Deadline => {
                    if deadline.is_some_and(|deadline| deadline <= authority_deadline.scheduler) {
                        emit_election_acquire_timeout(&mut emitter, &runtime, &args);
                        return EXIT_ACQUIRE_TIMEOUT;
                    }
                    emit_election_preacquire(
                        &mut emitter,
                        &runtime,
                        &args,
                        "uncertain",
                        "initial_authority_deadline_exceeded",
                    );
                    return EXIT_LOST;
                }
                DeadlineOutcome::Signal(signal) => return signal_during_wait(signal),
            };

        if cleanup_timed_out_election_response(
            &runtime,
            &response,
            deadline.is_some_and(|deadline| Instant::now() >= deadline),
        )
        .await
        {
            emit_election_acquire_timeout(&mut emitter, &runtime, &args);
            return EXIT_ACQUIRE_TIMEOUT;
        }

        match response {
            Ok(CampaignResponse::Leader {
                election_id,
                leader,
                leader_token,
                renew_after_ms,
            }) => {
                if authority_deadline.reached() {
                    cleanup_election_authority(&runtime, &election_id, &leader_token).await;
                    emit_election_preacquire(
                        &mut emitter,
                        &runtime,
                        &args,
                        "uncertain",
                        "initial_authority_deadline_exceeded",
                    );
                    return EXIT_LOST;
                }
                if election_id != args.election_id || leader.candidate_id != args.candidate {
                    cleanup_election_authority(&runtime, &election_id, &leader_token).await;
                    emit_election_preacquire(
                        &mut emitter,
                        &runtime,
                        &args,
                        "uncertain",
                        "authority_identity_mismatch",
                    );
                    return EXIT_LOST;
                }
                if let Err(exit_code) = emitter.emit_authority(
                    EventData {
                        event: "leader",
                        kind: "election",
                        name: &args.election_id,
                        server: &runtime.server,
                        candidate_id: Some(&args.candidate),
                        term: Some(leader.term),
                        expires_at: Some(leader.expires_at),
                        retry_after_ms: None,
                        reason_code: None,
                    },
                    authority_deadline,
                ) {
                    cleanup_election_authority(&runtime, &election_id, &leader_token).await;
                    emit_election_preacquire(
                        &mut emitter,
                        &runtime,
                        &args,
                        "uncertain",
                        "initial_authority_deadline_exceeded",
                    );
                    return exit_code;
                }
                return hold_election_lease(
                    &runtime,
                    &args,
                    leader_token,
                    leader.term,
                    leader.expires_at,
                    renew_after_ms,
                    request_started,
                    &mut signal,
                    &mut emitter,
                )
                .await;
            }
            Ok(CampaignResponse::Follower { retry_after_ms, .. }) => {
                let delay = random_wait_delay(Some(retry_after_ms), attempt);
                emitter.emit(EventData {
                    event: "waiting",
                    kind: "election",
                    name: &args.election_id,
                    server: &runtime.server,
                    candidate_id: Some(&args.candidate),
                    term: None,
                    expires_at: None,
                    retry_after_ms: Some(delay.as_millis() as u64),
                    reason_code: Some("follower"),
                });
                if let Some(code) = wait_pre_acquisition(delay, deadline, &mut signal).await {
                    return code;
                }
            }
            Err(error) if error.retryable_before_acquire() => {
                let delay = random_wait_delay(error.retry_after_ms, attempt);
                emitter.emit(EventData {
                    event: "waiting",
                    kind: "election",
                    name: &args.election_id,
                    server: &runtime.server,
                    candidate_id: Some(&args.candidate),
                    term: None,
                    expires_at: None,
                    retry_after_ms: Some(delay.as_millis() as u64),
                    reason_code: Some(&error.code),
                });
                if let Some(code) = wait_pre_acquisition(delay, deadline, &mut signal).await {
                    return code;
                }
            }
            Err(error) => {
                eprintln!("octostore: {}", terminal_safe(&error.diagnostic()));
                emitter.emit(EventData {
                    event: "error",
                    kind: "election",
                    name: &args.election_id,
                    server: &runtime.server,
                    candidate_id: Some(&args.candidate),
                    term: None,
                    expires_at: None,
                    retry_after_ms: None,
                    reason_code: Some(&error.code),
                });
                return if error.status.is_some_and(|status| status.is_client_error()) {
                    EXIT_USAGE
                } else {
                    EXIT_SOFTWARE
                };
            }
        }
        attempt = attempt.saturating_add(1);
    }
}

fn emit_election_acquire_timeout(
    emitter: &mut Emitter,
    runtime: &RuntimeOptions,
    args: &ElectionHoldArgs,
) {
    emitter.emit(EventData {
        event: "error",
        kind: "election",
        name: &args.election_id,
        server: &runtime.server,
        candidate_id: Some(&args.candidate),
        term: None,
        expires_at: None,
        retry_after_ms: None,
        reason_code: Some("acquire_timeout"),
    });
}

fn emit_election_preacquire(
    emitter: &mut Emitter,
    runtime: &RuntimeOptions,
    args: &ElectionHoldArgs,
    event: &str,
    reason: &str,
) {
    emitter.emit(EventData {
        event,
        kind: "election",
        name: &args.election_id,
        server: &runtime.server,
        candidate_id: Some(&args.candidate),
        term: None,
        expires_at: None,
        retry_after_ms: None,
        reason_code: Some(reason),
    });
}

async fn cleanup_election_authority(
    runtime: &RuntimeOptions,
    election_id: &str,
    leader_token: &str,
) {
    let deadline = Instant::now() + runtime.shutdown_timeout.min(MAX_SHUTDOWN_BUDGET);
    let request = runtime
        .client
        .post(endpoint(
            &runtime.server,
            &format!("/elections/{}/resign", encoded(election_id)),
        ))
        .json(&json!({"leader_token": leader_token}))
        .send();
    let _ = tokio::time::timeout_at(deadline.into(), request).await;
}

async fn cleanup_timed_out_election_response(
    runtime: &RuntimeOptions,
    response: &Result<CampaignResponse, ApiFailure>,
    acquisition_timed_out: bool,
) -> bool {
    if !acquisition_timed_out {
        return false;
    }
    if let Ok(CampaignResponse::Leader {
        election_id,
        leader_token,
        ..
    }) = response
    {
        cleanup_election_authority(runtime, election_id, leader_token).await;
    }
    true
}

#[allow(clippy::too_many_arguments)]
async fn hold_election_lease(
    runtime: &RuntimeOptions,
    args: &ElectionHoldArgs,
    leader_token: String,
    term: u64,
    mut expires_at: DateTime<Utc>,
    renew_after_ms: u64,
    request_started: AuthorityInstant,
    signal: &mut SignalFuture,
    emitter: &mut Emitter,
) -> i32 {
    let ttl = Duration::from_secs(u64::from(args.ttl));
    let mut schedule = match renewal_schedule(request_started, ttl, renew_after_ms) {
        Ok(schedule) => schedule,
        Err(code) => return code,
    };
    loop {
        if let Err(received) = wait_until_authority(schedule.0, signal).await {
            return shutdown_election(
                runtime,
                args,
                &leader_token,
                term,
                schedule.1,
                received,
                emitter,
            )
            .await;
        }

        let mut attempt = 0u32;
        loop {
            if schedule.1.reached() {
                emitter.emit(EventData {
                    event: "uncertain",
                    kind: "election",
                    name: &args.election_id,
                    server: &runtime.server,
                    candidate_id: Some(&args.candidate),
                    term: Some(term),
                    expires_at: Some(expires_at),
                    retry_after_ms: None,
                    reason_code: Some("renewal_deadline_exceeded"),
                });
                return EXIT_LOST;
            }
            let started = match AuthorityInstant::now() {
                Ok(started) => started,
                Err(code) => return code,
            };
            let request = async {
                parse_response::<CampaignResponse>(
                    runtime
                        .client
                        .post(endpoint(
                            &runtime.server,
                            &format!("/elections/{}/renew", encoded(&args.election_id)),
                        ))
                        .json(&json!({"leader_token":leader_token,"ttl_seconds":args.ttl}))
                        .send()
                        .await,
                    &[StatusCode::OK],
                )
                .await
            };
            let response = match with_signal_authority_deadline(request, schedule.1, signal).await {
                DeadlineOutcome::Signal(received) => {
                    return shutdown_election(
                        runtime,
                        args,
                        &leader_token,
                        term,
                        schedule.1,
                        received,
                        emitter,
                    )
                    .await;
                }
                DeadlineOutcome::Deadline => {
                    emitter.emit(EventData {
                        event: "uncertain",
                        kind: "election",
                        name: &args.election_id,
                        server: &runtime.server,
                        candidate_id: Some(&args.candidate),
                        term: Some(term),
                        expires_at: Some(expires_at),
                        retry_after_ms: None,
                        reason_code: Some("renewal_deadline_exceeded"),
                    });
                    return EXIT_LOST;
                }
                DeadlineOutcome::Completed(response) => {
                    if schedule.1.reached() {
                        emitter.emit(EventData {
                            event: "uncertain",
                            kind: "election",
                            name: &args.election_id,
                            server: &runtime.server,
                            candidate_id: Some(&args.candidate),
                            term: Some(term),
                            expires_at: Some(expires_at),
                            retry_after_ms: None,
                            reason_code: Some("renewal_deadline_exceeded"),
                        });
                        return EXIT_LOST;
                    }
                    response
                }
            };
            match response {
                Ok(CampaignResponse::Leader {
                    election_id,
                    leader,
                    renew_after_ms,
                    ..
                }) if election_id == args.election_id
                    && leader.candidate_id == args.candidate
                    && leader.term == term =>
                {
                    expires_at = leader.expires_at;
                    schedule = match renewal_schedule(started, ttl, renew_after_ms) {
                        Ok(schedule) => schedule,
                        Err(code) => return code,
                    };
                    if let Err(exit_code) = emitter.emit_authority(
                        EventData {
                            event: "renewed",
                            kind: "election",
                            name: &args.election_id,
                            server: &runtime.server,
                            candidate_id: Some(&args.candidate),
                            term: Some(term),
                            expires_at: Some(expires_at),
                            retry_after_ms: None,
                            reason_code: None,
                        },
                        schedule.1,
                    ) {
                        emitter.emit(EventData {
                            event: "uncertain",
                            kind: "election",
                            name: &args.election_id,
                            server: &runtime.server,
                            candidate_id: Some(&args.candidate),
                            term: Some(term),
                            expires_at: Some(expires_at),
                            retry_after_ms: None,
                            reason_code: Some("renewal_deadline_exceeded"),
                        });
                        return exit_code;
                    }
                    break;
                }
                Ok(_) => {
                    emitter.emit(EventData {
                        event: "lost",
                        kind: "election",
                        name: &args.election_id,
                        server: &runtime.server,
                        candidate_id: Some(&args.candidate),
                        term: Some(term),
                        expires_at: Some(expires_at),
                        retry_after_ms: None,
                        reason_code: Some("term_changed"),
                    });
                    return EXIT_LOST;
                }
                Err(error) if error.proves_loss() => {
                    emitter.emit(EventData {
                        event: "lost",
                        kind: "election",
                        name: &args.election_id,
                        server: &runtime.server,
                        candidate_id: Some(&args.candidate),
                        term: Some(term),
                        expires_at: Some(expires_at),
                        retry_after_ms: None,
                        reason_code: Some(&error.code),
                    });
                    return EXIT_LOST;
                }
                Err(_) => {
                    let delay = random_wait_delay(None, attempt)
                        .min(schedule.1.remaining().unwrap_or_default());
                    if let Err(received) = wait_with_signal(delay, signal).await {
                        return shutdown_election(
                            runtime,
                            args,
                            &leader_token,
                            term,
                            schedule.1,
                            received,
                            emitter,
                        )
                        .await;
                    }
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }
}

async fn shutdown_election(
    runtime: &RuntimeOptions,
    args: &ElectionHoldArgs,
    leader_token: &str,
    term: u64,
    authority_deadline: AuthorityDeadline,
    signal: Signal,
    emitter: &mut Emitter,
) -> i32 {
    let request = runtime
        .client
        .post(endpoint(
            &runtime.server,
            &format!("/elections/{}/resign", encoded(&args.election_id)),
        ))
        .json(&json!({"leader_token":leader_token}))
        .send();
    let cleanup_deadline = shutdown_deadline(runtime, authority_deadline);
    let released = tokio::time::timeout_at(cleanup_deadline.into(), async {
        parse_response::<ResignResponse>(request.await, &[StatusCode::OK]).await
    })
    .await;
    match released {
        Ok(Ok(response))
            if response.election_id == args.election_id
                && response.status == "vacant"
                && response.previous_term == term =>
        {
            emitter.emit(EventData {
                event: "released",
                kind: "election",
                name: &args.election_id,
                server: &runtime.server,
                candidate_id: Some(&args.candidate),
                term: Some(term),
                expires_at: None,
                retry_after_ms: None,
                reason_code: Some("signal"),
            })
        }
        _ => emitter.emit(EventData {
            event: "uncertain",
            kind: "election",
            name: &args.election_id,
            server: &runtime.server,
            candidate_id: Some(&args.candidate),
            term: Some(term),
            expires_at: None,
            retry_after_ms: None,
            reason_code: Some("release_unconfirmed"),
        }),
    }
    signal.exit_code()
}

async fn wait_pre_acquisition(
    delay: Duration,
    deadline: Option<Instant>,
    signal: &mut SignalFuture,
) -> Option<i32> {
    let delay = deadline
        .map(|deadline| delay.min(deadline.saturating_duration_since(Instant::now())))
        .unwrap_or(delay);
    wait_with_signal(delay, signal)
        .await
        .err()
        .map(Signal::exit_code)
}

async fn lock_status(args: LockTargetArgs) -> i32 {
    let runtime = match runtime_options(
        args.server.as_deref(),
        &args.timeouts,
        true,
        true,
        args.allow_insecure_http,
        args.json,
    ) {
        Ok(runtime) => runtime,
        Err(error) => return usage_error(error),
    };
    let token = match read_token() {
        Ok(token) => token,
        Err(error) => return usage_error(error),
    };
    match get_lock_status(&runtime, &args.name, &token).await {
        Ok(status) => {
            print_lock_status(&status, &runtime, &token);
            EXIT_OK
        }
        Err(error) => software_error(error.diagnostic_redacting(Some(&token))),
    }
}

async fn get_lock_status(
    runtime: &RuntimeOptions,
    name: &str,
    token: &str,
) -> Result<LockStatusResponse, ApiFailure> {
    parse_response(
        runtime
            .client
            .get(endpoint(
                &runtime.server,
                &format!("/locks/{}", encoded(name)),
            ))
            .bearer_auth(token)
            .send()
            .await,
        &[StatusCode::OK],
    )
    .await
}

fn print_lock_status(status: &LockStatusResponse, runtime: &RuntimeOptions, token: &str) {
    if runtime.json {
        let mut value = serde_json::to_value(status).expect("lock status must serialize");
        redact_json_strings(&mut value, Some(token));
        println!(
            "{}",
            serde_json::to_string(&value).expect("redacted lock status must serialize")
        );
    } else {
        println!(
            "{} · {} · term {} · authority {}",
            terminal_safe(&redact_text(&status.name, Some(token))),
            terminal_safe(&redact_text(&status.status, Some(token))),
            status.fencing_token,
            terminal_safe(&runtime.server)
        );
    }
}

fn read_token() -> Result<String, String> {
    if let Ok(path) = env::var("OCTOSTORE_TOKEN_FILE") {
        if path == "-" {
            let mut token = String::new();
            io::stdin()
                .read_to_string(&mut token)
                .map_err(|error| format!("could not read token from stdin: {error}"))?;
            return validate_token(token);
        }
        return validate_token(read_token_file(Path::new(&path))?);
    }
    if let Ok(token) = env::var("OCTOSTORE_TOKEN") {
        eprintln!(
            "octostore: warning: OCTOSTORE_TOKEN may be visible to same-user process inspection; prefer OCTOSTORE_TOKEN_FILE"
        );
        return validate_token(token);
    }
    Err("lock commands require OCTOSTORE_TOKEN_FILE or OCTOSTORE_TOKEN".to_string())
}

fn validate_token(token: String) -> Result<String, String> {
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("bearer token is empty".to_string());
    }
    if token.contains(['\r', '\n', '\0']) {
        return Err("bearer token must be one line".to_string());
    }
    Ok(token)
}

#[cfg(unix)]
fn read_token_file(path: &Path) -> Result<String, String> {
    use std::fs::OpenOptions;
    use std::os::unix::{fs::MetadataExt, fs::OpenOptionsExt, fs::PermissionsExt};

    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("could not open OCTOSTORE_TOKEN_FILE safely: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect OCTOSTORE_TOKEN_FILE: {error}"))?;
    if !metadata.is_file() {
        return Err("OCTOSTORE_TOKEN_FILE must be a regular file".to_string());
    }
    // SAFETY: geteuid has no preconditions and does not retain a pointer.
    let effective_user = unsafe { libc::geteuid() };
    if metadata.uid() != effective_user {
        return Err("OCTOSTORE_TOKEN_FILE must be owned by the current user".to_string());
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(
            "OCTOSTORE_TOKEN_FILE must have owner-only permissions (chmod 600)".to_string(),
        );
    }
    let mut token = String::new();
    file.read_to_string(&mut token)
        .map_err(|error| format!("could not read OCTOSTORE_TOKEN_FILE: {error}"))?;
    Ok(token)
}

#[cfg(not(unix))]
fn read_token_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("could not open OCTOSTORE_TOKEN_FILE: {error}"))?;
    if !file
        .metadata()
        .map_err(|error| format!("could not inspect OCTOSTORE_TOKEN_FILE: {error}"))?
        .is_file()
    {
        return Err("OCTOSTORE_TOKEN_FILE must be a regular file".to_string());
    }
    let mut token = String::new();
    file.read_to_string(&mut token)
        .map_err(|error| format!("could not read OCTOSTORE_TOKEN_FILE: {error}"))?;
    Ok(token)
}

async fn lock_hold(args: LockHoldArgs) -> i32 {
    if let Err(error) = validate_ttl(args.ttl, 1, 3_600) {
        return usage_error(error);
    }
    let deadline = match acquisition_deadline(args.acquire_timeout.as_deref()) {
        Ok(deadline) => deadline,
        Err(error) => return usage_error(error),
    };
    let runtime = match runtime_options(
        args.server.as_deref(),
        &args.timeouts,
        true,
        true,
        args.allow_insecure_http,
        args.json,
    ) {
        Ok(runtime) => runtime,
        Err(error) => return usage_error(error),
    };
    let token = match read_token() {
        Ok(token) => token,
        Err(error) => return usage_error(error),
    };
    let mut emitter = Emitter::new(runtime.json);
    let mut signal = signal_future();
    let mut session_attempt = 0u32;
    let (session, session_request_started) = loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            emit_lock_preacquire(&mut emitter, &runtime, &args, "error", "acquire_timeout");
            return EXIT_ACQUIRE_TIMEOUT;
        }
        let request_started = match AuthorityInstant::now() {
            Ok(started) => started,
            Err(code) => return code,
        };
        match with_signal_deadline(create_session(&runtime, &token), deadline, &mut signal).await {
            DeadlineOutcome::Completed(Ok(session)) => break (session, request_started),
            DeadlineOutcome::Completed(Err(error)) if error.retryable_before_acquire() => {
                let delay = random_wait_delay(error.retry_after_ms, session_attempt);
                emitter.emit(EventData {
                    event: "waiting",
                    kind: "lock",
                    name: &args.name,
                    server: &runtime.server,
                    candidate_id: None,
                    term: None,
                    expires_at: None,
                    retry_after_ms: Some(delay.as_millis() as u64),
                    reason_code: Some(&error.code),
                });
                if let Some(code) = wait_pre_acquisition(delay, deadline, &mut signal).await {
                    return code;
                }
                session_attempt = session_attempt.saturating_add(1);
            }
            DeadlineOutcome::Completed(Err(error)) => {
                return software_error(error.diagnostic_redacting(Some(&token)))
            }
            DeadlineOutcome::Deadline => {
                emit_lock_preacquire(&mut emitter, &runtime, &args, "error", "acquire_timeout");
                return EXIT_ACQUIRE_TIMEOUT;
            }
            DeadlineOutcome::Signal(received) => return received.exit_code(),
        }
    };
    let session_interval = match validated_session_interval(session.keepalive_interval_secs) {
        Ok(interval) => interval,
        Err(error) => {
            cleanup_session(&runtime, &token, session.session_id).await;
            return software_error(error.diagnostic_redacting(Some(&token)));
        }
    };
    let mut next_session_keepalive = match session_request_started.deadline_after(session_interval)
    {
        Ok(deadline) => deadline,
        Err(code) => return code,
    };
    let mut session_deadline = match session_confirmation_deadline(session_request_started) {
        Ok(deadline) => deadline,
        Err(code) => return code,
    };
    let mut attempt = 0u32;
    let mut next_acquire_at = Instant::now();

    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            cleanup_session(&runtime, &token, session.session_id).await;
            emit_lock_preacquire(&mut emitter, &runtime, &args, "error", "acquire_timeout");
            return EXIT_ACQUIRE_TIMEOUT;
        }
        if session_deadline.reached() {
            emit_lock_preacquire(
                &mut emitter,
                &runtime,
                &args,
                "uncertain",
                "session_confirmation_deadline_exceeded",
            );
            cleanup_session(&runtime, &token, session.session_id).await;
            return EXIT_LOST;
        }
        if next_session_keepalive.reached() {
            let keepalive_started = match AuthorityInstant::now() {
                Ok(started) => started,
                Err(code) => return code,
            };
            match with_signal_authority_deadline(
                keepalive_session(&runtime, &token, session.session_id),
                session_deadline,
                &mut signal,
            )
            .await
            {
                DeadlineOutcome::Completed(Ok(_)) => {
                    next_session_keepalive =
                        match keepalive_started.deadline_after(session_interval) {
                            Ok(deadline) => deadline,
                            Err(code) => return code,
                        };
                    session_deadline = match session_confirmation_deadline(keepalive_started) {
                        Ok(deadline) => deadline,
                        Err(code) => return code,
                    };
                    attempt = 0;
                }
                DeadlineOutcome::Completed(Err(error)) if error.retryable_before_acquire() => {
                    let delay = random_wait_delay(error.retry_after_ms, attempt)
                        .min(session_deadline.remaining().unwrap_or_default());
                    emitter.emit(EventData {
                        event: "waiting",
                        kind: "lock",
                        name: &args.name,
                        server: &runtime.server,
                        candidate_id: None,
                        term: None,
                        expires_at: None,
                        retry_after_ms: Some(delay.as_millis() as u64),
                        reason_code: Some(&error.code),
                    });
                    let retry_deadline = AuthorityInstant::now()
                        .and_then(|now| now.deadline_after(delay))
                        .map(|retry| retry.min(session_deadline));
                    match retry_deadline {
                        Ok(retry_deadline) => {
                            if let Err(received) =
                                wait_until_authority(retry_deadline, &mut signal).await
                            {
                                cleanup_session(&runtime, &token, session.session_id).await;
                                return received.exit_code();
                            }
                        }
                        Err(code) => {
                            cleanup_session(&runtime, &token, session.session_id).await;
                            return code;
                        }
                    }
                    attempt = attempt.saturating_add(1);
                    continue;
                }
                DeadlineOutcome::Completed(Err(error)) if error.proves_loss() => {
                    emit_lock_preacquire(&mut emitter, &runtime, &args, "uncertain", &error.code);
                    cleanup_session(&runtime, &token, session.session_id).await;
                    return EXIT_LOST;
                }
                DeadlineOutcome::Completed(Err(error)) => {
                    cleanup_session(&runtime, &token, session.session_id).await;
                    return software_error(error.diagnostic_redacting(Some(&token)));
                }
                DeadlineOutcome::Deadline => continue,
                DeadlineOutcome::Signal(received) => {
                    cleanup_session(&runtime, &token, session.session_id).await;
                    return received.exit_code();
                }
            }
        }
        if Instant::now() < next_acquire_at {
            let delay = next_acquire_at
                .saturating_duration_since(Instant::now())
                .min(next_session_keepalive.remaining().unwrap_or_default())
                .min(session_deadline.remaining().unwrap_or_default())
                .min(AUTHORITY_CLOCK_POLL);
            if let Err(received) = wait_with_signal(delay, &mut signal).await {
                cleanup_session(&runtime, &token, session.session_id).await;
                return received.exit_code();
            }
            continue;
        }
        let request_started = match AuthorityInstant::now() {
            Ok(started) => started,
            Err(code) => return code,
        };
        let lease_safety_deadline = match request_started.deadline_after(
            Duration::from_secs(u64::from(args.ttl)).mul_f64(AUTHORITY_SAFETY_FRACTION),
        ) {
            Ok(deadline) => deadline,
            Err(code) => return code,
        };
        let request = async {
            parse_response::<AcquireLockResponse>(
                runtime
                    .client
                    .post(endpoint(
                        &runtime.server,
                        &format!("/locks/{}/acquire", encoded(&args.name)),
                    ))
                    .bearer_auth(&token)
                    .json(&json!({
                        "ttl_seconds": args.ttl,
                        "session_id": session.session_id,
                        "ephemeral": true
                    }))
                    .send()
                    .await,
                &[StatusCode::OK, StatusCode::CONFLICT],
            )
            .await
        };
        let request_deadline = session_deadline
            .min(next_session_keepalive)
            .min(lease_safety_deadline);
        let request_deadline = match deadline {
            Some(acquisition_deadline) => {
                let acquisition_budget = AuthorityInstant::now().and_then(|now| {
                    now.deadline_after(
                        acquisition_deadline.saturating_duration_since(Instant::now()),
                    )
                });
                match acquisition_budget {
                    Ok(acquisition_budget) => request_deadline.min(acquisition_budget),
                    Err(code) => return code,
                }
            }
            None => request_deadline,
        };
        let response = match with_signal_authority_deadline(request, request_deadline, &mut signal)
            .await
        {
            DeadlineOutcome::Completed(response) => response,
            DeadlineOutcome::Deadline => {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    cleanup_session(&runtime, &token, session.session_id).await;
                    emit_lock_preacquire(&mut emitter, &runtime, &args, "error", "acquire_timeout");
                    return EXIT_ACQUIRE_TIMEOUT;
                }
                if lease_safety_deadline.reached() {
                    emit_lock_preacquire(
                        &mut emitter,
                        &runtime,
                        &args,
                        "uncertain",
                        "initial_authority_deadline_exceeded",
                    );
                    cleanup_session(&runtime, &token, session.session_id).await;
                    return EXIT_LOST;
                }
                continue;
            }
            DeadlineOutcome::Signal(received) => {
                cleanup_session(&runtime, &token, session.session_id).await;
                return received.exit_code();
            }
        };
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            cleanup_session(&runtime, &token, session.session_id).await;
            emit_lock_preacquire(&mut emitter, &runtime, &args, "error", "acquire_timeout");
            return EXIT_ACQUIRE_TIMEOUT;
        }
        if session_deadline.reached() {
            emit_lock_preacquire(
                &mut emitter,
                &runtime,
                &args,
                "uncertain",
                "session_confirmation_deadline_exceeded",
            );
            cleanup_session(&runtime, &token, session.session_id).await;
            return EXIT_LOST;
        }
        match response {
            Ok(AcquireLockResponse::Acquired {
                lease_id,
                fencing_token,
                expires_at,
                renew_after_ms,
                ..
            }) => {
                if lease_safety_deadline.reached() {
                    emit_lock_preacquire(
                        &mut emitter,
                        &runtime,
                        &args,
                        "uncertain",
                        "initial_authority_deadline_exceeded",
                    );
                    cleanup_session(&runtime, &token, session.session_id).await;
                    return EXIT_LOST;
                }
                let authority_deadline = lease_safety_deadline.min(session_deadline);
                if let Err(exit_code) = emitter.emit_authority(
                    EventData {
                        event: "acquired",
                        kind: "lock",
                        name: &args.name,
                        server: &runtime.server,
                        candidate_id: None,
                        term: Some(fencing_token),
                        expires_at: Some(expires_at),
                        retry_after_ms: None,
                        reason_code: None,
                    },
                    authority_deadline,
                ) {
                    emit_lock_preacquire(
                        &mut emitter,
                        &runtime,
                        &args,
                        "uncertain",
                        "initial_authority_deadline_exceeded",
                    );
                    cleanup_session(&runtime, &token, session.session_id).await;
                    return exit_code;
                }
                return hold_lock_lease(
                    &runtime,
                    &args,
                    &token,
                    session.session_id,
                    session_interval,
                    next_session_keepalive,
                    session_deadline,
                    lease_id,
                    fencing_token,
                    expires_at,
                    renew_after_ms,
                    request_started,
                    &mut signal,
                    &mut emitter,
                )
                .await;
            }
            Ok(AcquireLockResponse::Held { retry_after_ms, .. }) => {
                let delay = random_wait_delay(Some(retry_after_ms), attempt);
                next_acquire_at = Instant::now() + delay;
                emitter.emit(EventData {
                    event: "waiting",
                    kind: "lock",
                    name: &args.name,
                    server: &runtime.server,
                    candidate_id: None,
                    term: None,
                    expires_at: None,
                    retry_after_ms: Some(delay.as_millis() as u64),
                    reason_code: Some("held"),
                });
            }
            Ok(AcquireLockResponse::Delayed { retry_after_ms, .. }) => {
                let delay = random_wait_delay(Some(retry_after_ms), attempt);
                next_acquire_at = Instant::now() + delay;
                emitter.emit(EventData {
                    event: "waiting",
                    kind: "lock",
                    name: &args.name,
                    server: &runtime.server,
                    candidate_id: None,
                    term: None,
                    expires_at: None,
                    retry_after_ms: Some(delay.as_millis() as u64),
                    reason_code: Some("delayed"),
                });
            }
            Err(error) if error.retryable_before_acquire() => {
                let delay = random_wait_delay(error.retry_after_ms, attempt);
                next_acquire_at = Instant::now() + delay;
                emitter.emit(EventData {
                    event: "waiting",
                    kind: "lock",
                    name: &args.name,
                    server: &runtime.server,
                    candidate_id: None,
                    term: None,
                    expires_at: None,
                    retry_after_ms: Some(delay.as_millis() as u64),
                    reason_code: Some(&error.code),
                });
            }
            Err(error) => {
                cleanup_session(&runtime, &token, session.session_id).await;
                eprintln!(
                    "octostore: {}",
                    terminal_safe(&error.diagnostic_redacting(Some(&token)))
                );
                emitter.emit(EventData {
                    event: "error",
                    kind: "lock",
                    name: &args.name,
                    server: &runtime.server,
                    candidate_id: None,
                    term: None,
                    expires_at: None,
                    retry_after_ms: None,
                    reason_code: Some(&error.code),
                });
                return if error.status.is_some_and(|status| status.is_client_error()) {
                    EXIT_USAGE
                } else {
                    EXIT_SOFTWARE
                };
            }
        }
        attempt = attempt.saturating_add(1);
    }
}

fn emit_lock_preacquire(
    emitter: &mut Emitter,
    runtime: &RuntimeOptions,
    args: &LockHoldArgs,
    event: &str,
    reason: &str,
) {
    emitter.emit(EventData {
        event,
        kind: "lock",
        name: &args.name,
        server: &runtime.server,
        candidate_id: None,
        term: None,
        expires_at: None,
        retry_after_ms: None,
        reason_code: Some(reason),
    });
}

#[allow(clippy::too_many_arguments)]
async fn hold_lock_lease(
    runtime: &RuntimeOptions,
    args: &LockHoldArgs,
    token: &str,
    session_id: Uuid,
    session_interval: Duration,
    mut next_session_keepalive: AuthorityDeadline,
    mut session_deadline: AuthorityDeadline,
    lease_id: Uuid,
    term: u64,
    mut expires_at: DateTime<Utc>,
    renew_after_ms: u64,
    request_started: AuthorityInstant,
    signal: &mut SignalFuture,
    emitter: &mut Emitter,
) -> i32 {
    let ttl = Duration::from_secs(u64::from(args.ttl));
    let mut schedule = match renewal_schedule(request_started, ttl, renew_after_ms) {
        Ok(schedule) => schedule,
        Err(code) => return code,
    };
    loop {
        let next_action = schedule.0.min(next_session_keepalive);
        let authority_deadline = schedule.1.min(session_deadline);
        if let Err(received) = wait_until_authority(next_action, signal).await {
            return shutdown_lock(
                runtime,
                args,
                token,
                session_id,
                lease_id,
                term,
                authority_deadline,
                received,
                emitter,
            )
            .await;
        }

        let mut session_confirmed = !next_session_keepalive.reached();
        let mut lease_confirmed = !schedule.0.reached();
        let mut attempt = 0u32;
        loop {
            let authority_deadline = schedule.1.min(session_deadline);
            if authority_deadline.reached() {
                let reason = if session_deadline.no_later_than(schedule.1) {
                    "session_confirmation_deadline_exceeded"
                } else {
                    "renewal_deadline_exceeded"
                };
                emit_lock_loss(
                    emitter,
                    runtime,
                    args,
                    term,
                    expires_at,
                    "uncertain",
                    reason,
                );
                cleanup_session(runtime, token, session_id).await;
                return EXIT_LOST;
            }
            if !session_confirmed {
                let started = match AuthorityInstant::now() {
                    Ok(started) => started,
                    Err(code) => return code,
                };
                match with_signal_authority_deadline(
                    keepalive_session(runtime, token, session_id),
                    authority_deadline,
                    signal,
                )
                .await
                {
                    DeadlineOutcome::Signal(received) => {
                        return shutdown_lock(
                            runtime,
                            args,
                            token,
                            session_id,
                            lease_id,
                            term,
                            authority_deadline,
                            received,
                            emitter,
                        )
                        .await;
                    }
                    DeadlineOutcome::Completed(Ok(_)) => {
                        session_confirmed = true;
                        next_session_keepalive = match started.deadline_after(session_interval) {
                            Ok(deadline) => deadline,
                            Err(code) => return code,
                        };
                        session_deadline = match session_confirmation_deadline(started) {
                            Ok(deadline) => deadline,
                            Err(code) => return code,
                        };
                    }
                    DeadlineOutcome::Completed(Err(error)) if error.proves_loss() => {
                        emit_lock_loss(
                            emitter,
                            runtime,
                            args,
                            term,
                            expires_at,
                            "lost",
                            &error.code,
                        );
                        cleanup_session(runtime, token, session_id).await;
                        return EXIT_LOST;
                    }
                    DeadlineOutcome::Deadline | DeadlineOutcome::Completed(Err(_)) => {}
                }
            }
            if !lease_confirmed {
                let started = match AuthorityInstant::now() {
                    Ok(started) => started,
                    Err(code) => return code,
                };
                let request = async {
                    parse_response::<RenewLockResponse>(
                        runtime
                            .client
                            .post(endpoint(
                                &runtime.server,
                                &format!("/locks/{}/renew", encoded(&args.name)),
                            ))
                            .bearer_auth(token)
                            .json(&json!({"lease_id":lease_id,"ttl_seconds":args.ttl}))
                            .send()
                            .await,
                        &[StatusCode::OK],
                    )
                    .await
                };
                match with_signal_authority_deadline(request, authority_deadline, signal).await {
                    DeadlineOutcome::Signal(received) => {
                        return shutdown_lock(
                            runtime,
                            args,
                            token,
                            session_id,
                            lease_id,
                            term,
                            authority_deadline,
                            received,
                            emitter,
                        )
                        .await;
                    }
                    DeadlineOutcome::Completed(Ok(renewed)) if renewed.lease_id == lease_id => {
                        lease_confirmed = true;
                        expires_at = renewed.expires_at;
                        schedule = match renewal_schedule(started, ttl, renewed.renew_after_ms) {
                            Ok(schedule) => schedule,
                            Err(code) => return code,
                        };
                    }
                    DeadlineOutcome::Completed(Ok(_)) => {
                        emit_lock_loss(
                            emitter,
                            runtime,
                            args,
                            term,
                            expires_at,
                            "lost",
                            "lease_changed",
                        );
                        cleanup_session(runtime, token, session_id).await;
                        return EXIT_LOST;
                    }
                    DeadlineOutcome::Completed(Err(error)) if error.proves_loss() => {
                        emit_lock_loss(
                            emitter,
                            runtime,
                            args,
                            term,
                            expires_at,
                            "lost",
                            &error.code,
                        );
                        cleanup_session(runtime, token, session_id).await;
                        return EXIT_LOST;
                    }
                    DeadlineOutcome::Deadline | DeadlineOutcome::Completed(Err(_)) => {}
                }
            }
            if session_confirmed && lease_confirmed {
                let authority_deadline = schedule.1.min(session_deadline);
                if let Err(exit_code) = emitter.emit_authority(
                    EventData {
                        event: "renewed",
                        kind: "lock",
                        name: &args.name,
                        server: &runtime.server,
                        candidate_id: None,
                        term: Some(term),
                        expires_at: Some(expires_at),
                        retry_after_ms: None,
                        reason_code: None,
                    },
                    authority_deadline,
                ) {
                    let reason = if session_deadline.no_later_than(schedule.1) {
                        "session_confirmation_deadline_exceeded"
                    } else {
                        "renewal_deadline_exceeded"
                    };
                    emit_lock_loss(
                        emitter,
                        runtime,
                        args,
                        term,
                        expires_at,
                        "uncertain",
                        reason,
                    );
                    cleanup_session(runtime, token, session_id).await;
                    return exit_code;
                }
                break;
            }
            let authority_deadline = schedule.1.min(session_deadline);
            let delay = random_wait_delay(None, attempt)
                .min(authority_deadline.remaining().unwrap_or_default());
            if let Err(received) = wait_with_signal(delay, signal).await {
                return shutdown_lock(
                    runtime,
                    args,
                    token,
                    session_id,
                    lease_id,
                    term,
                    authority_deadline,
                    received,
                    emitter,
                )
                .await;
            }
            attempt = attempt.saturating_add(1);
        }
    }
}

fn emit_lock_loss(
    emitter: &mut Emitter,
    runtime: &RuntimeOptions,
    args: &LockHoldArgs,
    term: u64,
    expires_at: DateTime<Utc>,
    event: &str,
    reason: &str,
) {
    emitter.emit(EventData {
        event,
        kind: "lock",
        name: &args.name,
        server: &runtime.server,
        candidate_id: None,
        term: Some(term),
        expires_at: Some(expires_at),
        retry_after_ms: None,
        reason_code: Some(reason),
    });
}

async fn create_session(
    runtime: &RuntimeOptions,
    token: &str,
) -> Result<CreateSessionResponse, ApiFailure> {
    parse_response(
        runtime
            .client
            .post(endpoint(&runtime.server, "/sessions"))
            .bearer_auth(token)
            .json(&json!({"ttl_seconds":LOCK_SESSION_TTL_SECONDS}))
            .send()
            .await,
        &[StatusCode::CREATED],
    )
    .await
}

async fn keepalive_session(
    runtime: &RuntimeOptions,
    token: &str,
    session_id: Uuid,
) -> Result<KeepAliveResponse, ApiFailure> {
    let response: KeepAliveResponse = parse_response(
        runtime
            .client
            .post(endpoint(
                &runtime.server,
                &format!("/sessions/{session_id}/keepalive"),
            ))
            .bearer_auth(token)
            .send()
            .await,
        &[StatusCode::OK],
    )
    .await?;
    if response.session_id != session_id {
        return Err(ApiFailure {
            status: Some(StatusCode::OK),
            code: "session_identity_mismatch".to_string(),
            details: "server returned a keepalive response for a different session".to_string(),
            retry_after_ms: None,
            request_id: None,
            transport: false,
        });
    }
    Ok(response)
}

async fn cleanup_session(runtime: &RuntimeOptions, token: &str, session_id: Uuid) {
    cleanup_session_until(
        runtime,
        token,
        session_id,
        Instant::now() + runtime.shutdown_timeout,
    )
    .await;
}

async fn cleanup_session_until(
    runtime: &RuntimeOptions,
    token: &str,
    session_id: Uuid,
    deadline: Instant,
) {
    let _ = tokio::time::timeout_at(
        deadline.into(),
        runtime
            .client
            .delete(endpoint(
                &runtime.server,
                &format!("/sessions/{session_id}"),
            ))
            .bearer_auth(token)
            .send(),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn shutdown_lock(
    runtime: &RuntimeOptions,
    args: &LockHoldArgs,
    token: &str,
    session_id: Uuid,
    lease_id: Uuid,
    term: u64,
    authority_deadline: AuthorityDeadline,
    signal: Signal,
    emitter: &mut Emitter,
) -> i32 {
    let release = runtime
        .client
        .post(endpoint(
            &runtime.server,
            &format!("/locks/{}/release", encoded(&args.name)),
        ))
        .bearer_auth(token)
        .json(&json!({"lease_id":lease_id}))
        .send();
    let cleanup_deadline = shutdown_deadline(runtime, authority_deadline);
    let released = tokio::time::timeout_at(cleanup_deadline.into(), async {
        parse_response::<()>(release.await, &[StatusCode::OK]).await
    })
    .await;
    cleanup_session_until(runtime, token, session_id, cleanup_deadline).await;
    match released {
        Ok(Ok(())) => emitter.emit(EventData {
            event: "released",
            kind: "lock",
            name: &args.name,
            server: &runtime.server,
            candidate_id: None,
            term: Some(term),
            expires_at: None,
            retry_after_ms: None,
            reason_code: Some("signal"),
        }),
        _ => emitter.emit(EventData {
            event: "uncertain",
            kind: "lock",
            name: &args.name,
            server: &runtime.server,
            candidate_id: None,
            term: Some(term),
            expires_at: None,
            retry_after_ms: None,
            reason_code: Some("release_unconfirmed"),
        }),
    }
    signal.exit_code()
}

async fn election_watch(args: PublicTargetArgs) -> i32 {
    let runtime = match runtime_options(
        args.server.as_deref(),
        &args.timeouts,
        false,
        false,
        false,
        args.json,
    ) {
        Ok(runtime) => runtime,
        Err(error) => return usage_error(error),
    };
    watch_status_loop(
        &runtime,
        "election",
        &args.election_id,
        None,
        format!("/elections/{}/watch", encoded(&args.election_id)),
    )
    .await
}

async fn lock_watch(args: LockTargetArgs) -> i32 {
    let runtime = match runtime_options(
        args.server.as_deref(),
        &args.timeouts,
        true,
        true,
        args.allow_insecure_http,
        args.json,
    ) {
        Ok(runtime) => runtime,
        Err(error) => return usage_error(error),
    };
    let token = match read_token() {
        Ok(token) => token,
        Err(error) => return usage_error(error),
    };
    watch_status_loop(
        &runtime,
        "lock",
        &args.name,
        Some(token),
        format!("/locks/{}/watch", encoded(&args.name)),
    )
    .await
}

async fn watch_status_loop(
    runtime: &RuntimeOptions,
    kind: &str,
    name: &str,
    token: Option<String>,
    watch_path: String,
) -> i32 {
    let mut signal = signal_future();
    let mut sequence = 0u64;
    let mut reconnect_attempt = 0u32;
    loop {
        let status = match with_signal_deadline(
            watch_status(runtime, kind, name, token.as_deref()),
            Some(Instant::now() + runtime.request_timeout),
            &mut signal,
        )
        .await
        {
            DeadlineOutcome::Completed(status) => status,
            DeadlineOutcome::Deadline => Err(ApiFailure {
                status: None,
                code: "transport_error".to_string(),
                details: "watch reconciliation timed out".to_string(),
                retry_after_ms: None,
                request_id: None,
                transport: true,
            }),
            DeadlineOutcome::Signal(received) => return received.exit_code(),
        };
        match status {
            Ok(value) => {
                print_watch_value(runtime, kind, name, &value, token.as_deref(), &mut sequence)
            }
            Err(error) if error.retryable_before_acquire() => eprintln!(
                "octostore: watch reconcile failed: {}",
                terminal_safe(&error.diagnostic_redacting(token.as_deref()))
            ),
            Err(error) => return software_error(error.diagnostic_redacting(token.as_deref())),
        }

        let mut request = runtime
            .client
            .get(endpoint(&runtime.server, &watch_path))
            .timeout(Duration::from_secs(24 * 60 * 60))
            .header("accept", "text/event-stream");
        if let Some(token) = &token {
            request = request.bearer_auth(token);
        }
        let response = match with_signal_deadline(
            request.send(),
            Some(Instant::now() + runtime.request_timeout),
            &mut signal,
        )
        .await
        {
            DeadlineOutcome::Signal(received) => return received.exit_code(),
            DeadlineOutcome::Deadline => {
                eprintln!("octostore: watch connection timed out before response headers");
                let delay = random_wait_delay(None, reconnect_attempt);
                reconnect_attempt = reconnect_attempt.saturating_add(1);
                if let Err(received) = wait_with_signal(delay, &mut signal).await {
                    return received.exit_code();
                }
                continue;
            }
            DeadlineOutcome::Completed(Err(error)) => {
                eprintln!(
                    "octostore: watch connection failed: {}",
                    terminal_safe(&transport_failure(error).diagnostic())
                );
                let delay = random_wait_delay(None, reconnect_attempt);
                reconnect_attempt = reconnect_attempt.saturating_add(1);
                if let Err(received) = wait_with_signal(delay, &mut signal).await {
                    return received.exit_code();
                }
                continue;
            }
            DeadlineOutcome::Completed(Ok(response)) if response.status() != StatusCode::OK => {
                let error = match with_signal_deadline(
                    parse_response::<serde_json::Value>(Ok(response), &[StatusCode::OK]),
                    Some(Instant::now() + runtime.request_timeout),
                    &mut signal,
                )
                .await
                {
                    DeadlineOutcome::Completed(result) => result.unwrap_err(),
                    DeadlineOutcome::Deadline => ApiFailure {
                        status: None,
                        code: "transport_error".to_string(),
                        details: "watch error response body timed out".to_string(),
                        retry_after_ms: None,
                        request_id: None,
                        transport: true,
                    },
                    DeadlineOutcome::Signal(received) => return received.exit_code(),
                };
                if !error.retryable_before_acquire() {
                    return software_error(error.diagnostic_redacting(token.as_deref()));
                }
                eprintln!(
                    "octostore: watch unavailable: {}",
                    terminal_safe(&error.diagnostic_redacting(token.as_deref()))
                );
                let delay = random_wait_delay(error.retry_after_ms, reconnect_attempt);
                reconnect_attempt = reconnect_attempt.saturating_add(1);
                if let Err(received) = wait_with_signal(delay, &mut signal).await {
                    return received.exit_code();
                }
                continue;
            }
            DeadlineOutcome::Completed(Ok(response)) => response,
        };

        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        let mut reconnect_guidance_ms = None;
        loop {
            let chunk = match with_signal(stream.next(), &mut signal).await {
                Err(received) => return received.exit_code(),
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(_))) | Ok(None) => break,
            };
            let mut chunk_offset = 0;
            while chunk_offset < chunk.len() {
                let copied = match append_sse_bytes(&mut buffer, &chunk[chunk_offset..]) {
                    Ok(copied) => copied,
                    Err(()) => {
                        return software_error(format!(
                            "watch SSE frame exceeded {MAX_SSE_FRAME_BYTES} bytes"
                        ));
                    }
                };
                chunk_offset += copied;

                while let Some(end) = find_sse_frame(&buffer) {
                    debug_assert!(end <= MAX_SSE_FRAME_BYTES);
                    let frame = buffer.drain(..end).collect::<Vec<_>>();
                    if let Some(retry_ms) = sse_retry_ms(&frame) {
                        reconnect_guidance_ms = Some(retry_ms);
                    }
                    if frame
                        .split(|byte| *byte == b'\n')
                        .any(|line| line.starts_with(b"data:"))
                    {
                        let status = match with_signal_deadline(
                            watch_status(runtime, kind, name, token.as_deref()),
                            Some(Instant::now() + runtime.request_timeout),
                            &mut signal,
                        )
                        .await
                        {
                            DeadlineOutcome::Completed(status) => status,
                            DeadlineOutcome::Deadline => Err(ApiFailure {
                                status: None,
                                code: "transport_error".to_string(),
                                details: "watch reconciliation timed out".to_string(),
                                retry_after_ms: None,
                                request_id: None,
                                transport: true,
                            }),
                            DeadlineOutcome::Signal(received) => return received.exit_code(),
                        };
                        match status {
                            Ok(value) => {
                                print_watch_value(
                                    runtime,
                                    kind,
                                    name,
                                    &value,
                                    token.as_deref(),
                                    &mut sequence,
                                );
                                reconnect_attempt = 0;
                            }
                            Err(error) => eprintln!(
                                "octostore: watch reconcile failed: {}",
                                terminal_safe(&error.diagnostic_redacting(token.as_deref()))
                            ),
                        }
                    }
                }
            }
        }
        let delay = random_wait_delay(reconnect_guidance_ms, reconnect_attempt);
        reconnect_attempt = reconnect_attempt.saturating_add(1);
        if let Err(received) = wait_with_signal(delay, &mut signal).await {
            return received.exit_code();
        }
    }
}

async fn watch_status(
    runtime: &RuntimeOptions,
    kind: &str,
    name: &str,
    token: Option<&str>,
) -> Result<serde_json::Value, ApiFailure> {
    if kind == "election" {
        get_election_status(runtime, name)
            .await
            .and_then(|value| serde_json::to_value(value).map_err(json_failure))
    } else {
        get_lock_status(runtime, name, token.unwrap_or_default())
            .await
            .and_then(|value| serde_json::to_value(value).map_err(json_failure))
    }
}

fn json_failure(error: serde_json::Error) -> ApiFailure {
    ApiFailure {
        status: None,
        code: "invalid_response".to_string(),
        details: format!("could not serialize status: {error}"),
        retry_after_ms: None,
        request_id: None,
        transport: true,
    }
}

fn print_watch_value(
    runtime: &RuntimeOptions,
    kind: &str,
    name: &str,
    value: &serde_json::Value,
    secret: Option<&str>,
    sequence: &mut u64,
) {
    *sequence = sequence.saturating_add(1);
    if runtime.json {
        let mut snapshot = value.clone();
        redact_json_strings(&mut snapshot, secret);
        let event = WatchSnapshot {
            schema_version: 1,
            sequence: *sequence,
            event: "snapshot",
            kind,
            name,
            server: &runtime.server,
            snapshot: &snapshot,
            observed_at: Utc::now(),
        };
        println!(
            "{}",
            serde_json::to_string(&event).expect("watch snapshot must serialize")
        );
    } else {
        let status = value["status"].as_str().unwrap_or("unknown");
        println!(
            "{kind} {} · {} · authority {}",
            terminal_safe(&redact_text(name, secret)),
            terminal_safe(&redact_text(status, secret)),
            terminal_safe(&runtime.server)
        );
    }
}

fn sse_retry_ms(frame: &[u8]) -> Option<u64> {
    frame.split(|byte| *byte == b'\n').find_map(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let value = line.strip_prefix(b"retry:")?;
        std::str::from_utf8(value).ok()?.trim().parse().ok()
    })
}

fn find_sse_frame(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|position| position + 2)
        .or_else(|| {
            buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|position| position + 4)
        })
}

fn append_sse_bytes(buffer: &mut Vec<u8>, bytes: &[u8]) -> Result<usize, ()> {
    if bytes.is_empty() {
        return Ok(0);
    }

    let mut suffix = buffer[buffer.len().saturating_sub(3)..].to_vec();
    let frame_end = bytes.iter().enumerate().find_map(|(index, byte)| {
        suffix.push(*byte);
        if suffix.len() > 4 {
            suffix.remove(0);
        }
        if suffix.ends_with(b"\n\n") || suffix.ends_with(b"\r\n\r\n") {
            Some(buffer.len() + index + 1)
        } else {
            None
        }
    });

    let append_len = if let Some(frame_end) = frame_end {
        if frame_end > MAX_SSE_FRAME_BYTES {
            return Err(());
        }
        frame_end - buffer.len()
    } else {
        let pending_len = buffer.len().checked_add(bytes.len()).ok_or(())?;
        if pending_len > MAX_SSE_FRAME_BYTES {
            return Err(());
        }
        bytes.len()
    };
    if append_len == 0 {
        return Err(());
    }
    buffer.extend_from_slice(&bytes[..append_len]);
    Ok(append_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode as AxumStatusCode, routing::post, Router};
    use clap::CommandFactory;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tempfile::NamedTempFile;

    #[test]
    fn cli_contract_parses_all_leaf_commands_and_bare_server() {
        Cli::command().debug_assert();
        assert!(Cli::try_parse_from(["octostore"])
            .unwrap()
            .command
            .is_none());
        assert!(matches!(
            Cli::try_parse_from(["octostore", "serve"]).unwrap().command,
            Some(Command::Serve)
        ));
        for arguments in [
            vec!["octostore", "election", "create"],
            vec![
                "octostore",
                "election",
                "hold",
                "shared-room",
                "--candidate",
                "agent-a",
            ],
            vec!["octostore", "election", "status", "shared-room"],
            vec!["octostore", "election", "watch", "shared-room"],
            vec!["octostore", "lock", "hold", "repo/issue-1"],
            vec!["octostore", "lock", "status", "repo/issue-1"],
            vec!["octostore", "lock", "watch", "repo/issue-1"],
        ] {
            assert!(Cli::try_parse_from(arguments).is_ok());
        }
    }

    #[test]
    fn duration_parser_is_strict_and_bounded() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("3m").unwrap(), Duration::from_secs(180));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3_600));
        assert_eq!(parse_duration("60").unwrap(), Duration::from_secs(60));
        assert!(parse_duration("1.5s").is_err());
        assert!(parse_duration("25h").is_err());
        assert!(parse_duration("18446744073709551615h").is_err());
    }

    #[test]
    fn terminal_output_escapes_control_characters() {
        assert_eq!(terminal_safe("agent\n\u{1b}[31m"), "agent\\n\\u{1b}[31m");
        assert!(valid_agent_identifier("agent-atlas:1"));
        assert!(!valid_agent_identifier("agent\nleader"));
        assert!(!valid_agent_identifier("-agent"));
    }

    #[test]
    fn timing_jitter_never_violates_its_direction_or_ten_percent_bound() {
        let base = Duration::from_secs(10);
        assert_eq!(apply_later_jitter(base, 0), base);
        assert_eq!(apply_later_jitter(base, 1_000), Duration::from_secs(11));
        assert_eq!(apply_earlier_jitter(base, 0), base);
        assert_eq!(apply_earlier_jitter(base, 1_000), Duration::from_secs(9));
        assert_eq!(apply_later_jitter(base, 9_999), Duration::from_secs(11));
        assert_eq!(apply_earlier_jitter(base, 9_999), Duration::from_secs(9));
    }

    #[test]
    fn sub_millisecond_authority_budget_fails_closed_with_lost_exit() {
        let scheduler = Instant::now();
        assert_eq!(
            authority_remaining_ms(
                AuthorityDeadline {
                    scheduler: scheduler + Duration::from_micros(999),
                    continuous_ms: 10_001,
                },
                scheduler,
                10_000,
            ),
            Err(EXIT_LOST)
        );
        assert_eq!(
            authority_remaining_ms(
                AuthorityDeadline {
                    scheduler: scheduler + Duration::from_millis(1),
                    continuous_ms: 10_001,
                },
                scheduler,
                10_000,
            ),
            Ok(1)
        );
    }

    #[test]
    fn suspend_inclusive_clock_expires_authority_when_scheduler_clock_lags() {
        let scheduler = Instant::now();
        let deadline = AuthorityDeadline {
            scheduler: scheduler + Duration::from_secs(30),
            continuous_ms: 40_000,
        };

        assert_eq!(
            deadline.remaining_at(scheduler + Duration::from_secs(1), 40_000),
            Err(EXIT_LOST),
            "authority must expire after suspend-inclusive time reaches the deadline"
        );
    }

    #[test]
    fn authority_observation_timestamps_before_sampling_the_remaining_budget() {
        use std::cell::Cell;

        let origin = Instant::now();
        let deadline = AuthorityDeadline {
            scheduler: origin + Duration::from_secs(3),
            continuous_ms: 13_000,
        };
        let stage = Cell::new(0);
        let emitted_at = Utc::now();
        let (remaining_ms, observed_at, observed_continuous_ms) = authority_observation_with(
            deadline,
            || {
                assert_eq!(stage.replace(1), 0, "wall clock must be sampled first");
                emitted_at
            },
            || {
                assert_eq!(
                    stage.replace(2),
                    1,
                    "transferable continuous clock must be sampled second"
                );
                Ok(42_000)
            },
            || {
                assert_eq!(
                    stage.replace(3),
                    2,
                    "continuous budget must be sampled last"
                );
                origin + Duration::from_secs(1)
            },
            || Ok(11_000),
        )
        .unwrap();

        assert_eq!(stage.get(), 3);
        assert_eq!(observed_at, emitted_at);
        assert_eq!(observed_continuous_ms, 42_000);
        assert_eq!(remaining_ms, 2_000);
    }

    #[test]
    fn server_retry_guidance_is_never_shortened_by_local_backoff_cap() {
        let guidance = Duration::from_secs(42);
        for _ in 0..100 {
            let delay = random_wait_delay(Some(42_000), 99);
            assert!(delay >= guidance);
            assert!(delay <= guidance.mul_f64(1.1));
        }
        assert!(random_wait_delay(Some(0), 0) >= MIN_RETRY_DELAY);
        assert!(
            random_wait_delay(Some(u64::MAX), 0)
                <= Duration::from_millis(MAX_SERVER_RETRY_DELAY_MS).mul_f64(1.1)
        );
        assert!(random_wait_delay(None, 99) <= MAX_RETRY_DELAY);
    }

    #[test]
    fn session_confirmation_has_an_independent_continuous_deadline() {
        let started = AuthorityInstant {
            scheduler: Instant::now(),
            continuous_ms: 10_000,
        };
        let confirmation = session_confirmation_deadline(started).unwrap();
        assert_eq!(
            confirmation.scheduler.duration_since(started.scheduler),
            Duration::from_secs(24)
        );
        assert_eq!(confirmation.continuous_ms - started.continuous_ms, 24_000);

        let lease = started.deadline_after(Duration::from_secs(2_880)).unwrap();
        let effective = lease.min(confirmation);
        assert_eq!(effective.scheduler, confirmation.scheduler);
        assert_eq!(effective.continuous_ms, confirmation.continuous_ms);
    }

    #[test]
    fn cleartext_credentials_are_limited_to_loopback_without_override() {
        assert!(validate_server("http://127.0.0.1:3000", true, false).is_ok());
        assert!(validate_server("http://[::1]:3000", true, false).is_ok());
        assert!(validate_server("http://localhost:3000", true, false).is_ok());
        assert!(validate_server("http://example.test:3000", true, false).is_err());
        assert!(validate_server("http://example.test:3000", true, true).is_ok());
        assert!(validate_server("https://example.test", true, false).is_ok());
        assert!(validate_server("https://token@example.test", true, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn token_file_requires_owner_only_permissions() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "secret-token").unwrap();
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_token_file(file.path()).is_err());
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token_file(file.path()).unwrap(), "secret-token\n");
    }

    #[cfg(unix)]
    #[test]
    fn token_file_rejects_symlinks_without_a_check_then_open_race() {
        use std::os::unix::{fs::symlink, fs::PermissionsExt};
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        std::fs::write(&target, "secret-token\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &link).unwrap();
        assert!(read_token_file(&link).is_err());
    }

    #[test]
    fn lifecycle_event_schema_never_contains_capabilities() {
        let event = LifecycleEvent {
            schema_version: 1,
            sequence: 1,
            event: "acquired",
            kind: "lock",
            name: "repo/issue-1",
            server: "https://octostore.example",
            candidate_id: None,
            term: Some(42),
            expires_at: Some(Utc::now()),
            retry_after_ms: None,
            reason_code: None,
            authority_remaining_ms: Some(1_000),
            authority_observed_unix_ms: Some(1_700_000_000_000),
            authority_observed_continuous_ms: Some(42_000),
            observed_at: Utc::now(),
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["event"], "acquired");
        assert_eq!(value["authority_remaining_ms"], 1_000);
        assert_eq!(value["authority_observed_unix_ms"], 1_700_000_000_000_i64);
        assert_eq!(value["authority_observed_continuous_ms"], 42_000);
        assert!(value.get("lease_id").is_none());
        assert!(value.get("leader_token").is_none());
        assert!(value.get("token").is_none());
    }

    #[test]
    fn sse_frame_parser_handles_lf_and_crlf() {
        assert_eq!(find_sse_frame(b"data: {}\n\nrest"), Some(10));
        assert_eq!(find_sse_frame(b"data: {}\r\n\r\nrest"), Some(12));
        assert_eq!(find_sse_frame(b"data: {}\n"), None);
        assert_eq!(
            sse_retry_ms(b"event: state\nretry: 42000\ndata: {}\n\n"),
            Some(42_000)
        );
        assert_eq!(sse_retry_ms(b"retry: invalid\n\n"), None);
    }

    #[test]
    fn oversized_sse_append_is_rejected_before_pending_buffer_mutation() {
        let chunk = vec![b'x'; MAX_SSE_FRAME_BYTES + 1];
        let mut buffer = Vec::new();

        assert!(append_sse_bytes(&mut buffer, &chunk).is_err());
        assert!(buffer.is_empty());

        buffer.extend(std::iter::repeat_n(b'x', MAX_SSE_FRAME_BYTES - 1));
        let before = buffer.clone();
        assert!(append_sse_bytes(&mut buffer, b"yz").is_err());
        assert_eq!(buffer, before);
    }

    #[test]
    fn one_transport_chunk_can_supply_multiple_bounded_sse_frames() {
        let chunk = b"data: one\n\ndata: two\r\n\r\npartial";
        let mut buffer = Vec::new();
        let mut offset = 0;
        let mut frames = Vec::new();

        while offset < chunk.len() {
            offset += append_sse_bytes(&mut buffer, &chunk[offset..]).unwrap();
            while let Some(end) = find_sse_frame(&buffer) {
                frames.push(buffer.drain(..end).collect::<Vec<_>>());
            }
        }

        assert_eq!(frames, [b"data: one\n\n".as_slice(), b"data: two\r\n\r\n"]);
        assert_eq!(buffer, b"partial");
    }

    #[test]
    fn watch_json_uses_a_versioned_sequenced_snapshot_envelope() {
        let snapshot = serde_json::json!({"status":"held","fencing_token":42});
        let event = WatchSnapshot {
            schema_version: 1,
            sequence: 3,
            event: "snapshot",
            kind: "lock",
            name: "repo/issue-1",
            server: "https://octostore.example",
            snapshot: &snapshot,
            observed_at: Utc::now(),
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["sequence"], 3);
        assert_eq!(value["event"], "snapshot");
        assert_eq!(value["snapshot"]["fencing_token"], 42);
    }

    #[tokio::test]
    async fn timed_out_leader_response_is_resigned_before_timeout_is_reported() {
        let resignations = Arc::new(AtomicUsize::new(0));
        let resign_count = Arc::clone(&resignations);
        let router = Router::new().route(
            "/elections/deadline-room/resign",
            post(move || {
                let resign_count = Arc::clone(&resign_count);
                async move {
                    resign_count.fetch_add(1, Ordering::SeqCst);
                    AxumStatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        let server_task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let runtime = RuntimeOptions {
            server,
            client: Client::builder().no_proxy().build().unwrap(),
            request_timeout: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(1),
            json: true,
        };
        let response = Ok(CampaignResponse::Leader {
            election_id: "deadline-room".to_string(),
            leader: crate::elections::ElectionLeader {
                candidate_id: "deadline-agent".to_string(),
                metadata: None,
                term: 7,
                expires_at: Utc::now() + chrono::Duration::seconds(5),
            },
            leader_token: "known-election-capability".to_string(),
            renew_after_ms: 1_000,
        });

        assert!(cleanup_timed_out_election_response(&runtime, &response, true).await);
        assert_eq!(resignations.load(Ordering::SeqCst), 1);
        assert!(!cleanup_timed_out_election_response(&runtime, &response, false).await);
        assert_eq!(resignations.load(Ordering::SeqCst), 1);

        server_task.abort();
        let _ = server_task.await;
    }
}
