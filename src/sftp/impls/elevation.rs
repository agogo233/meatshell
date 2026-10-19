//! Working in the SFTP panel as another user (#438).
//!
//! Servers often refuse root logins, so a regular account cannot touch files
//! only root may read or write. Like WinSCP's "SFTP server" setting, the SFTP
//! channels can then run the server binary through `sudo` instead of
//! requesting the `sftp` subsystem (which always runs as the login user), or
//! run any custom SFTP server command. SFTP itself is unchanged: the client
//! speaks the same protocol over the exec channel's stdin/stdout.
//!
//! `sudo` mode works with passwordless sudo, or with the session's saved
//! login password, which is written to `sudo -S` on stdin before the SFTP
//! handshake starts (never on a command line).

use anyhow::{Context, Result};
use russh::client;

use crate::config::{AuthMethod, Session};
use crate::i18n::t;

/// `Session::sftp_mode` values.
pub(crate) const SFTP_MODE_SUDO: &str = "sudo";
pub(crate) const SFTP_MODE_CUSTOM: &str = "custom";

/// Where OpenSSH installs sftp-server on common systems: Debian/Ubuntu,
/// RHEL/Fedora/SUSE, Arch, macOS/BSD, and a couple of older layouts.
const SFTP_SERVER_PATHS: &[&str] = &[
    "/usr/lib/openssh/sftp-server",
    "/usr/libexec/openssh/sftp-server",
    "/usr/lib/ssh/sftp-server",
    "/usr/libexec/sftp-server",
    "/usr/lib/sftp-server",
    "/usr/sbin/sftp-server",
];

/// A problem with the session's SFTP privileges setting, worded for the user.
/// The SFTP panel shows it as is instead of its generic "SFTP unavailable"
/// hint, which would hide what to fix.
#[derive(Debug)]
pub(super) struct ElevationError(pub(super) String);

impl std::fmt::Display for ElevationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ElevationError {}

fn elevation_error(message: &str) -> anyhow::Error {
    anyhow::Error::new(ElevationError(message.to_string()))
}

/// How `sudo` is satisfied on this server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Sudo {
    /// NOPASSWD (or otherwise non-interactive) sudo.
    NoPassword,
    /// sudo asks for the login password, which is sent on stdin.
    Password(String),
}

impl Sudo {
    /// `sudo` invocation prefix. With a password, `-k` ignores any cached
    /// credentials so sudo *always* reads the password line: otherwise a
    /// cached timestamp would pass that line on to the SFTP server as junk.
    fn prefix(&self) -> &'static str {
        match self {
            Sudo::NoPassword => "sudo -n",
            Sudo::Password(_) => "sudo -S -k -p ''",
        }
    }

    /// What to write to the channel's stdin before anything else.
    fn stdin(&self) -> Option<String> {
        match self {
            Sudo::NoPassword => None,
            Sudo::Password(password) => Some(format!("{password}\n")),
        }
    }

    /// `cmd` (a POSIX shell command) run as root.
    fn wrap(&self, cmd: &str) -> String {
        format!("{} /bin/sh -c {}", self.prefix(), sh_quote(cmd))
    }
}

/// How an SFTP channel reaches the server's SFTP implementation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum SftpServer {
    /// The standard `sftp` subsystem, as the login user.
    #[default]
    Subsystem,
    /// Exec `command`, writing `stdin` first when set (the sudo password).
    Exec {
        command: String,
        stdin: Option<String>,
    },
}

/// Resolved once per SFTP connection, then used for every channel it opens.
#[derive(Clone, Debug, Default)]
pub(super) struct Elevation {
    server: SftpServer,
    /// Set in sudo mode: shell commands the panel runs (folder downloads use
    /// `tar` and `rm`) go through the same sudo so they can read what the
    /// SFTP channel can.
    sudo: Option<Sudo>,
}

