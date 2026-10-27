//! Understand an OpenSSH client command line (`ssh [options] destination
//! [command]`) well enough to turn it into a saved session (#501).
//!
//! Only what a session needs is extracted: destination, user, port, identity
//! file and jump hosts. Like OpenSSH, the first value obtained for a setting
//! wins, command-line values beat `~/.ssh/config`, and options may also follow
//! the destination. Everything after the first non-option argument that
//! follows the destination is the remote command and is ignored.

use std::path::Path;

use super::ssh_config;
use super::structs::ImportedHost;

/// OpenSSH options that take an argument (`ssh(1)`: -B -b -c -D -E -e -F -I
/// -i -J -L -l -m -O -o -P -p -Q -R -S -W -w).
const OPTIONS_WITH_ARGUMENT: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// `[user@]host[:port]` as written in a destination or a `-J` hop.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

/// What an `ssh` command line asked for, before `~/.ssh/config` is applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SshInvocation {
    /// Host as typed: a hostname, an address, or a `~/.ssh/config` alias.
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
    /// `-o HostName=…`, which replaces the typed host for the connection.
    pub hostname: Option<String>,
    /// `-J` / `-o ProxyJump=` hops, first hop first. Empty for `none`.
    pub jump: Vec<Endpoint>,
    /// `-F configfile`.
    pub config_file: Option<String>,
}

/// The connection an invocation resolves to after `~/.ssh/config`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SshTarget {
    /// The `Host` alias that matched, if any (a good session name).
    pub alias: Option<String>,
    pub host: String,
    pub port: u16,
    /// Empty when neither the command line nor the config named a user.
    pub user: String,
    /// Empty when no identity file was given.
    pub identity_file: String,
    pub jump: Vec<Endpoint>,
}

fn first<T>(slot: &mut Option<T>, value: T) {
    if slot.is_none() {
        *slot = Some(value);
    }
}

fn parse_port(value: &str) -> Result<u16, String> {
    match value.trim().parse::<u16>() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err(format!("invalid port: {value}")),
    }
}

/// Parse `[user@]host[:port]` or `ssh://[user@]host[:port]`. A bare
/// destination only carries a port in URI form; for `-J` hops `allow_port`
/// accepts `host:port` too, as OpenSSH does. IPv6 literals go in brackets
/// when a port follows.
pub(crate) fn parse_endpoint(text: &str, allow_port: bool) -> Result<Endpoint, String> {
    let (body, uri) = match text.strip_prefix("ssh://") {
        Some(rest) => (rest.trim_end_matches('/'), true),
        None => (text, false),
    };
    let (user, host_port) = match body.rsplit_once('@') {
        Some((user, rest)) if !user.is_empty() => (Some(user.to_string()), rest),
        Some((_, rest)) => (None, rest),
        None => (None, body),
    };
    let ported = uri || allow_port;
    let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| format!("invalid destination: {text}"))?;
        match after.strip_prefix(':') {
            Some(port) if ported => (host.to_string(), Some(parse_port(port)?)),
            None if after.is_empty() => (host.to_string(), None),
            _ => return Err(format!("invalid destination: {text}")),
        }
    } else if ported && host_port.matches(':').count() == 1 {
        let (host, port) = host_port.split_once(':').unwrap();
        (host.to_string(), Some(parse_port(port)?))
    } else {
        (host_port.to_string(), None)
    };
    if host.is_empty() {
        return Err(format!("missing host in destination: {text}"));
    }
    Ok(Endpoint { user, host, port })
}

fn parse_jump(value: &str) -> Result<Vec<Endpoint>, String> {
    if value.trim().eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|hop| parse_endpoint(hop.trim(), true))
        .collect()
}

/// Apply one option that takes an argument. Unknown ones are ignored.
fn apply_option(
    inv: &mut SshInvocation,
    jump_set: &mut bool,
    flag: char,
    value: &str,
) -> Result<(), String> {
    match flag {
        'p' => first(&mut inv.port, parse_port(value)?),
        'l' => first(&mut inv.user, value.to_string()),
        'i' => first(&mut inv.identity_file, value.to_string()),
        'F' => first(&mut inv.config_file, value.to_string()),
        'J' => {
            if !*jump_set {
                inv.jump = parse_jump(value)?;
                *jump_set = true;
            }
        }
        'o' => {
            let (key, val) = value
                .split_once('=')
                .or_else(|| value.trim().split_once(char::is_whitespace))
                .unwrap_or((value, ""));
            let val = val.trim().trim_matches('"');
            match key.trim().to_ascii_lowercase().as_str() {
                "port" => first(&mut inv.port, parse_port(val)?),
                "user" => first(&mut inv.user, val.to_string()),
                "identityfile" => first(&mut inv.identity_file, val.to_string()),
                "hostname" => first(&mut inv.hostname, val.to_string()),
                "proxyjump" => apply_option(inv, jump_set, 'J', val)?,
                _ => {}
            }
        }
        _ => {}
    }
    Ok(())
}

