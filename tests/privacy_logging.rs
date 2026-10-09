use std::fmt;
use std::fs;
use std::future::Future;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use hnssh::{
    AdmissionLimits, ContentError, ContentSnapshot, ContentSource, EntryDetails, FeedEntry,
    FeedEntryId, Link, Server, ServerConfig, SessionTimeouts, privacy_safe_process_entry,
};
use russh::client;
use russh::keys::{Algorithm, PrivateKey, ssh_key};
use russh::{Channel, ChannelMsg};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::{Notify, oneshot};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const PRIVATE_ENTRY_ID: FeedEntryId = 9_007_199_254_740_991;
const PRIVATE_TITLE: &str = "round4-private-content-title";
const PRIVATE_AUTHOR: &str = "round4-private-content-author";
const PRIVATE_URL: &str = "https://round4-private.example/secret-path";
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Every field name HNSSH is allowed to emit. A source key can only escape if
/// code adds a new field, which makes this test fail and forces review.
const ALLOWED_LOG_FIELDS: &[&str] = &[
    "listener.address",
    "operations.address",
    "operations.failure",
    "upstream.status",
    "task.scope",
    "task.failure",
    "error",
    "message",
];

#[derive(Clone, Debug)]
struct RecordedEvent {
    target: String,
    fields: Vec<(String, String)>,
}

#[derive(Clone, Default)]
struct FieldRecorder(Arc<Mutex<Vec<RecordedEvent>>>);

impl FieldRecorder {
    fn events(&self) -> Vec<RecordedEvent> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[derive(Default)]
struct EventVisitor {
    fields: Vec<(String, String)>,
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields
            .push((field.name().to_owned(), format!("{value:?}")));
    }
}

impl<S: tracing::Subscriber> Layer<S> for FieldRecorder {
    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(RecordedEvent {
                target: event.metadata().target().to_owned(),
                fields: visitor.fields,
            });
    }
}

/// Install the isolated integration-test subscriber once per process and return
/// the recorder shared by every scenario.
fn recorded_events() -> FieldRecorder {
    static RECORDER: OnceLock<FieldRecorder> = OnceLock::new();
    RECORDER
        .get_or_init(|| {
            let recorder = FieldRecorder::default();
            tracing_subscriber::registry()
                .with(tracing_subscriber::EnvFilter::new("hnssh=trace"))
                .with(recorder.clone())
                .init();
            recorder
        })
        .clone()
}

struct PrivacyContent {
    loaded: Arc<Notify>,
}

impl ContentSource for PrivacyContent {
    fn top_feed(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async { Ok(ContentSnapshot::fresh(vec![PRIVATE_ENTRY_ID])) })
    }

    fn feed_entry(
        &self,
        id: FeedEntryId,
    ) -> Pin<Box<dyn Future<Output = Result<ContentSnapshot<FeedEntry>, ContentError>> + Send + '_>>
    {
        Box::pin(async move {
            if id != PRIVATE_ENTRY_ID {
                return Err(ContentError::Unavailable);
            }
            self.loaded.notify_one();
            let primary_link = Link {
                label: PRIVATE_TITLE.to_owned(),
                destination: PRIVATE_URL.to_owned(),
            };
            Ok(ContentSnapshot::fresh(FeedEntry::Story(EntryDetails {
                id,
                source: Some("round4-private-content-source".to_owned()),
                primary_link: Some(primary_link.clone()),
                links: vec![primary_link],
                title: Some(PRIVATE_TITLE.to_owned()),
                score: Some(1),
                author: Some(PRIVATE_AUTHOR.to_owned()),
                published_at: None,
                comment_count: Some(0),
                comment_ids: Vec::new(),
                text: None,
                omitted_link_count: 0,
            })))
        })
    }
}

struct PanickingWriter;

impl Write for PanickingWriter {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        panic!("private-panicking-writer-payload");
    }

    fn flush(&mut self) -> io::Result<()> {
        panic!("private-panicking-writer-flush-payload");
    }
}

