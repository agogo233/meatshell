//! Telnet session worker (issue #17).
//!
//! Like [`crate::terminal::serial`], this mirrors [`crate::ssh::spawn_session`]'s public
//! surface so the terminal UI is reused unchanged. Telnet is just a TCP byte
//! stream with in-band option negotiation (RFC 854/855), so the worker:
//!
//! * strips IAC command sequences out of the data stream before it reaches the
//!   terminal,
//! * answers option negotiation with a minimal, conservative policy (let the
//!   server echo and run in character mode; advertise our window size),
//! * doubles `0xFF` bytes in user input as the protocol requires,
//! * re-sends NAWS (window size) on every terminal resize,
//! * reports the session's terminal type on request (TERMINAL-TYPE, RFC 1091).
//!
//! There is no SFTP and no resource monitor — a Telnet console is a raw pipe.
//!
//! When the session has a saved username / password, the worker answers the
//! device's first login and password prompts with them (#507); see
//! [`AutoLogin`].

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::config::{Secret, Session};
use crate::i18n::t;
use crate::ssh::{SessionCommand, SessionEvent, SessionHandle};

// Telnet protocol bytes (RFC 854).
const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;

// Options we care about.
const OPT_ECHO: u8 = 1;
const OPT_SGA: u8 = 3; // suppress go-ahead → character-at-a-time mode
const OPT_TTYPE: u8 = 24; // terminal type (RFC 1091)
const OPT_NAWS: u8 = 31; // negotiate about window size

// TERMINAL-TYPE subnegotiation commands (RFC 1091).
const TTYPE_IS: u8 = 0;
const TTYPE_SEND: u8 = 1;
/// Longest subnegotiation payload we keep; we only parse `TTYPE SEND`, so
/// anything longer (e.g. a server's own option payloads) is simply dropped.
const MAX_SUB_PAYLOAD: usize = 64;

pub fn spawn_telnet_session(
    runtime: &tokio::runtime::Handle,
    tab_id: String,
    session: Session,
    initial_cols: u32,
    initial_rows: u32,
) -> (SessionHandle, UnboundedReceiver<SessionEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<SessionEvent>();

    let evt_for_task = evt_tx.clone();
    let join = runtime.spawn(async move {
        if let Err(err) = run_telnet(
            session,
            cmd_rx,
            evt_for_task.clone(),
            initial_cols,
            initial_rows,
        )
        .await
        {
            let _ = evt_for_task.send(SessionEvent::Closed(format!("{err:#}")));
        }
    });

    (
        SessionHandle {
            tab_id,
            commands: cmd_tx,
            join,
        },
        evt_rx,
    )
}

/// How long after connecting, and how much output, to watch for login prompts.
const AUTOLOGIN_WINDOW: Duration = Duration::from_secs(60);
const AUTOLOGIN_BUDGET: usize = 64 * 1024;
/// Longest prompt line kept for matching.
const PROMPT_LINE_MAX: usize = 200;

#[derive(Debug, PartialEq, Eq)]
enum Prompt {
    User,
    Password,
}

/// Recognise a login prompt at the end of the current output line. Password
/// is checked first so "User password:" is a password prompt.
fn classify_prompt(line: &str) -> Option<Prompt> {
    let line = line.trim_end().to_lowercase();
    const PASSWORD: &[&str] = &["password:", "passwd:", "password：", "密码:", "密码："];
    const USER: &[&str] = &[
        "login:", "username:", "user name:", "user:", "login：", "username：",
        "用户名:", "用户名：", "账号:", "账号：", "帐号:", "帐号：",
    ];
    if PASSWORD.iter().any(|p| line.ends_with(p)) {
        Some(Prompt::Password)
    } else if USER.iter().any(|p| line.ends_with(p)) {
        Some(Prompt::User)
    } else {
        None
    }
}

/// Answers a device's first login prompt with the saved username and its
/// first password prompt with the saved password (#507). Each prompt is
/// answered at most once, so wrong credentials fall back to manual typing
/// instead of looping; a blank field is left for the user to type. It stops
/// after the password is sent, when a prompt it already answered comes back,
/// or once [`AUTOLOGIN_BUDGET`] characters of output went by.
struct AutoLogin {
    user: String,
    password: Secret,
    line: String,
    sent_user: bool,
    sent_password: bool,
    seen: usize,
    done: bool,
}

