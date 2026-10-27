//! FinalShell connection export compatibility.
//!
//! FinalShell stores the first eight decoded bytes as a per-password header. The
//! header seeds `java.util.Random`; an MD5 digest derived from that stream supplies
//! the DES key for the remaining bytes (`DES/ECB/PKCS5Padding`).

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use des::cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit};
use des::Des;
use serde::Deserialize;

use super::structs::{AuthMethod, PortForward, Secret, Session, SessionKind};

const JAVA_RANDOM_MULTIPLIER: u64 = 0x5DEECE66D;
const JAVA_RANDOM_ADDEND: u64 = 0xB;
const JAVA_RANDOM_MASK: u64 = (1_u64 << 48) - 1;

/// FinalShell `port_forwarding_list.type` values confirmed against exports.
const FINALSHELL_FORWARD_LOCAL: i64 = 100;
const FINALSHELL_FORWARD_REMOTE: i64 = 200;
const FINALSHELL_FORWARD_SOCKS5: i64 = 300;

/// The fields used by FinalShell's `*_connect_config.json` files. Unknown fields
/// are intentionally ignored so exports from different FinalShell releases remain
/// importable. `remote_port_forwarding` is unused (`{}` in current exports).
#[derive(Debug, Deserialize)]
struct FinalShellConnection {
    conection_type: i64,
    #[serde(default)]
    name: String,
    host: String,
    #[serde(default = "default_ssh_port")]
    port: u16,
    #[serde(default)]
    user_name: String,
    #[serde(default)]
    password: String,
    #[serde(default = "default_password_auth")]
    authentication_type: i64,
    #[serde(default)]
    terminal_encoding: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    port_forwarding_list: Vec<FinalShellPortForward>,
}

/// One SSH tunnel in FinalShell's `port_forwarding_list`.
#[derive(Debug, Deserialize)]
struct FinalShellPortForward {
    #[serde(default)]
    name: String,
    #[serde(default)]
    ip: String,
    #[serde(default)]
    port: u16,
    #[serde(default)]
    dst_address: String,
    #[serde(default)]
    dst_port: u16,
    #[serde(default)]
    id: Option<i64>,
    #[serde(rename = "type", default)]
    kind: i64,
}

fn default_ssh_port() -> u16 {
    22
}

fn default_password_auth() -> i64 {
    1
}

/// Parse either one FinalShell connection object or an array of connection
/// objects. FinalShell currently identifies SSH connection exports with type 100.
pub(super) fn parse_export(raw: &str) -> Result<Vec<Session>> {
    let value: serde_json::Value = serde_json::from_str(raw).context("not valid JSON")?;
    let connections: Vec<FinalShellConnection> = match value {
        object @ serde_json::Value::Object(_) => {
            vec![serde_json::from_value(object).context("not a FinalShell connection export")?]
        }
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(|value| {
                serde_json::from_value(value).context("invalid FinalShell connection entry")
            })
            .collect::<Result<_>>()?,
        _ => bail!("not a FinalShell connection export"),
    };

    if connections.is_empty() {
        bail!("FinalShell export contains no connections");
    }

    connections
        .into_iter()
        .map(FinalShellConnection::into_session)
        .collect()
}

impl FinalShellConnection {
    fn into_session(self) -> Result<Session> {
        if self.conection_type != 100 {
            bail!(
                "unsupported FinalShell connection type {} (only SSH type 100 is supported)",
                self.conection_type
            );
        }
        if self.authentication_type != 1 {
            bail!(
                "unsupported FinalShell authentication type {} (only password type 1 is supported)",
                self.authentication_type
            );
        }
        let host = self.host.trim().to_string();
        if host.is_empty() {
            bail!("FinalShell connection has an empty host");
        }
        if self.port == 0 {
            bail!("FinalShell connection {host} has an invalid port");
        }

        let password = if self.password.is_empty() {
            Secret::default()
        } else {
            Secret::new(
                decode_password(&self.password)
                    .with_context(|| format!("failed to decrypt FinalShell password for {host}"))?,
            )
        };
        let name = match self.name.trim() {
            "" => host.clone(),
            name => name.to_string(),
        };
        let encoding = match self.terminal_encoding.trim() {
            "" => "UTF-8".to_string(),
            encoding => encoding.to_string(),
        };

        Ok(Session {
            name,
            host,
            port: self.port,
            user: self.user_name.trim().to_string(),
            auth: AuthMethod::Password,
            password,
            kind: SessionKind::Ssh,
            encoding,
            note: self.description,
            forwards: map_port_forwards(self.port_forwarding_list),
            ..Session::new_empty()
        })
    }
}

