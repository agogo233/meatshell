//! SCP wire-protocol engine.
//!
//! The frame formats and the byte-by-byte protocol table live in
//! [`super::common`]; this module is the only place in the SCP backend that
//! touches the raw channel. It drives the exec channel through russh's
//! `wait()` loop rather than `into_stream()`, because the stream adapter
//! drops both stderr and the exit status — and a silent remote `scp` failure
//! must not look like a finished transfer.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use russh::client;
use russh::ChannelMsg;

use crate::i18n::t;

/// Timeout for any single protocol read (the handshake byte, a frame line, a
/// data chunk, an ack). A stalled remote — dead network, hung scp — surfaces
/// as an error instead of a transfer row that never moves.
const SCP_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on one frame line: real frames are tiny, so anything longer is a
/// corrupt stream and must not balloon the buffer.
const SCP_MAX_LINE: usize = 64 * 1024;

/// Cap on the stderr collected for error messages: stderr can grow without
/// bound on a chatty remote, and the beginning of an scp error is the
/// informative part.
const SCP_MAX_STDERR: usize = 8 * 1024;

/// Buffered reader/writer over one exec channel running a remote `scp`.
pub(super) struct ScpPeer {
    channel: russh::Channel<client::Msg>,
    /// Lookahead bytes from a `Data` message the caller has not consumed.
    buf: Vec<u8>,
    pos: usize,
    /// Collected stderr, capped; appended to error messages.
    stderr: Vec<u8>,
    /// The exit status once the remote reported one.
    exit: Option<u32>,
    /// Set once the channel closed (a `Close` message).
    closed: bool,
}

impl ScpPeer {
    /// Open a channel and run `command` (a complete `scp ... -f/-t` command).
    pub(super) async fn open<H: client::Handler>(
        handle: &client::Handle<H>,
        command: &str,
    ) -> Result<Self> {
        let channel = handle
            .channel_open_session()
            .await
            .context("open scp transfer channel")?;
        channel
            .exec(true, command.as_bytes())
            .await
            .context("start remote scp")?;
        Ok(Self {
            channel,
            buf: Vec::new(),
            pos: 0,
            stderr: Vec::new(),
            exit: None,
            closed: false,
        })
    }

    /// Wait for at least one more buffered byte; `Ok(false)` = the channel
    /// ended. Drains stderr as it arrives so the remote window can never
    /// fill up on a chatty error.
    async fn pump(&mut self) -> Result<bool> {
        while self.pos >= self.buf.len() {
            if self.closed {
                return Ok(false);
            }
            let msg = tokio::time::timeout(SCP_IO_TIMEOUT, self.channel.wait())
                .await
                .map_err(|_| anyhow!("scp {}", t("通道读取超时", "channel read timed out")))?
                .ok_or_else(|| anyhow!("scp {}", t("连接已关闭", "connection closed")))?;
            if self.absorb(msg) {
                self.closed = true;
                return Ok(false);
            }
            // An empty Data message loops for the next one.
        }
        Ok(true)
    }

    /// Absorb one channel message: buffer Data, cap-collect stderr, record
    /// the exit status. Returns true when the channel closed. `Eof` and
    /// anything else decide nothing — the next message does.
    fn absorb(&mut self, msg: ChannelMsg) -> bool {
        match msg {
            ChannelMsg::Data { data } => {
                self.buf = data.to_vec();
                self.pos = 0;
            }
            ChannelMsg::ExtendedData { data, .. } => {
                if self.stderr.len() < SCP_MAX_STDERR {
                    let keep = (SCP_MAX_STDERR - self.stderr.len()).min(data.len());
                    self.stderr.extend_from_slice(&data[..keep]);
                }
            }
            ChannelMsg::ExitStatus { exit_status } => {
                self.exit = Some(exit_status);
            }
            ChannelMsg::Close => return true,
            _ => {}
        }
        false
    }

    /// Consume one byte; `Ok(None)` = the channel ended.
    pub(super) async fn read_byte(&mut self) -> Result<Option<u8>> {
        if !self.pump().await? {
            return Ok(None);
        }
        let byte = self.buf[self.pos];
        self.pos += 1;
        Ok(Some(byte))
    }