impl AutoLogin {
    /// `None` when the session has nothing to send.
    fn new(user: &str, password: &Secret) -> Option<Self> {
        let user = user.trim().to_string();
        if user.is_empty() && password.is_empty() {
            return None;
        }
        Some(Self {
            user,
            password: password.clone(),
            line: String::new(),
            sent_user: false,
            sent_password: false,
            seen: 0,
            done: false,
        })
    }

    fn done(&self) -> bool {
        self.done
    }

    /// Feed decoded terminal output; returns what to type (ending in Enter)
    /// when the output ends at a prompt we should answer.
    fn feed(&mut self, text: &str) -> Option<Secret> {
        if self.done {
            return None;
        }
        self.seen += text.len();
        for ch in text.chars() {
            if ch == '\r' || ch == '\n' {
                self.line.clear();
            } else if !ch.is_control() {
                self.line.push(ch);
            }
        }
        if self.line.len() > PROMPT_LINE_MAX {
            let cut = self.line.len() - PROMPT_LINE_MAX;
            let cut = (cut..self.line.len())
                .find(|&i| self.line.is_char_boundary(i))
                .unwrap_or(self.line.len());
            self.line.drain(..cut);
        }
        let reply = match classify_prompt(&self.line) {
            Some(Prompt::User) if self.sent_user => {
                // Asked again: the login failed. Let the user take over.
                self.done = true;
                None
            }
            Some(Prompt::User) if !self.user.is_empty() => {
                self.sent_user = true;
                Some(Secret::new(format!("{}\r", self.user)))
            }
            Some(Prompt::Password) if self.sent_password => {
                self.done = true;
                None
            }
            Some(Prompt::Password) if !self.password.is_empty() => {
                self.sent_password = true;
                self.done = true; // nothing left to answer
                Some(Secret::new(format!("{}\r", self.password.as_str())))
            }
            _ => None,
        };
        if reply.is_some() {
            self.line.clear();
        }
        if self.seen > AUTOLOGIN_BUDGET {
            self.done = true;
        }
        reply
    }
}

/// User input as sent on the wire: re-encoded to the session charset, with
/// IAC (0xFF) doubled per RFC 854.
fn wire_input(encoder: &crate::terminal::TerminalEncoding, bytes: &[u8]) -> Vec<u8> {
    let bytes = encoder.encode(bytes);
    let mut out = Vec::with_capacity(bytes.len());
    for b in bytes {
        out.push(b);
        if b == IAC {
            out.push(IAC);
        }
    }
    out
}

/// Incoming-byte parser state for stripping IAC sequences.
enum TnState {
    Data,
    Iac,
    Opt(u8),         // saw IAC <DO/DONT/WILL/WONT>, awaiting option byte
    Sub(Vec<u8>),    // inside subnegotiation (payload so far), awaiting IAC
    SubIac(Vec<u8>), // inside subnegotiation, saw IAC (awaiting SE)
}

/// `IAC SB TERMINAL-TYPE IS <term> IAC SE` — our answer to `TTYPE SEND`.
fn ttype_is_subneg(term: &str) -> Vec<u8> {
    let mut v = vec![IAC, SB, OPT_TTYPE, TTYPE_IS];
    // Terminal types are plain ASCII (see `normalize_term_type`), so no byte
    // here can be IAC and nothing needs doubling.
    v.extend_from_slice(term.as_bytes());
    v.extend_from_slice(&[IAC, SE]);
    v
}

fn naws_subneg(cols: u32, rows: u32) -> Vec<u8> {
    let w = (cols.clamp(1, u16::MAX as u32)) as u16;
    let h = (rows.clamp(1, u16::MAX as u32)) as u16;
    let mut v = vec![IAC, SB, OPT_NAWS];
    // Width / height are 16-bit; any 255 byte inside must be doubled.
    for b in [
        (w >> 8) as u8,
        (w & 0xff) as u8,
        (h >> 8) as u8,
        (h & 0xff) as u8,
    ] {
        v.push(b);
        if b == IAC {
            v.push(IAC);
        }
    }
    v.extend_from_slice(&[IAC, SE]);
    v
}

