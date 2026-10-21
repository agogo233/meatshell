//! Language detection for the built-in editor's syntax highlighting.
//!
//! Detection is deliberately conservative: only well-known extensions and
//! shebang lines map to a grammar; anything else degrades to `Plain`, which
//! disables the highlight overlay entirely and keeps the plain-text editor.

/// Languages the lightweight per-line highlighter understands. `Plain` means
/// "no grammar" — the editor falls back to its original uncoloured rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Plain,
    Shell,
    Json,
    Yaml,
    Toml,
    Ini,
    Python,
    Dockerfile,
    Makefile,
    Nginx,
}

/// Files that are shells without a usable extension (`~/.bashrc` and friends).
const SHELL_PLAIN_NAMES: &[&str] =
    &[".bashrc", ".bash_profile", ".profile", ".zshrc", ".zprofile", ".bash_aliases"];

/// Extension (including the dot, lowercased) → language. `conf`/`cfg` map to
/// the Ini grammar, which covers `key = value` + `[section]` + `#`/`;`
/// comments — close enough for the server config files this editor targets.
fn ext_lang(ext: &str) -> Lang {
    match ext {
        "sh" | "bash" | "zsh" => Lang::Shell,
        "json" => Lang::Json,
        "yml" | "yaml" => Lang::Yaml,
        "toml" => Lang::Toml,
        "ini" | "cfg" | "conf" | "properties" | "env" => Lang::Ini,
        "py" | "pyw" => Lang::Python,
        "mk" | "mak" => Lang::Makefile,
        _ => Lang::Plain,
    }
}

/// Shebang → language. Only interpreters with a dedicated grammar respond.
fn shebang_lang(first_line: &str) -> Lang {
    let Some(rest) = first_line.strip_prefix("#!") else {
        return Lang::Plain;
    };
    // Trim the trailing newline: `ends_with("/sh")` below (and the word
    // splits) otherwise never match the real "first line of file" form.
    let rest = rest.trim_end();
    // "…/env python3", "…/bin/bash", "…/bin/sh …"
    let has_word = |needle: &str| rest.split(['/', ' ']).any(|w| w.starts_with(needle));
    if has_word("python") {
        Lang::Python
    } else if has_word("bash") || has_word("zsh") || rest.contains("/sh ") || rest.ends_with("/sh") {
        Lang::Shell
    } else {
        Lang::Plain
    }
}

/// Detect the grammar for `name` (file name as shown in the editor title) with
/// `first_line` as the file's first line for the shebang / nginx heuristics.
pub fn detect(name: &str, first_line: &str) -> Lang {
    let lower = name.to_ascii_lowercase();
    if SHELL_PLAIN_NAMES.contains(&lower.as_str()) {
        return Lang::Shell;
    }
    // Extensionless-by-convention names must be checked BEFORE the extension
    // split: `Dockerfile.prod` has a "prod" extension, not a Dockerfile one.
    if lower.starts_with("dockerfile") || lower.ends_with(".dockerfile") {
        return Lang::Dockerfile;
    }
    // `makefile`, `GNUmakefile`, `foo.makefile` (suffix form) and
    // `Makefile.old` — but NOT `makefile_helper.sh` (the extension rule wins
    // over a mere prefix), hence the precise predicates.
    if lower.ends_with("makefile") || lower.starts_with("makefile.") {
        return Lang::Makefile;
    }
    // Only the canonical nginx main config; other `.conf` files stay Ini.
    if lower == "nginx.conf" {
        return Lang::Nginx;
    }
    if let Some(ext) = lower.rsplit_once('.').map(|(_, e)| e) {
        let lang = ext_lang(ext);
        if lang != Lang::Plain {
            return lang;
        }
    }
    let shebang = shebang_lang(first_line);
    if shebang != Lang::Plain {
        return shebang;
    }
    // Heuristic for unnamed/oddly-named server blocks: only when the FIRST
    // line is unmistakably nginx configuration (INI/other grammars never open
    // with these tokens, so a false positive is practically impossible).
    let fl = first_line.trim_start();
    if ["worker_processes", "events {", "http {", "server {", "upstream "]
        .iter()
        .any(|p| fl.starts_with(p))
    {
        return Lang::Nginx;
    }
    Lang::Plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_extension() {
        assert_eq!(detect("deploy.sh", ""), Lang::Shell);
        assert_eq!(detect("data.JSON", ""), Lang::Json); // case-insensitive
        assert_eq!(detect("a.json", ""), Lang::Json);
        assert_eq!(detect("b.YML", ""), Lang::Yaml);
        assert_eq!(detect("c.toml", ""), Lang::Toml);
        assert_eq!(detect("other.conf", ""), Lang::Ini);
        assert_eq!(detect("nginx.conf", ""), Lang::Nginx);
        assert_eq!(detect("app.py", ""), Lang::Python);
        assert_eq!(detect("README.md", ""), Lang::Plain);
        assert_eq!(detect("noext", ""), Lang::Plain);
    }

    #[test]
    fn detects_dotfile_shells() {
        assert_eq!(detect(".bashrc", ""), Lang::Shell);
        assert_eq!(detect(".zshrc", ""), Lang::Shell);
        assert_eq!(detect(".profile", ""), Lang::Shell);
    }

    #[test]
    fn detects_new_languages() {
        assert_eq!(detect("Dockerfile", ""), Lang::Dockerfile);
        assert_eq!(detect("dockerfile", ""), Lang::Dockerfile);
        assert_eq!(detect("Dockerfile.prod", ""), Lang::Dockerfile); // name wins over ext
        assert_eq!(detect("Makefile", ""), Lang::Makefile);
        assert_eq!(detect("GNUmakefile", ""), Lang::Makefile);
        assert_eq!(detect("Makefile.old", ""), Lang::Makefile);
        assert_eq!(detect("build.mk", ""), Lang::Makefile);
        assert_eq!(detect("nginx.conf", ""), Lang::Nginx);
        assert_eq!(detect("weird-name", "events {\n"), Lang::Nginx);
        assert_eq!(detect("weird-name", "worker_processes auto;\n"), Lang::Nginx);
        assert_eq!(detect("weird-name", "[main]\n"), Lang::Plain);
        assert_eq!(detect("makefile_helper.sh", ""), Lang::Shell); // ext wins
    }

    #[test]
    fn detects_shebangs() {
        assert_eq!(detect("run", "#!/usr/bin/env python3\n"), Lang::Python);
        assert_eq!(detect("run", "#!/bin/bash\nset -e\n"), Lang::Shell);
        assert_eq!(detect("run", "#!/bin/sh\n"), Lang::Shell);
        assert_eq!(detect("run", "#!/usr/bin/env node\n"), Lang::Plain);
        assert_eq!(detect("run", "# not a shebang\n"), Lang::Plain);
    }
}