/// Parse an `ssh` argv (`argv[0]` is the program and is skipped).
pub(crate) fn parse_command_line(argv: &[String]) -> Result<SshInvocation, String> {
    let mut inv = SshInvocation::default();
    let mut jump_set = false;
    let mut destination: Option<String> = None;
    let mut args = argv.iter().skip(1);
    let mut options_done = false;
    while let Some(arg) = args.next() {
        if !options_done && arg == "--" {
            options_done = true;
            continue;
        }
        let is_option = !options_done && arg.len() > 1 && arg.starts_with('-');
        if !is_option {
            if destination.is_none() {
                destination = Some(arg.clone());
                // OpenSSH keeps reading options after the destination.
                continue;
            }
            break; // remote command
        }
        let mut chars = arg[1..].char_indices();
        while let Some((at, flag)) = chars.next() {
            if !OPTIONS_WITH_ARGUMENT.contains(flag) {
                continue; // a flag such as -v / -t / -4
            }
            let attached = &arg[1 + at + flag.len_utf8()..];
            let value = if attached.is_empty() {
                args.next()
                    .ok_or_else(|| format!("option -{flag} requires an argument"))?
                    .as_str()
            } else {
                attached
            };
            apply_option(&mut inv, &mut jump_set, flag, value)?;
            break;
        }
    }
    let destination = destination.ok_or_else(|| "no destination host".to_string())?;
    let endpoint = parse_endpoint(&destination, false)?;
    inv.host = endpoint.host;
    if let Some(user) = endpoint.user {
        first(&mut inv.user, user);
    }
    if let Some(port) = endpoint.port {
        first(&mut inv.port, port);
    }
    Ok(inv)
}

/// Find the `~/.ssh/config` block for `host`, matched like OpenSSH: by alias,
/// case-insensitively.
pub(crate) fn find_config_host<'a>(
    hosts: &'a [ImportedHost],
    host: &str,
) -> Option<&'a ImportedHost> {
    hosts.iter().find(|h| h.alias.eq_ignore_ascii_case(host))
}