    /// Consume one frame line, excluding the trailing `\n`; `Ok(None)` = the
    /// channel ended before anything arrived. A partial line at EOF is a
    /// truncated frame, which is an error.
    pub(super) async fn read_line(&mut self) -> Result<Option<String>> {
        let mut line: Vec<u8> = Vec::new();
        loop {
            if self.pos < self.buf.len() {
                if let Some(nl) = self.buf[self.pos..].iter().position(|&b| b == b'\n') {
                    line.extend_from_slice(&self.buf[self.pos..self.pos + nl]);
                    self.pos += nl + 1;
                    return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
                }
                line.extend_from_slice(&self.buf[self.pos..]);
                self.pos = self.buf.len();
                if line.len() > SCP_MAX_LINE {
                    return Err(anyhow!("scp {}", t("帧超长", "frame line too long")));
                }
            }
            if !self.pump().await? {
                if line.is_empty() {
                    return Ok(None);
                }
                return Err(anyhow!("scp {}", t("帧不完整", "truncated scp frame")));
            }
        }
    }

    /// Consume up to `max` more bytes into `out`; returns how many were
    /// appended (`0` = the channel ended).
    pub(super) async fn read_chunk(&mut self, out: &mut Vec<u8>, max: usize) -> Result<usize> {
        if !self.pump().await? {
            return Ok(0);
        }
        let start = self.pos;
        let end = (self.pos + max).min(self.buf.len());
        out.extend_from_slice(&self.buf[start..end]);
        self.pos = end;
        Ok(end - start)
    }

    /// Send one `\x00` acknowledgement byte.
    pub(super) async fn ack(&mut self) -> Result<()> {
        self.channel
            .data(&b"\x00"[..])
            .await
            .context("send scp ack")
    }

    /// Send raw bytes: a frame line, file data, or the `\x00` terminator.
    pub(super) async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.channel.data(bytes).await.context("send scp data")
    }

    /// Read a response byte: `\x00` = acknowledged; `\x01`/`\x02` carry an
    /// error line instead; anything else is a protocol break.
    pub(super) async fn expect_ack(&mut self) -> Result<()> {
        match self.read_byte().await? {
            Some(0) => Ok(()),
            Some(1) | Some(2) => {
                let line = self.read_line().await?.unwrap_or_default();
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    Err(anyhow!(
                        "{}",
                        t("远端报告了一个错误", "the remote reported an error")
                    ))
                } else {
                    Err(anyhow!("{trimmed}"))
                }
            }
            Some(other) => Err(anyhow!(
                "scp {} {other:#04x}",
                t("意外应答字节", "unexpected ack byte")
            )),
            None => Err(anyhow!(
                "scp {}",
                t("应答前连接已关闭", "connection closed before the ack")
            )),
        }
    }

    /// Drain to the channel's end and return the exit status (`None` when
    /// the remote never reported one). Call once the protocol finished, so a
    /// failed remote scp cannot hide behind a clean byte exchange. The drain
    /// is bounded by one `SCP_IO_TIMEOUT` overall: a remote that keeps
    /// streaming instead of exiting surfaces as an error, and one that just
    /// drops the connection ends cleanly here.
    pub(super) async fn finish(&mut self) -> Result<Option<u32>> {
        let deadline = tokio::time::Instant::now() + SCP_IO_TIMEOUT;
        while !self.closed {
            let msg = match tokio::time::timeout_at(deadline, self.channel.wait()).await {
                Ok(msg) => msg,
                Err(_) => {
                    return Err(anyhow!(
                        "scp {}",
                        t(
                            "等待远端退出超时",
                            "timed out waiting for the remote to exit"
                        )
                    ));
                }
            };
            let Some(msg) = msg else {
                break;
            };
            if self.absorb(msg) {
                self.closed = true;
            }
            // The protocol is over; discard whatever else the remote sent.
            self.pos = self.buf.len();
        }
        Ok(self.exit)
    }

    /// The collected stderr text (appended to error messages by callers).
    pub(super) fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// Decorate an error with the stderr collected on the channel, if any.
    pub(super) fn with_stderr(&self, error: anyhow::Error) -> anyhow::Error {
        let tail = self.stderr_text();
        let trimmed = tail.trim();
        if trimmed.is_empty() {
            error
        } else {
            error.context(format!("{}: {}", t("远端输出", "remote output"), trimmed))
        }
    }

    /// Close the channel mid-protocol (abort and cancel paths).
    pub(super) async fn close(&mut self) {
        let _ = self.channel.close().await;
    }
}