struct TrustHostKey {
    expected: ssh_key::PublicKey,
    disconnected: Option<oneshot::Sender<()>>,
}

impl client::Handler for TrustHostKey {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(server_public_key == &self.expected)
    }

    async fn disconnected(
        &mut self,
        _reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        if let Some(disconnected) = self.disconnected.take() {
            let _ = disconnected.send(());
        }
        Ok(())
    }
}

fn make_host_key() -> TestResult<(TempDir, std::path::PathBuf, ssh_key::PublicKey)> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("host-key");
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    let public_key = key.public_key().clone();
    fs::write(&path, key.to_openssh(ssh_key::LineEnding::LF)?.as_bytes())?;
    Ok((directory, path, public_key))
}

async fn read_greeting(stream: &mut TcpStream) -> TestResult<String> {
    let mut greeting = Vec::new();
    timeout(TEST_TIMEOUT, async {
        while !greeting.ends_with(b"\n") {
            greeting.push(stream.read_u8().await?);
        }
        Ok::<_, io::Error>(())
    })
    .await??;
    Ok(String::from_utf8(greeting)?)
}

async fn expect_success(channel: &mut Channel<client::Msg>) -> TestResult {
    timeout(TEST_TIMEOUT, async {
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Success) => return Ok(()),
                Some(ChannelMsg::Failure) => return Err("SSH request failed".into()),
                Some(_) => {}
                None => return Err("SSH channel closed before success".into()),
            }
        }
    })
    .await?
}

async fn read_close(channel: &mut Channel<client::Msg>) -> TestResult<Vec<u8>> {
    timeout(TEST_TIMEOUT, async {
        let mut output = Vec::new();
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => output.extend_from_slice(&data),
                ChannelMsg::Close => return Ok(output),
                _ => {}
            }
        }
        Err("channel ended without close".into())
    })
    .await?
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(ALPHABET[((value >> 18) & 0x3f) as usize] as char);
        output.push(ALPHABET[((value >> 12) & 0x3f) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[((value >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(value & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn contains_key_shaped_token(value: &str) -> bool {
    value
        .split(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '+' | '/' | '=' | '-' | '_'))
        })
        .any(|token| {
            let token = token.trim_matches('=');
            let hex =
                token.len() >= 64 && token.chars().all(|character| character.is_ascii_hexdigit());
            let base64 = token.len() >= 43
                && token.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '+' | '/')
                });
            hex || base64
        })
}

fn assert_allowlisted_schema(
    events: &[RecordedEvent],
    listen_address: SocketAddr,
    private_username: &str,
    private_terminal: &str,
) {
    assert!(
        !events.is_empty(),
        "no HNSSH events were captured; the schema assertion would be vacuous"
    );
    assert!(
        events.iter().any(|event| event
            .fields
            .iter()
            .any(|(name, value)| name == "message" && value == "browsing session ended")),
        "session teardown was not observed: {events:#?}"
    );
    let peer_text = listen_address.ip().to_string();
    let private_entry_id = PRIVATE_ENTRY_ID.to_string();
    for event in events {
        assert!(
            event.target.starts_with("hnssh"),
            "unexpected log target {}",
            event.target
        );
        for (name, value) in &event.fields {
            assert!(
                ALLOWED_LOG_FIELDS.contains(&name.as_str()),
                "log schema gained an unallowlisted field {name:?}; source-key material \
                 could now escape: {events:#?}"
            );
            for private in [
                private_username,
                private_terminal,
                PRIVATE_TITLE,
                PRIVATE_AUTHOR,
                PRIVATE_URL,
                private_entry_id.as_str(),
            ] {
                assert!(
                    !value.contains(private),
                    "private visitor/content value {private:?} escaped through field {name:?}"
                );
            }
            if name != "listener.address" {
                assert!(
                    !value.contains(&peer_text),
                    "peer address {peer_text:?} escaped through field {name:?}"
                );
            }
            assert!(
                !contains_key_shaped_token(value),
                "key-shaped material escaped through field {name:?}: {value:?}"
            );
        }
    }
}

