#![cfg(unix)]

use axum::{
    body::{Body, Bytes},
    http::{Response, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use futures::{stream, StreamExt};
use rcgen::{BasicConstraints, Certificate, CertificateParams, IsCa};
use rustls::{Certificate as RustlsCertificate, PrivateKey, ServerConfig};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    net::TcpListener,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsAcceptor;

const BINARY: &str = env!("CARGO_BIN_EXE_octostore");

struct Server {
    child: Child,
    url: String,
    database: std::path::PathBuf,
    _directory: TempDir,
}

impl Server {
    async fn start() -> Self {
        Self::start_with_ca_bundle(None).await
    }

    async fn start_with_ca_bundle(ca_bundle: Option<&Path>) -> Self {
        Self::start_with_auth(ca_bundle, Some("test-user:test-token"), false).await
    }

    async fn start_local_registration() -> Self {
        Self::start_with_auth(None, None, true).await
    }

    async fn start_with_auth(
        ca_bundle: Option<&Path>,
        static_tokens: Option<&str>,
        local_registration: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("octostore.db");
        let mut command = Command::new(BINARY);
        command
            .arg("serve")
            .env("BIND_ADDR", format!("127.0.0.1:{port}"))
            .env("DATABASE_URL", &database)
            .env_remove("GITHUB_CLIENT_ID")
            .env_remove("GITHUB_CLIENT_SECRET")
            .env_remove("STATIC_TOKENS")
            .env_remove("STATIC_TOKENS_FILE")
            .env_remove("LOCAL_REGISTRATION")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(static_tokens) = static_tokens {
            command.env("STATIC_TOKENS", static_tokens);
        }
        if local_registration {
            command.env("LOCAL_REGISTRATION", "true");
        }
        if let Some(ca_bundle) = ca_bundle {
            command
                .env("OCTOSTORE_CA_BUNDLE", ca_bundle)
                .env("OCTOSTORE_WEBHOOK_ALLOW_PRIVATE_NETWORKS", "true")
                .env("NO_PROXY", "localhost,127.0.0.1")
                .env("no_proxy", "localhost,127.0.0.1");
        }
        let child = command.spawn().unwrap();
        let server = Self {
            child,
            url: format!("http://127.0.0.1:{port}"),
            database,
            _directory: directory,
        };
        let client = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if client
                .get(format!("{}/health", server.url))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return server;
            }
            assert!(Instant::now() < deadline, "server did not become healthy");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

struct JsonProcess {
    child: Child,
    receiver: Receiver<(bool, String)>,
    lines: Vec<String>,
    stderr_lines: Vec<String>,
}

impl JsonProcess {
    fn spawn(arguments: &[&str], environment: &HashMap<&str, String>) -> Self {
        let mut command = Command::new(BINARY);
        command
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in environment {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        let stdout_sender = sender.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = stdout_sender.send((false, line));
            }
        });
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = sender.send((true, line));
            }
        });
        Self {
            child,
            receiver,
            lines: Vec::new(),
            stderr_lines: Vec::new(),
        }
    }

    fn drain_output(&mut self) -> Vec<String> {
        let mut stdout = Vec::new();
        while let Ok((is_stderr, line)) = self.receiver.try_recv() {
            if is_stderr {
                self.stderr_lines.push(line);
            } else {
                self.lines.push(line.clone());
                stdout.push(line);
            }
        }
        stdout
    }

    async fn event(&mut self, expected: &str, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            for line in self.drain_output() {
                let value: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|error| panic!("invalid JSONL '{line}': {error}"));
                if value["event"] == expected {
                    return value;
                }
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "process exited {status} before event {expected}; stdout: {:?}; stderr: {:?}",
                    self.lines, self.stderr_lines
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {expected}; stdout: {:?}; stderr: {:?}",
                self.lines,
                self.stderr_lines
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn signal(&self, signal: &str) {
        let status = Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        assert!(status.success());
    }

    async fn exit(&mut self, expected: i32, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            self.drain_output();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.drain_output();
                assert_eq!(
                    status.code(),
                    Some(expected),
                    "stdout: {:?}; stderr: {:?}",
                    self.lines,
                    self.stderr_lines
                );
                return status;
            }
            assert!(Instant::now() < deadline, "process did not exit");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn assert_secret_free(&self) {
        let output = format!(
            "{}\n{}",
            self.lines.join("\n"),
            self.stderr_lines.join("\n")
        );
        for secret_marker in ["leader_token", "lease_id", "test-token"] {
            assert!(!output.contains(secret_marker), "leaked {secret_marker}");
        }
    }

    fn command_line(&self) -> String {
        let output = Command::new("ps")
            .args(["-o", "command=", "-p", &self.child.id().to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn assert_absent(&self, values: &[&str]) {
        let output = format!(
            "{}\n{}",
            self.lines.join("\n"),
            self.stderr_lines.join("\n")
        );
        for value in values {
            assert!(!output.contains(value), "output leaked {value}: {output}");
        }
    }
}

impl Drop for JsonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Copy)]
enum ResponseStall {
    Headers,
    Body,
}

struct MockServer {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn start(router: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { url, task }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct PrivateHttpsServer {
    url: String,
    ca_bundle: std::path::PathBuf,
    requests: tokio::sync::mpsc::UnboundedReceiver<String>,
    task: tokio::task::JoinHandle<()>,
    _directory: TempDir,
}

impl PrivateHttpsServer {
    async fn start() -> Self {
        Self::start_with_redirect_target(None).await
    }

    async fn start_with_redirect_target(redirect_target: Option<String>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let ca_bundle = directory.path().join("private-ca.pem");

        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = Certificate::from_params(ca_params).unwrap();
        std::fs::write(&ca_bundle, ca.serialize_pem().unwrap()).unwrap();

        let server =
            Certificate::from_params(CertificateParams::new(vec!["localhost".to_string()]))
                .unwrap();
        let certificate = RustlsCertificate(server.serialize_der_with_signer(&ca).unwrap());
        let private_key = PrivateKey(server.serialize_private_key_der());
        let tls_config = ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], private_key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(tls_config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("https://localhost:{port}");
        let internal_redirect = format!("{url}/internal-target");
        let same_authority_downgrade = format!("http://localhost:{port}/downgrade");
        let (request_sender, requests) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut stream = stream;
                let mut first_byte = [0u8; 1];
                if stream
                    .peek(&mut first_byte)
                    .await
                    .is_ok_and(|count| count == 1)
                    && first_byte[0] != 0x16
                {
                    let Some(request) = read_http_request(&mut stream).await else {
                        continue;
                    };
                    let _ = request_sender.send(request);
                    let response = b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(response).await;
                    let _ = stream.shutdown().await;
                    continue;
                }
                let mut stream = match acceptor.accept(stream).await {
                    Ok(stream) => stream,
                    Err(error) => {
                        let _ = request_sender.send(format!("TLS_ERROR: {error}"));
                        continue;
                    }
                };
                let Some(request) = read_http_request(&mut stream).await else {
                    continue;
                };
                let _ = request_sender.send(request.clone());
                let request_line = request.lines().next().unwrap_or_default();
                let (status, content_type, extra_headers, body) = if request_line
                    .starts_with("GET /elections/private-ca-room ")
                {
                    (
                        "200 OK",
                        "application/json",
                        String::new(),
                        r#"{"election_id":"private-ca-room","status":"vacant","retry_after_ms":1000}"#,
                    )
                } else if request_line.starts_with("GET /locks/redirect-downgrade ") {
                    (
                        "307 Temporary Redirect",
                        "text/plain",
                        format!("Location: {same_authority_downgrade}\r\n"),
                        "redirect",
                    )
                } else if request_line.starts_with("POST /hook ") {
                    ("204 No Content", "text/plain", String::new(), "")
                } else if request_line.starts_with("POST /redirect-downgrade ") {
                    let location = redirect_target
                        .as_ref()
                        .map(|target| format!("Location: {target}/downgrade\r\n"))
                        .unwrap_or_default();
                    ("307 Temporary Redirect", "text/plain", location, "redirect")
                } else if request_line.starts_with("POST /redirect-internal ") {
                    (
                        "308 Permanent Redirect",
                        "text/plain",
                        format!("Location: {internal_redirect}\r\n"),
                        "redirect",
                    )
                } else if request_line.starts_with("POST /internal-target ") {
                    ("204 No Content", "text/plain", String::new(), "")
                } else {
                    ("404 Not Found", "text/plain", String::new(), "not found")
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        Self {
            url,
            ca_bundle,
            requests,
            task,
            _directory: directory,
        }
    }

    async fn request(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .expect("private HTTPS server did not receive a request")
            .expect("private HTTPS server stopped before receiving a request")
    }
}

impl Drop for PrivateHttpsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_http_request<S>(stream: &mut S) -> Option<String>
where
    S: tokio::io::AsyncRead + Unpin,
{
    const MAX_REQUEST_BYTES: usize = 128 * 1024;
    let mut request = Vec::new();
    let (header_end, content_length) = loop {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        request.extend_from_slice(&chunk[..count]);
        if request.len() > MAX_REQUEST_BYTES {
            return None;
        }
        if let Some(header_end) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
        {
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (header_end, content_length);
        }
    };
    while request.len() < header_end.saturating_add(content_length) {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        request.extend_from_slice(&chunk[..count]);
        if request.len() > MAX_REQUEST_BYTES {
            return None;
        }
    }
    String::from_utf8(request).ok()
}

fn json_response(value: Value, status: StatusCode) -> Response<Body> {
    (status, Json(value)).into_response()
}

async fn stalled_response(stall: ResponseStall, eventual: Value) -> Response<Body> {
    match stall {
        ResponseStall::Headers => {
            tokio::time::sleep(Duration::from_secs(30)).await;
            json_response(eventual, StatusCode::OK)
        }
        ResponseStall::Body => {
            let body = stream::once(async {
                Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"{"))
            })
            .chain(stream::pending());
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from_stream(body))
                .unwrap()
        }
    }
}

fn election_leader(candidate: &str, renew_after_ms: u64) -> Value {
    serde_json::json!({
        "status":"leader",
        "election_id":"deadline-room",
        "leader":{
            "candidate_id":candidate,
            "term":7,
            "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339()
        },
        "leader_token":"known-election-capability",
        "renew_after_ms":renew_after_ms
    })
}

async fn election_mock(stall: ResponseStall, stall_renewal: bool) -> MockServer {
    let campaign_stall = stall;
    let renew_stall = stall;
    let router = Router::new()
        .route(
            "/elections/:id/campaign",
            post(move || async move {
                if stall_renewal {
                    json_response(election_leader("deadline-agent", 100), StatusCode::OK)
                } else {
                    stalled_response(campaign_stall, election_leader("deadline-agent", 100)).await
                }
            }),
        )
        .route(
            "/elections/:id/renew",
            post(move || async move {
                stalled_response(renew_stall, election_leader("deadline-agent", 100)).await
            }),
        );
    MockServer::start(router).await
}

const KNOWN_LEASE_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
const KNOWN_SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440001";
const MISMATCHED_SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440099";

async fn lock_mock(stall: ResponseStall, stall_renewal: bool) -> MockServer {
    let acquire_stall = stall;
    let renew_stall = stall;
    let router = Router::new()
        .route(
            "/sessions",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "session_id":"550e8400-e29b-41d4-a716-446655440001",
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
                        "keepalive_interval_secs":15
                    }),
                    StatusCode::CREATED,
                )
            }),
        )
        .route(
            "/sessions/:id/keepalive",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "session_id":"550e8400-e29b-41d4-a716-446655440001",
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339()
                    }),
                    StatusCode::OK,
                )
            }),
        )
        .route("/sessions/:id", delete(|| async { StatusCode::NO_CONTENT }))
        .route(
            "/locks/:name/acquire",
            post(move || async move {
                let acquired = serde_json::json!({
                    "status":"acquired",
                    "lease_id":KNOWN_LEASE_ID,
                    "fencing_token":9,
                    "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339(),
                    "renew_after_ms":100,
                    "metadata":null
                });
                if stall_renewal {
                    json_response(acquired, StatusCode::OK)
                } else {
                    stalled_response(acquire_stall, acquired).await
                }
            }),
        )
        .route(
            "/locks/:name/renew",
            post(move || async move {
                stalled_response(
                    renew_stall,
                    serde_json::json!({
                        "lease_id":KNOWN_LEASE_ID,
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339(),
                        "renew_after_ms":100
                    }),
                )
                .await
            }),
        );
    MockServer::start(router).await
}

async fn session_deadline_mock() -> MockServer {
    let router = Router::new()
        .route(
            "/sessions",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "session_id":"550e8400-e29b-41d4-a716-446655440001",
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
                        "keepalive_interval_secs":1
                    }),
                    StatusCode::CREATED,
                )
            }),
        )
        .route(
            "/sessions/:id/keepalive",
            post(|| async {
                stalled_response(
                    ResponseStall::Body,
                    serde_json::json!({
                        "session_id":"550e8400-e29b-41d4-a716-446655440001",
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339()
                    }),
                )
                .await
            }),
        )
        .route("/sessions/:id", delete(|| async { StatusCode::NO_CONTENT }))
        .route(
            "/locks/:name/acquire",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "status":"acquired",
                        "lease_id":KNOWN_LEASE_ID,
                        "fencing_token":9,
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(3_600)).to_rfc3339(),
                        "renew_after_ms":1_800_000,
                        "metadata":null
                    }),
                    StatusCode::OK,
                )
            }),
        );
    MockServer::start(router).await
}

async fn stale_election_mock(campaigns: Arc<AtomicUsize>) -> MockServer {
    let router = Router::new()
        .route(
            "/elections/:id/campaign",
            post(move || {
                let campaigns = Arc::clone(&campaigns);
                async move {
                    campaigns.fetch_add(1, Ordering::SeqCst);
                    json_response(election_leader("deadline-agent", 100), StatusCode::OK)
                }
            }),
        )
        .route(
            "/elections/:id/renew",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "error":"Lease is not current",
                        "code":"lease_not_current",
                        "details":"stale test capability",
                        "request_id":"req_stale"
                    }),
                    StatusCode::NOT_FOUND,
                )
            }),
        );
    MockServer::start(router).await
}

async fn stale_lock_mock(acquisitions: Arc<AtomicUsize>) -> MockServer {
    let router = Router::new()
        .route(
            "/sessions",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "session_id":"550e8400-e29b-41d4-a716-446655440001",
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
                        "keepalive_interval_secs":15
                    }),
                    StatusCode::CREATED,
                )
            }),
        )
        .route("/sessions/:id", delete(|| async { StatusCode::NO_CONTENT }))
        .route(
            "/locks/:name/acquire",
            post(move || {
                let acquisitions = Arc::clone(&acquisitions);
                async move {
                    acquisitions.fetch_add(1, Ordering::SeqCst);
                    json_response(
                        serde_json::json!({
                            "status":"acquired",
                            "lease_id":KNOWN_LEASE_ID,
                            "fencing_token":9,
                            "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339(),
                            "renew_after_ms":100,
                            "metadata":null
                        }),
                        StatusCode::OK,
                    )
                }
            }),
        )
        .route(
            "/locks/:name/renew",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "error":"Lease is not current",
                        "code":"lease_not_current",
                        "details":"stale test capability",
                        "request_id":"req_stale"
                    }),
                    StatusCode::NOT_FOUND,
                )
            }),
        );
    MockServer::start(router).await
}

async fn shutdown_election_mock() -> MockServer {
    let router = Router::new()
        .route(
            "/elections/:id/campaign",
            post(|| async {
                let mut leader = election_leader("deadline-agent", 150_000);
                leader["leader"]["expires_at"] = serde_json::json!(
                    (chrono::Utc::now() + chrono::Duration::seconds(300)).to_rfc3339()
                );
                json_response(leader, StatusCode::OK)
            }),
        )
        .route(
            "/elections/:id/resign",
            post(|| async {
                stalled_response(
                    ResponseStall::Body,
                    serde_json::json!({"election_id":"deadline-room","status":"vacant","previous_term":7}),
                )
                .await
            }),
        );
    MockServer::start(router).await
}

async fn mismatched_resign_mock() -> MockServer {
    let router = Router::new()
        .route(
            "/elections/:id/campaign",
            post(|| async {
                let mut leader = election_leader("deadline-agent", 150_000);
                leader["leader"]["expires_at"] = serde_json::json!((chrono::Utc::now()
                    + chrono::Duration::seconds(300))
                .to_rfc3339());
                json_response(leader, StatusCode::OK)
            }),
        )
        .route(
            "/elections/:id/resign",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "election_id":"other-room",
                        "status":"leader",
                        "previous_term":999
                    }),
                    StatusCode::OK,
                )
            }),
        );
    MockServer::start(router).await
}

async fn shutdown_lock_mock() -> MockServer {
    let router = Router::new()
        .route(
            "/sessions",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "session_id":"550e8400-e29b-41d4-a716-446655440001",
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
                        "keepalive_interval_secs":15
                    }),
                    StatusCode::CREATED,
                )
            }),
        )
        .route(
            "/sessions/:id",
            delete(|| async {
                stalled_response(ResponseStall::Headers, serde_json::json!({})).await
            }),
        )
        .route(
            "/locks/:name/acquire",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "status":"acquired",
                        "lease_id":KNOWN_LEASE_ID,
                        "fencing_token":9,
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(120)).to_rfc3339(),
                        "renew_after_ms":60_000,
                        "metadata":null
                    }),
                    StatusCode::OK,
                )
            }),
        )
        .route(
            "/locks/:name/release",
            post(|| async {
                stalled_response(ResponseStall::Body, serde_json::json!({})).await
            }),
        );
    MockServer::start(router).await
}

async fn lock_waiting_mock(outcome: &'static str) -> MockServer {
    let router = Router::new()
        .route(
            "/sessions",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "session_id":"550e8400-e29b-41d4-a716-446655440001",
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
                        "keepalive_interval_secs":15
                    }),
                    StatusCode::CREATED,
                )
            }),
        )
        .route("/sessions/:id", delete(|| async { StatusCode::NO_CONTENT }))
        .route(
            "/locks/:name/acquire",
            post(move || async move {
                let response = match outcome {
                    "held" => serde_json::json!({
                        "status":"held",
                        "holder_id":"550e8400-e29b-41d4-a716-446655440002",
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339(),
                        "retry_after_ms":100,
                        "metadata":null
                    }),
                    "delayed" => serde_json::json!({
                        "status":"delayed",
                        "available_at":(chrono::Utc::now() + chrono::Duration::milliseconds(100)).to_rfc3339(),
                        "lock_delay_seconds":1,
                        "retry_after_ms":100
                    }),
                    _ => unreachable!("unsupported lock wait outcome"),
                };
                json_response(response, StatusCode::CONFLICT)
            }),
        );
    MockServer::start(router).await
}

