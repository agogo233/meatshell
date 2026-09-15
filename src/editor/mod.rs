//! Built-in editor syntax highlighting (SFTP viewer/editor, #70 follow-up).
//!
//! `lang`    — conservative language detection (extension + shebang);
//! `highlight` — per-line tokeniser + incremental `VecModel<HlLine>` sync;
//! `eol`     — LF normalisation of the document text (Windows `\r\n`).

pub(crate) mod bracket;
pub(crate) mod eol;
pub(crate) mod highlight;
pub(crate) mod lang;