impl Elevation {
    /// Work out how to open SFTP channels for `session`. Fails with a message
    /// for the user when sudo mode cannot work here.
    pub(super) async fn resolve<H: client::Handler>(
        handle: &client::Handle<H>,
        session: &Session,
        password: &str,
    ) -> Result<Self> {
        match session.sftp_mode.as_str() {
            SFTP_MODE_SUDO => {
                let sudo = detect_sudo(handle, session, password).await?;
                Ok(Self {
                    server: SftpServer::Exec {
                        command: sudo_sftp_server_command(&sudo),
                        stdin: sudo.stdin(),
                    },
                    sudo: Some(sudo),
                })
            }
            SFTP_MODE_CUSTOM if !session.sftp_command.trim().is_empty() => Ok(Self {
                server: SftpServer::Exec {
                    command: session.sftp_command.trim().to_string(),
                    stdin: None,
                },
                sudo: None,
            }),
            _ => Ok(Self::default()),
        }
    }

    /// True when SFTP channels do not use the plain subsystem.
    pub(super) fn is_custom(&self) -> bool {
        self.server != SftpServer::Subsystem
    }

    /// The error to show when the SFTP handshake fails on a custom server: it
    /// usually means the command could not start (sudo refused, no such
    /// sftp-server), which closes the channel without an SFTP reply.
    pub(super) fn handshake_error(&self) -> anyhow::Error {
        elevation_error(if self.sudo.is_some() {
            t(
                "无法通过 sudo 启动 sftp-server（服务器上可能找不到 sftp-server，或 sudo 不允许运行它）",
                "could not start sftp-server through sudo (it may not be installed where expected, or sudo does not allow it)",
            )
        } else {
            t(
                "自定义 SFTP 服务器命令未能启动，请检查会话设置中的命令",
                "the custom SFTP server command did not start; check the command in the session settings",
            )
        })
    }

    /// Short label for status messages ("as root" etc.); empty by default.
    pub(super) fn label(&self) -> String {
        match (&self.server, &self.sudo) {
            (SftpServer::Subsystem, _) => String::new(),
            (_, Some(_)) => t("（root）", " (root)").into(),
            (_, None) => t("（自定义服务器）", " (custom server)").into(),
        }
    }

    /// Open one SFTP channel and return its byte stream, ready for the SFTP
    /// handshake.
    pub(super) async fn open_stream<H: client::Handler>(
        &self,
        handle: &client::Handle<H>,
    ) -> Result<russh::ChannelStream<client::Msg>> {
        let channel = handle
            .channel_open_session()
            .await
            .context("open sftp channel")?;
        match &self.server {
            SftpServer::Subsystem => {
                channel
                    .request_subsystem(true, "sftp")
                    .await
                    .context("request sftp subsystem")?;
            }
            SftpServer::Exec { command, stdin } => {
                channel
                    .exec(true, command.as_bytes())
                    .await
                    .context("start sftp server command")?;
                if let Some(stdin) = stdin {
                    channel
                        .data(stdin.as_bytes())
                        .await
                        .context("send sudo password")?;
                }
            }
        }
        Ok(channel.into_stream())
    }

    /// Run a shell command the panel needs (tar / rm for folder downloads)
    /// with the same privileges as its SFTP channels. Returns the exit status.
    pub(super) async fn exec_shell<H: client::Handler>(
        &self,
        handle: &client::Handle<H>,
        cmd: &str,
    ) -> Result<u32> {
        let status = match &self.sudo {
            Some(sudo) => exec_status(handle, &sudo.wrap(cmd), sudo.stdin().as_deref()).await?,
            None => exec_status(handle, cmd, None).await?,
        };
        Ok(status.unwrap_or(255))
    }
}