/// Resolve an invocation against the parsed config blocks. Command-line values
/// win; the config fills the gaps; the port defaults to 22.
pub(crate) fn resolve(
    inv: &SshInvocation,
    config: &[ImportedHost],
    home: &Path,
) -> Result<SshTarget, String> {
    let block = find_config_host(config, &inv.host);
    let host = inv
        .hostname
        .clone()
        .or_else(|| block.map(|b| b.hostname.clone()))
        .unwrap_or_else(|| inv.host.clone());
    if !ssh_config::is_valid_hostname(&host) {
        return Err(format!("invalid host name: {host}"));
    }
    let user = inv
        .user
        .clone()
        .or_else(|| block.map(|b| b.user.clone()).filter(|u| !u.is_empty()))
        .unwrap_or_default();
    let port = inv.port.or_else(|| block.map(|b| b.port)).unwrap_or(22);
    let identity_file = match &inv.identity_file {
        Some(path) => ssh_config::expand_tilde(path, home),
        None => block.map(|b| b.identity_file.clone()).unwrap_or_default(),
    };
    Ok(SshTarget {
        alias: block.map(|b| b.alias.clone()),
        host,
        port,
        user,
        identity_file,
        jump: inv.jump.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    fn parse(line: &str) -> SshInvocation {
        parse_command_line(&argv(line)).unwrap()
    }

    #[test]
    fn plain_and_user_at_host() {
        let inv = parse("ssh 192.168.100.2");
        assert_eq!(inv.host, "192.168.100.2");
        assert_eq!(inv.user, None);
        assert_eq!(inv.port, None);
        let inv = parse("ssh.exe root@pve.lan");
        assert_eq!(
            (inv.user.as_deref(), inv.host.as_str()),
            (Some("root"), "pve.lan")
        );
    }

    #[test]
    fn options_before_after_and_attached() {
        let inv = parse("ssh -p 2222 -i ~/.ssh/k jeff@h");
        assert_eq!(inv.port, Some(2222));
        assert_eq!(inv.identity_file.as_deref(), Some("~/.ssh/k"));
        // Options after the destination still apply; attached values work.
        let inv = parse("ssh h -p2200 -ljeff");
        assert_eq!(inv.port, Some(2200));
        assert_eq!(inv.user.as_deref(), Some("jeff"));
        // Flags can be bundled with a trailing option that takes a value.
        let inv = parse("ssh -tvp 2022 h");
        assert_eq!(inv.port, Some(2022));
    }

    #[test]
    fn first_value_wins_like_openssh() {
        let inv = parse("ssh -l first -o User=second third@h");
        assert_eq!(inv.user.as_deref(), Some("first"));
        let inv = parse("ssh -p 1 -p 2 h");
        assert_eq!(inv.port, Some(1));
    }

    #[test]
    fn remote_command_is_ignored() {
        let inv = parse("ssh h uptime -p 99");
        assert_eq!(inv.host, "h");
        assert_eq!(inv.port, None);
        let inv = parse("ssh -- h -p 99");
        assert_eq!(inv.host, "h");
        assert_eq!(inv.port, None);
    }

    #[test]
    fn options_with_arguments_do_not_become_the_destination() {
        let inv = parse("ssh -L 8080:localhost:80 -D 1080 -o ServerAliveInterval=30 -E log.txt h");
        assert_eq!(inv.host, "h");
    }

    #[test]
    fn dash_o_settings() {
        let inv =
            parse("ssh -o Port=2201 -o HostName=10.0.0.9 -oUser=deploy -o IdentityFile=~/k alias");
        assert_eq!(inv.port, Some(2201));
        assert_eq!(inv.hostname.as_deref(), Some("10.0.0.9"));
        assert_eq!(inv.user.as_deref(), Some("deploy"));
        assert_eq!(inv.identity_file.as_deref(), Some("~/k"));
        let inv = parse_command_line(&["ssh".into(), "-o".into(), "Port 2202".into(), "h".into()])
            .unwrap();
        assert_eq!(inv.port, Some(2202));
    }

    #[test]
    fn uri_destination_and_ipv6() {
        let inv = parse("ssh ssh://admin@example.com:2222");
        assert_eq!(
            (inv.user.as_deref(), inv.host.as_str(), inv.port),
            (Some("admin"), "example.com", Some(2222))
        );
        let inv = parse("ssh root@fe80::1");
        assert_eq!(inv.host, "fe80::1");
        assert_eq!(inv.port, None);
        let inv = parse("ssh ssh://[2001:db8::1]:22");
        assert_eq!((inv.host.as_str(), inv.port), ("2001:db8::1", Some(22)));
    }

    #[test]
    fn jump_hosts() {
        let inv = parse("ssh -J bastion,ops@hop2:2200 target");
        assert_eq!(
            inv.jump,
            vec![
                Endpoint {
                    user: None,
                    host: "bastion".into(),
                    port: None
                },
                Endpoint {
                    user: Some("ops".into()),
                    host: "hop2".into(),
                    port: Some(2200)
                },
            ]
        );
        assert!(parse("ssh -J none target").jump.is_empty());
        assert_eq!(parse("ssh -o ProxyJump=b target").jump.len(), 1);
    }

    #[test]
    fn errors() {
        assert!(parse_command_line(&argv("ssh -v")).is_err());
        assert!(parse_command_line(&argv("ssh -p")).is_err());
        assert!(parse_command_line(&argv("ssh -p abc h")).is_err());
        assert!(parse_command_line(&argv("ssh @")).is_err());
    }

    fn block(alias: &str, hostname: &str, user: &str, port: u16, key: &str) -> ImportedHost {
        ImportedHost {
            alias: alias.into(),
            hostname: hostname.into(),
            user: user.into(),
            port,
            identity_file: key.into(),
        }
    }

    #[test]
    fn resolve_applies_config_under_command_line() {
        let home = Path::new("/home/me");
        let config = vec![block(
            "prod",
            "10.0.0.5",
            "deploy",
            2222,
            "/home/me/.ssh/prod",
        )];
        let target = resolve(&parse("ssh PROD"), &config, home).unwrap();
        assert_eq!(target.alias.as_deref(), Some("prod"));
        assert_eq!(
            (
                target.host.as_str(),
                target.port,
                target.user.as_str(),
                target.identity_file.as_str()
            ),
            ("10.0.0.5", 2222, "deploy", "/home/me/.ssh/prod")
        );
        let target = resolve(
            &parse("ssh -p 22 -i ~/.ssh/other admin@prod"),
            &config,
            home,
        )
        .unwrap();
        assert_eq!((target.port, target.user.as_str()), (22, "admin"));
        assert_eq!(target.identity_file, "/home/me/.ssh/other");
        let target = resolve(&parse("ssh 192.168.1.9"), &config, home).unwrap();
        assert_eq!(target.alias, None);
        assert_eq!((target.port, target.user.as_str()), (22, ""));
    }

    #[test]
    fn resolve_rejects_hostile_hosts() {
        let home = Path::new("/home/me");
        assert!(resolve(&parse("ssh a;rm"), &[], home).is_err());
    }
}
