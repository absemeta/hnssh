//! Complete-frame mailbox between synchronous rendering and SSH flow control.
//!
//! Only the writer's active frame and the latest pending frame survive a slow
//! peer. ANSI diffs are computed against the last *submitted* complete frame,
//! never against a frame replaced in the mailbox. An active diff is never
//! superseded mid-stream (except at termination, when only restoration follows).
use std::io;
use std::sync::Arc;
use std::time::Duration;

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use russh::ChannelWriteHalf;
use russh::server;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::ui::{RenderOutput, TerminalHyperlink};

// ST closes a possibly partial OSC from a cancelled frame; explicitly close
// OSC 8 before restoring styles and modes. Neither adds visible content.
pub(crate) const LEAVE_SCREEN: &[u8] = b"\x1b\\\x1b]8;;\x1b\\\x1b[0m\x1b[?25h\x1b[?1049l";
const ENTER_SCREEN: &[u8] = b"\x1b[?1049h\x1b[0m\x1b[2J\x1b[H\x1b[?25l";
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

struct CompleteFrame {
    cells: Buffer,
    hyperlinks: Vec<TerminalHyperlink>,
    viewport_revision: u64,
}

pub(crate) struct TerminalOutput {
    // Sink deliberately discards Ratatui's local diffs: the writer must diff
    // against its delivered baseline, not the actor's most recent render.
    terminal: Terminal<CrosstermBackend<io::Sink>>,
    pending: watch::Sender<Option<Arc<CompleteFrame>>>,
    finish: Option<oneshot::Sender<(u32, &'static [u8])>>,
    writer: JoinSet<()>,
    failed: CancellationToken,
    viewport_revision: u64,
}

impl TerminalOutput {
    pub(crate) fn new(
        area: Rect,
        channel: ChannelWriteHalf<server::Msg>,
        handle: server::Handle,
    ) -> io::Result<Self> {
        let terminal = Terminal::with_options(
            CrosstermBackend::new(io::sink()),
            TerminalOptions {
                viewport: Viewport::Fixed(area),
            },
        )?;
        let (pending, frames) = watch::channel(None);
        let (finish, finished) = oneshot::channel();
        let failed = CancellationToken::new();
        let failure = failed.clone();
        let mut writer = JoinSet::new();
        writer.spawn(async move {
            write_frames(channel, handle, frames, finished, failure).await;
        });
        Ok(Self {
            terminal,
            pending,
            finish: Some(finish),
            writer,
            failed,
            viewport_revision: 0,
        })
    }

    pub(crate) fn failed(&self) -> CancellationToken {
        self.failed.clone()
    }

    pub(crate) fn resize(&mut self, area: Rect) -> io::Result<()> {
        // Even a resize away and back while blocked invalidates the physical
        // screen, though the writer may never observe the intermediate size.
        self.viewport_revision = self.viewport_revision.wrapping_add(1);
        self.terminal.resize(area)
    }

    pub(crate) fn draw(
        &mut self,
        render: impl FnOnce(&mut Frame) -> RenderOutput,
    ) -> io::Result<Vec<usize>> {
        let mut output = RenderOutput::default();
        let frame = self.terminal.draw(|frame| output = render(frame))?;
        self.pending.send_replace(Some(Arc::new(CompleteFrame {
            cells: frame.buffer.clone(),
            hyperlinks: output.hyperlinks,
            viewport_revision: self.viewport_revision,
        })));
        Ok(output.comment_candidates)
    }