async fn watch_reconnect_mock(
    connections: Arc<AtomicUsize>,
    opened_at: Arc<Mutex<Vec<Instant>>>,
) -> MockServer {
    let watch_connections = Arc::clone(&connections);
    let watch_opened_at = Arc::clone(&opened_at);
    let router = Router::new()
        .route(
            "/elections/:id",
            get(|| async {
                json_response(
                    serde_json::json!({
                        "election_id":"watch-room",
                        "status":"vacant",
                        "retry_after_ms":0
                    }),
                    StatusCode::OK,
                )
            }),
        )
        .route(
            "/elections/:id/watch",
            get(move || {
                let connections = Arc::clone(&watch_connections);
                let opened_at = Arc::clone(&watch_opened_at);
                async move {
                    connections.fetch_add(1, Ordering::SeqCst);
                    opened_at.lock().unwrap().push(Instant::now());
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(Body::from("retry: 400\nevent: state\ndata: {}\n\n"))
                        .unwrap()
                }
            }),
        );
    MockServer::start(router).await
}

async fn delayed_election_authority_mock(delay: Duration, candidate: &'static str) -> MockServer {
    let router = Router::new().route(
        "/elections/:id/campaign",
        post(move || async move {
            tokio::time::sleep(delay).await;
            json_response(election_leader(candidate, 100), StatusCode::OK)
        }),
    );
    MockServer::start(router).await
}

async fn delayed_lock_authority_mock(delay: Duration) -> MockServer {
    let router = Router::new()
        .route(
            "/sessions",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "session_id":"550e8400-e29b-41d4-a716-446655440001",
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
                        "keepalive_interval_secs":15
                    }),
                    StatusCode::CREATED,
                )
            }),
        )
        .route("/sessions/:id", delete(|| async { StatusCode::NO_CONTENT }))
        .route(
            "/locks/:name/acquire",
            post(move || async move {
                tokio::time::sleep(delay).await;
                json_response(
                    serde_json::json!({
                        "status":"acquired",
                        "lease_id":KNOWN_LEASE_ID,
                        "fencing_token":9,
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(1)).to_rfc3339(),
                        "renew_after_ms":500,
                        "metadata":null
                    }),
                    StatusCode::OK,
                )
            }),
        );
    MockServer::start(router).await
}

async fn guided_lock_wait_mock(
    acquisitions: Arc<AtomicUsize>,
    keepalives: Arc<AtomicUsize>,
) -> MockServer {
    let acquire_count = Arc::clone(&acquisitions);
    let keepalive_count = Arc::clone(&keepalives);
    let router = Router::new()
        .route(
            "/sessions",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "session_id":"550e8400-e29b-41d4-a716-446655440001",
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
                        "keepalive_interval_secs":1
                    }),
                    StatusCode::CREATED,
                )
            }),
        )
        .route(
            "/sessions/:id/keepalive",
            post(move || {
                let keepalives = Arc::clone(&keepalive_count);
                async move {
                    keepalives.fetch_add(1, Ordering::SeqCst);
                    json_response(
                        serde_json::json!({
                            "session_id":"550e8400-e29b-41d4-a716-446655440001",
                            "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339()
                        }),
                        StatusCode::OK,
                    )
                }
            }),
        )
        .route("/sessions/:id", delete(|| async { StatusCode::NO_CONTENT }))
        .route(
            "/locks/:name/acquire",
            post(move || {
                let acquisitions = Arc::clone(&acquire_count);
                async move {
                    acquisitions.fetch_add(1, Ordering::SeqCst);
                    json_response(
                        serde_json::json!({
                            "status":"held",
                            "holder_id":"550e8400-e29b-41d4-a716-446655440002",
                            "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(3)).to_rfc3339(),
                            "retry_after_ms":2500,
                            "metadata":null
                        }),
                        StatusCode::OK,
                    )
                }
            }),
        );
    MockServer::start(router).await
}

#[derive(Clone, Copy)]
enum SessionMismatchPhase {
    BeforeAcquisition,
    AfterAcquisition,
}

async fn mismatched_session_keepalive_mock(phase: SessionMismatchPhase) -> MockServer {
    let router = Router::new()
        .route(
            "/sessions",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "session_id":KNOWN_SESSION_ID,
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
                        "keepalive_interval_secs":1
                    }),
                    StatusCode::CREATED,
                )
            }),
        )
        .route(
            "/sessions/:id/keepalive",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "session_id":MISMATCHED_SESSION_ID,
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339()
                    }),
                    StatusCode::OK,
                )
            }),
        )
        .route("/sessions/:id", delete(|| async { StatusCode::NO_CONTENT }))
        .route(
            "/locks/:name/acquire",
            post(move || async move {
                let response = match phase {
                    SessionMismatchPhase::BeforeAcquisition => serde_json::json!({
                        "status":"held",
                        "holder_id":"550e8400-e29b-41d4-a716-446655440002",
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339(),
                        "retry_after_ms":5_000,
                        "metadata":null
                    }),
                    SessionMismatchPhase::AfterAcquisition => serde_json::json!({
                        "status":"acquired",
                        "lease_id":KNOWN_LEASE_ID,
                        "fencing_token":9,
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339(),
                        "renew_after_ms":2_500,
                        "metadata":null
                    }),
                };
                json_response(response, StatusCode::OK)
            }),
        );
    MockServer::start(router).await
}

async fn retrying_session_mock(session_attempts: Arc<AtomicUsize>) -> MockServer {
    let attempts = Arc::clone(&session_attempts);
    let router = Router::new()
        .route(
            "/sessions",
            post(move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        return json_response(
                            serde_json::json!({
                                "code":"upstream_unavailable",
                                "details":"retry session creation",
                                "retry_after_ms":100
                            }),
                            StatusCode::SERVICE_UNAVAILABLE,
                        );
                    }
                    json_response(
                        serde_json::json!({
                            "session_id":"550e8400-e29b-41d4-a716-446655440001",
                            "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
                            "keepalive_interval_secs":15
                        }),
                        StatusCode::CREATED,
                    )
                }
            }),
        )
        .route("/sessions/:id", delete(|| async { StatusCode::NO_CONTENT }))
        .route(
            "/locks/:name/acquire",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "status":"acquired",
                        "lease_id":KNOWN_LEASE_ID,
                        "fencing_token":9,
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339(),
                        "renew_after_ms":2500,
                        "metadata":null
                    }),
                    StatusCode::OK,
                )
            }),
        )
        .route(
            "/locks/:name/release",
            post(|| async { json_response(serde_json::Value::Null, StatusCode::OK) }),
        );
    MockServer::start(router).await
}

#[derive(Clone, Copy)]
enum WatchFailure {
    StalledHeaders,
    StalledErrorBody,
    OversizedFrame,
}

async fn pathological_watch_mock(failure: WatchFailure) -> MockServer {
    let router = Router::new()
        .route(
            "/elections/:id",
            get(|| async {
                json_response(
                    serde_json::json!({
                        "election_id":"watch-room",
                        "status":"vacant",
                        "retry_after_ms":0
                    }),
                    StatusCode::OK,
                )
            }),
        )
        .route(
            "/elections/:id/watch",
            get(move || async move {
                match failure {
                    WatchFailure::StalledHeaders => {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        Response::builder()
                            .status(StatusCode::OK)
                            .body(Body::empty())
                            .unwrap()
                    }
                    WatchFailure::StalledErrorBody => {
                        let body = stream::once(async {
                            Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"{"))
                        })
                        .chain(stream::pending());
                        Response::builder()
                            .status(StatusCode::SERVICE_UNAVAILABLE)
                            .header("content-type", "application/json")
                            .body(Body::from_stream(body))
                            .unwrap()
                    }
                    WatchFailure::OversizedFrame => {
                        let body = stream::once(async {
                            Ok::<Bytes, std::convert::Infallible>(Bytes::from(vec![
                                b'x';
                                64 * 1024 + 1
                            ]))
                        })
                        .chain(stream::pending());
                        Response::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "text/event-stream")
                            .body(Body::from_stream(body))
                            .unwrap()
                    }
                }
            }),
        );
    MockServer::start(router).await
}

fn oversized_chunked_body() -> Body {
    let chunks = stream::iter(
        (0..=1024).map(|_| Ok::<Bytes, std::convert::Infallible>(Bytes::from(vec![b'x'; 1024]))),
    );
    Body::from_stream(chunks)
}

async fn oversized_api_response_mock() -> MockServer {
    let router = Router::new()
        .route(
            "/elections/oversized-success",
            get(move || async move {
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(oversized_chunked_body())
                    .unwrap()
            }),
        )
        .route(
            "/elections/oversized-error",
            get(move || async move {
                Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .header("content-type", "application/json")
                    .body(oversized_chunked_body())
                    .unwrap()
            }),
        );
    MockServer::start(router).await
}

async fn zero_timing_election_mock(renewals: Arc<AtomicUsize>) -> MockServer {
    let leader = || {
        serde_json::json!({
            "status":"leader",
            "election_id":"zero-timing",
            "leader":{
                "candidate_id":"timing-agent",
                "term":7,
                "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339()
            },
            "leader_token":"known-election-capability",
            "renew_after_ms":0
        })
    };
    let renewal_counter = Arc::clone(&renewals);
    let router = Router::new()
        .route(
            "/elections/zero-timing/campaign",
            post(move || async move { json_response(leader(), StatusCode::OK) }),
        )
        .route(
            "/elections/zero-timing/renew",
            post(move || {
                let renewal_counter = Arc::clone(&renewal_counter);
                async move {
                    renewal_counter.fetch_add(1, Ordering::SeqCst);
                    json_response(leader(), StatusCode::OK)
                }
            }),
        )
        .route(
            "/elections/zero-timing/resign",
            post(|| async { json_response(serde_json::json!({}), StatusCode::OK) }),
        );
    MockServer::start(router).await
}

async fn zero_session_interval_mock() -> MockServer {
    let router = Router::new()
        .route(
            "/sessions",
            post(|| async {
                json_response(
                    serde_json::json!({
                        "session_id":"550e8400-e29b-41d4-a716-446655440001",
                        "expires_at":(chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339(),
                        "keepalive_interval_secs":0
                    }),
                    StatusCode::CREATED,
                )
            }),
        )
        .route(
            "/sessions/:id",
            delete(|| async { StatusCode::NO_CONTENT }),
        );
    MockServer::start(router).await
}

const REFLECTED_TOKEN: &str = "req_0123456789abcdef0123456789abcdef";

async fn credential_reflection_mock() -> MockServer {
    let reflected_status = || async {
        json_response(
            serde_json::json!({
                "name": format!("{REFLECTED_TOKEN}\nacquired\u{1b}[31m"),
                "status": REFLECTED_TOKEN,
                "holder_id": null,
                "fencing_token": 0,
                "expires_at": null,
                "metadata": format!("reflected metadata: {REFLECTED_TOKEN}"),
                "acl": {"acquire": [REFLECTED_TOKEN]}
            }),
            StatusCode::OK,
        )
    };
    let router = Router::new()
        .route("/locks/success-reflection", get(reflected_status))
        .route("/locks/watch-reflection", get(reflected_status))
        .route(
            "/locks/error-reflection",
            get(|| async {
                Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header("content-type", "application/json")
                    .header("x-request-id", REFLECTED_TOKEN)
                    .body(Body::from(
                        serde_json::json!({
                            "code": REFLECTED_TOKEN,
                            "details": format!("reflected details: {REFLECTED_TOKEN}"),
                            "request_id": REFLECTED_TOKEN
                        })
                        .to_string(),
                    ))
                    .unwrap()
            }),
        )
        .route(
            "/locks/watch-reflection/watch",
            get(|| async {
                let body = stream::pending::<Result<Bytes, std::convert::Infallible>>();
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(body))
                    .unwrap()
            }),
        );
    MockServer::start(router).await
}

fn token_file(directory: &Path) -> String {
    token_file_with(directory, "test-token")
}

fn token_file_with(directory: &Path, token: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join("octostore-token");
    std::fs::write(&path, format!("{token}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path.to_string_lossy().to_string()
}

fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

async fn wait_for_child(child: &mut Child, timeout: Duration) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "child process did not exit");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn process_is_alive(pid: &str, process_group: bool) -> bool {
    let target = if process_group {
        format!("-{pid}")
    } else {
        pid.to_string()
    };
    let mut arguments = vec!["-0".to_owned()];
    if process_group {
        arguments.push("--".to_owned());
    }
    arguments.push(target);
    Command::new("kill")
        .args(&arguments)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn assert_isolated_process_group(pid: u32) {
    let process_group = |target: u32| {
        let output = Command::new("ps")
            .args(["-o", "pgid=", "-p", &target.to_string()])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "could not inspect process group for {target}"
        );
        output
            .stdout
            .iter()
            .map(|byte| *byte as char)
            .collect::<String>()
            .trim()
            .parse::<u32>()
            .unwrap_or_else(|_| panic!("invalid process group for {target}"))
    };

    let child_group = process_group(pid);
    let test_group = process_group(std::process::id());
    assert_eq!(
        child_group, pid,
        "refusing to group-signal a child that is not its own process-group leader"
    );
    assert_ne!(
        child_group, test_group,
        "refusing to group-signal the test runner process group"
    );
}

fn identity_process_group(identity: &str) -> String {
    identity
        .split_whitespace()
        .nth(1)
        .expect("published process identity must include a PGID")
        .to_owned()
}

async fn wait_for_processes_gone(pids: &[(&str, bool)], timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let survivors = pids
            .iter()
            .filter(|(pid, process_group)| process_is_alive(pid, *process_group))
            .map(|(pid, process_group)| {
                if *process_group {
                    format!("process group {pid}")
                } else {
                    format!("process {pid}")
                }
            })
            .collect::<Vec<_>>();
        if survivors.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "protected processes survived teardown: {}",
            survivors.join(", ")
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn supervisor_state_dirs(directory: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("octostore-supervisor.")
        })
        .map(|entry| entry.path())
        .collect()
}

async fn wait_for_supervisor_state_removed(directory: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let state = supervisor_state_dirs(directory);
        if state.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "watchdog state survived teardown: {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[test]
#[serial_test::serial]
fn cli_help_version_and_usage_exit_contract() {
    for arguments in [["--help"].as_slice(), ["--version"].as_slice()] {
        let output = Command::new(BINARY).args(arguments).output().unwrap();
        assert!(output.status.success());
    }
    let output = Command::new(BINARY).arg("not-a-command").output().unwrap();
    assert_eq!(output.status.code(), Some(64));
    let output = Command::new(BINARY)
        .args(["lock", "status", "task"])
        .env_remove("OCTOSTORE_URL")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(64));
    let forged = Command::new(BINARY)
        .args([
            "election",
            "hold",
            "shared-room",
            "--candidate",
            "agent\nacquired\u{1b}[31m",
        ])
        .output()
        .unwrap();
    assert_eq!(forged.status.code(), Some(64));
    let forged_stderr = String::from_utf8_lossy(&forged.stderr);
    assert!(!forged_stderr.lines().any(|line| line == "acquired"));
    assert!(forged_stderr.contains("candidate must be"));
}

#[tokio::test]
#[serial_test::serial]
async fn local_registration_is_explicit_collision_safe_and_never_reveals_existing_tokens() {
    let client = reqwest::Client::new();
    let mut registration_server = Server::start_local_registration().await;
    let first = client
        .post(format!("{}/auth/register", registration_server.url))
        .json(&serde_json::json!({"username":"ops"}))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), reqwest::StatusCode::OK);
    assert_eq!(
        first.headers().get(reqwest::header::CACHE_CONTROL),
        Some(&reqwest::header::HeaderValue::from_static("no-store"))
    );
    let first_body: Value = first.json().await.unwrap();
    let first_token = first_body["token"].as_str().unwrap().to_string();
    assert!(!first_token.is_empty());

    let repeated = client
        .post(format!("{}/auth/register", registration_server.url))
        .json(&serde_json::json!({"username":"ops"}))
        .send()
        .await
        .unwrap();
    assert_eq!(repeated.status(), reqwest::StatusCode::CONFLICT);
    let repeated_body = repeated.text().await.unwrap();
    assert!(!repeated_body.contains(&first_token));
    assert!(!repeated_body.contains("token"));

    let mixed_case = client
        .post(format!("{}/auth/register", registration_server.url))
        .json(&serde_json::json!({"username":"OPS"}))
        .send()
        .await
        .unwrap();
    assert_eq!(mixed_case.status(), reqwest::StatusCode::CONFLICT);
    let mixed_case_body = mixed_case.text().await.unwrap();
    assert!(!mixed_case_body.contains(&first_token));
    assert!(!mixed_case_body.contains("token"));

    let legacy_token = "legacy-oauth-secret";
    let legacy_user_id = uuid::Uuid::new_v4();
    rusqlite::Connection::open(&registration_server.database)
        .unwrap()
        .execute(
            "INSERT INTO users (id, github_id, github_username, token, namespace, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                legacy_user_id.to_string(),
                1234_i64,
                "LegacyUser",
                legacy_token,
                Option::<String>::None,
                chrono::Utc::now().to_rfc3339()
            ],
        )
        .unwrap();
    let legacy_collision = client
        .post(format!("{}/auth/register", registration_server.url))
        .json(&serde_json::json!({"username":"legacyuser"}))
        .send()
        .await
        .unwrap();
    assert_eq!(legacy_collision.status(), reqwest::StatusCode::CONFLICT);
    let legacy_body = legacy_collision.text().await.unwrap();
    assert!(!legacy_body.contains(legacy_token));
    assert!(!legacy_body.contains("token"));
    registration_server.stop();

    let mut static_server = Server::start().await;
    let static_collision = client
        .post(format!("{}/auth/register", static_server.url))
        .json(&serde_json::json!({"username":"test-user"}))
        .send()
        .await
        .unwrap();
    assert_eq!(static_collision.status(), reqwest::StatusCode::NOT_FOUND);
    let static_body = static_collision.text().await.unwrap();
    assert!(!static_body.contains("test-token"));
    static_server.stop();
}

