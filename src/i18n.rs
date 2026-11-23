//! Translations of the user interface.
//!
//! Every text the user sees is looked up by a key ("settings.lang.title")
//! in the tables `lang/<code>.lang`, which are compiled into the exe. The
//! language is one global setting (`set_lang`), because the library also
//! produces messages (errors) the GUI shows and knows nothing of the GUI.
//!
//! File format, one entry per line:
//!
//! ```text
//! # comment
//! sync.err.differs = {path} differs in size from its source
//! ```
//!
//! `{name}` is a placeholder that `tr!("key", name = value)` fills in.
//! Escapes in the text: `\n` = line break, `\t` = tab, `\\` = backslash.
//! Any other backslash stays as it is (Windows paths).
//!
//! A key missing in a language falls back to English, a key missing
//! everywhere shows the key itself, so gaps are visible.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

/// A language of the user interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    De,
}

impl Lang {
    /// All languages, in the order the settings offer them.
    pub const ALL: [Lang; 2] = [Lang::En, Lang::De];

    /// The code stored in lp16-tool.toml ("language").
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::De => "de",
        }
    }

    /// The name of the language in the language itself.
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::De => "Deutsch",
        }
    }

    /// The language of a code; unknown or empty codes give English.
    pub fn from_code(code: &str) -> Lang {
        Lang::ALL
            .into_iter()
            .find(|l| l.code().eq_ignore_ascii_case(code.trim()))
            .unwrap_or(Lang::En)
    }

    fn source(self) -> &'static str {
        match self {
            Lang::En => include_str!("../lang/en.lang"),
            Lang::De => include_str!("../lang/de.lang"),
        }
    }

    fn table(self) -> &'static HashMap<&'static str, String> {
        static EN: OnceLock<HashMap<&'static str, String>> = OnceLock::new();
        static DE: OnceLock<HashMap<&'static str, String>> = OnceLock::new();
        let cell = match self {
            Lang::En => &EN,
            Lang::De => &DE,
        };
        cell.get_or_init(|| parse(self.source()).into_iter().collect())
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Switch the language of all texts looked up from now on.
pub fn set_lang(lang: Lang) {
    let i = Lang::ALL.iter().position(|l| *l == lang).unwrap_or(0);
    CURRENT.store(i as u8, Ordering::Relaxed);
}

/// The current language.
pub fn lang() -> Lang {
    Lang::ALL.get(CURRENT.load(Ordering::Relaxed) as usize).copied().unwrap_or(Lang::En)
}

/// The text of `key` in the current language.
pub fn t(key: &'static str) -> &'static str {
    t_in(lang(), key)
}

/// The text of `key` in `lang` (no global state, for tests).
pub fn t_in(lang: Lang, key: &'static str) -> &'static str {
    if let Some(s) = lang.table().get(key) {
        return s;
    }
    if let Some(s) = Lang::En.table().get(key) {
        return s;
    }
    key
}

/// Fill the `{name}` placeholders of `template`. Unknown placeholders and
/// lone braces stay as they are; values are not scanned again.
pub fn fill(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after.find('}');
        let name = close.map(|c| &after[..c]);
        match name.and_then(|n| args.iter().find(|(k, _)| *k == n)) {
            Some((_, v)) => {
                out.push_str(&v.to_string());
                rest = &after[close.unwrap() + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Look up a text: `tr!("key")` gives a `&'static str`,
/// `tr!("key", path = p.display(), n = 3)` a `String` with the
/// placeholders `{path}` and `{n}` filled in.
#[macro_export]
macro_rules! tr {
    ($key:literal) => {
        $crate::i18n::t($key)
    };
    ($key:literal, $($name:ident = $val:expr),+ $(,)?) => {
        $crate::i18n::fill(
            $crate::i18n::t($key),
            &[$((stringify!($name), &($val) as &dyn ::std::fmt::Display)),+],
        )
    };
}

/// The entries of a .lang file, in file order.
fn parse(source: &'static str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            out.push((key.trim(), unescape(value.trim())));
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('n') => {
                    out.push('\n');
                    chars.next();
                }
                Some('t') => {
                    out.push('\t');
                    chars.next();
                }
                Some('\\') => {
                    out.push('\\');
                    chars.next();
                }
                _ => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashSet};

    fn placeholders(s: &str) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        let mut rest = s;
        while let Some(open) = rest.find('{') {
            rest = &rest[open + 1..];
            if let Some(close) = rest.find('}') {
                let name = &rest[..close];
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    set.insert(name.to_string());
                }
            }
        }
        set
    }

    #[test]
    fn no_duplicate_keys() {
        for lang in Lang::ALL {
            let mut seen = HashSet::new();
            for (k, _) in parse(lang.source()) {
                assert!(seen.insert(k), "{}: key {} twice", lang.code(), k);
            }
        }
    }

    #[test]
    fn all_languages_have_the_same_keys_and_placeholders() {
        let en = Lang::En.table();
        for lang in Lang::ALL {
            let table = lang.table();
            for (k, v) in en {
                let other = table.get(k).unwrap_or_else(|| panic!("{}: missing key {}", lang.code(), k));
                assert_eq!(placeholders(v), placeholders(other), "{}: placeholders of {}", lang.code(), k);
            }
            for k in table.keys() {
                assert!(en.contains_key(k), "{}: key {} is not in en.lang", lang.code(), k);
            }
        }
    }

    /// Every key used in the sources (`tr!("…"`) is in en.lang.
    #[test]
    fn all_used_keys_exist() {
        let en = Lang::En.table();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut missing = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            // this file only has examples
            if path.extension().and_then(|e| e.to_str()) != Some("rs") || path.ends_with("i18n.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let mut rest = text.as_str();
            while let Some(i) = rest.find("tr!(\"") {
                // not the end of a longer macro name (include_str!)
                let own = rest[..i].chars().next_back().map_or(true, |c| !(c.is_alphanumeric() || c == '_'));
                rest = &rest[i + 5..];
                let key = &rest[..rest.find('"').unwrap()];
                if own && !en.contains_key(key) {
                    missing.push(format!("{}: {}", path.file_name().unwrap().to_string_lossy(), key));
                }
            }
        }
        assert!(missing.is_empty(), "keys missing in en.lang:\n{}", missing.join("\n"));
    }

    #[test]
    fn lookup_and_fallback() {
        assert_eq!(t_in(Lang::De, "lang.name"), "Deutsch");
        assert_eq!(t_in(Lang::En, "lang.name"), "English");
        assert_eq!(t_in(Lang::De, "no.such.key"), "no.such.key");
    }

    #[test]
    fn fill_placeholders() {
        let n = 3;
        assert_eq!(fill("{n} of {total}", &[("n", &n), ("total", &"5")]), "3 of 5");
        // unknown placeholders and lone braces stay, values are not scanned
        assert_eq!(fill("{x} {", &[("n", &1)]), "{x} {");
        assert_eq!(fill("{n}", &[("n", &"{n}")]), "{n}");
    }

    #[test]
    fn escapes() {
        assert_eq!(unescape(r"a\nb\\c \<Song>"), "a\nb\\c \\<Song>");
    }

    #[test]
    fn codes() {
        assert_eq!(Lang::from_code("DE"), Lang::De);
        assert_eq!(Lang::from_code(""), Lang::En);
        assert_eq!(Lang::from_code("xx"), Lang::En);
    }
}
