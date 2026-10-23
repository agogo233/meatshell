use super::*;

#[path = "check.rs"]
mod check;

fn release_body() -> serde_json::Value {
    serde_json::json!({
        "tag_name": "v0.8.0", "draft": false, "prerelease": false,
        "assets": [{
            "name": "meatshell-v0.8.0-windows-x86_64.msi",
            "browser_download_url": "https://github.com/agogo233/meatshell/releases/download/v0.8.0/meatshell-v0.8.0-windows-x86_64.msi",
            "size": 3,
            "digest": format!("sha256:{:x}", Sha256::digest(b"abc"))
        }]
    })
}

#[test]
fn only_offers_newer_stable_releases() {
    let mut body = release_body();
    assert!(newer_release(&body.to_string(), "0.7.7").unwrap().is_some());
    assert!(newer_release(&body.to_string(), "0.8.0").unwrap().is_none());
    assert!(newer_release(&body.to_string(), "0.9.0").unwrap().is_none());
    body["prerelease"] = true.into();
    assert!(newer_release(&body.to_string(), "0.7.7").unwrap().is_none());
    body["prerelease"] = false.into();
    body["draft"] = true.into();
    assert!(newer_release(&body.to_string(), "0.7.7").unwrap().is_none());
}

#[test]
fn malformed_release_is_an_error() {
    for tag in ["", "v0.8", "v0.8.0.extra", "v0.8.0-rc1", "v0.8.0/evil"] {
        let mut body = release_body();
        body["tag_name"] = tag.into();
        assert!(newer_release(&body.to_string(), "0.7.7").is_err(), "{tag}");
    }
    assert!(newer_release("<html>bad gateway</html>", "0.7.7").is_err());
}

#[test]
fn selects_the_matching_official_msi_and_requires_a_digest() {
    let mut body = release_body();
    let release: Release = serde_json::from_value(body.clone()).unwrap();
    assert!(release.windows_installer().is_ok());
    body["assets"][0]["name"] = "meatshell-v0.8.0-windows-x86_64.zip".into();
    let release: Release = serde_json::from_value(body).unwrap();
    assert!(release.windows_installer().is_err());
    for digest in [
        serde_json::Value::Null,
        "sha256:bad".into(),
        "md5:1234".into(),
    ] {
        let mut body = release_body();
        body["assets"][0]["digest"] = digest;
        let release: Release = serde_json::from_value(body).unwrap();
        assert!(release.windows_installer().is_err());
    }
}

#[test]
fn rejects_unexpected_download_origins_and_release_paths() {
    for url in [
        "http://github.com/agogo233/meatshell/releases/download/v0.8.0/meatshell-v0.8.0-windows-x86_64.msi",
        "https://github.com.evil.test/yituorou/meatshell/releases/download/v0.8.0/meatshell-v0.8.0-windows-x86_64.msi",
        "https://github.com/someone/other/releases/download/v0.8.0/meatshell-v0.8.0-windows-x86_64.msi",
        "https://github.com/agogo233/meatshell/releases/download/v0.7.0/meatshell-v0.8.0-windows-x86_64.msi",
    ] {
        let mut body = release_body();
        body["assets"][0]["browser_download_url"] = url.into();
        let release: Release = serde_json::from_value(body).unwrap();
        assert!(release.windows_installer().is_err(), "{url}");
    }
}

#[test]
fn verifies_download_and_reports_progress() {
    let mut output = Vec::new();
    let mut progress = Vec::new();
    let hash = format!("{:x}", Sha256::digest(b"abc"));
    copy_verified(&b"abc"[..], &mut output, 3, &hash, |n, total| {
        progress.push((n, total))
    })
    .unwrap();
    assert_eq!(output, b"abc");
    assert_eq!(progress.last(), Some(&(3, 3)));
}

#[test]
fn rejects_truncated_oversized_and_corrupted_downloads() {
    let hash = format!("{:x}", Sha256::digest(b"abc"));
    for bytes in [b"ab".as_slice(), b"abcd".as_slice(), b"xyz".as_slice()] {
        assert!(copy_verified(bytes, Vec::new(), 3, &hash, |_, _| {}).is_err());
    }
}

#[test]
fn propagates_network_and_disk_failures() {
    struct FailedRead;
    impl Read for FailedRead {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connection interrupted",
            ))
        }
    }
    struct FullDisk;
    impl Write for FullDisk {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert!(copy_verified(FailedRead, Vec::new(), 3, "", |_, _| {}).is_err());
    assert!(copy_verified(&b"abc"[..], FullDisk, 3, "", |_, _| {}).is_err());
}