#[tokio::test]
#[serial_test::serial]
async fn private_ca_https_trust_is_shared_by_cli_and_webhook_clients() {
    let mut private_https = PrivateHttpsServer::start().await;
    let fixture_cas = reqwest::Certificate::from_pem_bundle(
        &std::fs::read(&private_https.ca_bundle).expect("read fixture CA bundle"),
    )
    .expect("parse fixture CA bundle");
    let fixture_builder = fixture_cas
        .into_iter()
        .fold(reqwest::Client::builder(), |builder, certificate| {
            builder.add_root_certificate(certificate)
        });
    let fixture_client = fixture_builder.no_proxy().build().unwrap();
    let fixture_response = fixture_client
        .get(format!("{}/elections/private-ca-room", private_https.url))
        .send()
        .await
        .expect("private HTTPS fixture must present a valid CA-signed certificate");
    assert_eq!(fixture_response.status(), reqwest::StatusCode::OK);
    let fixture_request = private_https.request().await;
    assert!(fixture_request.starts_with("GET /elections/private-ca-room "));

    // Keep the Tokio runtime available to the in-process TLS fixture while the
    // child CLI performs its handshake.
    let cli_output = tokio::process::Command::new(BINARY)
        .args([
            "election",
            "status",
            "private-ca-room",
            "--server",
            &private_https.url,
            "--json",
        ])
        .env("OCTOSTORE_CA_BUNDLE", &private_https.ca_bundle)
        .env("NO_PROXY", "localhost,127.0.0.1")
        .env("no_proxy", "localhost,127.0.0.1")
        .output()
        .await
        .unwrap();
    let cli_request = private_https.request().await;
    assert!(
        cli_output.status.success(),
        "CLI rejected private CA after sending {cli_request:?}: {}",
        String::from_utf8_lossy(&cli_output.stderr),
    );
    let status: Value = serde_json::from_slice(&cli_output.stdout).unwrap();
    assert_eq!(status["election_id"], "private-ca-room");
    assert_eq!(status["status"], "vacant");
    assert!(cli_request.starts_with("GET /elections/private-ca-room "));
    assert!(!cli_request.contains("test-token"));

    let mut server = Server::start_with_ca_bundle(Some(&private_https.ca_bundle)).await;
    let api_client = reqwest::Client::new();
    let create_response = api_client
        .post(format!("{}/webhooks", server.url))
        .bearer_auth("test-token")
        .json(&serde_json::json!({
            "url":format!("{}/hook", private_https.url),
            "events":["acquired"],
            "lock_pattern":"private-ca-*"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(create_response.status(), reqwest::StatusCode::CREATED);

    let acquire_response = api_client
        .post(format!("{}/locks/private-ca-lock/acquire", server.url))
        .bearer_auth("test-token")
        .json(&serde_json::json!({"ttl_seconds":5}))
        .send()
        .await
        .unwrap();
    assert_eq!(acquire_response.status(), reqwest::StatusCode::OK);
    let webhook_request = private_https.request().await;
    assert!(webhook_request.starts_with("POST /hook "));
    assert!(webhook_request.contains(r#""event":"acquired""#));
    assert!(webhook_request.contains(r#""lock":"private-ca-lock""#));
    assert!(!webhook_request.contains("test-token"));
    server.stop();
}

#[tokio::test]
#[serial_test::serial]
async fn credentialed_cli_never_follows_a_same_authority_tls_downgrade() {
    let mut private_https = PrivateHttpsServer::start().await;
    let cli_output = tokio::process::Command::new(BINARY)
        .args([
            "lock",
            "status",
            "redirect-downgrade",
            "--server",
            &private_https.url,
            "--json",
        ])
        .env("OCTOSTORE_TOKEN", "test-token")
        .env("OCTOSTORE_CA_BUNDLE", &private_https.ca_bundle)
        .env("NO_PROXY", "localhost,127.0.0.1")
        .env("no_proxy", "localhost,127.0.0.1")
        .output()
        .await
        .unwrap();

    assert_eq!(cli_output.status.code(), Some(70));
    let initial_request = private_https.request().await;
    assert!(initial_request.starts_with("GET /locks/redirect-downgrade "));
    assert!(
        initial_request
            .to_ascii_lowercase()
            .contains("authorization: bearer test-token"),
        "credential was not sent to the validated HTTPS origin: {initial_request:?}"
    );

    if let Ok(Some(downgraded_request)) =
        tokio::time::timeout(Duration::from_millis(250), private_https.requests.recv()).await
    {
        panic!(
            "CLI followed an HTTPS-to-HTTP redirect and delivered a target request: {downgraded_request:?}"
        );
    }
    let output = format!(
        "{}\n{}",
        String::from_utf8_lossy(&cli_output.stdout),
        String::from_utf8_lossy(&cli_output.stderr)
    );
    assert!(
        !output.contains("test-token"),
        "CLI output leaked its token"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn credentialed_loopback_cli_ignores_ambient_proxies() {
    let mut server =
        Server::start_with_auth(None, Some("test-user:proxy-secret-token"), false).await;
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", proxy_listener.local_addr().unwrap());
    let proxy_hits = Arc::new(AtomicUsize::new(0));
    let observed_hits = Arc::clone(&proxy_hits);
    let proxy_task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = proxy_listener.accept().await {
            observed_hits.fetch_add(1, Ordering::SeqCst);
            let mut request = vec![0_u8; 16 * 1024];
            let _ = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut request)).await;
            let _ = stream
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            let _ = stream.shutdown().await;
        }
    });

    let cli_output = tokio::process::Command::new(BINARY)
        .args([
            "lock",
            "status",
            "proxy-isolation",
            "--server",
            &server.url,
            "--json",
        ])
        .env("OCTOSTORE_TOKEN", "proxy-secret-token")
        .env("HTTP_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url)
        .env("HTTPS_PROXY", &proxy_url)
        .env("https_proxy", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("all_proxy", &proxy_url)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .await
        .unwrap();

    assert_eq!(
        cli_output.status.code(),
        Some(0),
        "CLI did not reach the intended loopback API directly: {}",
        String::from_utf8_lossy(&cli_output.stderr)
    );
    let status: Value = serde_json::from_slice(&cli_output.stdout).unwrap();
    assert_eq!(status["name"], "proxy-isolation");
    assert_eq!(status["status"], "free");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        proxy_hits.load(Ordering::SeqCst),
        0,
        "credentialed loopback request reached an ambient proxy"
    );
    let output = format!(
        "{}\n{}",
        String::from_utf8_lossy(&cli_output.stdout),
        String::from_utf8_lossy(&cli_output.stderr)
    );
    assert!(!output.contains("proxy-secret-token"));
    proxy_task.abort();
    server.stop();
}

#[tokio::test]
#[serial_test::serial]
async fn credentialed_cli_redacts_malicious_server_reflection_from_stdout_and_stderr() {
    let server = credential_reflection_mock().await;
    let directory = tempfile::tempdir().unwrap();
    let token_path = token_file_with(directory.path(), REFLECTED_TOKEN);

    for json in [false, true] {
        let mut command = tokio::process::Command::new(BINARY);
        command.args([
            "lock",
            "status",
            "success-reflection",
            "--server",
            &server.url,
        ]);
        if json {
            command.arg("--json");
        }
        let output = command
            .env("OCTOSTORE_TOKEN_FILE", &token_path)
            .output()
            .await
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "successful reflected status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains(REFLECTED_TOKEN));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(REFLECTED_TOKEN));
        assert!(String::from_utf8_lossy(&output.stdout).contains("[REDACTED]"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("\nacquired"));
    }

    let error = tokio::process::Command::new(BINARY)
        .args([
            "lock",
            "status",
            "error-reflection",
            "--server",
            &server.url,
            "--json",
        ])
        .env("OCTOSTORE_TOKEN_FILE", &token_path)
        .output()
        .await
        .unwrap();
    assert_eq!(error.status.code(), Some(70));
    assert!(!String::from_utf8_lossy(&error.stdout).contains(REFLECTED_TOKEN));
    assert!(!String::from_utf8_lossy(&error.stderr).contains(REFLECTED_TOKEN));
    assert!(String::from_utf8_lossy(&error.stderr).contains("[REDACTED]"));

    let environment = HashMap::from([
        ("OCTOSTORE_URL", server.url.clone()),
        ("OCTOSTORE_TOKEN_FILE", token_path),
    ]);
    let mut watch = JsonProcess::spawn(
        &["lock", "watch", "watch-reflection", "--json"],
        &environment,
    );
    let snapshot = watch.event("snapshot", Duration::from_secs(2)).await;
    assert_eq!(
        snapshot["snapshot"]["metadata"],
        "reflected metadata: [REDACTED]"
    );
    assert_eq!(snapshot["snapshot"]["status"], "[REDACTED]");
    watch.signal("INT");
    watch.exit(130, Duration::from_secs(2)).await;
    watch.assert_absent(&[REFLECTED_TOKEN]);
}

#[tokio::test]
#[serial_test::serial]
async fn election_hold_protects_its_leader_capability_transport() {
    let rejected = tokio::process::Command::new(BINARY)
        .args([
            "election",
            "hold",
            "cleartext-capability",
            "--candidate",
            "agent-a",
            "--server",
            "http://192.0.2.1:3000",
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(rejected.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("secret capability"));

    let mut server = Server::start().await;
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", proxy_listener.local_addr().unwrap());
    let proxy_hits = Arc::new(AtomicUsize::new(0));
    let observed_hits = Arc::clone(&proxy_hits);
    let proxy_task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = proxy_listener.accept().await {
            observed_hits.fetch_add(1, Ordering::SeqCst);
            let mut request = vec![0_u8; 16 * 1024];
            let _ = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut request)).await;
            let _ = stream
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            let _ = stream.shutdown().await;
        }
    });
    let environment = HashMap::from([
        ("HTTP_PROXY", proxy_url.clone()),
        ("http_proxy", proxy_url.clone()),
        ("HTTPS_PROXY", proxy_url.clone()),
        ("https_proxy", proxy_url.clone()),
        ("ALL_PROXY", proxy_url.clone()),
        ("all_proxy", proxy_url),
        ("NO_PROXY", String::new()),
        ("no_proxy", String::new()),
    ]);
    let mut process = JsonProcess::spawn(
        &[
            "election",
            "hold",
            "proxy-election",
            "--candidate",
            "agent-a",
            "--ttl",
            "30",
            "--server",
            &server.url,
            "--json",
        ],
        &environment,
    );
    let leader = process.event("leader", Duration::from_secs(5)).await;
    assert_eq!(leader["name"], "proxy-election");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        proxy_hits.load(Ordering::SeqCst),
        0,
        "election hold sent its campaign or leader capability through an ambient proxy"
    );
    process.signal("TERM");
    process.exit(143, Duration::from_secs(5)).await;
    process.assert_secret_free();
    proxy_task.abort();
    server.stop();
}

#[tokio::test]
#[serial_test::serial]
async fn webhook_https_delivery_never_follows_downgrade_or_internal_redirects() {
    let target_hits = Arc::new(AtomicUsize::new(0));
    let counted_hits = Arc::clone(&target_hits);
    let target = MockServer::start(Router::new().route(
        "/downgrade",
        post(move || {
            let hits = Arc::clone(&counted_hits);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                StatusCode::NO_CONTENT
            }
        }),
    ))
    .await;
    let mut private_https =
        PrivateHttpsServer::start_with_redirect_target(Some(target.url.clone())).await;
    let mut server = Server::start_with_ca_bundle(Some(&private_https.ca_bundle)).await;
    let api_client = reqwest::Client::new();

    for redirect_kind in ["downgrade", "internal"] {
        let lock_name = format!("redirect-{redirect_kind}-lock");
        let create_response = api_client
            .post(format!("{}/webhooks", server.url))
            .bearer_auth("test-token")
            .json(&serde_json::json!({
                "url": format!("{}/redirect-{redirect_kind}", private_https.url),
                "events": ["acquired"],
                "lock_pattern": lock_name,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(create_response.status(), reqwest::StatusCode::CREATED);

        let acquire_response = api_client
            .post(format!("{}/locks/{lock_name}/acquire", server.url))
            .bearer_auth("test-token")
            .json(&serde_json::json!({"ttl_seconds": 5}))
            .send()
            .await
            .unwrap();
        assert_eq!(acquire_response.status(), reqwest::StatusCode::OK);

        for _ in 0..2 {
            let request = private_https.request().await;
            assert!(
                request.starts_with(&format!("POST /redirect-{redirect_kind} ")),
                "webhook client followed a forbidden redirect: {request:?}"
            );
        }
    }

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        target_hits.load(Ordering::SeqCst),
        0,
        "HTTPS webhook redirect reached a loopback HTTP target"
    );
    server.stop();
}

#[tokio::test]
#[serial_test::serial]
async fn watch_jsonl_envelopes_are_sequenced_and_honor_reconnect_guidance() {
    let connections = Arc::new(AtomicUsize::new(0));
    let opened_at = Arc::new(Mutex::new(Vec::new()));
    let server = watch_reconnect_mock(Arc::clone(&connections), Arc::clone(&opened_at)).await;
    let mut process = JsonProcess::spawn(
        &[
            "election",
            "watch",
            "watch-room",
            "--server",
            &server.url,
            "--json",
        ],
        &HashMap::new(),
    );

    let first = process.event("snapshot", Duration::from_secs(2)).await;
    assert_eq!(first["schema_version"], 1);
    assert_eq!(first["sequence"], 1);
    assert_eq!(first["kind"], "election");
    assert_eq!(first["name"], "watch-room");
    assert_eq!(first["server"], server.url);
    assert_eq!(first["snapshot"]["status"], "vacant");
    assert!(first["observed_at"].is_string());

    let deadline = Instant::now() + Duration::from_secs(3);
    while connections.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < deadline, "watch did not reconnect");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    process.signal("INT");
    process.exit(130, Duration::from_secs(2)).await;

    let opened_at = opened_at.lock().unwrap();
    assert!(
        opened_at[1].duration_since(opened_at[0]) >= Duration::from_millis(400),
        "SSE retry guidance must be a minimum reconnect delay"
    );
    let snapshots = process
        .lines
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|value| value["event"] == "snapshot")
        .collect::<Vec<_>>();
    assert!(
        snapshots.len() >= 3,
        "expected initial and signaled snapshots"
    );
    for (index, snapshot) in snapshots.iter().enumerate() {
        assert_eq!(snapshot["schema_version"], 1);
        assert_eq!(snapshot["sequence"], (index + 1) as u64);
        assert_eq!(snapshot["kind"], "election");
        assert_eq!(snapshot["name"], "watch-room");
        assert_eq!(snapshot["snapshot"]["status"], "vacant");
    }
    process.assert_secret_free();
}

#[tokio::test]
#[serial_test::serial]
async fn lock_hold_jsonl_distinguishes_held_from_delayed() {
    for expected_reason in ["held", "delayed"] {
        let server = lock_waiting_mock(expected_reason).await;
        let directory = tempfile::tempdir().unwrap();
        let environment = HashMap::from([
            ("OCTOSTORE_URL", server.url.clone()),
            ("OCTOSTORE_TOKEN_FILE", token_file(directory.path())),
        ]);
        let mut process = JsonProcess::spawn(
            &[
                "lock",
                "hold",
                "wait-state-lock",
                "--ttl",
                "5",
                "--acquire-timeout",
                "2s",
                "--json",
            ],
            &environment,
        );

        let waiting = process.event("waiting", Duration::from_secs(2)).await;
        assert_eq!(waiting["schema_version"], 1);
        assert_eq!(waiting["kind"], "lock");
        assert_eq!(waiting["name"], "wait-state-lock");
        assert_eq!(waiting["reason_code"], expected_reason);
        assert!(waiting["retry_after_ms"].as_u64().unwrap() >= 100);
        let error = process.event("error", Duration::from_secs(3)).await;
        assert_eq!(error["reason_code"], "acquire_timeout");
        process.exit(11, Duration::from_secs(2)).await;
        process.assert_secret_free();
    }
}

#[tokio::test]
#[serial_test::serial]
async fn acquisition_timeout_caps_headers_body_and_decode_for_both_primitives() {
    for (stall_name, stall) in [
        ("election headers", ResponseStall::Headers),
        ("election body", ResponseStall::Body),
    ] {
        let server = election_mock(stall, false).await;
        let mut process = JsonProcess::spawn(
            &[
                "election",
                "hold",
                "deadline-room",
                "--candidate",
                "deadline-agent",
                "--ttl",
                "5",
                "--acquire-timeout",
                "300ms",
                "--request-timeout",
                "10s",
                "--server",
                &server.url,
                "--json",
            ],
            &HashMap::new(),
        );
        let started = Instant::now();
        let event = process.event("error", Duration::from_secs(2)).await;
        assert_eq!(event["reason_code"], "acquire_timeout");
        process.exit(11, Duration::from_secs(2)).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{stall_name} exceeded the bounded acquisition timeout"
        );
        assert!(!process
            .lines
            .iter()
            .any(|line| line.contains("\"event\":\"leader\"")));
        process.assert_absent(&["known-election-capability"]);
    }

    for (stall_name, stall) in [
        ("lock headers", ResponseStall::Headers),
        ("lock body", ResponseStall::Body),
    ] {
        let server = lock_mock(stall, false).await;
        let directory = tempfile::tempdir().unwrap();
        let token = token_file(directory.path());
        let environment = HashMap::from([
            ("OCTOSTORE_URL", server.url.clone()),
            ("OCTOSTORE_TOKEN_FILE", token),
        ]);
        let mut process = JsonProcess::spawn(
            &[
                "lock",
                "hold",
                "deadline-lock",
                "--ttl",
                "5",
                "--acquire-timeout",
                "300ms",
                "--request-timeout",
                "10s",
                "--json",
            ],
            &environment,
        );
        let started = Instant::now();
        let event = process.event("error", Duration::from_secs(2)).await;
        assert_eq!(event["reason_code"], "acquire_timeout");
        process.exit(11, Duration::from_secs(2)).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{stall_name} exceeded the bounded acquisition timeout"
        );
        assert!(!process
            .lines
            .iter()
            .any(|line| line.contains("\"event\":\"acquired\"")));
        process.assert_absent(&[KNOWN_LEASE_ID, "test-token"]);
    }
}

#[tokio::test]
#[serial_test::serial]
async fn cli_bounds_chunked_success_and_error_response_bodies() {
    let server = oversized_api_response_mock().await;
    for election in ["oversized-success", "oversized-error"] {
        let output = tokio::process::Command::new(BINARY)
            .args([
                "election",
                "status",
                election,
                "--server",
                &server.url,
                "--request-timeout",
                "2s",
            ])
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(70));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("server response exceeded 1048576 bytes"),
            "oversized {election} response was not rejected: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn malicious_zero_server_timings_fail_closed_without_request_storms() {
    let renewals = Arc::new(AtomicUsize::new(0));
    let election_server = zero_timing_election_mock(Arc::clone(&renewals)).await;
    let mut election = JsonProcess::spawn(
        &[
            "election",
            "hold",
            "zero-timing",
            "--candidate",
            "timing-agent",
            "--ttl",
            "5",
            "--server",
            &election_server.url,
            "--json",
        ],
        &HashMap::new(),
    );
    election.event("leader", Duration::from_secs(2)).await;
    tokio::time::sleep(Duration::from_millis(450)).await;
    let renewal_count = renewals.load(Ordering::SeqCst);
    assert!(
        (1..=5).contains(&renewal_count),
        "renewal storm: {renewal_count}"
    );
    election.signal("TERM");
    election.exit(143, Duration::from_secs(2)).await;

    let lock_server = zero_session_interval_mock().await;
    let directory = tempfile::tempdir().unwrap();
    let token_path = token_file(directory.path());
    let output = tokio::process::Command::new(BINARY)
        .args([
            "lock",
            "hold",
            "zero-session",
            "--server",
            &lock_server.url,
            "--json",
        ])
        .env("OCTOSTORE_TOKEN_FILE", token_path)
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(70));
    assert!(String::from_utf8_lossy(&output.stderr).contains("keepalive_interval_secs"));
}

