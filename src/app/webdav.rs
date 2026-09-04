use super::*;

fn webdav_url(base: &str, remote_path: &str) -> Result<String> {
    let base = base.trim().trim_end_matches('/');
    if !base.starts_with("http://") && !base.starts_with("https://") {
        anyhow::bail!(
            "{}",
            t(
                "WebDAV 地址必须以 http:// 或 https:// 开头",
                "WebDAV URL must start with http:// or https://"
            )
        );
    }
    if base.starts_with("http://") && webdav_url_uses_port(base, 5006) {
        anyhow::bail!(
            "{}",
            t(
                "飞牛 WebDAV 的 5006 通常是 HTTPS 端口，请改用 https://...:5006；如果要用 HTTP，请改用 5005 端口",
                "FnOS WebDAV port 5006 is usually HTTPS; use https://...:5006, or use port 5005 for HTTP"
            )
        );
    }
    if base.starts_with("https://") && webdav_url_uses_port(base, 5005) {
        anyhow::bail!(
            "{}",
            t(
                "飞牛 WebDAV 的 5005 通常是 HTTP 端口，请改用 http://...:5005；如果要用 HTTPS，请改用 5006 端口",
                "FnOS WebDAV port 5005 is usually HTTP; use http://...:5005, or use port 5006 for HTTPS"
            )
        );
    }
    if base.ends_with(".json") {
        return Ok(base.to_string());
    }
    let remote = remote_path.trim().trim_start_matches('/');
    if (webdav_url_uses_port(base, 5005) || webdav_url_uses_port(base, 5006))
        && !webdav_url_has_path(base)
        && !remote.contains('/')
    {
        anyhow::bail!(
            "{}",
            t(
                "飞牛 WebDAV 需要写入某个共享目录，不能直接写到根路径；请把 WebDAV 地址改成 https://IP:5006/all/，或把远端文件改成 all/meatshell-connections.json",
                "FnOS WebDAV needs a writable shared folder, not the server root; use https://IP:5006/all/ or set the remote file to all/meatshell-connections.json"
            )
        );
    }
    if remote.is_empty() {
        anyhow::bail!("{}", t("远端文件不能为空", "remote file cannot be empty"));
    }
    Ok(format!("{base}/{remote}"))
}

fn webdav_url_uses_port(base: &str, port: u16) -> bool {
    let Some(authority) = base.split("://").nth(1) else {
        return false;
    };
    let host_port = authority.split('/').next().unwrap_or(authority);
    host_port
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        == Some(port)
}

fn webdav_url_has_path(base: &str) -> bool {
    let Some(authority) = base.split("://").nth(1) else {
        return false;
    };
    authority
        .split_once('/')
        .is_some_and(|(_, path)| !path.is_empty())
}

fn webdav_auth_header(username: &str, password: &str) -> Option<String> {
    if username.is_empty() && password.is_empty() {
        return None;
    }
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    Some(format!(
        "Basic {}",
        STANDARD.encode(format!("{username}:{password}"))
    ))
}

fn webdav_auth_req(mut req: ureq::Request, auth: Option<&str>) -> ureq::Request {
    if let Some(auth) = auth {
        req = req.set("Authorization", auth);
    }
    req
}

fn webdav_agent(accept_invalid_certs: bool) -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(20));
    if accept_invalid_certs {
        let tls_config = ureq::rustls::ClientConfig::builder_with_provider(
            ureq::rustls::crypto::ring::default_provider().into(),
        )
        .with_protocol_versions(&[&ureq::rustls::version::TLS12, &ureq::rustls::version::TLS13])
        .expect("rustls ring provider supports TLS 1.2 and TLS 1.3")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(WebDavAcceptAnyCertVerifier))
        .with_no_client_auth();
        builder = builder.tls_config(Arc::new(tls_config));
    }
    builder.build()
}

