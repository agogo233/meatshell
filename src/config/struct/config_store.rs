use std::path::PathBuf;

use super::ConfigFile;

pub struct ConfigStore {
    pub(crate) path: PathBuf,
    pub(crate) backup_dir: Option<PathBuf>,
    pub(crate) cache: ConfigFile,
    /// ChaCha20-Poly1305 key loaded from (or freshly generated into)
    /// `secret.key` in the same directory as `sessions.json`.
    pub(crate) key: [u8; 32],
    /// Saved secrets that carried our encryption prefix but could not be
    /// opened with the current key (a regenerated `secret.key`). Cleared at
    /// load; counted so the UI can warn once instead of silently using the
    /// undecryptable blob as a literal password.
    pub(crate) lost_secrets: usize,
}