    pub(crate) async fn finish(&mut self, status: u32, notice: &'static [u8]) {
        self.pending.send_replace(None);
        if let Some(finish) = self.finish.take() {
            let _ = finish.send((status, notice));
        }
        while self.writer.join_next().await.is_some() {}
    }
}

async fn write_frames(
    channel: ChannelWriteHalf<server::Msg>,
    handle: server::Handle,
    mut frames: watch::Receiver<Option<Arc<CompleteFrame>>>,
    mut finished: oneshot::Receiver<(u32, &'static [u8])>,
    failed: CancellationToken,
) {
    let mut previous: Option<Arc<CompleteFrame>> = None;
    let mut entering = true;
    let (status, notice) = loop {
        let next = tokio::select! {
            biased;
            status = &mut finished => break status.unwrap_or((1, b"")),
            changed = frames.changed() => {
                if changed.is_err() { break (1, b"".as_slice()); }
                frames.borrow_and_update().clone()
            }
        };
        let Some(next) = next else { continue };
        let bytes = match encode_frame(previous.as_deref(), &next, entering) {
            Ok(bytes) => bytes,
            Err(_) => {
                failed.cancel();
                break (1, b"".as_slice());
            }
        };
        // ChannelWriteHalf, unlike Handle::data, reserves the peer's SSH
        // window and chunks at its maximum packet size. Russh's event queue
        // therefore contains bounded packets, not arbitrarily large frames.
        let sent = tokio::select! {
            biased;
            status = &mut finished => break status.unwrap_or((1, b"")),
            sent = channel.data_bytes(bytes) => sent,
        };
        if sent.is_err() {
            failed.cancel();
            break (1, b"".as_slice());
        }
        entering = false;
        previous = Some(next);
    };

    // Fixed-size restoration may bypass the exhausted window to be attempted
    // even after a cancelled, partially submitted frame. It is the ONLY data
    // sent via Handle::data; no render can build up in Russh's pending queue.
    // A dead/unwritable peer cannot extend this teardown indefinitely.
    let id = channel.id();
    let _ = timeout(CLEANUP_TIMEOUT, async {
        let mut restoration = LEAVE_SCREEN.to_vec();
        restoration.extend_from_slice(notice);
        let _ = handle.data(id, restoration).await;
        let _ = handle.exit_status_request(id, status).await;
        let _ = handle.eof(id).await;
        let _ = handle.close(id).await;
    })
    .await;
}

fn encode_frame(
    previous: Option<&CompleteFrame>,
    next: &CompleteFrame,
    entering: bool,
) -> io::Result<Vec<u8>> {
    // Hyperlinks are overlaid after Ratatui draws. Repaint when either screen
    // has overlays so unchanged cells cannot retain stale link/style state.
    let repaint = previous.is_none_or(|old| {
        old.cells.area != next.cells.area
            || old.viewport_revision != next.viewport_revision
            || !old.hyperlinks.is_empty()
            || !next.hyperlinks.is_empty()
    });
    let mut bytes = Vec::new();
    if entering {
        bytes.extend_from_slice(ENTER_SCREEN);
    }
    if repaint {
        bytes.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[H");
    }
    let empty = Buffer::empty(next.cells.area);
    let baseline = if repaint {
        &empty
    } else {
        &previous.expect("diff baseline").cells
    };
    {
        let mut backend = CrosstermBackend::new(&mut bytes);
        backend.draw(baseline.diff(&next.cells).into_iter())?;
        backend.hide_cursor()?;
        backend.flush()?;
    }
    for hyperlink in &next.hyperlinks {
        append_terminal_hyperlink(&mut bytes, hyperlink);
    }
    Ok(bytes)
}

fn append_terminal_hyperlink(output: &mut Vec<u8>, hyperlink: &TerminalHyperlink) {
    let position = format!(
        "\x1b[{};{}H",
        hyperlink.row.saturating_add(1),
        hyperlink.column.saturating_add(1),
    );
    output.extend_from_slice(position.as_bytes());
    output.extend_from_slice(b"\x1b]8;;");
    output.extend_from_slice(hyperlink.destination.as_bytes());
    output.extend_from_slice(b"\x1b\\\x1b[34;4m");
    output.extend_from_slice(hyperlink.text.as_bytes());
    output.extend_from_slice(b"\x1b]8;;\x1b\\\x1b[0m");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_hyperlink_uses_osc_8_and_one_based_coordinates() {
        let mut output = Vec::new();
        append_terminal_hyperlink(
            &mut output,
            &TerminalHyperlink {
                column: 1,
                row: 3,
                text: "https://example.com/story".to_owned(),
                destination: "https://example.com/story".to_owned(),
            },
        );
        assert_eq!(
            output,
            b"\x1b[4;2H\x1b]8;;https://example.com/story\x1b\\\x1b[34;4mhttps://example.com/story\x1b]8;;\x1b\\\x1b[0m"
        );
    }
}
