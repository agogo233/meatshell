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
    /// Secrets that were still sealed with the old fixed export key and could
    /// be recovered at load. They are plain in the cache now, so a save is
    /// forced to reseal them under this machine's key; counted so the UI can
    /// tell the user the migration happened instead of it staying silent.
    pub(crate) recovered_secrets: usize,
}