#[tokio::test]
#[serial_test::serial]
async fn late_or_mismatched_initial_authority_is_never_emitted() {
    let server =
        delayed_election_authority_mock(Duration::from_millis(4_500), "deadline-agent").await;
    let mut election = JsonProcess::spawn(
        &[
            "election",
            "hold",
            "deadline-room",
            "--candidate",
            "deadline-agent",
            "--ttl",
            "5",
            "--request-timeout",
            "10s",
            "--server",
            &server.url,
            "--json",
        ],
        &HashMap::new(),
    );
    let uncertain = election.event("uncertain", Duration::from_secs(6)).await;
    assert_eq!(
        uncertain["reason_code"],
        "initial_authority_deadline_exceeded"
    );
    election.exit(20, Duration::from_secs(2)).await;
    assert!(!election
        .lines
        .iter()
        .any(|line| line.contains("\"event\":\"leader\"")));

    let server = delayed_lock_authority_mock(Duration::from_millis(1_100)).await;
    let directory = tempfile::tempdir().unwrap();
    let environment = HashMap::from([
        ("OCTOSTORE_URL", server.url.clone()),
        ("OCTOSTORE_TOKEN_FILE", token_file(directory.path())),
    ]);
    let mut lock = JsonProcess::spawn(
        &[
            "lock",
            "hold",
            "late-lock",
            "--ttl",
            "1",
            "--request-timeout",
            "10s",
            "--json",
        ],
        &environment,
    );
    let uncertain = lock.event("uncertain", Duration::from_secs(3)).await;
    assert_eq!(
        uncertain["reason_code"],
        "initial_authority_deadline_exceeded"
    );
    lock.exit(20, Duration::from_secs(3)).await;
    assert!(!lock
        .lines
        .iter()
        .any(|line| line.contains("\"event\":\"acquired\"")));

    let server = delayed_election_authority_mock(Duration::ZERO, "intruder").await;
    let mut mismatch = JsonProcess::spawn(
        &[
            "election",
            "hold",
            "deadline-room",
            "--candidate",
            "expected-agent",
            "--ttl",
            "5",
            "--server",
            &server.url,
            "--json",
        ],
        &HashMap::new(),
    );
    let uncertain = mismatch.event("uncertain", Duration::from_secs(2)).await;
    assert_eq!(uncertain["reason_code"], "authority_identity_mismatch");
    mismatch.exit(20, Duration::from_secs(2)).await;
    assert!(!mismatch
        .lines
        .iter()
        .any(|line| line.contains("\"event\":\"leader\"")));
}

