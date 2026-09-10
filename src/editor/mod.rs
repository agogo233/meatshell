//! Built-in editor syntax highlighting (SFTP viewer/editor, #70 follow-up).
//!
//! `lang`    — conservative language detection (extension + shebang);
//! `highlight` — per-line tokeniser + incremental `VecModel<HlLine>` sync.

pub(crate) mod highlight;
pub(crate) mod lang;