fn map_port_forwards(entries: Vec<FinalShellPortForward>) -> Vec<PortForward> {
    entries
        .into_iter()
        .filter_map(FinalShellPortForward::into_port_forward)
        .collect()
}

impl FinalShellPortForward {
    fn into_port_forward(self) -> Option<PortForward> {
        let kind = match self.kind {
            FINALSHELL_FORWARD_LOCAL => "local",
            FINALSHELL_FORWARD_REMOTE => "remote",
            FINALSHELL_FORWARD_SOCKS5 => "dynamic",
            other => {
                tracing::warn!(
                    "skipping FinalShell port forward {:?}: unsupported type {other}",
                    self.name
                );
                return None;
            }
        };
        // SOCKS5/dynamic: MeatShell ignores host/host_port; FinalShell still
        // writes dst_address 127.0.0.1 and dst_port 0 in many exports.
        let (host, host_port) = if kind == "dynamic" {
            (String::new(), 0)
        } else {
            (self.dst_address, self.dst_port)
        };
        Some(PortForward {
            id: self.id.map(|id| id.to_string()).unwrap_or_default(),
            kind: kind.to_string(),
            auto_start: true,
            name: self.name,
            bind_addr: self.ip,
            bind_port: self.port,
            host,
            host_port,
        })
    }
}