fn webdav_error(e: ureq::Error) -> anyhow::Error {
    if let ureq::Error::Status(status, response) = e {
        let url = response.get_url().to_string();
        let body = response.into_string().unwrap_or_default();
        let body = body.trim();
        let detail = if body.is_empty() {
            String::new()
        } else {
            format!(": {}", body.chars().take(240).collect::<String>())
        };
        if status == 400 {
            return anyhow::anyhow!(
                "{}: {url}: status code 400{detail}",
                t(
                    "请求被 WebDAV 服务拒绝，请检查地址协议/端口是否匹配，以及远端文件所在目录是否已开启 WebDAV 协议访问",
                    "WebDAV rejected the request; check the URL scheme/port and whether the remote folder allows WebDAV access"
                )
            );
        }
        if status == 405 {
            return anyhow::anyhow!(
                "{}: {url}: status code 405{detail}",
                t(
                    "当前 WebDAV 路径不允许上传；飞牛请写入已开启协议访问的共享目录，例如 WebDAV 地址填 https://IP:5006/all/，或远端文件填 all/meatshell-connections.json",
                    "The current WebDAV path does not allow upload; for FnOS, write into a shared folder such as https://IP:5006/all/ or set remote file to all/meatshell-connections.json"
                )
            );
        }
        return anyhow::anyhow!("{url}: status code {status}{detail}");
    }
    let msg = e.to_string();
    if msg.contains("UnknownIssuer") || msg.contains("invalid peer certificate") {
        anyhow::anyhow!(
            "{} ({msg})",
            t(
                "HTTPS 证书不受信任；如果这是可信 NAS/局域网 WebDAV，请在设置里开启“信任自签名/内网证书”",
                "HTTPS certificate is not trusted; enable \"Trust self-signed / intranet certs\" for a trusted NAS/LAN WebDAV"
            )
        )
    } else {
        anyhow::anyhow!("{msg}")
    }
}

fn webdav_parent_dirs(url: &str) -> Vec<String> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Vec::new();
    };
    let Some((authority, path)) = rest.split_once('/') else {
        return Vec::new();
    };
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.len() <= 1 {
        return Vec::new();
    }
    let mut dirs = Vec::with_capacity(parts.len() - 1);
    let mut current = format!("{scheme}://{authority}");
    for part in parts.iter().take(parts.len() - 1) {
        current.push('/');
        current.push_str(part);
        current.push('/');
        dirs.push(current.clone());
        current.pop();
    }
    dirs
}

fn webdav_dir_missing_or_no_create_error() -> anyhow::Error {
    anyhow::anyhow!(
        "{}",
        t(
            "文件夹不存在也无权限创建",
            "folder does not exist and cannot be created"
        )
    )
}

fn webdav_dir_exists(agent: &ureq::Agent, url: &str, auth: Option<&str>) -> Result<bool> {
    let req = webdav_auth_req(agent.request("PROPFIND", url).set("Depth", "0"), auth);
    match req.call() {
        Ok(_) => Ok(true),
        Err(ureq::Error::Status(status, _)) if status == 404 || status == 409 => Ok(false),
        Err(ureq::Error::Status(status, _)) if status == 401 || status == 403 || status == 405 => {
            Err(webdav_dir_missing_or_no_create_error())
        }
        Err(e) => Err(webdav_error(e)),
    }
}

fn webdav_create_dir(agent: &ureq::Agent, url: &str, auth: Option<&str>) -> Result<()> {
    let req = webdav_auth_req(agent.request("MKCOL", url), auth);
    match req.call() {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(status, _)) if status == 405 => Ok(()),
        Err(ureq::Error::Status(status, _))
            if status == 401 || status == 403 || status == 404 || status == 409 =>
        {
            Err(webdav_dir_missing_or_no_create_error())
        }
        Err(e) => Err(webdav_error(e)),
    }
}

fn webdav_ensure_parent_dirs(agent: &ureq::Agent, url: &str, auth: Option<&str>) -> Result<()> {
    for dir in webdav_parent_dirs(url) {
        if !webdav_dir_exists(agent, &dir, auth)? {
            webdav_create_dir(agent, &dir, auth)?;
        }
    }
    Ok(())
}