/// Is sudo usable without a password, or with the session's login password?
async fn detect_sudo<H: client::Handler>(
    handle: &client::Handle<H>,
    session: &Session,
    password: &str,
) -> Result<Sudo> {
    match exec_status(handle, "sudo -n true", None).await? {
        Some(0) => return Ok(Sudo::NoPassword),
        Some(127) => {
            return Err(elevation_error(t(
                "服务器上没有 sudo，无法以 root 身份使用 SFTP",
                "sudo is not installed on the server, so SFTP cannot run as root",
            )))
        }
        _ => {}
    }
    // With key authentication the password field holds the key passphrase,
    // never the account password, so it must not be sent to sudo.
    if matches!(session.auth, AuthMethod::Key) || password.is_empty() {
        return Err(elevation_error(t(
            "sudo 需要密码：请为此会话保存登录密码，或在服务器上为该用户配置免密 sudo（NOPASSWD）",
            "sudo needs a password: save the login password for this session, or allow passwordless sudo (NOPASSWD) for this user",
        )));
    }
    let sudo = Sudo::Password(password.to_string());
    match exec_status(handle, &sudo.wrap("true"), sudo.stdin().as_deref()).await? {
        Some(0) => Ok(sudo),
        _ => Err(elevation_error(t(
            "sudo 拒绝了登录密码，或该用户没有 sudo 权限",
            "sudo rejected the login password, or this user may not use sudo",
        ))),
    }
}

/// Shell command that finds sftp-server and runs it through sudo. Wrapped in
/// `/bin/sh -c` so it works whatever the user's login shell is (fish, ...).
fn sudo_sftp_server_command(sudo: &Sudo) -> String {
    let script = format!(
        "for p in {}; do if [ -x \"$p\" ]; then exec {} \"$p\"; fi; done; \
         echo 'meatshell: sftp-server not found' >&2; exit 127",
        SFTP_SERVER_PATHS.join(" "),
        sudo.prefix()
    );
    format!("/bin/sh -c {}", sh_quote(&script))
}

/// Run `cmd` on its own exec channel, optionally feeding `stdin`, and return
/// its exit status (`None` if the server reported none, e.g. it was killed).
async fn exec_status<H: client::Handler>(
    handle: &client::Handle<H>,
    cmd: &str,
    stdin: Option<&str>,
) -> Result<Option<u32>> {
    let mut channel = handle
        .channel_open_session()
        .await
        .context("open exec channel")?;
    channel
        .exec(true, cmd.as_bytes())
        .await
        .context("exec remote command")?;
    if let Some(stdin) = stdin {
        channel
            .data(stdin.as_bytes())
            .await
            .context("write stdin")?;
    }
    let _ = channel.eof().await;
    let mut status = None;
    while let Some(msg) = channel.wait().await {
        match msg {
            russh::ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            russh::ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(status)
}

/// Single-quoted POSIX shell word.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwordless_sudo_runs_sftp_server_non_interactively() {
        let cmd = sudo_sftp_server_command(&Sudo::NoPassword);
        assert!(cmd.starts_with("/bin/sh -c '"));
        assert!(cmd.contains("exec sudo -n \"$p\""));
        assert!(cmd.contains("/usr/lib/openssh/sftp-server"));
        assert!(cmd.contains("/usr/libexec/openssh/sftp-server"));
        assert_eq!(Sudo::NoPassword.stdin(), None);
    }

    #[test]
    fn password_sudo_reads_the_password_from_stdin_never_the_command_line() {
        let sudo = Sudo::Password("s3cr'et".into());
        let cmd = sudo_sftp_server_command(&sudo);
        // The empty prompt's quotes are escaped for the outer single quotes.
        assert!(cmd.contains(r#"exec sudo -S -k -p '\'''\'' "$p""#));
        assert!(!cmd.contains("s3cr"));
        assert_eq!(sudo.stdin().as_deref(), Some("s3cr'et\n"));
    }

    #[test]
    fn shell_commands_are_quoted_inside_sudo() {
        let wrapped = Sudo::NoPassword.wrap("tar -cf '/tmp/a b.tar' -C '/root' 'x'");
        assert_eq!(
            wrapped,
            r#"sudo -n /bin/sh -c 'tar -cf '\''/tmp/a b.tar'\'' -C '\''/root'\'' '\''x'\'''"#
        );
    }

    #[test]
    fn modes_without_elevation_keep_the_subsystem() {
        let elevation = Elevation::default();
        assert!(!elevation.is_custom());
        assert_eq!(elevation.label(), "");
    }
}