fn decode_password(encoded: &str) -> Result<String> {
    let blob = STANDARD
        .decode(encoded)
        .context("password is not valid Base64")?;
    if blob.len() < 16 || (blob.len() - 8) % 8 != 0 {
        bail!("password payload has an invalid length");
    }
    let (head, ciphertext) = blob.split_at(8);
    let key = derive_des_key(head)?;
    let cipher = Des::new_from_slice(&key).expect("DES keys are always eight bytes");
    let mut plaintext = ciphertext.to_vec();
    for block in plaintext.chunks_exact_mut(8) {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    remove_pkcs5_padding(&mut plaintext)?;

    // FinalShell has existed across both UTF-8-default and legacy Chinese JREs.
    // Match Java's `new String(bytes)` behaviour for the common encodings without
    // ever using a lossy conversion for a credential.
    match String::from_utf8(plaintext) {
        Ok(value) => Ok(value),
        Err(err) => encoding_rs::GBK
            .decode_without_bom_handling_and_without_replacement(err.as_bytes())
            .map(|value| value.into_owned())
            .context("decrypted password is neither UTF-8 nor GBK"),
    }
}

fn remove_pkcs5_padding(bytes: &mut Vec<u8>) -> Result<()> {
    let padding = bytes.last().copied().context("empty DES plaintext")? as usize;
    if padding == 0
        || padding > 8
        || padding > bytes.len()
        || !bytes[bytes.len() - padding..]
            .iter()
            .all(|byte| *byte as usize == padding)
    {
        bail!("invalid DES padding");
    }
    bytes.truncate(bytes.len() - padding);
    Ok(())
}

fn derive_des_key(head: &[u8]) -> Result<[u8; 8]> {
    if head.len() != 8 {
        bail!("FinalShell password header must be eight bytes");
    }

    let divisor = JavaRandom::new(java_byte(head[5])).next_int(127);
    if divisor == 0 {
        bail!("FinalShell password header produced an invalid key divisor");
    }
    let seed = 3_680_984_568_597_093_857_i64 / divisor as i64;
    let mut random = JavaRandom::new(seed);
    for _ in 0..java_byte(head[0]).max(0) {
        random.next_long();
    }

    let mut secondary = JavaRandom::new(random.next_long());
    let values = [
        java_byte(head[4]),
        secondary.next_long(),
        java_byte(head[7]),
        java_byte(head[3]),
        secondary.next_long(),
        java_byte(head[1]),
        random.next_long(),
        java_byte(head[2]),
    ];
    let mut key_material = Vec::with_capacity(values.len() * 8);
    for value in values {
        key_material.extend_from_slice(&value.to_be_bytes());
    }
    let digest = md5::compute(&key_material);
    let mut key = [0_u8; 8];
    key.copy_from_slice(&digest.0[..8]);
    Ok(key)
}

fn java_byte(byte: u8) -> i64 {
    i8::from_ne_bytes([byte]) as i64
}

/// Bit-for-bit implementation of the parts of `java.util.Random` used by
/// FinalShell's password-key derivation.
struct JavaRandom {
    seed: u64,
}

impl JavaRandom {
    fn new(seed: i64) -> Self {
        Self {
            seed: ((seed as u64) ^ JAVA_RANDOM_MULTIPLIER) & JAVA_RANDOM_MASK,
        }
    }

    fn next(&mut self, bits: u32) -> i32 {
        self.seed = self
            .seed
            .wrapping_mul(JAVA_RANDOM_MULTIPLIER)
            .wrapping_add(JAVA_RANDOM_ADDEND)
            & JAVA_RANDOM_MASK;
        (self.seed >> (48 - bits)) as u32 as i32
    }

    fn next_int(&mut self, bound: i32) -> i32 {
        debug_assert!(bound > 0);
        if bound & (bound - 1) == 0 {
            return ((bound as i64 * self.next(31) as i64) >> 31) as i32;
        }
        loop {
            let bits = self.next(31);
            let value = bits % bound;
            if bits.wrapping_sub(value).wrapping_add(bound - 1) >= 0 {
                return value;
            }
        }
    }

    fn next_long(&mut self) -> i64 {
        let high = self.next(32) as i64;
        let low = self.next(32) as i64;
        high.wrapping_shl(32).wrapping_add(low)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrypts_java_generated_password_vector() {
        // Generated with the Java algorithm and a fixed eight-byte header.
        assert_eq!(
            decode_password("AwcLDRETFx1OXQgZJNatCplesw+x/P04").unwrap(),
            "meatshell-test"
        );
    }

    #[test]
    fn parses_single_finalshell_connection() {
        let raw = r#"{
            "conection_type": 100,
            "name": "development",
            "host": "192.0.2.10",
            "port": 2222,
            "user_name": "admin",
            "password": "AwcLDRETFx1OXQgZJNatCplesw+x/P04",
            "authentication_type": 1,
            "terminal_encoding": "UTF-8",
            "description": "imported from FinalShell"
        }"#;
        let sessions = parse_export(raw).unwrap();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.name, "development");
        assert_eq!(session.host, "192.0.2.10");
        assert_eq!(session.port, 2222);
        assert_eq!(session.user, "admin");
        assert_eq!(session.password.as_str(), "meatshell-test");
        assert_eq!(session.note, "imported from FinalShell");
        assert!(session.forwards.is_empty());
    }

    #[test]
    fn rejects_non_ssh_finalshell_connections() {
        let raw = r#"{"conection_type": 7, "host": "192.0.2.10"}"#;
        let error = parse_export(raw).unwrap_err().to_string();
        assert!(error.contains("unsupported FinalShell connection type"));
    }

    fn ssh_connection_with_forwards(forwards: &str) -> String {
        format!(
            r#"{{
                "conection_type": 100,
                "name": "imported",
                "host": "192.0.2.10",
                "port": 22,
                "user_name": "root",
                "authentication_type": 1,
                "remote_port_forwarding": {{}},
                "port_forwarding_list": {forwards}
            }}"#
        )
    }

    fn find_forward<'a>(session: &'a Session, name: &str) -> &'a PortForward {
        session
            .forwards
            .iter()
            .find(|forward| forward.name == name)
            .unwrap_or_else(|| panic!("missing imported forward {name}"))
    }

    #[test]
    fn imports_local_port_forwards() {
        let raw = ssh_connection_with_forwards(
            r#"[
                {"port": 3308, "ip": "127.0.0.1", "dst_address": "127.0.0.1", "name": "MySQL", "dst_port": 3306, "id": 121327396, "type": 100},
                {"port": 6679, "ip": "127.0.0.1", "dst_address": "127.0.0.1", "name": "redis", "dst_port": 6379, "id": 1432965430, "type": 100}
            ]"#,
        );
        let session = &parse_export(&raw).unwrap()[0];
        assert_eq!(session.forwards.len(), 2);

        let mysql = find_forward(session, "MySQL");
        assert_eq!(mysql.kind, "local");
        assert_eq!(mysql.bind_addr, "127.0.0.1");
        assert_eq!(mysql.bind_port, 3308);
        assert_eq!(mysql.host, "127.0.0.1");
        assert_eq!(mysql.host_port, 3306);
        assert_eq!(mysql.id, "121327396");
        assert!(mysql.auto_start);

        let redis = find_forward(session, "redis");
        assert_eq!(redis.kind, "local");
        assert_eq!(redis.bind_port, 6679);
        assert_eq!(redis.host, "127.0.0.1");
        assert_eq!(redis.host_port, 6379);
        assert_eq!(redis.id, "1432965430");
        assert!(redis.auto_start);
    }

    #[test]
    fn imports_remote_port_forward() {
        let raw = ssh_connection_with_forwards(
            r#"[{
                "port": 1234,
                "ip": "127.0.0.1",
                "dst_address": "127.0.0.1",
                "name": "远程隧道测试",
                "dst_port": 2345,
                "id": 1889610311,
                "type": 200
            }]"#,
        );
        let session = &parse_export(&raw).unwrap()[0];
        let forward = find_forward(session, "远程隧道测试");
        assert_eq!(forward.kind, "remote");
        assert_eq!(forward.bind_addr, "127.0.0.1");
        assert_eq!(forward.bind_port, 1234);
        assert_eq!(forward.host, "127.0.0.1");
        assert_eq!(forward.host_port, 2345);
        assert_eq!(forward.id, "1889610311");
        assert!(forward.auto_start);
    }

    #[test]
    fn imports_socks5_dynamic_forward_with_zero_dst_port() {
        let raw = ssh_connection_with_forwards(
            r#"[{
                "port": 5678,
                "ip": "127.0.0.1",
                "dst_address": "127.0.0.1",
                "name": "SOCKS5测试",
                "dst_port": 0,
                "id": 1612811717,
                "type": 300
            }]"#,
        );
        let session = &parse_export(&raw).unwrap()[0];
        let forward = find_forward(session, "SOCKS5测试");
        assert_eq!(forward.kind, "dynamic");
        assert_eq!(forward.bind_addr, "127.0.0.1");
        assert_eq!(forward.bind_port, 5678);
        assert_eq!(forward.host, "");
        assert_eq!(forward.host_port, 0);
        assert_eq!(forward.id, "1612811717");
        assert!(forward.auto_start);
    }

    #[test]
    fn imports_sample_socks5_and_remote_together() {
        let raw = r#"{
            "forwarding_auto_reconnect": false,
            "user_name": "root",
            "remote_port_forwarding": {},
            "conection_type": 100,
            "authentication_type": 1,
            "host": "10.99.100.7",
            "id": "226yrdf4ahr1ktie",
            "port_forwarding_list": [
                {
                    "port": 5678,
                    "ip": "127.0.0.1",
                    "dst_address": "127.0.0.1",
                    "name": "SOCKS5测试",
                    "dst_port": 0,
                    "id": 1612811717,
                    "type": 300
                },
                {
                    "port": 1234,
                    "ip": "127.0.0.1",
                    "dst_address": "127.0.0.1",
                    "name": "远程隧道测试",
                    "dst_port": 2345,
                    "id": 1889610311,
                    "type": 200
                }
            ],
            "port": 22,
            "name": "10.99.100.7(new)"
        }"#;
        let session = &parse_export(raw).unwrap()[0];
        assert_eq!(session.name, "10.99.100.7(new)");
        assert_eq!(session.forwards.len(), 2);
        assert_eq!(session.forwards[0].kind, "dynamic");
        assert_eq!(session.forwards[0].name, "SOCKS5测试");
        assert_eq!(session.forwards[1].kind, "remote");
        assert_eq!(session.forwards[1].name, "远程隧道测试");
    }

    #[test]
    fn skips_unknown_port_forward_type_without_failing_session() {
        let raw = ssh_connection_with_forwards(
            r#"[
                {"port": 3308, "ip": "127.0.0.1", "dst_address": "127.0.0.1", "name": "MySQL", "dst_port": 3306, "id": 121327396, "type": 100},
                {"port": 9999, "ip": "127.0.0.1", "dst_address": "10.0.0.1", "name": "unknown", "dst_port": 80, "id": 1, "type": 999}
            ]"#,
        );
        let session = &parse_export(&raw).unwrap()[0];
        assert_eq!(session.host, "192.0.2.10");
        assert_eq!(session.forwards.len(), 1);
        assert_eq!(session.forwards[0].name, "MySQL");
        assert_eq!(session.forwards[0].kind, "local");
    }
}
