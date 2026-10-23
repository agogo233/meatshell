use super::*;
use std::net::TcpListener;
use std::thread;
use std::time::Instant;

fn server(responses: Vec<String>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let thread = thread::spawn(move || {
        for response in responses {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing update request");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
            }
            assert!(String::from_utf8(request)
                .unwrap()
                .to_lowercase()
                .contains("user-agent: meatshell-update-check"));
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    (url, thread)
}

fn response(status: u16, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} Test\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",
        body.len()
    )
}

#[test]
fn manifest_checks_do_not_need_the_api_even_during_a_cooldown() {
    let body = release_body().to_string().replace("0.8.0", "99.0.0");
    let (manifest, server) = server(vec![response(200, "", &body)]);
    let cooldown = Mutex::new(Some(SystemTime::now() + Duration::from_secs(3600)));
    let release = check_release(&manifest, "http://127.0.0.1:0", &cooldown)
        .unwrap()
        .unwrap();
    assert_eq!(release.tag_name, "v99.0.0");
    assert!(release.windows_installer().is_ok());
    server.join().unwrap();
}

#[test]
fn current_manifest_does_not_fall_back_to_the_api() {
    let mut body = release_body();
    body["tag_name"] = env!("CARGO_PKG_VERSION").into();
    let (manifest, server) = server(vec![response(200, "", &body.to_string())]);
    assert!(
        check_release(&manifest, "http://127.0.0.1:0", &Mutex::new(None))
            .unwrap()
            .is_none()
    );
    server.join().unwrap();
}

#[test]
fn absent_manifest_uses_the_legacy_api_after_cooldown_expires() {
    let (manifest, manifest_server) = server(vec![response(404, "", "")]);
    let (api, api_server) = server(vec![response(200, "", &release_body().to_string())]);
    let cooldown = Mutex::new(Some(UNIX_EPOCH));
    assert!(check_release(&manifest, &api, &cooldown).is_ok());
    manifest_server.join().unwrap();
    api_server.join().unwrap();
}

#[test]
fn invalid_manifest_and_http_errors_do_not_silently_use_the_api() {
    for reply in [
        response(200, "", "invalid json"),
        response(403, "", ""),
        response(500, "", ""),
    ] {
        let (manifest, server) = server(vec![reply]);
        let error = check_release(&manifest, "http://127.0.0.1:0", &Mutex::new(None)).unwrap_err();
        assert!(!format!("{error:#}").contains("Could not check GitHub releases"));
        server.join().unwrap();
    }
}

#[test]
fn repeated_checks_respect_api_reset_time() {
    let reset = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let headers = format!("X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {reset}\r\n");
    let (manifest, manifest_server) = server(vec![response(404, "", ""); 2]);
    let (api, api_server) = server(vec![response(403, &headers, "API rate limit exceeded")]);
    let cooldown = Mutex::new(None);
    for _ in 0..2 {
        let error = check_release(&manifest, &api, &cooldown).unwrap_err();
        assert_eq!(
            error.downcast_ref::<RateLimited>().unwrap().retry_at,
            UNIX_EPOCH + Duration::from_secs(reset)
        );
    }
    manifest_server.join().unwrap();
    api_server.join().unwrap();
}

#[test]
fn retry_after_takes_priority_and_unknown_limits_wait_a_minute() {
    let now = UNIX_EPOCH + Duration::from_secs(1_000);
    let reply: ureq::Response = response(
        403,
        "Retry-After: 120\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 1100\r\n",
        "",
    )
    .parse()
    .unwrap();
    assert_eq!(
        rate_limit(&reply, now).unwrap().retry_at,
        now + Duration::from_secs(120)
    );
    let reply: ureq::Response = response(429, "", "").parse().unwrap();
    assert_eq!(
        rate_limit(&reply, now).unwrap().retry_at,
        now + Duration::from_secs(60)
    );
    let reply: ureq::Response = response(403, "", "Forbidden").parse().unwrap();
    assert!(rate_limit(&reply, now).is_none());
}

#[test]
fn retry_after_accepts_http_dates_and_expired_reset_waits_a_minute() {
    let now = UNIX_EPOCH;
    let reply: ureq::Response = response(429, "Retry-After: Thu, 01 Jan 1970 00:02:00 GMT\r\n", "")
        .parse()
        .unwrap();
    assert_eq!(
        rate_limit(&reply, now).unwrap().retry_at,
        now + Duration::from_secs(120)
    );
    let reply: ureq::Response = response(
        403,
        "X-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 0\r\n",
        "",
    )
    .parse()
    .unwrap();
    assert_eq!(
        rate_limit(&reply, now).unwrap().retry_at,
        now + Duration::from_secs(60)
    );
}