/// Certificate verification is only meaningful for TLS, so skipping it over
/// plain HTTP would ship every saved credential in cleartext. Enforced in the
/// two transport chokepoints below, which every WebDAV path goes through.
pub(super) fn require_tls_for_skipped_verification(
    accept_invalid_certs: bool,
    base_url: &str,
) -> Result<()> {
    if !accept_invalid_certs || base_url.trim().to_ascii_lowercase().starts_with("https://") {
        return Ok(());
    }
    anyhow::bail!(
        "{}",
        t(
            "关闭证书校验时仅支持 https:// 地址，否则密码会明文传输",
            "with certificate verification disabled only https:// is allowed; plain HTTP would send passwords in cleartext"
        )
    )
}

/// One explicit confirmation before certificate verification is switched off:
/// once it is off, a man in the middle can read the credentials of every saved
/// session. Persisted settings keep whatever the user answered here.
pub(super) fn confirm_skip_cert_verification() -> bool {
    rfd::MessageDialog::new()
        .set_title(t("关闭证书校验？", "Disable certificate verification?"))
        .set_description(t(
            "关闭后本机无法确认 WebDAV 服务器身份，已保存的所有会话密码都可能被中间人读取。建议仅用于自签证书的可信局域网。",
            "After this the app cannot verify the WebDAV server's identity, and the passwords of every saved session can be read by a man in the middle. Use it only for trusted self-signed servers.",
        ))
        .set_level(rfd::MessageLevel::Warning)
        .set_buttons(rfd::MessageButtons::YesNo)
        .show()
        == rfd::MessageDialogResult::Yes
}

/// Suffix for WebDAV status lines while verification is off, so the risk stays
/// visible at the point of use and not only in the settings page.
pub(super) fn webdav_status_suffix(accept_invalid_certs: bool) -> String {
    if accept_invalid_certs {
        t("（证书校验已关闭）", "(certificate verification disabled)").to_string()
    } else {
        String::new()
    }
}

pub(super) fn webdav_put_json(
    base_url: &str,
    remote_path: &str,
    username: &str,
    password: &str,
    accept_invalid_certs: bool,
    json: String,
) -> Result<()> {
    require_tls_for_skipped_verification(accept_invalid_certs, base_url)?;
    let url = webdav_url(base_url, remote_path)?;
    let agent = webdav_agent(accept_invalid_certs);
    let auth = webdav_auth_header(username, password);
    webdav_ensure_parent_dirs(&agent, &url, auth.as_deref())?;
    let req = webdav_auth_req(
        agent.put(&url).set("Content-Type", "application/json"),
        auth.as_deref(),
    );
    req.send_string(&json).map(|_| ()).map_err(webdav_error)
}

pub(super) fn webdav_get_json(
    base_url: &str,
    remote_path: &str,
    username: &str,
    password: &str,
    accept_invalid_certs: bool,
) -> Result<String> {
    require_tls_for_skipped_verification(accept_invalid_certs, base_url)?;
    let url = webdav_url(base_url, remote_path)?;
    let agent = webdav_agent(accept_invalid_certs);
    let auth = webdav_auth_header(username, password);
    let req = webdav_auth_req(agent.get(&url), auth.as_deref());
    req.call()
        .map_err(webdav_error)?
        .into_string()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_cert_verification_requires_https() {
        // Verification on: plain HTTP is an ordinary (unencrypted) choice.
        assert!(require_tls_for_skipped_verification(false, "http://nas:5005/all/").is_ok());
        // Verification off: HTTP must be refused, HTTPS stays allowed.
        assert!(require_tls_for_skipped_verification(true, "http://nas:5005/all/").is_err());
        assert!(require_tls_for_skipped_verification(true, "https://nas:5006/all/").is_ok());
    }
}