#[tokio::test]
#[serial_test::serial]
async fn lock_retry_guidance_is_independent_from_session_keepalive() {
    let acquisitions = Arc::new(AtomicUsize::new(0));
    let keepalives = Arc::new(AtomicUsize::new(0));
    let server = guided_lock_wait_mock(Arc::clone(&acquisitions), Arc::clone(&keepalives)).await;
    let directory = tempfile::tempdir().unwrap();
    let environment = HashMap::from([
        ("OCTOSTORE_URL", server.url.clone()),
        ("OCTOSTORE_TOKEN_FILE", token_file(directory.path())),
    ]);
    let mut lock = JsonProcess::spawn(
        &[
            "lock",
            "hold",
            "guided-lock",
            "--ttl",
            "5",
            "--acquire-timeout",
            "1800ms",
            "--json",
        ],
        &environment,
    );
    let waiting = lock.event("waiting", Duration::from_secs(2)).await;
    assert!(waiting["retry_after_ms"].as_u64().unwrap() >= 2_500);
    let error = lock.event("error", Duration::from_secs(3)).await;
    assert_eq!(error["reason_code"], "acquire_timeout");
    lock.exit(11, Duration::from_secs(2)).await;
    assert_eq!(acquisitions.load(Ordering::SeqCst), 1);
    assert!(
        keepalives.load(Ordering::SeqCst) >= 1,
        "session keepalive should run without authorizing an early acquisition retry"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn mismatched_session_keepalive_fails_closed_before_acquisition() {
    let server = mismatched_session_keepalive_mock(SessionMismatchPhase::BeforeAcquisition).await;
    let directory = tempfile::tempdir().unwrap();
    let environment = HashMap::from([
        ("OCTOSTORE_URL", server.url.clone()),
        ("OCTOSTORE_TOKEN_FILE", token_file(directory.path())),
    ]);
    let mut lock = JsonProcess::spawn(
        &[
            "lock",
            "hold",
            "mismatched-session-before-acquire",
            "--ttl",
            "5",
            "--json",
        ],
        &environment,
    );

    let _ = lock.event("waiting", Duration::from_secs(2)).await;
    let uncertain = lock.event("uncertain", Duration::from_secs(3)).await;
    assert_eq!(uncertain["reason_code"], "session_identity_mismatch");
    lock.exit(20, Duration::from_secs(2)).await;
    assert!(!lock
        .lines
        .iter()
        .any(|line| line.contains("\"event\":\"acquired\"")));
    lock.assert_absent(&[KNOWN_SESSION_ID, MISMATCHED_SESSION_ID, "test-token"]);
}

#[tokio::test]
#[serial_test::serial]
async fn mismatched_session_keepalive_fails_closed_after_acquisition() {
    let server = mismatched_session_keepalive_mock(SessionMismatchPhase::AfterAcquisition).await;
    let directory = tempfile::tempdir().unwrap();
    let environment = HashMap::from([
        ("OCTOSTORE_URL", server.url.clone()),
        ("OCTOSTORE_TOKEN_FILE", token_file(directory.path())),
    ]);
    let mut lock = JsonProcess::spawn(
        &[
            "lock",
            "hold",
            "mismatched-session-after-acquire",
            "--ttl",
            "5",
            "--json",
        ],
        &environment,
    );

    let acquired = lock.event("acquired", Duration::from_secs(2)).await;
    assert!(acquired["authority_remaining_ms"].as_u64().unwrap() > 0);
    assert!(acquired["authority_observed_unix_ms"].as_i64().unwrap() > 0);
    assert!(
        acquired["authority_observed_continuous_ms"]
            .as_u64()
            .unwrap()
            > 0
    );
    let lost = lock.event("lost", Duration::from_secs(3)).await;
    assert_eq!(lost["reason_code"], "session_identity_mismatch");
    lock.exit(20, Duration::from_secs(2)).await;
    assert!(!lock
        .lines
        .iter()
        .any(|line| line.contains("\"event\":\"renewed\"")));
    lock.assert_absent(&[
        KNOWN_SESSION_ID,
        MISMATCHED_SESSION_ID,
        KNOWN_LEASE_ID,
        "test-token",
    ]);
}

#[tokio::test]
#[serial_test::serial]
async fn retryable_session_creation_is_retried_before_lock_acquisition() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let server = retrying_session_mock(Arc::clone(&attempts)).await;
    let directory = tempfile::tempdir().unwrap();
    let environment = HashMap::from([
        ("OCTOSTORE_URL", server.url.clone()),
        ("OCTOSTORE_TOKEN_FILE", token_file(directory.path())),
    ]);
    let mut lock = JsonProcess::spawn(
        &["lock", "hold", "retry-session-lock", "--ttl", "5", "--json"],
        &environment,
    );
    let waiting = lock.event("waiting", Duration::from_secs(2)).await;
    assert_eq!(waiting["reason_code"], "upstream_unavailable");
    let acquired = lock.event("acquired", Duration::from_secs(3)).await;
    assert!(acquired["authority_remaining_ms"].as_u64().unwrap() > 0);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    lock.signal("INT");
    let _ = lock.event("released", Duration::from_secs(3)).await;
    lock.exit(130, Duration::from_secs(2)).await;
}

#[tokio::test]
#[serial_test::serial]
async fn watch_is_signal_aware_and_rejects_oversized_unterminated_frames() {
    for failure in [WatchFailure::StalledHeaders, WatchFailure::StalledErrorBody] {
        let server = pathological_watch_mock(failure).await;
        let mut watch = JsonProcess::spawn(
            &[
                "election",
                "watch",
                "watch-room",
                "--request-timeout",
                "10s",
                "--server",
                &server.url,
                "--json",
            ],
            &HashMap::new(),
        );
        let _ = watch.event("snapshot", Duration::from_secs(2)).await;
        let started = Instant::now();
        watch.signal("TERM");
        watch.exit(143, Duration::from_secs(2)).await;
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    let server = pathological_watch_mock(WatchFailure::OversizedFrame).await;
    let mut watch = JsonProcess::spawn(
        &[
            "election",
            "watch",
            "watch-room",
            "--server",
            &server.url,
            "--json",
        ],
        &HashMap::new(),
    );
    let _ = watch.event("snapshot", Duration::from_secs(2)).await;
    watch.exit(70, Duration::from_secs(2)).await;
    assert!(watch
        .stderr_lines
        .iter()
        .any(|line| line.contains("SSE frame exceeded 65536 bytes")));
}

#[tokio::test]
#[serial_test::serial]
async fn renewal_deadline_caps_stalled_headers_and_bodies_for_both_primitives() {
    for stall in [ResponseStall::Headers, ResponseStall::Body] {
        let server = election_mock(stall, true).await;
        let mut process = JsonProcess::spawn(
            &[
                "election",
                "hold",
                "deadline-room",
                "--candidate",
                "deadline-agent",
                "--ttl",
                "5",
                "--request-timeout",
                "10s",
                "--server",
                &server.url,
                "--json",
            ],
            &HashMap::new(),
        );
        let _ = process.event("leader", Duration::from_secs(2)).await;
        assert!(!process.command_line().contains("known-election-capability"));
        let uncertain = process.event("uncertain", Duration::from_secs(6)).await;
        assert_eq!(uncertain["reason_code"], "renewal_deadline_exceeded");
        process.exit(20, Duration::from_secs(2)).await;
        process.assert_absent(&["known-election-capability"]);
    }

    for stall in [ResponseStall::Headers, ResponseStall::Body] {
        let server = lock_mock(stall, true).await;
        let directory = tempfile::tempdir().unwrap();
        let token = token_file(directory.path());
        let environment = HashMap::from([
            ("OCTOSTORE_URL", server.url.clone()),
            ("OCTOSTORE_TOKEN_FILE", token),
        ]);
        let mut process = JsonProcess::spawn(
            &[
                "lock",
                "hold",
                "deadline-lock",
                "--ttl",
                "5",
                "--request-timeout",
                "10s",
                "--json",
            ],
            &environment,
        );
        let _ = process.event("acquired", Duration::from_secs(2)).await;
        assert!(!process.command_line().contains(KNOWN_LEASE_ID));
        let uncertain = process.event("uncertain", Duration::from_secs(6)).await;
        assert_eq!(uncertain["reason_code"], "renewal_deadline_exceeded");
        process.exit(20, Duration::from_secs(4)).await;
        process.assert_absent(&[KNOWN_LEASE_ID, "test-token"]);
    }
}

#[tokio::test]
#[serial_test::serial]
async fn long_lock_ttl_cannot_outlive_session_confirmation_deadline() {
    let server = session_deadline_mock().await;
    let directory = tempfile::tempdir().unwrap();
    let token = token_file(directory.path());
    let environment = HashMap::from([
        ("OCTOSTORE_URL", server.url.clone()),
        ("OCTOSTORE_TOKEN_FILE", token),
    ]);
    let mut process = JsonProcess::spawn(
        &[
            "lock",
            "hold",
            "long-session-lock",
            "--ttl",
            "3600",
            "--request-timeout",
            "30s",
            "--json",
        ],
        &environment,
    );
    let started = Instant::now();
    let _ = process.event("acquired", Duration::from_secs(2)).await;
    let uncertain = process.event("uncertain", Duration::from_secs(27)).await;
    assert_eq!(
        uncertain["reason_code"],
        "session_confirmation_deadline_exceeded"
    );
    assert!(started.elapsed() < Duration::from_secs(27));
    process.exit(20, Duration::from_secs(4)).await;
    process.assert_absent(&[KNOWN_LEASE_ID, "test-token"]);
}

#[tokio::test]
#[serial_test::serial]
async fn confirmed_stale_lease_exits_lost_without_reacquisition() {
    let campaigns = Arc::new(AtomicUsize::new(0));
    let server = stale_election_mock(Arc::clone(&campaigns)).await;
    let mut election = JsonProcess::spawn(
        &[
            "election",
            "hold",
            "deadline-room",
            "--candidate",
            "deadline-agent",
            "--ttl",
            "5",
            "--server",
            &server.url,
            "--json",
        ],
        &HashMap::new(),
    );
    let _ = election.event("leader", Duration::from_secs(2)).await;
    let lost = election.event("lost", Duration::from_secs(2)).await;
    assert_eq!(lost["reason_code"], "lease_not_current");
    election.exit(20, Duration::from_secs(2)).await;
    assert_eq!(campaigns.load(Ordering::SeqCst), 1);

    let acquisitions = Arc::new(AtomicUsize::new(0));
    let server = stale_lock_mock(Arc::clone(&acquisitions)).await;
    let directory = tempfile::tempdir().unwrap();
    let environment = HashMap::from([
        ("OCTOSTORE_URL", server.url.clone()),
        ("OCTOSTORE_TOKEN_FILE", token_file(directory.path())),
    ]);
    let mut lock = JsonProcess::spawn(
        &["lock", "hold", "stale-lock", "--ttl", "5", "--json"],
        &environment,
    );
    let _ = lock.event("acquired", Duration::from_secs(2)).await;
    let lost = lock.event("lost", Duration::from_secs(2)).await;
    assert_eq!(lost["reason_code"], "lease_not_current");
    lock.exit(20, Duration::from_secs(4)).await;
    assert_eq!(acquisitions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[serial_test::serial]
async fn signal_cleanup_uses_one_two_second_total_budget() {
    let server = shutdown_election_mock().await;
    let mut election = JsonProcess::spawn(
        &[
            "election",
            "hold",
            "deadline-room",
            "--candidate",
            "deadline-agent",
            "--ttl",
            "300",
            "--shutdown-timeout",
            "10s",
            "--server",
            &server.url,
            "--json",
        ],
        &HashMap::new(),
    );
    let _ = election.event("leader", Duration::from_secs(2)).await;
    let started = Instant::now();
    election.signal("INT");
    let uncertain = election.event("uncertain", Duration::from_secs(3)).await;
    assert_eq!(uncertain["reason_code"], "release_unconfirmed");
    election.exit(130, Duration::from_secs(2)).await;
    assert!(started.elapsed() < Duration::from_secs(3));

    let server = shutdown_lock_mock().await;
    let directory = tempfile::tempdir().unwrap();
    let environment = HashMap::from([
        ("OCTOSTORE_URL", server.url.clone()),
        ("OCTOSTORE_TOKEN_FILE", token_file(directory.path())),
    ]);
    let mut lock = JsonProcess::spawn(
        &[
            "lock",
            "hold",
            "shutdown-lock",
            "--ttl",
            "120",
            "--shutdown-timeout",
            "10s",
            "--json",
        ],
        &environment,
    );
    let _ = lock.event("acquired", Duration::from_secs(2)).await;
    let started = Instant::now();
    lock.signal("INT");
    let uncertain = lock.event("uncertain", Duration::from_secs(3)).await;
    assert_eq!(uncertain["reason_code"], "release_unconfirmed");
    lock.exit(130, Duration::from_secs(2)).await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "lock release and session cleanup must share one budget"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn election_release_requires_matching_vacant_term_confirmation() {
    let server = mismatched_resign_mock().await;
    let mut election = JsonProcess::spawn(
        &[
            "election",
            "hold",
            "deadline-room",
            "--candidate",
            "deadline-agent",
            "--ttl",
            "300",
            "--server",
            &server.url,
            "--json",
        ],
        &HashMap::new(),
    );
    let _ = election.event("leader", Duration::from_secs(2)).await;
    election.signal("INT");
    let uncertain = election.event("uncertain", Duration::from_secs(2)).await;
    assert_eq!(uncertain["reason_code"], "release_unconfirmed");
    election.exit(130, Duration::from_secs(2)).await;
    election.assert_absent(&["known-election-capability"]);
}

#[tokio::test]
#[serial_test::serial]
async fn election_hold_times_out_then_fails_over_with_signal_contract() {
    let server = Server::start().await;
    let environment = HashMap::new();
    let mut leader = JsonProcess::spawn(
        &[
            "election",
            "hold",
            "shared-e2e-room",
            "--candidate",
            "agent-a",
            "--ttl",
            "5",
            "--server",
            &server.url,
            "--json",
        ],
        &environment,
    );
    let first = leader.event("leader", Duration::from_secs(5)).await;
    assert!(first["authority_remaining_ms"].as_u64().unwrap() > 0);
    assert!(first["authority_observed_unix_ms"].as_i64().unwrap() > 0);
    assert!(first["authority_observed_continuous_ms"].as_u64().unwrap() > 0);

    let mut timed_out = JsonProcess::spawn(
        &[
            "election",
            "hold",
            "shared-e2e-room",
            "--candidate",
            "agent-timeout",
            "--ttl",
            "5",
            "--acquire-timeout",
            "1s",
            "--server",
            &server.url,
            "--json",
        ],
        &environment,
    );
    let _ = timed_out.event("waiting", Duration::from_secs(5)).await;
    let _ = timed_out.event("error", Duration::from_secs(5)).await;
    timed_out.exit(11, Duration::from_secs(3)).await;

    let mut follower = JsonProcess::spawn(
        &[
            "election",
            "hold",
            "shared-e2e-room",
            "--candidate",
            "agent-b",
            "--ttl",
            "5",
            "--server",
            &server.url,
            "--json",
        ],
        &environment,
    );
    let _ = follower.event("waiting", Duration::from_secs(5)).await;
    leader.signal("INT");
    let _ = leader.event("released", Duration::from_secs(5)).await;
    leader.exit(130, Duration::from_secs(3)).await;

    let replacement = follower.event("leader", Duration::from_secs(8)).await;
    assert!(replacement["term"].as_u64().unwrap() > first["term"].as_u64().unwrap());
    assert!(replacement["authority_remaining_ms"].as_u64().unwrap() > 0);
    follower.signal("TERM");
    let _ = follower.event("released", Duration::from_secs(5)).await;
    follower.exit(143, Duration::from_secs(3)).await;
    leader.assert_secret_free();
    follower.assert_secret_free();
}

#[tokio::test]
#[serial_test::serial]
async fn election_hold_exits_uncertain_after_authority_disappears() {
    let mut server = Server::start().await;
    let mut hold = JsonProcess::spawn(
        &[
            "election",
            "hold",
            "uncertain-e2e-room",
            "--candidate",
            "agent-a",
            "--ttl",
            "5",
            "--request-timeout",
            "1s",
            "--server",
            &server.url,
            "--json",
        ],
        &HashMap::new(),
    );
    let _ = hold.event("leader", Duration::from_secs(5)).await;
    server.stop();
    let _ = hold.event("uncertain", Duration::from_secs(8)).await;
    hold.exit(20, Duration::from_secs(3)).await;
    assert_eq!(
        hold.lines
            .iter()
            .filter(|line| line.contains("\"event\":\"leader\""))
            .count(),
        1,
        "a hold process must never reacquire after uncertainty"
    );
    hold.assert_secret_free();
}

#[tokio::test]
#[serial_test::serial]
async fn same_token_lock_holds_contend_and_fail_over_by_session() {
    let server = Server::start().await;
    let token = token_file(server._directory.path());
    let environment = HashMap::from([
        ("OCTOSTORE_URL", server.url.clone()),
        ("OCTOSTORE_TOKEN_FILE", token),
    ]);
    let arguments = [
        "lock",
        "hold",
        "repo/octostore/issue-1842",
        "--ttl",
        "5",
        "--json",
    ];
    let mut owner = JsonProcess::spawn(&arguments, &environment);
    let first = owner.event("acquired", Duration::from_secs(5)).await;
    let mut waiter = JsonProcess::spawn(&arguments, &environment);
    let _ = waiter.event("waiting", Duration::from_secs(5)).await;

    owner.signal("INT");
    let _ = owner.event("released", Duration::from_secs(5)).await;
    owner.exit(130, Duration::from_secs(3)).await;
    let replacement = waiter.event("acquired", Duration::from_secs(8)).await;
    assert!(replacement["term"].as_u64().unwrap() > first["term"].as_u64().unwrap());

    waiter.signal("TERM");
    let _ = waiter.event("released", Duration::from_secs(5)).await;
    waiter.exit(143, Duration::from_secs(3)).await;
    owner.assert_secret_free();
    waiter.assert_secret_free();
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_gates_worker_and_cancels_it_on_uncertainty() {
    use std::os::unix::fs::PermissionsExt;
    let mut server = Server::start().await;
    let worker = server._directory.path().join("worker.sh");
    let marker = server._directory.path().join("worker.pid");
    std::fs::write(
        &worker,
        format!(
            "#!/bin/sh\necho $$ > '{}'\ntrap 'exit 0' TERM INT\nwhile :; do sleep 1; done\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let mut process = Command::new(supervisor)
        .args([
            "election",
            "supervised-e2e-room",
            "agent-a",
            worker.to_str().unwrap(),
            "--",
            BINARY,
            "election",
            "hold",
            "supervised-e2e-room",
            "--candidate",
            "agent-a",
            "--ttl",
            "5",
            "--request-timeout",
            "1s",
            "--server",
            &server.url,
            "--json",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(
            process.try_wait().unwrap().is_none(),
            "supervisor exited before worker start"
        );
        assert!(
            Instant::now() < deadline,
            "worker did not start after acquisition"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let worker_pid = std::fs::read_to_string(&marker).unwrap();
    server.stop();
    let status = wait_for_child(&mut process, Duration::from_secs(8)).await;
    assert_eq!(status.code(), Some(20));

    let worker_status = Command::new("kill")
        .args(["-0", worker_pid.trim()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(
        !worker_status.success(),
        "worker remained alive after uncertainty"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_rejects_candidate_and_sequence_mismatches() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("worker.sh");
    let marker = directory.path().join("worker-started");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntouch '{}'\nwhile :; do sleep 1; done\n",
            marker.display()
        ),
    );

    let wrong_candidate = directory.path().join("wrong-candidate.sh");
    write_executable(
        &wrong_candidate,
        "#!/bin/sh\nprintf '%s\\n' '{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"intruder\",\"term\":7,\"authority_remaining_ms\":3000}'\nsleep 30\n",
    );
    let output = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "expected-agent",
            worker.to_str().unwrap(),
            "--",
            wrong_candidate.to_str().unwrap(),
            "--ttl",
            "5",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(70));
    assert!(!marker.exists(), "candidate mismatch must not start work");

    let bad_sequence = directory.path().join("bad-sequence.sh");
    write_executable(
        &bad_sequence,
        "#!/bin/sh\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"expected-agent\",\"term\":7,\"authority_remaining_ms\":3000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}\\n' \"$observed_ms\" \"$continuous_ms\"\nprintf '%s\\n' '{\"schema_version\":1,\"sequence\":1,\"event\":\"renewed\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"expected-agent\",\"term\":7,\"authority_remaining_ms\":3000}'\nsleep 30\n",
    );
    let output = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "expected-agent",
            worker.to_str().unwrap(),
            "--",
            bad_sequence.to_str().unwrap(),
            "--ttl",
            "5",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(70));
    assert!(String::from_utf8_lossy(&output.stderr).contains("sequence did not increase"));
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_treats_release_as_terminal_and_caps_overrides() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("worker.sh");
    let starts = directory.path().join("worker-starts");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\necho start >> '{}'\ntrap 'exit 0' TERM INT\nwhile :; do sleep 1; done\n",
            starts.display()
        ),
    );
    let hold = directory.path().join("release-then-reacquire.sh");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":7,\"authority_remaining_ms\":3000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile [ ! -e '{}' ]; do sleep 0.01; done\nprintf '%s\\n' '{{\"schema_version\":1,\"sequence\":2,\"event\":\"released\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":7}}'\nprintf '%s\\n' '{{\"schema_version\":1,\"sequence\":3,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":8,\"authority_remaining_ms\":3000}}'\n",
            starts.display()
        ),
    );
    let status = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0));
    let start_count = std::fs::read_to_string(&starts).unwrap().lines().count();
    assert_eq!(
        start_count, 1,
        "release must prevent later reacquisition events"
    );

    let status = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_MAX_SILENCE_SECONDS", "21")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(64));
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_rejects_subsecond_budget_before_worker_start() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("worker.sh");
    let marker = directory.path().join("worker-started");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntouch '{}'\nwhile :; do sleep 1; done\n",
            marker.display()
        ),
    );
    let hold = directory.path().join("subsecond-hold.sh");
    write_executable(
        &hold,
        "#!/bin/sh\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{\"schema_version\":1,\"sequence\":1,\"event\":\"acquired\",\"kind\":\"lock\",\"name\":\"room\",\"term\":17,\"authority_remaining_ms\":999,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}\\n' \"$observed_ms\" \"$continuous_ms\"\nsleep 30\n",
    );

    let output = Command::new(&supervisor)
        .args([
            "lock",
            "room",
            "-",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(20));
    assert!(
        !marker.exists(),
        "an unusable budget must gate worker launch"
    );
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("without a usable remaining safety budget"));
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_rejects_missing_future_and_oversized_authority_freshness() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let marker = directory.path().join("invalid-freshness-worker-started");
    let worker = directory.path().join("invalid-freshness-worker.sh");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntouch '{}'\nwhile :; do sleep 1; done\n",
            marker.display()
        ),
    );
    let cases = [
        (
            "missing",
            "#!/bin/sh\nprintf '%s\\n' '{\"schema_version\":1,\"sequence\":1,\"event\":\"acquired\",\"kind\":\"lock\",\"name\":\"room\",\"term\":33,\"authority_remaining_ms\":3000}'\nsleep 30\n",
            70,
        ),
        (
            "future",
            "#!/bin/sh\nobserved_ms=$(($(jq -nr 'now * 1000 | floor') + 60000))\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{\"schema_version\":1,\"sequence\":1,\"event\":\"acquired\",\"kind\":\"lock\",\"name\":\"room\",\"term\":33,\"authority_remaining_ms\":3000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}\\n' \"$observed_ms\" \"$continuous_ms\"\nsleep 30\n",
            20,
        ),
        (
            "oversized",
            "#!/bin/sh\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{\"schema_version\":1,\"sequence\":1,\"event\":\"acquired\",\"kind\":\"lock\",\"name\":\"room\",\"term\":33,\"authority_remaining_ms\":300001,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}\\n' \"$observed_ms\" \"$continuous_ms\"\nsleep 30\n",
            20,
        ),
        (
            "string",
            "#!/bin/sh\nprintf '%s\\n' '{\"schema_version\":1,\"sequence\":1,\"event\":\"acquired\",\"kind\":\"lock\",\"name\":\"room\",\"term\":33,\"authority_remaining_ms\":\"3000\",\"authority_observed_unix_ms\":\"1700000000000\",\"authority_observed_continuous_ms\":\"42000\"}'\nsleep 30\n",
            70,
        ),
    ];

    for (label, script, expected_code) in cases {
        let hold = directory.path().join(format!("{label}-freshness-hold.sh"));
        write_executable(&hold, script);
        let output = Command::new(&supervisor)
            .args([
                "lock",
                "room",
                "-",
                worker.to_str().unwrap(),
                "--",
                hold.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(expected_code),
            "{label} authority freshness used the wrong exit contract"
        );
        assert!(
            !marker.exists(),
            "{label} authority freshness started protected work"
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_rejects_initial_authority_that_expired_in_the_event_pipe() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("stale-event-worker.sh");
    let worker_instruction = directory.path().join("worker-instruction-started");
    let worker_descendant = directory.path().join("worker-descendant-started");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntouch '{}'\n( touch '{}'; while :; do sleep 1; done ) &\nwhile :; do sleep 1; done\n",
            worker_instruction.display(),
            worker_descendant.display()
        ),
    );
    let emitted = directory.path().join("authority-event-emitted");
    let hold = directory.path().join("stale-event-hold.sh");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":31,\"authority_remaining_ms\":1600,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\ntouch '{}'\nprintf '%s\\n' '{{\"schema_version\":1,\"sequence\":2,\"event\":\"uncertain\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":31}}'\nsleep 30\n",
            emitted.display()
        ),
    );
    let pre_read = directory.path().join("pre-read");
    std::fs::create_dir(&pre_read).unwrap();
    let ready = pre_read.join("ready");
    let continue_file = pre_read.join("continue");

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TEST_PRE_READ_DIR", &pre_read)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let queued_deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() || !emitted.exists() {
        assert!(
            process.try_wait().unwrap().is_none(),
            "supervisor exited before the initial authority event was queued"
        );
        assert!(
            Instant::now() < queued_deadline,
            "initial authority event was not queued behind the paused reader"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    std::fs::write(&continue_file, "continue\n").unwrap();

    let status = wait_for_child(&mut process, Duration::from_secs(4)).await;
    assert_eq!(status.code(), Some(20));
    assert!(
        !worker_instruction.exists(),
        "a stale initial authority event started the worker instruction"
    );
    assert!(
        !worker_descendant.exists(),
        "a stale initial authority event started a worker descendant"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_revalidates_initial_authority_inside_the_watchdog() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("pre-monitor-worker.sh");
    let worker_instruction = directory.path().join("pre-monitor-worker-started");
    let worker_descendant = directory.path().join("pre-monitor-descendant-started");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntouch '{}'\n( touch '{}'; while :; do sleep 1; done ) &\nwhile :; do sleep 1; done\n",
            worker_instruction.display(),
            worker_descendant.display()
        ),
    );
    let hold = directory.path().join("pre-monitor-hold.sh");
    write_executable(
        &hold,
        "#!/bin/sh\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":34,\"authority_remaining_ms\":1600,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}\\n' \"$observed_ms\" \"$continuous_ms\"\nsleep 30\n",
    );
    let pre_monitor = directory.path().join("pre-monitor");
    std::fs::create_dir(&pre_monitor).unwrap();
    let ready = pre_monitor.join("ready");
    let continue_file = pre_monitor.join("continue");

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TEST_PRE_MONITOR_DIR", &pre_monitor)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        assert!(
            process.try_wait().unwrap().is_none(),
            "supervisor exited before accepting initial authority"
        );
        assert!(
            Instant::now() < ready_deadline,
            "supervisor never exposed the pre-watchdog interval"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    std::fs::write(&continue_file, "continue\n").unwrap();

    let status = wait_for_child(&mut process, Duration::from_secs(4)).await;
    assert_eq!(status.code(), Some(20));
    assert!(
        !worker_instruction.exists(),
        "authority that expired before watchdog arming started the worker"
    );
    assert!(
        !worker_descendant.exists(),
        "authority that expired before watchdog arming started a descendant"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_partial_pre_monitor_pause_preserves_emitted_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("partial-pause-worker.sh");
    let worker_started = directory.path().join("partial-pause-worker-started");
    let worker_after_deadline = directory.path().join("partial-pause-worker-after-deadline");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\ntouch '{}'\nsleep 3\ntouch '{}'\nwhile :; do sleep 1; done\n",
            worker_started.display(),
            worker_after_deadline.display()
        ),
    );
    let hold = directory.path().join("partial-pause-hold.sh");
    let hold_after_deadline = directory.path().join("partial-pause-hold-after-deadline");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":35,\"authority_remaining_ms\":4000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nsleep 4.2\ntouch '{}'\nwhile :; do sleep 1; done\n",
            hold_after_deadline.display()
        ),
    );
    let pre_monitor = directory.path().join("partial-pre-monitor");
    std::fs::create_dir(&pre_monitor).unwrap();
    let ready = pre_monitor.join("ready");
    let continue_file = pre_monitor.join("continue");

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TEST_PRE_MONITOR_DIR", &pre_monitor)
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < ready_deadline,
            "supervisor never exposed the partial pre-monitor pause"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let emitted_deadline_observed = Instant::now();
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    std::fs::write(&continue_file, "continue\n").unwrap();

    let worker_deadline = Instant::now() + Duration::from_secs(1);
    while !worker_started.exists() {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < worker_deadline,
            "fresh authority did not start the worker after the partial pause"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let status = wait_for_child(&mut process, Duration::from_secs(5)).await;
    assert_eq!(status.code(), Some(20));
    assert!(
        emitted_deadline_observed.elapsed() < Duration::from_millis(4_350),
        "partial pause extended containment beyond the original authority deadline"
    );
    assert!(
        !worker_after_deadline.exists(),
        "partial pause extended worker authority beyond the emitted deadline"
    );
    assert!(
        !hold_after_deadline.exists(),
        "partial pause extended hold authority beyond the emitted deadline"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_reports_authority_after_required_worker_readiness() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("worker.sh");
    let worker_started = directory.path().join("worker-started");
    let allow_ready = directory.path().join("allow-ready");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap 'exit 0' TERM INT\ntouch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.01; done\n: >\"$OCTOSTORE_SUPERVISOR_READY_FILE\"\nwhile :; do sleep 1; done\n",
            worker_started.display(),
            allow_ready.display()
        ),
    );
    let hold = directory.path().join("readiness-hold.sh");
    write_executable(
        &hold,
        "#!/bin/sh\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":31,\"authority_remaining_ms\":5000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}\\n' \"$observed_ms\" \"$continuous_ms\"\nsleep 30\n",
    );
    let events = directory.path().join("events.jsonl");
    let event_output = std::fs::File::create(&events).unwrap();
    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_REQUIRE_WORKER_READY", "1")
        .stdout(event_output)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let worker_deadline = Instant::now() + Duration::from_secs(3);
    while !worker_started.exists() {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < worker_deadline,
            "worker did not reach its readiness gate"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !std::fs::read_to_string(&events)
            .unwrap()
            .contains("\"event\":\"leader\""),
        "supervisor published leadership before required worker readiness"
    );

    std::fs::write(&allow_ready, "ready\n").unwrap();
    let event_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if std::fs::read_to_string(&events)
            .unwrap()
            .contains("\"event\":\"leader\"")
        {
            break;
        }
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < event_deadline,
            "supervisor did not publish leadership after worker readiness"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(Command::new("kill")
        .args(["-TERM", &process.id().to_string()])
        .status()
        .unwrap()
        .success());
    let status = wait_for_child(&mut process, Duration::from_secs(3)).await;
    assert_eq!(status.code(), Some(143));
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_gates_worker_before_publishing_handoff() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("worker.sh");
    let side_effect = directory.path().join("worker-side-effect");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntouch '{}'\nwhile :; do sleep 1; done\n",
            side_effect.display()
        ),
    );
    let hold = directory.path().join("tight-budget-hold.sh");
    write_executable(
        &hold,
        "#!/bin/sh\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{\"schema_version\":1,\"sequence\":1,\"event\":\"acquired\",\"kind\":\"lock\",\"name\":\"room\",\"term\":18,\"authority_remaining_ms\":1600,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}\\n' \"$observed_ms\" \"$continuous_ms\"\nsleep 30\n",
    );
    let handoff = directory.path().join("handoff");
    std::fs::create_dir(&handoff).unwrap();
    let ready = handoff.join("ready");

    let mut process = Command::new(&supervisor)
        .args([
            "lock",
            "room",
            "-",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env("OCTOSTORE_SUPERVISOR_TEST_HANDOFF_DIR", &handoff)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        assert!(
            process.try_wait().unwrap().is_none(),
            "supervisor exited before publishing the gated worker group"
        );
        assert!(
            Instant::now() < ready_deadline,
            "supervisor never published the gated worker group"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !side_effect.exists(),
        "worker executed before the handoff was released"
    );
    let worker_group = std::fs::read_to_string(&ready).unwrap();
    let worker_group = worker_group.trim().to_string();

    let stopped = Command::new("kill")
        .args(["-STOP", &process.id().to_string()])
        .status()
        .unwrap();
    assert!(
        stopped.success(),
        "could not pause the supervisor at handoff"
    );
    tokio::time::sleep(Duration::from_millis(2_100)).await;

    assert!(
        !side_effect.exists(),
        "gated work produced a side effect after the authority deadline"
    );
    let worker_status = Command::new("kill")
        .args(["-0", "--", &format!("-{worker_group}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(
        !worker_status.success(),
        "gated worker group survived the watchdog deadline"
    );

    let resumed = Command::new("kill")
        .args(["-CONT", &process.id().to_string()])
        .status()
        .unwrap();
    assert!(resumed.success(), "could not resume the paused supervisor");
    let status = wait_for_child(&mut process, Duration::from_secs(3)).await;
    assert_eq!(status.code(), Some(20));
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_keeps_a_watchdog_during_renewal_handoff() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("renewal-resistant-worker.sh");
    let started = directory.path().join("worker-started");
    let after_expiry = directory.path().join("worker-ran-after-renewal-expiry");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntouch '{}'\ntrap '' TERM INT\nsleep 2\ntouch '{}'\nwhile :; do sleep 1; done\n",
            started.display(),
            after_expiry.display()
        ),
    );
    let hold = directory.path().join("renewal-handoff-hold.sh");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":24,\"authority_remaining_ms\":5000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile [ ! -e '{}' ]; do sleep 0.01; done\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":2,\"event\":\"renewed\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":24,\"authority_remaining_ms\":1600,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nsleep 30\n",
            started.display()
        ),
    );
    let handoff = directory.path().join("renewal-handoff");
    std::fs::create_dir(&handoff).unwrap();
    let ready = handoff.join("ready");

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env("OCTOSTORE_SUPERVISOR_TEST_RENEWAL_HANDOFF_DIR", &handoff)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(4);
    while !ready.exists() {
        assert!(
            process.try_wait().unwrap().is_none(),
            "supervisor exited before arming the renewal watchdog"
        );
        assert!(
            Instant::now() < ready_deadline,
            "supervisor never reached the renewal watchdog handoff"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let worker_group = std::fs::read_to_string(&ready).unwrap();
    let worker_group = worker_group.trim().to_string();

    let stopped = Command::new("kill")
        .args(["-STOP", &process.id().to_string()])
        .status()
        .unwrap();
    assert!(
        stopped.success(),
        "could not pause the supervisor during watchdog renewal"
    );
    tokio::time::sleep(Duration::from_millis(2_300)).await;

    assert!(
        !after_expiry.exists(),
        "renewal handoff allowed a post-deadline worker side effect"
    );
    let worker_status = Command::new("kill")
        .args(["-0", "--", &format!("-{worker_group}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(
        !worker_status.success(),
        "worker group survived the replacement watchdog deadline"
    );

    let resumed = Command::new("kill")
        .args(["-CONT", &process.id().to_string()])
        .status()
        .unwrap();
    assert!(resumed.success(), "could not resume the paused supervisor");
    let status = wait_for_child(&mut process, Duration::from_secs(3)).await;
    assert_eq!(status.code(), Some(20));
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_preserves_old_watchdog_during_replacement_arm_failure() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory
        .path()
        .join("replacement-failure-resistant-worker.sh");
    let started = directory.path().join("worker-started");
    let worker_pids = directory.path().join("worker-pids");
    let after_expiry = directory.path().join("worker-ran-after-authority-expiry");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\n(\n  trap '' TERM INT\n  sleep 4\n  touch '{}'\n  while :; do sleep 1; done\n) &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\ntouch '{}'\nwhile :; do sleep 1; done\n",
            after_expiry.display(),
            worker_pids.display(),
            started.display()
        ),
    );
    let hold = directory.path().join("replacement-arm-failure-hold.sh");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":25,\"authority_remaining_ms\":3500,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile [ ! -e '{}' ]; do sleep 0.01; done\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":2,\"event\":\"renewed\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":25,\"authority_remaining_ms\":5000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nsleep 30\n",
            started.display()
        ),
    );
    let arm_failure = directory.path().join("renewal-arm-failure");
    std::fs::create_dir(&arm_failure).unwrap();
    let ready = arm_failure.join("ready");

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env(
            "OCTOSTORE_SUPERVISOR_TEST_RENEWAL_ARM_FAILURE_DIR",
            &arm_failure,
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Startup may contend with independent release-review gates on the same
    // host. This bounds fixture readiness without weakening the authority
    // deadline assertions below or hiding an early supervisor exit.
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            process.try_wait().unwrap().is_none(),
            "supervisor exited before exposing the replacement-arm failure"
        );
        assert!(
            Instant::now() < ready_deadline,
            "supervisor never reached the replacement-arm failure path"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let worker_group = std::fs::read_to_string(&ready).unwrap();
    let worker_group = worker_group.trim().to_string();
    let protected_pids: Vec<String> = std::fs::read_to_string(&worker_pids)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(protected_pids.len(), 2);

    let stopped = Command::new("kill")
        .args(["-STOP", &process.id().to_string()])
        .status()
        .unwrap();
    assert!(
        stopped.success(),
        "could not pause the supervisor after replacement-arm failure"
    );
    tokio::time::sleep(Duration::from_millis(4_500)).await;

    assert!(
        !after_expiry.exists(),
        "replacement-arm failure allowed a post-deadline side effect"
    );
    let worker_status = Command::new("kill")
        .args(["-0", "--", &format!("-{worker_group}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(
        !worker_status.success(),
        "worker group survived after replacement-arm failure"
    );

    let resumed = Command::new("kill")
        .args(["-CONT", &process.id().to_string()])
        .status()
        .unwrap();
    assert!(resumed.success(), "could not resume the paused supervisor");
    let status = wait_for_child(&mut process, Duration::from_secs(3)).await;
    assert_eq!(status.code(), Some(20));
    for pid in protected_pids {
        let status = Command::new("kill")
            .args(["-0", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "protected process {pid} survived replacement-arm failure"
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn old_watchdog_contains_replacement_arm_failure_after_supervisor_crash() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("crash-resistant-worker.sh");
    let started = directory.path().join("crash-worker-started");
    let worker_pids = directory.path().join("crash-worker-pids");
    let after_expiry = directory
        .path()
        .join("crash-worker-ran-after-authority-expiry");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\n(\n  trap '' TERM INT\n  sleep 4\n  touch '{}'\n  while :; do sleep 1; done\n) &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\ntouch '{}'\nwhile :; do sleep 1; done\n",
            after_expiry.display(),
            worker_pids.display(),
            started.display()
        ),
    );
    let hold = directory.path().join("crash-arm-failure-hold.sh");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":32,\"authority_remaining_ms\":3500,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile [ ! -e '{}' ]; do sleep 0.01; done\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":2,\"event\":\"renewed\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":32,\"authority_remaining_ms\":5000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nsleep 4\n",
            started.display()
        ),
    );
    let arm_failure = directory.path().join("crash-renewal-arm-failure");
    std::fs::create_dir(&arm_failure).unwrap();
    let ready = arm_failure.join("ready");

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env("TMPDIR", directory.path())
        .env(
            "OCTOSTORE_SUPERVISOR_TEST_RENEWAL_ARM_FAILURE_DIR",
            &arm_failure,
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        assert!(
            process.try_wait().unwrap().is_none(),
            "supervisor exited before exposing the replacement-arm failure"
        );
        assert!(
            Instant::now() < ready_deadline,
            "supervisor never reached the replacement-arm failure path"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let worker_group = std::fs::read_to_string(&ready).unwrap();
    let worker_group = worker_group.trim().to_string();
    let protected_pids: Vec<String> = std::fs::read_to_string(&worker_pids)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(protected_pids.len(), 2);

    let killed = Command::new("kill")
        .args(["-KILL", &process.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success(), "could not crash only the supervisor");
    let status = wait_for_child(&mut process, Duration::from_secs(2)).await;
    assert!(
        !status.success(),
        "SIGKILLed supervisor exited successfully"
    );
    tokio::time::sleep(Duration::from_millis(4_500)).await;

    assert!(
        !after_expiry.exists(),
        "a crashed supervisor allowed a post-deadline side effect"
    );
    let worker_status = Command::new("kill")
        .args(["-0", "--", &format!("-{worker_group}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(
        !worker_status.success(),
        "worker group survived after its supervisor crashed"
    );
    for pid in protected_pids {
        let status = Command::new("kill")
            .args(["-0", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "protected process {pid} survived its supervisor crash"
        );
    }
    let supervisor_tmp_dirs = std::fs::read_dir(directory.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("octostore-supervisor.")
        })
        .collect::<Vec<_>>();
    assert!(
        supervisor_tmp_dirs.is_empty(),
        "crashed supervisor left watchdog state behind"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn detached_guardian_contains_hold_after_whole_supervisor_group_dies_before_authority() {
    use std::os::unix::process::CommandExt;

    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("must-not-start.sh");
    let worker_side_effect = directory.path().join("worker-started-before-authority");
    write_executable(
        &worker,
        &format!("#!/bin/sh\ntouch '{}'\n", worker_side_effect.display()),
    );
    let hold = directory.path().join("pre-authority-hold.sh");
    let hold_pids = directory.path().join("pre-authority-hold-pids");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\nsh -c 'trap \"\" TERM INT; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nwhile :; do sleep 1; done\n",
            hold_pids.display()
        ),
    );

    let mut command = Command::new(&supervisor);
    command
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env("TMPDIR", directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.process_group(0);
    let mut process = command.spawn().unwrap();
    assert_isolated_process_group(process.id());

    let ready_deadline = Instant::now() + Duration::from_secs(3);
    while !hold_pids.exists() || supervisor_state_dirs(directory.path()).is_empty() {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < ready_deadline,
            "detached hold was not published before authority"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = supervisor_state_dirs(directory.path()).remove(0);
    let hold_group = std::fs::read_to_string(state.join("hold-pid")).unwrap();
    let hold_group = hold_group.trim().to_string();
    let hold_members = std::fs::read_to_string(&hold_pids).unwrap();
    let hold_members = hold_members.lines().map(str::to_owned).collect::<Vec<_>>();

    let killed = Command::new("kill")
        .args(["-KILL", "--", &format!("-{}", process.id())])
        .status()
        .unwrap();
    assert!(
        killed.success(),
        "could not kill the supervisor process group"
    );
    let status = wait_for_child(&mut process, Duration::from_secs(2)).await;
    assert!(!status.success());

    let mut protected = vec![(hold_group.as_str(), true)];
    protected.extend(hold_members.iter().map(|pid| (pid.as_str(), false)));
    wait_for_processes_gone(&protected, Duration::from_secs(3)).await;
    wait_for_supervisor_state_removed(directory.path(), Duration::from_secs(2)).await;
    assert!(!worker_side_effect.exists());
}

#[tokio::test]
#[serial_test::serial]
async fn detached_guardian_survives_whole_supervisor_group_kill_after_worker_start() {
    use std::os::unix::process::CommandExt;

    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("group-kill-worker.sh");
    let worker_pids = directory.path().join("group-kill-worker-pids");
    let worker_after = directory.path().join("worker-ran-after-group-kill");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\n(sh -c 'trap \"\" TERM INT; sleep 3; touch \"$1\"; while :; do sleep 1; done' sh '{}') &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nwhile :; do sleep 1; done\n",
            worker_after.display(),
            worker_pids.display()
        ),
    );
    let hold = directory.path().join("group-kill-hold.sh");
    let hold_pids = directory.path().join("group-kill-hold-pids");
    let hold_after = directory.path().join("hold-ran-after-group-kill");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\n(sh -c 'trap \"\" TERM INT; sleep 3; touch \"$1\"; while :; do sleep 1; done' sh '{}') &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":51,\"authority_remaining_ms\":5000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile :; do sleep 1; done\n",
            hold_after.display(),
            hold_pids.display()
        ),
    );

    let mut command = Command::new(&supervisor);
    command
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env("TMPDIR", directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.process_group(0);
    let mut process = command.spawn().unwrap();
    assert_isolated_process_group(process.id());

    let ready_deadline = Instant::now() + Duration::from_secs(3);
    while !worker_pids.exists() || !hold_pids.exists() {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < ready_deadline,
            "protected groups did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = supervisor_state_dirs(directory.path()).remove(0);
    let worker_group = std::fs::read_to_string(state.join("worker-pid")).unwrap();
    let hold_group = std::fs::read_to_string(state.join("hold-pid")).unwrap();
    let worker_group = worker_group.trim().to_string();
    let hold_group = hold_group.trim().to_string();
    let member_text = format!(
        "{} {}",
        std::fs::read_to_string(&worker_pids).unwrap(),
        std::fs::read_to_string(&hold_pids).unwrap()
    );
    let members = member_text
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();

    let killed = Command::new("kill")
        .args(["-KILL", "--", &format!("-{}", process.id())])
        .status()
        .unwrap();
    assert!(
        killed.success(),
        "could not kill the supervisor process group"
    );
    let status = wait_for_child(&mut process, Duration::from_secs(2)).await;
    assert!(!status.success());

    let mut protected = vec![(worker_group.as_str(), true), (hold_group.as_str(), true)];
    protected.extend(members.iter().map(|pid| (pid.as_str(), false)));
    wait_for_processes_gone(&protected, Duration::from_secs(3)).await;
    wait_for_supervisor_state_removed(directory.path(), Duration::from_secs(2)).await;
    assert!(!worker_after.exists());
    assert!(!hold_after.exists());
}

#[tokio::test]
#[serial_test::serial]
async fn stopped_guardian_wakes_after_original_kill_deadline_without_new_term_grace() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("late-guardian-worker.sh");
    let worker_pids = directory.path().join("late-guardian-worker-pids");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\nsh -c 'trap \"\" TERM INT; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nwhile :; do sleep 1; done\n",
            worker_pids.display()
        ),
    );
    let hold = directory.path().join("late-guardian-hold.sh");
    let hold_pids = directory.path().join("late-guardian-hold-pids");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\nsh -c 'trap \"\" TERM INT; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":52,\"authority_remaining_ms\":2600,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile :; do sleep 1; done\n",
            hold_pids.display()
        ),
    );

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env("TMPDIR", directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // This is fixture startup, not the guardian's kill deadline. Allow host
    // contention while continuing to fail immediately if the supervisor exits.
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    while !worker_pids.exists()
        || !hold_pids.exists()
        || supervisor_state_dirs(directory.path()).is_empty()
    {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < ready_deadline,
            "late-wake fixture did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = supervisor_state_dirs(directory.path()).remove(0);
    let guardian_identity = std::fs::read_to_string(state.join("guardian-identity")).unwrap();
    let guardian_pid = guardian_identity
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    let worker_group = std::fs::read_to_string(state.join("worker-pid")).unwrap();
    let hold_group = std::fs::read_to_string(state.join("hold-pid")).unwrap();
    let worker_group = worker_group.trim().to_string();
    let hold_group = hold_group.trim().to_string();
    let member_text = format!(
        "{} {}",
        std::fs::read_to_string(&worker_pids).unwrap(),
        std::fs::read_to_string(&hold_pids).unwrap()
    );
    let members = member_text
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();

    assert!(Command::new("kill")
        .args(["-STOP", &guardian_pid])
        .status()
        .unwrap()
        .success());
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    let resumed_at = Instant::now();
    assert!(Command::new("kill")
        .args(["-CONT", &guardian_pid])
        .status()
        .unwrap()
        .success());

    let mut protected = vec![(worker_group.as_str(), true), (hold_group.as_str(), true)];
    protected.extend(members.iter().map(|pid| (pid.as_str(), false)));
    wait_for_processes_gone(&protected, Duration::from_secs(1)).await;
    assert!(
        resumed_at.elapsed() < Duration::from_millis(700),
        "late guardian wake granted a fresh TERM grace"
    );
    let status = wait_for_child(&mut process, Duration::from_secs(2)).await;
    assert_eq!(status.code(), Some(20));
    wait_for_supervisor_state_removed(directory.path(), Duration::from_secs(2)).await;
}

#[tokio::test]
#[serial_test::serial]
async fn substituted_worker_identity_cannot_redirect_guardian_group_signals() {
    use std::os::unix::process::CommandExt;

    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("identity-worker.sh");
    let worker_started = directory.path().join("identity-worker-started");
    let worker_pids = directory.path().join("identity-worker-pids");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\nprintf '%s\\n' \"$$\" > '{}'\ntouch '{}'\nwhile :; do sleep 1; done\n",
            worker_pids.display(),
            worker_started.display(),
        ),
    );
    let hold = directory.path().join("identity-hold.sh");
    let hold_pids = directory.path().join("identity-hold-pids");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\nprintf '%s\\n' \"$$\" > '{}'\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":53,\"authority_remaining_ms\":5000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile :; do sleep 1; done\n",
            hold_pids.display()
        ),
    );

    let mut sentinel_command = Command::new("sh");
    sentinel_command
        .args(["-c", "trap '' TERM INT HUP; while :; do sleep 1; done"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    sentinel_command.process_group(0);
    let mut sentinel = sentinel_command.spawn().unwrap();
    assert_isolated_process_group(sentinel.id());
    let sentinel_pid = sentinel.id().to_string();
    let sentinel_identity_output = Command::new("ps")
        .args([
            "-o",
            "pid=",
            "-o",
            "pgid=",
            "-o",
            "lstart=",
            "-p",
            &sentinel_pid,
        ])
        .output()
        .unwrap();
    let sentinel_identity = String::from_utf8(sentinel_identity_output.stdout)
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env("TMPDIR", directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(3);
    while !worker_started.exists()
        || !worker_pids.exists()
        || !hold_pids.exists()
        || supervisor_state_dirs(directory.path()).is_empty()
    {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < ready_deadline,
            "identity fixture did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = supervisor_state_dirs(directory.path()).remove(0);
    let worker_group =
        identity_process_group(&std::fs::read_to_string(state.join("worker-identity")).unwrap());
    let hold_group =
        identity_process_group(&std::fs::read_to_string(state.join("hold-identity")).unwrap());
    let worker_members = std::fs::read_to_string(&worker_pids).unwrap();
    let hold_members = std::fs::read_to_string(&hold_pids).unwrap();
    std::fs::write(
        state.join("worker-identity"),
        format!("{sentinel_identity}\n"),
    )
    .unwrap();

    assert!(Command::new("kill")
        .args(["-HUP", &process.id().to_string()])
        .status()
        .unwrap()
        .success());
    let status = wait_for_child(&mut process, Duration::from_secs(3)).await;
    assert_eq!(status.code(), Some(129));
    let protected = [
        (worker_group.as_str(), true),
        (hold_group.as_str(), true),
        (worker_members.trim(), false),
        (hold_members.trim(), false),
    ];
    wait_for_processes_gone(&protected, Duration::from_secs(2)).await;
    assert!(
        process_is_alive(&sentinel_pid, true),
        "substituted identity redirected a guardian signal to an unrelated group"
    );

    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{sentinel_pid}")])
        .status();
    let _ = wait_for_child(&mut sentinel, Duration::from_secs(2)).await;
    wait_for_supervisor_state_removed(directory.path(), Duration::from_secs(2)).await;
}

#[tokio::test]
#[serial_test::serial]
async fn stale_pgid_identity_never_signals_reused_unrelated_group() {
    use std::os::unix::process::CommandExt;

    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("stale-pgid-worker.sh");
    let worker_started = directory.path().join("stale-pgid-worker-started");
    let worker_pids = directory.path().join("stale-pgid-worker-pids");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT HUP\nprintf '%s\\n' \"$$\" > '{}'\ntouch '{}'\nwhile :; do sleep 1; done\n",
            worker_pids.display(),
            worker_started.display(),
        ),
    );
    let hold = directory.path().join("stale-pgid-hold.sh");
    let hold_pids = directory.path().join("stale-pgid-hold-pids");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT HUP\nprintf '%s\\n' \"$$\" > '{}'\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":54,\"authority_remaining_ms\":10000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile :; do sleep 1; done\n",
            hold_pids.display()
        ),
    );

    let sentinel_signaled = directory.path().join("unrelated-sentinel-signaled");
    let mut sentinel_command = Command::new("sh");
    sentinel_command
        .args([
            "-c",
            "trap 'touch \"$1\"; exit 0' TERM; while :; do sleep 1; done",
            "sh",
            sentinel_signaled.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    sentinel_command.process_group(0);
    let mut sentinel = sentinel_command.spawn().unwrap();
    assert_isolated_process_group(sentinel.id());
    let sentinel_pid = sentinel.id().to_string();
    // Same numeric PID/PGID, deliberately old start time: this deterministically
    // models a stale published group identity after PID/PGID reuse.
    let stale_identity = format!("{sentinel_pid} {sentinel_pid} Wed Jan 1 00:00:00 2020");

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env(
            "OCTOSTORE_SUPERVISOR_TEST_STALE_GROUP_IDENTITY",
            &stale_identity,
        )
        .env("TMPDIR", directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(3);
    while !worker_started.exists()
        || !worker_pids.exists()
        || !hold_pids.exists()
        || supervisor_state_dirs(directory.path()).is_empty()
    {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < ready_deadline,
            "stale-PGID fixture did not start protected work"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = supervisor_state_dirs(directory.path()).remove(0);
    let worker_group =
        identity_process_group(&std::fs::read_to_string(state.join("worker-identity")).unwrap());
    let hold_group =
        identity_process_group(&std::fs::read_to_string(state.join("hold-identity")).unwrap());
    let worker_members = std::fs::read_to_string(&worker_pids).unwrap();
    let hold_members = std::fs::read_to_string(&hold_pids).unwrap();
    assert!(Command::new("kill")
        .args(["-HUP", &process.id().to_string()])
        .status()
        .unwrap()
        .success());
    let status = wait_for_child(&mut process, Duration::from_secs(4)).await;
    assert_eq!(status.code(), Some(129));
    let protected = [
        (worker_group.as_str(), true),
        (hold_group.as_str(), true),
        (worker_members.trim(), false),
        (hold_members.trim(), false),
    ];
    wait_for_processes_gone(&protected, Duration::from_secs(2)).await;
    assert!(
        process_is_alive(&sentinel_pid, true),
        "stale PID/PGID identity redirected a signal to unrelated work"
    );
    assert!(
        !sentinel_signaled.exists(),
        "stale PID/PGID identity delivered TERM to unrelated work"
    );

    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{sentinel_pid}")])
        .status();
    let _ = wait_for_child(&mut sentinel, Duration::from_secs(2)).await;
    wait_for_supervisor_state_removed(directory.path(), Duration::from_secs(2)).await;
}

#[tokio::test]
#[serial_test::serial]
async fn anchor_fails_closed_when_fixture_teardown_removes_supervisor_state() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("teardown-worker.sh");
    let worker_started = directory.path().join("teardown-worker-started");
    let worker_pids = directory.path().join("teardown-worker-pids");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT HUP\nsh -c 'trap \"\" TERM INT HUP; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\ntouch '{}'\nwhile :; do sleep 1; done\n",
            worker_pids.display(),
            worker_started.display()
        ),
    );
    let hold = directory.path().join("teardown-hold.sh");
    let hold_pids = directory.path().join("teardown-hold-pids");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT HUP\nsh -c 'trap \"\" TERM INT HUP; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":55,\"authority_remaining_ms\":10000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile :; do sleep 1; done\n",
            hold_pids.display()
        ),
    );

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env("TMPDIR", directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(3);
    while !worker_started.exists()
        || !worker_pids.exists()
        || !hold_pids.exists()
        || supervisor_state_dirs(directory.path()).is_empty()
    {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < ready_deadline,
            "teardown leak fixture did not start protected groups"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = supervisor_state_dirs(directory.path()).remove(0);
    let worker_group =
        identity_process_group(&std::fs::read_to_string(state.join("worker-identity")).unwrap());
    let hold_group =
        identity_process_group(&std::fs::read_to_string(state.join("hold-identity")).unwrap());
    let worker_members = std::fs::read_to_string(&worker_pids).unwrap();
    let worker_members = worker_members.lines().collect::<Vec<_>>();
    let hold_members = std::fs::read_to_string(&hold_pids).unwrap();
    let hold_members = hold_members.lines().collect::<Vec<_>>();

    // TempDir drops this directory if an assertion panics. The anchor must
    // make that path fail closed, rather than leave detached wrappers waiting
    // forever for TERM-resistant fixture children.
    let removal_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match std::fs::remove_dir_all(&state) {
            Ok(()) => break,
            Err(error)
                if error.kind() == std::io::ErrorKind::DirectoryNotEmpty
                    && Instant::now() < removal_deadline =>
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("could not remove supervisor state anchor: {error}"),
        }
    }

    let mut protected = vec![(worker_group.as_str(), true), (hold_group.as_str(), true)];
    protected.extend(worker_members.iter().map(|pid| (*pid, false)));
    protected.extend(hold_members.iter().map(|pid| (*pid, false)));
    wait_for_processes_gone(&protected, Duration::from_secs(3)).await;

    let _ = Command::new("kill")
        .args(["-KILL", &process.id().to_string()])
        .status();
    let status = wait_for_child(&mut process, Duration::from_secs(3)).await;
    assert!(
        !status.success(),
        "state-removal fixture supervisor exited successfully"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn queued_watchdog_notification_cannot_strand_stopped_supervisor_groups() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("queued-notification-worker.sh");
    let worker_pids = directory.path().join("queued-notification-worker-pids");
    let worker_after_deadline = directory
        .path()
        .join("queued-notification-worker-after-deadline");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\nsh -c 'trap \"\" TERM INT; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nsleep 4.3\ntouch '{}'\nwhile :; do sleep 1; done\n",
            worker_pids.display(),
            worker_after_deadline.display()
        ),
    );
    let hold = directory.path().join("queued-notification-hold.sh");
    let hold_pids = directory.path().join("queued-notification-hold-pids");
    let hold_after_deadline = directory
        .path()
        .join("queued-notification-hold-after-deadline");
    let emitted = directory.path().join("queued-notification-emitted");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\nsh -c 'trap \"\" TERM INT; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\ntouch '{}'\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":41,\"authority_remaining_ms\":4000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nsleep 4.3\ntouch '{}'\nwhile :; do sleep 1; done\n",
            hold_pids.display(),
            emitted.display(),
            hold_after_deadline.display()
        ),
    );
    let notification = directory.path().join("watchdog-notification");
    std::fs::create_dir(&notification).unwrap();
    let notification_ready = notification.join("ready");
    let notification_continue = notification.join("continue");

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env(
            "OCTOSTORE_SUPERVISOR_TEST_WATCHDOG_NOTIFICATION_DIR",
            &notification,
        )
        .env("TMPDIR", directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let started_deadline = Instant::now() + Duration::from_secs(2);
    while !emitted.exists() || !worker_pids.exists() || !hold_pids.exists() {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < started_deadline,
            "queued-notification fixture groups did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let authority_observed = Instant::now();
    let stopped = Command::new("kill")
        .args(["-STOP", &process.id().to_string()])
        .status()
        .unwrap();
    assert!(stopped.success(), "could not stop the supervisor");

    let notification_deadline = authority_observed + Duration::from_secs(4);
    while !notification_ready.exists() {
        assert!(
            Instant::now() < notification_deadline,
            "watchdog did not successfully queue USR1 to the stopped supervisor"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let killed = Command::new("kill")
        .args(["-KILL", &process.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success(), "could not kill the stopped supervisor");
    std::fs::write(&notification_continue, "continue\n").unwrap();
    let status = wait_for_child(&mut process, Duration::from_secs(2)).await;
    assert!(
        !status.success(),
        "SIGKILLed supervisor exited successfully"
    );

    let worker_fields = std::fs::read_to_string(&worker_pids).unwrap();
    let worker_fields = worker_fields.split_whitespace().collect::<Vec<_>>();
    let hold_fields = std::fs::read_to_string(&hold_pids).unwrap();
    let hold_fields = hold_fields.split_whitespace().collect::<Vec<_>>();
    assert_eq!(worker_fields.len(), 2);
    assert_eq!(hold_fields.len(), 2);
    let protected = [
        (worker_fields[0], true),
        (worker_fields[0], false),
        (worker_fields[1], false),
        (hold_fields[0], true),
        (hold_fields[0], false),
        (hold_fields[1], false),
    ];
    wait_for_processes_gone(&protected, Duration::from_secs(2)).await;
    wait_for_supervisor_state_removed(directory.path(), Duration::from_secs(1)).await;
    assert!(
        authority_observed.elapsed() < Duration::from_millis(4_350),
        "watchdog teardown exceeded the original four-second authority deadline"
    );
    assert!(!worker_after_deadline.exists());
    assert!(!hold_after_deadline.exists());
}

#[tokio::test]
#[serial_test::serial]
async fn published_worker_wrapper_exits_when_supervisor_dies_before_readiness() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("pre-ready-worker.sh");
    let worker_side_effect = directory.path().join("pre-ready-worker-side-effect");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntouch '{}'\nwhile :; do sleep 1; done\n",
            worker_side_effect.display()
        ),
    );
    let hold = directory.path().join("pre-ready-hold.sh");
    let hold_pids = directory.path().join("pre-ready-hold-pids");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\nsh -c 'trap \"\" TERM INT; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"acquired\",\"kind\":\"lock\",\"name\":\"room\",\"term\":42,\"authority_remaining_ms\":10000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile :; do sleep 1; done\n",
            hold_pids.display()
        ),
    );
    let published = directory.path().join("worker-published");
    std::fs::create_dir(&published).unwrap();
    let published_ready = published.join("ready");

    let mut process = Command::new(&supervisor)
        .args([
            "lock",
            "room",
            "-",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env("OCTOSTORE_SUPERVISOR_TEST_WORKER_PUBLISHED_DIR", &published)
        .env("TMPDIR", directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(5);
    while !published_ready.exists() || !hold_pids.exists() {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < ready_deadline,
            "worker wrapper did not publish before readiness"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = supervisor_state_dirs(directory.path());
    assert_eq!(state.len(), 1, "expected one supervisor state directory");
    let worker_group = std::fs::read_to_string(state[0].join("worker-pid")).unwrap();
    let worker_group = worker_group.trim().to_string();
    assert!(process_is_alive(&worker_group, true));
    assert!(!worker_side_effect.exists());

    let killed = Command::new("kill")
        .args(["-KILL", &process.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success(), "could not crash the pre-ready supervisor");
    let status = wait_for_child(&mut process, Duration::from_secs(2)).await;
    assert!(
        !status.success(),
        "SIGKILLed supervisor exited successfully"
    );
    let hold_fields = std::fs::read_to_string(&hold_pids).unwrap();
    let hold_fields = hold_fields.split_whitespace().collect::<Vec<_>>();
    let protected = [
        (worker_group.as_str(), true),
        (hold_fields[0], true),
        (hold_fields[0], false),
        (hold_fields[1], false),
    ];
    wait_for_processes_gone(&protected, Duration::from_secs(3)).await;
    wait_for_supervisor_state_removed(directory.path(), Duration::from_secs(1)).await;
    assert!(
        !worker_side_effect.exists(),
        "worker instruction ran after its supervisor died before readiness"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn terminal_containment_keeps_watchdog_until_groups_are_proven_dead() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("terminal-worker.sh");
    let worker_started = directory.path().join("terminal-worker-started");
    let worker_pids = directory.path().join("terminal-worker-pids");
    let worker_after_terminal = directory.path().join("terminal-worker-after-event");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\nsh -c 'trap \"\" TERM INT; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\ntouch '{}'\nsleep 4\ntouch '{}'\nwhile :; do sleep 1; done\n",
            worker_pids.display(),
            worker_started.display(),
            worker_after_terminal.display()
        ),
    );
    let hold = directory.path().join("terminal-hold.sh");
    let hold_pids = directory.path().join("terminal-hold-pids");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\nsh -c 'trap \"\" TERM INT; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":43,\"authority_remaining_ms\":10000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile [ ! -e '{}' ]; do sleep 0.01; done\nprintf '%s\\n' '{{\"schema_version\":1,\"sequence\":2,\"event\":\"released\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":43}}'\nwhile :; do sleep 1; done\n",
            hold_pids.display(),
            worker_started.display()
        ),
    );
    let contained = directory.path().join("terminal-contained");
    std::fs::create_dir(&contained).unwrap();
    let contained_ready = contained.join("ready");
    let events = directory.path().join("terminal-events.jsonl");
    let event_output = std::fs::File::create(&events).unwrap();

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env(
            "OCTOSTORE_SUPERVISOR_TEST_TERMINAL_CONTAINED_DIR",
            &contained,
        )
        .env("TMPDIR", directory.path())
        .stdout(event_output)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(5);
    while !contained_ready.exists() || !worker_pids.exists() || !hold_pids.exists() {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < ready_deadline,
            "terminal containment hook was not reached"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let worker_fields = std::fs::read_to_string(&worker_pids).unwrap();
    let worker_fields = worker_fields.split_whitespace().collect::<Vec<_>>();
    let hold_fields = std::fs::read_to_string(&hold_pids).unwrap();
    let hold_fields = hold_fields.split_whitespace().collect::<Vec<_>>();
    let protected = [
        (worker_fields[0], true),
        (worker_fields[0], false),
        (worker_fields[1], false),
        (hold_fields[0], true),
        (hold_fields[0], false),
        (hold_fields[1], false),
    ];
    wait_for_processes_gone(&protected, Duration::from_secs(1)).await;
    let event_text = std::fs::read_to_string(&events).unwrap();
    assert!(event_text.contains("\"event\":\"leader\""));
    assert!(
        !event_text.contains("\"event\":\"released\""),
        "released was observable before containment finished"
    );
    assert_eq!(
        supervisor_state_dirs(directory.path()).len(),
        1,
        "terminal pause retired its watchdog before containment was committed"
    );

    let killed = Command::new("kill")
        .args(["-KILL", &process.id().to_string()])
        .status()
        .unwrap();
    assert!(
        killed.success(),
        "could not crash the terminal-paused supervisor"
    );
    let status = wait_for_child(&mut process, Duration::from_secs(2)).await;
    assert!(
        !status.success(),
        "SIGKILLed supervisor exited successfully"
    );
    wait_for_supervisor_state_removed(directory.path(), Duration::from_secs(2)).await;
    assert!(
        !worker_after_terminal.exists(),
        "terminal event left protected work alive"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn main_fallback_contains_groups_when_detached_guardian_dies() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("guardian-death-worker.sh");
    let worker_pids = directory.path().join("guardian-death-worker-pids");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT HUP\nsh -c 'trap \"\" TERM INT HUP; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nwhile :; do sleep 1; done\n",
            worker_pids.display()
        ),
    );
    let hold = directory.path().join("guardian-death-hold.sh");
    let hold_pids = directory.path().join("guardian-death-hold-pids");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT HUP\nsh -c 'trap \"\" TERM INT HUP; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":61,\"authority_remaining_ms\":10000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile :; do sleep 1; done\n",
            hold_pids.display()
        ),
    );
    let contained = directory.path().join("guardian-death-contained");
    std::fs::create_dir(&contained).unwrap();
    let contained_ready = contained.join("ready");
    let contained_continue = contained.join("continue");
    let events = directory.path().join("guardian-death-events.jsonl");
    let event_output = std::fs::File::create(&events).unwrap();

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env(
            "OCTOSTORE_SUPERVISOR_TEST_TERMINAL_CONTAINED_DIR",
            &contained,
        )
        .env("TMPDIR", directory.path())
        .stdout(event_output)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(4);
    while !worker_pids.exists()
        || !hold_pids.exists()
        || supervisor_state_dirs(directory.path()).is_empty()
    {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < ready_deadline,
            "guardian-death fixture did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = supervisor_state_dirs(directory.path()).remove(0);
    let guardian_identity = std::fs::read_to_string(state.join("guardian-identity")).unwrap();
    let guardian_pid = guardian_identity.split_whitespace().next().unwrap();
    let worker_group = std::fs::read_to_string(state.join("worker-pid")).unwrap();
    let hold_group = std::fs::read_to_string(state.join("hold-pid")).unwrap();
    let worker_group = worker_group.trim().to_string();
    let hold_group = hold_group.trim().to_string();
    let worker_members = std::fs::read_to_string(&worker_pids).unwrap();
    let worker_members = worker_members
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let hold_members = std::fs::read_to_string(&hold_pids).unwrap();
    let hold_members = hold_members.lines().map(str::to_owned).collect::<Vec<_>>();

    // Worker startup can precede stdout delivery on Linux. Establish the
    // leader handoff precondition before injecting guardian failure.
    let leader_deadline = Instant::now() + Duration::from_secs(2);
    while !std::fs::read_to_string(&events)
        .unwrap()
        .contains("\"event\":\"leader\"")
    {
        assert!(
            Instant::now() < leader_deadline,
            "leader handoff was not observable before guardian failure"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(Command::new("kill")
        .args(["-KILL", guardian_pid])
        .status()
        .unwrap()
        .success());
    let containment_deadline = Instant::now() + Duration::from_secs(4);
    while !contained_ready.exists() {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < containment_deadline,
            "main process did not finish fallback containment"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut protected = vec![(worker_group.as_str(), true), (hold_group.as_str(), true)];
    protected.extend(worker_members.iter().map(|pid| (pid.as_str(), false)));
    protected.extend(hold_members.iter().map(|pid| (pid.as_str(), false)));
    wait_for_processes_gone(&protected, Duration::from_secs(1)).await;
    let event_text = std::fs::read_to_string(&events).unwrap();
    assert!(event_text.contains("\"event\":\"leader\""));
    assert!(
        !event_text.contains("\"event\":\"uncertain\""),
        "uncertainty was observable before fallback containment finished"
    );

    std::fs::write(&contained_continue, "continue\n").unwrap();
    let status = wait_for_child(&mut process, Duration::from_secs(3)).await;
    assert_eq!(status.code(), Some(20));
    assert!(
        std::fs::read_to_string(&events)
            .unwrap()
            .contains("\"event\":\"uncertain\""),
        "guardian death did not report post-containment uncertainty"
    );
    let recovery_state = supervisor_state_dirs(directory.path());
    assert_eq!(
        recovery_state.len(),
        1,
        "guardian fallback discarded its recovery evidence"
    );
    assert_eq!(
        std::fs::read_to_string(recovery_state[0].join("guardian-contained"))
            .unwrap()
            .trim(),
        "0",
        "fallback did not record proven containment"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn guardian_signals_surviving_group_members_after_wrapper_leader_death() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("dead-wrapper-worker.sh");
    let worker_pids = directory.path().join("dead-wrapper-worker-pids");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT HUP\nsh -c 'trap \"\" TERM INT HUP; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nwhile :; do sleep 1; done\n",
            worker_pids.display()
        ),
    );
    let hold = directory.path().join("dead-wrapper-hold.sh");
    let hold_pids = directory.path().join("dead-wrapper-hold-pids");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT HUP\nsh -c 'trap \"\" TERM INT HUP; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"acquired\",\"kind\":\"lock\",\"name\":\"room\",\"term\":62,\"authority_remaining_ms\":10000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile :; do sleep 1; done\n",
            hold_pids.display()
        ),
    );

    let mut process = Command::new(&supervisor)
        .args([
            "lock",
            "room",
            "-",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env("TMPDIR", directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(4);
    while !worker_pids.exists()
        || !hold_pids.exists()
        || supervisor_state_dirs(directory.path()).is_empty()
    {
        assert!(process.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < ready_deadline,
            "wrapper-death fixture did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = supervisor_state_dirs(directory.path()).remove(0);
    let worker_group = std::fs::read_to_string(state.join("worker-pid")).unwrap();
    let hold_group = std::fs::read_to_string(state.join("hold-pid")).unwrap();
    let worker_group = worker_group.trim().to_string();
    let hold_group = hold_group.trim().to_string();
    let worker_members = std::fs::read_to_string(&worker_pids).unwrap();
    let worker_members = worker_members
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let hold_members = std::fs::read_to_string(&hold_pids).unwrap();
    let hold_members = hold_members.lines().map(str::to_owned).collect::<Vec<_>>();

    assert!(Command::new("kill")
        .args(["-KILL", "--", &worker_group])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("kill")
        .args(["-TERM", &process.id().to_string()])
        .status()
        .unwrap()
        .success());
    let status = wait_for_child(&mut process, Duration::from_secs(4)).await;
    assert!(
        matches!(status.code(), Some(143) | Some(20)),
        "wrapper death may be observed before its TERM signal; both exits prove containment"
    );

    let mut protected = vec![(worker_group.as_str(), true), (hold_group.as_str(), true)];
    protected.extend(worker_members.iter().map(|pid| (pid.as_str(), false)));
    protected.extend(hold_members.iter().map(|pid| (pid.as_str(), false)));
    wait_for_processes_gone(&protected, Duration::from_secs(2)).await;
    wait_for_supervisor_state_removed(directory.path(), Duration::from_secs(1)).await;
}

#[tokio::test]
#[serial_test::serial]
async fn direct_setsid_escape_is_detected_and_contained() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("setsid-worker.sh");
    let escaped_pid_file = directory.path().join("escaped-worker-pid");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\nexec perl -MPOSIX=setsid -e 'setsid(); exec @ARGV' sh -c 'trap \"\" TERM INT HUP; echo $$ > \"$1\"; while :; do sleep 1; done' sh '{}'\n",
            escaped_pid_file.display()
        ),
    );
    let hold = directory.path().join("setsid-hold.sh");
    write_executable(
        &hold,
        "#!/bin/sh\ntrap '' TERM INT HUP\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":63,\"authority_remaining_ms\":8000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile :; do sleep 1; done\n",
    );

    let output = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env("TMPDIR", directory.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(70));
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("direct worker left the supervised process group"));
    assert!(
        escaped_pid_file.exists(),
        "setsid worker never entered its new session"
    );
    let escaped_pid = std::fs::read_to_string(&escaped_pid_file).unwrap();
    let escaped_pid = escaped_pid.trim();
    wait_for_processes_gone(
        &[(escaped_pid, true), (escaped_pid, false)],
        Duration::from_secs(2),
    )
    .await;
    wait_for_supervisor_state_removed(directory.path(), Duration::from_secs(1)).await;
}

#[tokio::test]
#[serial_test::serial]
async fn continuous_initial_deadline_contains_detached_groups_after_supervisor_crash() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("rollback-worker.sh");
    let worker_started = directory.path().join("rollback-worker-started");
    let worker_pids = directory.path().join("rollback-worker-pids");
    let worker_after_deadline = directory.path().join("rollback-worker-after-deadline");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\n(\n  trap '' TERM INT\n  sleep 3.2\n  touch '{}'\n  while :; do sleep 1; done\n) &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\ntouch '{}'\nwhile :; do sleep 1; done\n",
            worker_after_deadline.display(),
            worker_pids.display(),
            worker_started.display()
        ),
    );
    let hold = directory.path().join("rollback-hold.sh");
    let hold_pids = directory.path().join("rollback-hold-pids");
    let hold_after_deadline = directory.path().join("rollback-hold-after-deadline");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\n(\n  trap '' TERM INT\n  sleep 3.2\n  touch '{}'\n  while :; do sleep 1; done\n) &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nobserved_ms=$(($(jq -nr 'now * 1000 | floor') - 1000))\ncontinuous_ms=$(($(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000') - 2000))\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":35,\"authority_remaining_ms\":5000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile :; do sleep 1; done\n",
            hold_after_deadline.display(),
            hold_pids.display()
        ),
    );

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env("TMPDIR", directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let started_deadline = Instant::now() + Duration::from_secs(3);
    while !worker_started.exists() || !worker_pids.exists() || !hold_pids.exists() {
        assert!(
            process.try_wait().unwrap().is_none(),
            "supervisor exited before starting the rollback fixture"
        );
        assert!(
            Instant::now() < started_deadline,
            "rollback fixture groups did not start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let killed = Command::new("kill")
        .args(["-KILL", &process.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success(), "could not crash only the supervisor");
    let status = wait_for_child(&mut process, Duration::from_secs(2)).await;
    assert!(
        !status.success(),
        "SIGKILLed supervisor exited successfully"
    );
    tokio::time::sleep(Duration::from_millis(4_000)).await;

    assert!(
        !worker_after_deadline.exists(),
        "civil-clock rollback extended worker authority"
    );
    assert!(
        !hold_after_deadline.exists(),
        "crashed supervisor left the capability-bearing hold alive"
    );
    let protected_pids = format!(
        "{} {}",
        std::fs::read_to_string(&worker_pids).unwrap(),
        std::fs::read_to_string(&hold_pids).unwrap()
    );
    for pid in protected_pids.split_whitespace() {
        let status = Command::new("kill")
            .args(["-0", pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "detached process {pid} survived its supervisor crash"
        );
    }
    let supervisor_tmp_dirs = std::fs::read_dir(directory.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("octostore-supervisor.")
        })
        .collect::<Vec<_>>();
    assert!(
        supervisor_tmp_dirs.is_empty(),
        "crashed supervisor left watchdog state behind"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn continuous_renewal_deadline_survives_clock_rollback_and_supervisor_crash() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("renewal-rollback-worker.sh");
    let worker_started = directory.path().join("renewal-rollback-worker-started");
    let worker_pids = directory.path().join("renewal-rollback-worker-pids");
    let worker_after_deadline = directory
        .path()
        .join("renewal-rollback-worker-after-deadline");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\n(\n  trap '' TERM INT\n  sleep 3.2\n  touch '{}'\n  while :; do sleep 1; done\n) &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\ntouch '{}'\nwhile :; do sleep 1; done\n",
            worker_after_deadline.display(),
            worker_pids.display(),
            worker_started.display()
        ),
    );
    let hold = directory.path().join("renewal-rollback-hold.sh");
    let hold_pids = directory.path().join("renewal-rollback-hold-pids");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\nsh -c 'trap \"\" TERM INT; while :; do sleep 1; done' &\ndescendant=$!\nprintf '%s\\n%s\\n' \"$$\" \"$descendant\" > '{}'\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"leader\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":36,\"authority_remaining_ms\":8000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile [ ! -e '{}' ]; do sleep 0.01; done\nobserved_ms=$(($(jq -nr 'now * 1000 | floor') - 1000))\ncontinuous_ms=$(($(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000') - 2000))\nprintf '{{\"schema_version\":1,\"sequence\":2,\"event\":\"renewed\",\"kind\":\"election\",\"name\":\"room\",\"candidate_id\":\"agent\",\"term\":36,\"authority_remaining_ms\":5000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile :; do sleep 1; done\n",
            hold_pids.display(),
            worker_started.display()
        ),
    );
    let handoff = directory.path().join("renewal-rollback-handoff");
    std::fs::create_dir(&handoff).unwrap();
    let ready = handoff.join("ready");
    let continue_file = handoff.join("continue");
    let done = handoff.join("done");

    let mut process = Command::new(&supervisor)
        .args([
            "election",
            "room",
            "agent",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .env("OCTOSTORE_SUPERVISOR_TEST_RENEWAL_HANDOFF_DIR", &handoff)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let ready_deadline = Instant::now() + Duration::from_secs(4);
    while !ready.exists() || !worker_pids.exists() || !hold_pids.exists() {
        assert!(
            process.try_wait().unwrap().is_none(),
            "supervisor exited before arming the rollback renewal watchdog"
        );
        assert!(
            Instant::now() < ready_deadline,
            "rollback renewal watchdog was not armed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    std::fs::write(&continue_file, "continue\n").unwrap();
    let done_deadline = Instant::now() + Duration::from_secs(2);
    while !done.exists() {
        assert!(
            process.try_wait().unwrap().is_none(),
            "supervisor exited before completing watchdog handoff"
        );
        assert!(
            Instant::now() < done_deadline,
            "supervisor did not retire the old watchdog"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        process.try_wait().unwrap().is_none(),
        "supervisor exited before the post-handoff crash"
    );

    let killed = Command::new("kill")
        .args(["-KILL", &process.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success(), "could not crash only the supervisor");
    let status = wait_for_child(&mut process, Duration::from_secs(2)).await;
    assert!(
        !status.success(),
        "SIGKILLed supervisor exited successfully"
    );
    tokio::time::sleep(Duration::from_millis(4_000)).await;

    assert!(
        !worker_after_deadline.exists(),
        "civil-clock rollback extended renewed worker authority"
    );
    let protected_pids = format!(
        "{} {}",
        std::fs::read_to_string(&worker_pids).unwrap(),
        std::fs::read_to_string(&hold_pids).unwrap()
    );
    for pid in protected_pids.split_whitespace() {
        let status = Command::new("kill")
            .args(["-0", pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "renewal rollback process {pid} survived supervisor crash"
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_kills_resistant_worker_inside_tight_authority_budget() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("resistant-worker.sh");
    let started = directory.path().join("worker-started");
    let after_expiry = directory.path().join("worker-ran-after-expiry");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntouch '{}'\ntrap '' TERM INT\nsleep 2.6\ntouch '{}'\nwhile :; do sleep 1; done\n",
            started.display(),
            after_expiry.display()
        ),
    );
    let hold = directory.path().join("tight-budget-hold.sh");
    write_executable(
        &hold,
        "#!/bin/sh\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{\"schema_version\":1,\"sequence\":1,\"event\":\"acquired\",\"kind\":\"lock\",\"name\":\"room\",\"term\":19,\"authority_remaining_ms\":2500,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}\\n' \"$observed_ms\" \"$continuous_ms\"\nsleep 30\n",
    );

    let output = Command::new(&supervisor)
        .args([
            "lock",
            "room",
            "-",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(20));
    assert!(
        started.exists(),
        "the tight but usable budget should start work"
    );
    assert!(
        !after_expiry.exists(),
        "TERM-resistant work survived beyond its authority budget"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_keeps_healthy_long_ttl_elections_through_renewal() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let hold = directory.path().join("healthy-election-hold.sh");
    write_executable(
        &hold,
        r#"#!/bin/sh
ttl_seconds=$2
authority_remaining_ms=$((ttl_seconds * 1000))
observed_ms=$(jq -nr 'now * 1000 | floor')
continuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf "%.0f", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')
printf '{"schema_version":1,"sequence":1,"event":"leader","kind":"election","name":"room","candidate_id":"agent","term":23,"authority_remaining_ms":%s,"authority_observed_unix_ms":%s,"authority_observed_continuous_ms":%s}\n' "$authority_remaining_ms" "$observed_ms" "$continuous_ms"
sleep 1.2
observed_ms=$(jq -nr 'now * 1000 | floor')
continuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf "%.0f", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')
printf '{"schema_version":1,"sequence":2,"event":"renewed","kind":"election","name":"room","candidate_id":"agent","term":23,"authority_remaining_ms":%s,"authority_observed_unix_ms":%s,"authority_observed_continuous_ms":%s}\n' "$authority_remaining_ms" "$observed_ms" "$continuous_ms"
sleep 1.2
printf '%s\n' '{"schema_version":1,"sequence":3,"event":"released","kind":"election","name":"room","candidate_id":"agent","term":23}'
"#,
    );

    let worker_60 = directory.path().join("worker-60.sh");
    let worker_300 = directory.path().join("worker-300.sh");
    let marker_60 = directory.path().join("worker-60-started");
    let marker_300 = directory.path().join("worker-300-started");
    write_executable(
        &worker_60,
        &format!(
            "#!/bin/sh\ntouch '{}'\ntrap 'exit 0' TERM INT\nwhile :; do sleep 1; done\n",
            marker_60.display()
        ),
    );
    write_executable(
        &worker_300,
        &format!(
            "#!/bin/sh\ntouch '{}'\ntrap 'exit 0' TERM INT\nwhile :; do sleep 1; done\n",
            marker_300.display()
        ),
    );

    let spawn_supervisor = |worker: &Path, ttl: &str| {
        Command::new(&supervisor)
            .args([
                "election",
                "room",
                "agent",
                worker.to_str().unwrap(),
                "--",
                hold.to_str().unwrap(),
                "--ttl",
                ttl,
            ])
            .env("OCTOSTORE_SUPERVISOR_MAX_SILENCE_SECONDS", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut ttl_60 = spawn_supervisor(&worker_60, "60");
    let mut ttl_300 = spawn_supervisor(&worker_300, "300");
    let (status_60, status_300) = tokio::join!(
        wait_for_child(&mut ttl_60, Duration::from_secs(7)),
        wait_for_child(&mut ttl_300, Duration::from_secs(7))
    );

    assert_eq!(status_60.code(), Some(0));
    assert_eq!(status_300.code(), Some(0));
    assert!(marker_60.exists(), "60-second election never started work");
    assert!(
        marker_300.exists(),
        "300-second election never started work"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_kills_term_resistant_worker_group_after_heartbeat_loss() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("resistant-worker.sh");
    let pids = directory.path().join("worker-pids");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' TERM INT\nsh -c 'trap \"\" TERM INT; while :; do sleep 1; done' &\nchild=$!\nprintf '%s %s %s\\n' \"$$\" \"$child\" \"$OCTOSTORE_FENCING_TERM\" > '{}'\nwhile :; do sleep 1; done\n",
            pids.display()
        ),
    );
    let silent_hold = directory.path().join("silent-hold.sh");
    write_executable(
        &silent_hold,
        "#!/bin/sh\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{\"schema_version\":1,\"sequence\":1,\"event\":\"acquired\",\"kind\":\"lock\",\"name\":\"room\",\"term\":17,\"authority_remaining_ms\":23000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}\\n' \"$observed_ms\" \"$continuous_ms\"\nsleep 30\n",
    );

    let mut process = Command::new(&supervisor)
        .args([
            "lock",
            "room",
            "-",
            worker.to_str().unwrap(),
            "--",
            silent_hold.to_str().unwrap(),
            "--ttl",
            "3600",
        ])
        .env("OCTOSTORE_SUPERVISOR_MAX_SILENCE_SECONDS", "2")
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(4);
    while !pids.exists() {
        assert!(process.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "worker group did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let status = wait_for_child(&mut process, Duration::from_secs(6)).await;
    assert_eq!(status.code(), Some(20));
    let fields = std::fs::read_to_string(&pids).unwrap();
    let fields = fields.split_whitespace().collect::<Vec<_>>();
    assert_eq!(
        fields[2], "17",
        "supervisor must hand the fencing term to work"
    );
    for pid in &fields[..2] {
        let status = Command::new("kill")
            .args(["-0", pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "TERM-resistant process {pid} survived KILL escalation"
        );
    }
}

#[tokio::test]
#[serial_test::serial]
async fn reference_supervisor_hup_stops_detached_resistant_groups_and_exits_129() {
    let directory = tempfile::tempdir().unwrap();
    let supervisor = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/reference-supervisor.sh");
    let worker = directory.path().join("hup-resistant-worker.sh");
    let worker_pids = directory.path().join("hup-worker-pids");
    write_executable(
        &worker,
        &format!(
            "#!/bin/sh\ntrap '' HUP TERM INT\nsh -c 'trap \"\" HUP TERM INT; while :; do sleep 1; done' &\nchild=$!\nprintf '%s %s\\n' \"$$\" \"$child\" > '{}'\nwhile :; do sleep 1; done\n",
            worker_pids.display()
        ),
    );
    let hold = directory.path().join("hup-resistant-hold.sh");
    let hold_pid = directory.path().join("hup-hold-pid");
    write_executable(
        &hold,
        &format!(
            "#!/bin/sh\ntrap '' HUP TERM INT\necho $$ > '{}'\nobserved_ms=$(jq -nr 'now * 1000 | floor')\ncontinuous_ms=$(perl -MTime::HiRes=clock_gettime -e 'printf \"%.0f\", clock_gettime($^O eq q(darwin) ? Time::HiRes::CLOCK_MONOTONIC_RAW() : Time::HiRes::CLOCK_BOOTTIME()) * 1000')\nprintf '{{\"schema_version\":1,\"sequence\":1,\"event\":\"acquired\",\"kind\":\"lock\",\"name\":\"room\",\"term\":29,\"authority_remaining_ms\":23000,\"authority_observed_unix_ms\":%s,\"authority_observed_continuous_ms\":%s}}\\n' \"$observed_ms\" \"$continuous_ms\"\nwhile :; do sleep 1; done\n",
            hold_pid.display()
        ),
    );

    let mut process = Command::new(&supervisor)
        .args([
            "lock",
            "room",
            "-",
            worker.to_str().unwrap(),
            "--",
            hold.to_str().unwrap(),
            "--ttl",
            "3600",
        ])
        .env("OCTOSTORE_SUPERVISOR_TERM_GRACE_SECONDS", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    while !worker_pids.exists() || !hold_pid.exists() {
        assert!(process.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "supervised groups did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let signal_status = Command::new("kill")
        .args(["-HUP", &process.id().to_string()])
        .status()
        .unwrap();
    assert!(signal_status.success());
    let status = wait_for_child(&mut process, Duration::from_secs(6)).await;
    assert_eq!(status.code(), Some(129));

    let pids = format!(
        "{} {}",
        std::fs::read_to_string(&worker_pids).unwrap(),
        std::fs::read_to_string(&hold_pid).unwrap()
    );
    for pid in pids.split_whitespace() {
        let status = Command::new("kill")
            .args(["-0", pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "HUP left detached protected process {pid} running"
        );
    }
}