fn ipv6_loopback_available() -> bool {
    std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_ok()
}

async fn rejection_and_teardown_schema(listen_address: SocketAddr) -> TestResult<()> {
    let private_username = "round4-private-visitor-name";
    let private_terminal = "round4-private-terminal-\u{1b}[31m-\u{202e}";
    let recorder = recorded_events();
    let observed_before = recorder.events().len();

    let (_key_directory, host_key_path, public_key) = make_host_key()?;
    let content_loaded = Arc::new(Notify::new());
    let server = Server::bind_with_content(
        ServerConfig {
            listen_address,
            operations_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            host_key_path,
            admission_limits: AdmissionLimits {
                maximum_handshakes: 1,
                maximum_browsing_sessions: 1,
                maximum_connections_per_source: 10,
                maximum_source_buckets: 20,
                source_attempts_per_window: 1_000,
                source_attempt_burst: 100,
                source_attempt_window: Duration::from_secs(60),
            },
            session_timeouts: SessionTimeouts::default(),
        },
        Arc::new(PrivacyContent {
            loaded: Arc::clone(&content_loaded),
        }),
    )
    .await?;
    let address = server.local_addr()?;
    let operations_address = server.operations_addr();
    assert_eq!(address.ip(), listen_address.ip());
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(server.run(shutdown.clone()));

    let mut held = TcpStream::connect(address).await?;
    assert!(read_greeting(&mut held).await?.starts_with("SSH-2.0-"));
    let mut rejected = TcpStream::connect(address).await?;
    assert!(read_greeting(&mut rejected).await?.contains("busy"));
    drop(rejected);
    drop(held);

    let (mut connection, disconnected) = timeout(TEST_TIMEOUT, async {
        loop {
            let (closed, observed_close) = oneshot::channel();
            let handler = TrustHostKey {
                expected: public_key.clone(),
                disconnected: Some(closed),
            };
            match client::connect(Arc::new(client::Config::default()), address, handler).await {
                Ok(connection) => return (connection, observed_close),
                Err(_) => sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await?;
    assert!(
        connection
            .authenticate_none(private_username)
            .await?
            .success()
    );
    let mut channel = connection.channel_open_session().await?;
    channel
        .request_pty(true, private_terminal, 80, 24, 0, 0, &[])
        .await?;
    expect_success(&mut channel).await?;
    channel.request_shell(true).await?;
    expect_success(&mut channel).await?;
    timeout(TEST_TIMEOUT, content_loaded.notified()).await?;
    channel.data_bytes(b"q".as_slice()).await?;
    let channel_output = read_close(&mut channel).await?;
    assert!(!channel_output.is_empty());
    timeout(TEST_TIMEOUT, disconnected).await??;
    drop(connection);

    let metrics = reqwest::get(format!(
        "http://{operations_address}/metrics?visitor={private_username}"
    ))
    .await?
    .text()
    .await?;
    let private_source = address.ip().to_string();
    let private_entry_id = PRIVATE_ENTRY_ID.to_string();
    for private in [
        private_username,
        private_terminal,
        private_source.as_str(),
        PRIVATE_TITLE,
        PRIVATE_AUTHOR,
        PRIVATE_URL,
        private_entry_id.as_str(),
    ] {
        assert!(
            !metrics.contains(private),
            "private visitor value {private:?} escaped through metrics"
        );
    }
    assert!(!contains_key_shaped_token(&metrics));

    shutdown.cancel();
    timeout(TEST_TIMEOUT, task).await???;

    let events = recorder.events()[observed_before..].to_vec();
    assert_allowlisted_schema(&events, address, private_username, private_terminal);
    Ok(())
}

#[tokio::test]
async fn rejection_and_teardown_logs_match_the_allowlisted_private_schema() -> TestResult<()> {
    rejection_and_teardown_schema(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).await?;
    if ipv6_loopback_available() {
        rejection_and_teardown_schema(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)).await?;
    } else {
        eprintln!("IPv6 loopback is unavailable; skipped the optional IPv6 privacy scenario");
    }
    Ok(())
}

#[test]
fn production_panic_boundary_redacts_payloads_and_resists_a_panicking_writer() -> TestResult {
    const CHILD: &str = "HNSSH_PANIC_HOOK_CHILD";
    const PRIVATE_PAYLOAD: &str = "private-panic-payload-must-never-escape";
    if let Some(mode) = std::env::var_os(CHILD) {
        return privacy_safe_process_entry(|| match mode.to_string_lossy().as_ref() {
            // Production ordering: the hook is installed by the wrapper before
            // tracing setup and before the Tokio runtime is constructed.
            "startup" => {
                tracing_subscriber::fmt()
                    .without_time()
                    .with_ansi(false)
                    .with_env_filter("hnssh=trace")
                    .init();
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("test runtime")
                    .block_on(async { panic!("{PRIVATE_PAYLOAD}") })
            }
            "payload" => panic!("{PRIVATE_PAYLOAD}"),
            "writer" => {
                tracing_subscriber::fmt()
                    .without_time()
                    .with_ansi(false)
                    .with_writer(|| PanickingWriter)
                    .init();
                tracing::warn!("force the adversarial writer path");
                unreachable!("the writer must panic")
            }
            mode => panic!("unexpected child mode: {mode}"),
        });
    }

    for mode in ["startup", "payload", "writer"] {
        let output = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "production_panic_boundary_redacts_payloads_and_resists_a_panicking_writer",
                "--nocapture",
            ])
            .env(CHILD, mode)
            .output()?;
        assert!(
            !output.status.success(),
            "child panic in {mode} mode was unexpectedly hidden"
        );
        let stdout = String::from_utf8(output.stdout)?;
        let stderr = String::from_utf8(output.stderr)?;
        assert_eq!(
            stderr.matches("HNSSH panic: payload redacted").count(),
            1,
            "panic hook recursed or was skipped in {mode} mode: stdout={stdout:?}, stderr={stderr:?}"
        );
        for private in [
            PRIVATE_PAYLOAD,
            "private-panicking-writer-payload",
            "private-panicking-writer-flush-payload",
        ] {
            assert!(
                !stdout.contains(private),
                "private value {private:?} escaped to stdout in {mode} mode"
            );
            assert!(
                !stderr.contains(private),
                "private value {private:?} escaped to stderr in {mode} mode"
            );
        }
    }
    Ok(())
}

#[test]
fn key_shape_canary_detects_real_hmac_sha256_encodings() {
    let raw: [u8; 32] = std::array::from_fn(|index| (index as u8).wrapping_mul(7));
    let hex = raw
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let base64 = base64_encode(&raw);

    assert!(contains_key_shaped_token(&hex));
    assert!(contains_key_shaped_token(&base64));
    assert!(contains_key_shaped_token(&format!(
        "source key {hex} observed"
    )));
    assert!(!contains_key_shaped_token("browsing session ended"));
    assert!(!contains_key_shaped_token("127.0.0.1:54321"));
}

#[test]
fn production_binary_startup_path_runs_under_panic_protection() -> TestResult {
    let output = Command::new(env!("CARGO_BIN_EXE_hnssh"))
        .args(["--host-key", "/definitely/missing/hnssh-host-key"])
        .output()?;
    assert!(
        !output.status.success(),
        "startup with a missing host key must fail"
    );
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("LoadHostKey"),
        "real startup path did not reach host-key loading: {stderr:?}"
    );
    assert!(
        !stderr.contains("HNSSH panic"),
        "startup error path emitted a panic marker: {stderr:?}"
    );
    Ok(())
}
