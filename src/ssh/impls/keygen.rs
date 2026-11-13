//! In-app SSH keypair generator.
//!
//! Generates an Ed25519 or RSA keypair with the `ssh-key` crate (already a
//! dependency via russh-keys), serialises the private key as an unencrypted
//! OpenSSH PEM and the public key as a single `ssh-ed25519 AAAA…` line, and
//! writes both to disk under `~/.ssh` with 0600 permissions so the pair can be
//! dropped into any SSH client. Nothing here touches a live connection; the
//! upload half lives in `ssh::push_authorized_keys` because it reuses the
//! private connection/auth helpers.

use std::path::{Path, PathBuf};
use std::{fs, io};

use anyhow::{anyhow, Context, Result};
use rand::rngs::OsRng;
use ssh_key::{Algorithm, HashAlg, LineEnding, PrivateKey};
use uuid::Uuid;

use crate::i18n::t;

/// A freshly generated keypair and the strings the UI needs.
pub(crate) struct GeneratedKey {
    /// OpenSSH PEM private key (unencrypted), ready to paste into a session.
    pub(crate) private_key_pem: String,
    /// Single-line OpenSSH public key (`ssh-ed25519 AAAA… comment`).
    pub(crate) public_key_line: String,
    /// SHA-256 fingerprint shown in the UI for verification.
    pub(crate) fingerprint: String,
    /// Where the private key was written (empty until `write_key_files`).
    pub(crate) private_key_path: PathBuf,
    /// Where the public key was written (empty until `write_key_files`).
    pub(crate) public_key_path: PathBuf,
}

/// Which algorithm the user picked in the generate dialog.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyAlgorithm {
    Ed25519,
    Rsa,
}

impl KeyAlgorithm {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            KeyAlgorithm::Ed25519 => "ed25519",
            KeyAlgorithm::Rsa => "rsa",
        }
    }

    /// Map the dialog choice onto `ssh_key::Algorithm`. RSA uses the crate's
    /// default key size (3072): the `Algorithm::Rsa` variant only carries the
    /// signature hash, not a bit length, so the size is fixed here.
    fn to_ssh_algorithm(self) -> Algorithm {
        match self {
            KeyAlgorithm::Ed25519 => Algorithm::Ed25519,
            KeyAlgorithm::Rsa => Algorithm::Rsa {
                hash: Some(HashAlg::Sha256),
            },
        }
    }
}

/// Generate a keypair. Synchronous and potentially slow (RSA-3072 takes a
/// moment), so callers run this inside `spawn_blocking` and never while
/// holding the UI thread.
pub(crate) fn generate_keypair(algo: KeyAlgorithm) -> Result<GeneratedKey> {
    let mut rng = OsRng;
    let private = PrivateKey::random(&mut rng, algo.to_ssh_algorithm())
        .context("failed to generate SSH keypair")?;
    let public = private.public_key().clone();
    let private_key_pem = private
        .to_openssh(LineEnding::LF)
        .context("failed to serialise private key")?
        .to_string();
    let public_key_line = public
        .to_openssh()
        .context("failed to serialise public key")?;
    let fingerprint = public.fingerprint(HashAlg::Sha256).to_string();

    Ok(GeneratedKey {
        private_key_pem,
        public_key_line,
        fingerprint,
        private_key_path: PathBuf::new(),
        public_key_path: PathBuf::new(),
    })
}

/// Persist the keypair under `~/.ssh` as
/// `meatshell_<algo>_<short-uuid>.{key,pub}`. The private key is created with
/// 0600 permissions on POSIX platforms; the public key is world-readable.
pub(crate) fn write_key_files(mut key: GeneratedKey, algo: KeyAlgorithm) -> Result<GeneratedKey> {
    let dir = ssh_dir().context("could not locate ~/.ssh")?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let base = format!("meatshell_{}_{}", algo.as_str(), Uuid::new_v4().simple());
    let priv_path = dir.join(format!("{base}.key"));
    let pub_path = dir.join(format!("{base}.pub"));

    write_private(&priv_path, &key.private_key_pem)?;
    fs::write(&pub_path, &key.public_key_line)
        .with_context(|| format!("write {}", pub_path.display()))?;

    key.private_key_path = priv_path;
    key.public_key_path = pub_path;
    Ok(key)
}

/// Locate `~/.ssh`, creating nothing (the caller makes the dir on success).
fn ssh_dir() -> Result<PathBuf> {
    let home = directories::UserDirs::new()
        .map(|u| u.home_dir().to_path_buf())
        .ok_or_else(|| anyhow!("{}", t("找不到主目录", "no home directory found")))?;
    Ok(home.join(".ssh"))
}

/// Write the private key with 0600 permissions. Windows has no POSIX mode
/// bits, so it falls back to the file system's default ACLs there.
fn write_private(path: &Path, pem: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true).mode(0o600);
        let mut f = opts
            .open(path)
            .with_context(|| format!("create {}", path.display()))?;
        use io::Write;
        f.write_all(pem.as_bytes()).context("write private key")?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, pem).with_context(|| format!("create {}", path.display()))?;
        Ok(())
    }
}