//! Privacy-safe panic and asynchronous task-failure reporting.
//!
//! Panic payloads can contain arbitrary values captured by assertions or
//! visitor-adjacent code. They must never be formatted into operational output.
//! The process hook uses one best-effort low-level stderr write: it does not
//! allocate, lock stderr, or invoke tracing, so a panic inside a tracing writer
//! cannot recursively re-enter that writer. Joined task failures receive richer
//! category-only structured diagnostics after unwinding has completed.

use std::panic;
use std::sync::Once;

use tokio::task::JoinError;
use tracing::warn;

static INSTALL_HOOK: Once = Once::new();
const REDACTED_PANIC: &[u8] = b"HNSSH panic: payload redacted\n";

/// Execute the real process startup boundary under payload-redacting panic
/// protection. HNSSH's synchronous `main` places tracing setup and Tokio runtime
/// construction inside this closure, so the hook is active before either.
pub fn privacy_safe_process_entry<T>(entry: impl FnOnce() -> T) -> T {
    install_privacy_safe_panic_hook();
    entry()
}

pub(crate) fn install_privacy_safe_panic_hook() {
    INSTALL_HOOK.call_once(|| {
        panic::set_hook(Box::new(|_| write_redacted_stderr()));
    });
}

#[cfg(unix)]
fn write_redacted_stderr() {
    // SAFETY: this is one best-effort write from a static byte slice to the
    // process stderr descriptor. No Rust lock, allocator, formatter, tracing
    // subscriber, or panic payload is touched. Short writes are acceptable in
    // an already-panicking process.
    let _ = unsafe {
        libc::write(
            libc::STDERR_FILENO,
            REDACTED_PANIC.as_ptr().cast(),
            REDACTED_PANIC.len(),
        )
    };
}

#[cfg(not(unix))]
fn write_redacted_stderr() {
    // HNSSH supports macOS and Linux. Keep unsupported targets payload-safe by
    // emitting nothing rather than falling back to a potentially locked writer.
}

/// Report a Tokio task failure without invoking `JoinError`'s payload-bearing
/// `Display` or `Debug` implementations.
pub(crate) fn report_join_error(error: JoinError, scope: &'static str) {
    if error.is_cancelled() {
        return;
    }
    let failure = if error.is_panic() { "panic" } else { "join" };
    warn!(
        task.scope = scope,
        task.failure = failure,
        "asynchronous task failed"
    );
}