async fn run_telnet(
    session: Session,
    mut commands: UnboundedReceiver<SessionCommand>,
    events: UnboundedSender<SessionEvent>,
    initial_cols: u32,
    initial_rows: u32,
) -> Result<()> {
    let host = session.host.trim().to_string();
    let port = if session.port == 0 { 23 } else { session.port };
    let addr = format!("{host}:{port}");

    let _ = events.send(SessionEvent::Status(format!(
        "{} {} ...",
        t("Telnet 连接中", "Telnet connecting"),
        addr
    )));

    // Direct, or tunnel through a SOCKS5 / HTTP proxy (reuses issue #7 plumbing).
    let stream = match crate::ssh::proxy::resolve(session.proxy.as_str()) {
        Some(p) => {
            let _ = events.send(SessionEvent::Status(format!(
                "{} {} → {}",
                t("经代理连接", "via proxy"),
                crate::ssh::proxy::describe(&p),
                addr
            )));
            crate::ssh::proxy::connect(&p, &host, port)
                .await
                .with_context(|| format!("proxy connect to {addr} failed"))?
        }
        None => TcpStream::connect(&addr)
            .await
            .with_context(|| format!("connect {addr} failed"))?,
    };
    let _ = stream.set_nodelay(true);

    let _ = events.send(SessionEvent::Connected);
    let _ = events.send(SessionEvent::Status(format!(
        "{} {}",
        t("已连接", "Connected"),
        addr
    )));

    let (mut rd, mut wr) = tokio::io::split(stream);

    // Proactively advertise the options we support: we will suppress go-ahead
    // and report our window size. Most gear replies DO and starts echoing.
    let mut hello = vec![IAC, WILL, OPT_SGA, IAC, WILL, OPT_NAWS];
    hello.extend_from_slice(&naws_subneg(initial_cols, initial_rows));
    wr.write_all(&hello).await.context("telnet write hello")?;
    wr.flush().await.ok();

    let mut state = TnState::Data;
    let term_type = session.effective_term_type().to_string();
    let mut buf = [0u8; 4096];
    // Per-session character set (#435): GBK console gear, Big5, etc. The
    // decoder is stateful so a multibyte char split across reads survives.
    let mut decoder = crate::terminal::TerminalEncoding::new(&session.encoding);
    let encoder = crate::terminal::TerminalEncoding::new(&session.encoding);
    // Saved credentials answer the first login / password prompts (#507).
    let mut autologin = AutoLogin::new(&session.user, &session.password);
    let connected_at = Instant::now();

    loop {
        tokio::select! {
            cmd = commands.recv() => {
                match cmd {
                    Some(SessionCommand::RawInput(bytes)) => {
                        // Never log keystroke bytes — they can be passwords (#15).
                        tracing::debug!("telnet write len={} bytes", bytes.len());
                        let out = wire_input(&encoder, &bytes);
                        if wr.write_all(&out).await.is_err() {
                            let _ = events.send(SessionEvent::Closed(
                                t("写入失败", "write failed").into()));
                            break;
                        }
                        let _ = wr.flush().await;
                    }
                    Some(SessionCommand::Resize(cols, rows)) => {
                        let _ = wr.write_all(&naws_subneg(cols, rows)).await;
                        let _ = wr.flush().await;
                    }
                    Some(SessionCommand::AddTunnel { .. })
                    | Some(SessionCommand::StopTunnel(_))
                    | Some(SessionCommand::StartTunnel(_))
                    | Some(SessionCommand::DeleteTunnel(_))
                    | Some(SessionCommand::SetResourceMonitoring(_)) => {}
                    Some(SessionCommand::KillProcess { reply, .. }) => {
                        let _ = reply.send(crate::ssh::ProcessKillResult {
                            success: false,
                            message: t("Telnet 不支持远程进程操作", "Remote process control is unavailable for Telnet sessions").into(),
                        });
                    }
                    Some(SessionCommand::Close) | None => break,
                }
            }
            r = rd.read(&mut buf) => {
                match r {
                    Ok(0) => break, // peer closed
                    Ok(n) => {
                        let mut data = Vec::with_capacity(n);
                        let mut replies: Vec<u8> = Vec::new();
                        process_incoming(&buf[..n], &mut state, &term_type, &mut data, &mut replies);
                        if !replies.is_empty() {
                            let _ = wr.write_all(&replies).await;
                            let _ = wr.flush().await;
                        }
                        if !data.is_empty() {
                            let text = decoder.decode(&data);
                            if !text.is_empty() {
                                let answer = match autologin.as_mut() {
                                    Some(login) if connected_at.elapsed() < AUTOLOGIN_WINDOW => {
                                        login.feed(&text)
                                    }
                                    _ => None,
                                };
                                if autologin.as_ref().is_some_and(|l| l.done())
                                    || connected_at.elapsed() >= AUTOLOGIN_WINDOW
                                {
                                    autologin = None;
                                }
                                let _ = events.send(SessionEvent::Output(text));
                                if let Some(answer) = answer {
                                    // Never log the answer; it may be the password.
                                    let out = wire_input(&encoder, answer.as_str().as_bytes());
                                    let _ = wr.write_all(&out).await;
                                    let _ = wr.flush().await;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::Closed(format!(
                            "{}: {e}", t("读取错误", "read error"))));
                        break;
                    }
                }
            }
        }
    }

    let _ = events.send(SessionEvent::Closed(
        t("连接已关闭", "connection closed").into(),
    ));
    Ok(())
}

/// Feed `input` through the IAC state machine: plain bytes accumulate into
/// `data`, and any negotiation responses we owe go into `replies`.
/// `term_type` answers a server's TERMINAL-TYPE request (#498).
fn process_incoming(
    input: &[u8],
    state: &mut TnState,
    term_type: &str,
    data: &mut Vec<u8>,
    replies: &mut Vec<u8>,
) {
    for &b in input {
        let next = match std::mem::replace(state, TnState::Data) {
            TnState::Data => {
                if b == IAC {
                    TnState::Iac
                } else {
                    data.push(b);
                    TnState::Data
                }
            }
            TnState::Iac => match b {
                IAC => {
                    // Escaped 0xFF literal in the data stream.
                    data.push(IAC);
                    TnState::Data
                }
                DO | DONT | WILL | WONT => TnState::Opt(b),
                SB => TnState::Sub(Vec::new()),
                // Standalone commands (GA, NOP, DM, …) — ignore.
                _ => TnState::Data,
            },
            TnState::Opt(cmd) => {
                respond_negotiation(cmd, b, replies);
                TnState::Data
            }
            TnState::Sub(payload) => {
                if b == IAC {
                    TnState::SubIac(payload)
                } else {
                    TnState::Sub(push_sub_byte(payload, b))
                }
            }
            TnState::SubIac(payload) => match b {
                // IAC SE ends the block: answer the one payload we understand.
                SE => {
                    if payload == [OPT_TTYPE, TTYPE_SEND] {
                        replies.extend_from_slice(&ttype_is_subneg(term_type));
                    }
                    TnState::Data
                }
                // IAC IAC is an escaped 0xFF payload byte.
                IAC => TnState::Sub(push_sub_byte(payload, IAC)),
                _ => TnState::Sub(payload),
            },
        };
        *state = next;
    }
}

fn push_sub_byte(mut payload: Vec<u8>, b: u8) -> Vec<u8> {
    if payload.len() < MAX_SUB_PAYLOAD {
        payload.push(b);
    }
    payload
}

/// Conservative negotiation policy. Returns nothing; appends a 3-byte reply.
fn respond_negotiation(cmd: u8, opt: u8, replies: &mut Vec<u8>) {
    match cmd {
        // Server asks us to enable an option.
        DO => {
            let accept = matches!(opt, OPT_SGA | OPT_NAWS | OPT_TTYPE);
            replies.extend_from_slice(&[IAC, if accept { WILL } else { WONT }, opt]);
        }
        // Server tells us to disable — always agree.
        DONT => replies.extend_from_slice(&[IAC, WONT, opt]),
        // Server offers to enable an option on its side.
        WILL => {
            // Let the server echo and run in character mode; refuse the rest.
            let accept = matches!(opt, OPT_ECHO | OPT_SGA);
            replies.extend_from_slice(&[IAC, if accept { DO } else { DONT }, opt]);
        }
        // Server says it won't — acknowledge.
        WONT => replies.extend_from_slice(&[IAC, DONT, opt]),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(input: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut state = TnState::Data;
        let (mut data, mut replies) = (Vec::new(), Vec::new());
        process_incoming(input, &mut state, "xterm", &mut data, &mut replies);
        (data, replies)
    }

    #[test]
    fn agrees_to_report_terminal_type() {
        let (_, replies) = feed(&[IAC, DO, OPT_TTYPE]);
        assert_eq!(replies, [IAC, WILL, OPT_TTYPE]);
    }

    #[test]
    fn answers_terminal_type_send_with_session_type() {
        let (data, replies) = feed(&[b'a', IAC, SB, OPT_TTYPE, TTYPE_SEND, IAC, SE, b'b']);
        assert_eq!(data, b"ab");
        let mut expected = vec![IAC, SB, OPT_TTYPE, TTYPE_IS];
        expected.extend_from_slice(b"xterm");
        expected.extend_from_slice(&[IAC, SE]);
        assert_eq!(replies, expected);
    }

    #[test]
    fn answers_a_request_split_across_reads() {
        let mut state = TnState::Data;
        let (mut data, mut replies) = (Vec::new(), Vec::new());
        for chunk in [&[IAC, SB][..], &[OPT_TTYPE], &[TTYPE_SEND, IAC], &[SE]] {
            process_incoming(chunk, &mut state, "vt100", &mut data, &mut replies);
        }
        assert!(data.is_empty());
        assert!(replies.ends_with(b"vt100\xff\xf0"));
    }

    fn login(user: &str, password: &str) -> AutoLogin {
        AutoLogin::new(user, &Secret::new(password)).unwrap()
    }

    fn answer(login: &mut AutoLogin, text: &str) -> Option<String> {
        login.feed(text).map(|s| s.as_str().to_string())
    }

    #[test]
    fn recognises_common_prompts() {
        for p in ["login: ", "Username:", "User Name:", "R1 login:", "用户名：", "账号:"] {
            assert_eq!(classify_prompt(p), Some(Prompt::User), "{p}");
        }
        for p in ["Password: ", "password:", "User password:", "密码："] {
            assert_eq!(classify_prompt(p), Some(Prompt::Password), "{p}");
        }
        for p in ["Last login: Mon Oct 26 from 10.0.0.1", "Router>", "$ ", ""] {
            assert_eq!(classify_prompt(p), None, "{p}");
        }
    }

    #[test]
    fn answers_login_then_password_once() {
        let mut l = login("admin", "s3cret");
        assert_eq!(answer(&mut l, "Welcome\r\n"), None);
        assert_eq!(answer(&mut l, "\r\nlogin: ").as_deref(), Some("admin\r"));
        // The echoed username does not trigger anything.
        assert_eq!(answer(&mut l, "admin\r\n"), None);
        assert_eq!(answer(&mut l, "Password: ").as_deref(), Some("s3cret\r"));
        assert!(l.done());
        assert_eq!(answer(&mut l, "\r\nPassword: "), None);
    }

    #[test]
    fn prompts_split_across_reads_still_match() {
        let mut l = login("admin", "pw");
        assert_eq!(answer(&mut l, "\r\nUser"), None);
        assert_eq!(answer(&mut l, "name: ").as_deref(), Some("admin\r"));
    }

    #[test]
    fn blank_fields_are_left_to_the_user() {
        // Password-only devices (e.g. a console line asking just "Password:").
        let mut l = login("", "pw");
        assert_eq!(answer(&mut l, "login: "), None);
        assert_eq!(answer(&mut l, "\r\nPassword: ").as_deref(), Some("pw\r"));
        // Username only: the password prompt is the user's.
        let mut l = login("admin", "");
        assert_eq!(answer(&mut l, "login: ").as_deref(), Some("admin\r"));
        assert_eq!(answer(&mut l, "\r\nPassword: "), None);
        assert!(AutoLogin::new(" ", &Secret::default()).is_none());
    }

    #[test]
    fn a_failed_login_is_not_retried() {
        let mut l = login("admin", "");
        assert_eq!(answer(&mut l, "login: ").as_deref(), Some("admin\r"));
        assert_eq!(answer(&mut l, "\r\nLogin incorrect\r\nlogin: "), None);
        assert!(l.done());
    }

    #[test]
    fn stops_watching_after_the_output_budget() {
        let mut l = login("admin", "pw");
        let noise = "x".repeat(AUTOLOGIN_BUDGET + 1) + "\r\n";
        assert_eq!(answer(&mut l, &noise), None);
        assert!(l.done());
        assert_eq!(answer(&mut l, "login: "), None);
    }

    /// End to end: the worker logs in to a fake device that prompts for a
    /// username and password, then hands the session to the user.
    #[test]
    fn worker_logs_in_to_a_device() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        /// One answer line from the client, without its telnet negotiation.
        async fn read_line(rd: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> String {
            let mut buf = Vec::new();
            rd.read_until(b'\r', &mut buf).await.unwrap();
            let mut text = Vec::new();
            let mut i = 0;
            while i < buf.len() {
                if buf[i] == IAC {
                    if buf.get(i + 1) == Some(&SB) {
                        while i < buf.len() && !(buf[i] == IAC && buf.get(i + 1) == Some(&SE)) {
                            i += 1;
                        }
                        i += 2;
                    } else {
                        i += 3;
                    }
                } else {
                    text.push(buf[i]);
                    i += 1;
                }
            }
            String::from_utf8(text).unwrap()
        }
        // The device: prompt for a username, then a password.
        let device = rt.spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = sock.into_split();
            let mut rd = BufReader::new(rd);
            wr.write_all(b"Welcome to R1\r\n\r\nR1 login: ").await.unwrap();
            let user = read_line(&mut rd).await;
            wr.write_all(b"\r\nPassword: ").await.unwrap();
            let password = read_line(&mut rd).await;
            wr.write_all(b"\r\nR1> ").await.unwrap();
            (user, password)
        });

        let mut session = Session::new_empty();
        session.kind = crate::config::SessionKind::Telnet;
        session.host = "127.0.0.1".into();
        session.port = port;
        session.user = "admin".into();
        session.password = Secret::new("s3cret");
        let (handle, mut events) =
            spawn_telnet_session(rt.handle(), "t1".into(), session, 80, 24);
        let (user, password) = rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(10), device).await })
            .expect("device finished")
            .unwrap();
        assert_eq!(user, "admin\r");
        assert_eq!(password, "s3cret\r");
        // The prompts still reach the terminal.
        let output = rt.block_on(async {
            let mut all = String::new();
            while let Ok(Some(ev)) =
                tokio::time::timeout(Duration::from_millis(500), events.recv()).await
            {
                if let SessionEvent::Output(text) = ev {
                    all.push_str(&text);
                }
            }
            all
        });
        assert!(output.contains("R1 login: "), "{output:?}");
        assert!(output.contains("Password: "), "{output:?}");
        let _ = handle.commands.send(SessionCommand::Close);
    }

    #[test]
    fn wire_input_doubles_iac_bytes() {
        let utf8 = crate::terminal::TerminalEncoding::new("UTF-8");
        assert_eq!(wire_input(&utf8, b"ab\r"), b"ab\r");
        let latin1 = crate::terminal::TerminalEncoding::new("ISO-8859-1");
        let out = wire_input(&latin1, "ÿ".as_bytes());
        assert_eq!(out, [IAC, IAC]);
    }

    #[test]
    fn ignores_other_subnegotiations() {
        let (data, replies) = feed(&[IAC, SB, OPT_NAWS, 0, 80, 0, 24, IAC, SE, b'z']);
        assert_eq!(data, b"z");
        assert!(replies.is_empty());
    }
}
