use super::{blank_forward_draft, validated_port_forwards};
use crate::tunnel::is_loopback_bind;

#[test]
fn blank_rows_are_ignored_when_saving() {
    assert!(validated_port_forwards(&[blank_forward_draft()])
        .unwrap()
        .is_empty());
}

#[test]
fn filled_rows_are_saved_without_an_add_step() {
    let mut local = blank_forward_draft();
    local.bind_port = "8080".into();
    local.host = "service.internal".into();
    local.host_port = "80".into();

    let mut dynamic = blank_forward_draft();
    dynamic.kind = "dynamic".into();
    dynamic.bind_port = "1080".into();

    let forwards = validated_port_forwards(&[local, dynamic]).unwrap();
    assert_eq!(forwards.len(), 2);
    assert_eq!(forwards[0].bind_port, 8080);
    assert_eq!(forwards[0].host, "service.internal");
    assert_eq!(forwards[1].kind, "dynamic");
    assert_eq!(forwards[1].host_port, 0);
}

#[test]
fn partially_filled_rows_block_saving() {
    let mut draft = blank_forward_draft();
    draft.bind_port = "8080".into();
    assert!(validated_port_forwards(&[draft]).is_err());
}

#[test]
fn wide_bind_addresses_block_local_forwards() {
    let mut local = blank_forward_draft();
    local.bind_port = "8080".into();
    local.host = "service.internal".into();
    local.host_port = "80".into();
    local.bind_addr = "0.0.0.0".into();

    assert!(validated_port_forwards(&[local]).is_err());
}

#[test]
fn loopback_variants_are_accepted_for_local_forwards() {
    for addr in ["", "  ", "127.0.0.1", "localhost", "::1"] {
        let mut local = blank_forward_draft();
        local.bind_port = "8080".into();
        local.host = "service.internal".into();
        local.host_port = "80".into();
        local.bind_addr = addr.into();

        let forwards = validated_port_forwards(&[local]).unwrap();
        assert!(is_loopback_bind(&forwards[0].bind_addr), "{addr:?}");
    }
}

#[test]
fn wide_bind_addresses_stay_allowed_for_remote_forwards() {
    let mut remote = blank_forward_draft();
    remote.kind = "remote".into();
    remote.bind_port = "22".into();
    remote.host = "10.0.0.5".into();
    remote.host_port = "22".into();
    remote.bind_addr = "0.0.0.0".into();

    let forwards = validated_port_forwards(&[remote]).unwrap();
    assert_eq!(forwards[0].bind_addr, "0.0.0.0");
}

#[test]
fn is_loopback_bind_accepts_only_loopback_addresses() {
    for addr in ["", "  ", "127.0.0.1", "localhost", "::1"] {
        assert!(is_loopback_bind(addr), "{addr:?}");
    }
    for addr in ["0.0.0.0", "::", "[::1]", "192.168.1.10", "10.0.0.5", "127.0.0.2"] {
        assert!(!is_loopback_bind(addr), "{addr:?}");
    }
}
