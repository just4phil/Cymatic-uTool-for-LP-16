//! When are two paths the same file? One rule for every module.
//!
//! Windows paths: upper / lower case does not matter (umlauts too), `/` is
//! `\`, a doubled `\` counts once and a `\` at the end does not count. A
//! leading `\\` (network path) stays.

/// The comparison key of a path: equal keys = same file.
pub fn path_key(path: &str) -> String {
    let s = path.replace('/', "\\");
    let (lead, rest) = match s.strip_prefix("\\\\") {
        Some(rest) => ("\\\\", rest),
        None => ("", s.as_str()),
    };
    let mut out = String::with_capacity(s.len());
    out.push_str(lead);
    let mut after_sep = false;
    for c in rest.chars() {
        if c == '\\' {
            if after_sep {
                continue;
            }
            after_sep = true;
        } else {
            after_sep = false;
        }
        out.push(c);
    }
    while out.len() > lead.len() && out.ends_with('\\') {
        out.pop();
    }
    out.to_lowercase()
}

/// Do `a` and `b` name the same file?
pub fn same_path(a: &str, b: &str) -> bool {
    path_key(a) == path_key(b)
}

/// `root` joined with `rest` (a path starting with or without `\`), with one
/// `\` between them also when `root` ends with one ("D:\" + "\LP16" =
/// "D:\LP16").
pub fn join_root(root: &str, rest: &str) -> String {
    let root = root.trim_end_matches(['\\', '/']);
    let rest = rest.trim_start_matches(['\\', '/']);
    if rest.is_empty() {
        format!("{}\\", root)
    } else {
        format!("{}\\{}", root, rest)
    }
}

/// `path` relative to `root` (without the leading `\`), if it lies below it
/// by [`path_key`]'s rule; None otherwise.
pub fn relative_to<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let root_key = path_key(root);
    if root_key.is_empty() {
        return None;
    }
    // the shortest start of `path` with the key of `root`, followed by a separator
    for (i, c) in path.char_indices() {
        if (c == '\\' || c == '/') && path_key(&path[..i]) == root_key {
            let rest = path[i..].trim_start_matches(['\\', '/']);
            return (!rest.is_empty()).then_some(rest);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_rule_for_every_spelling() {
        let a = r"C:\Lib\LP16\Multitrack Songs\Intro.songprj";
        for b in [
            r"c:\lib\lp16\multitrack songs\intro.SONGPRJ",
            r"C:/Lib/LP16/Multitrack Songs/Intro.songprj",
            r"C:\Lib\\LP16\Multitrack Songs\Intro.songprj",
            r"C:/Lib/\LP16\Multitrack Songs\Intro.songprj",
        ] {
            assert!(same_path(a, b), "{}", b);
        }
        assert!(!same_path(a, r"C:\Lib\LP16\Multitrack Songs\Outro.songprj"));
        // umlauts: not only ASCII
        assert!(same_path(r"C:\Ä\Über.songprj", r"c:\ä\über.songprj"));
        // a folder with and without "\" at the end
        assert!(same_path(r"E:\AUDIO\Song\v1", r"e:\audio\song\v1\"));
        // network paths keep their "\\"
        assert_eq!(path_key(r"\\NAS\\Share\x"), r"\\nas\share\x");
        assert!(!same_path(r"\\nas\share", r"\nas\share"));
    }

    #[test]
    fn join_and_relative() {
        assert_eq!(join_root(r"D:\", r"\LP16\a.songprj"), r"D:\LP16\a.songprj");
        assert_eq!(join_root(r"C:\Lib", r"\LP16\a.songprj"), r"C:\Lib\LP16\a.songprj");
        assert_eq!(join_root("C:/Lib/", r"LP16\a.songprj"), r"C:/Lib\LP16\a.songprj");
        assert_eq!(join_root("D:", ""), r"D:\");

        assert_eq!(relative_to(r"C:\Lib\LP16\a.songprj", r"C:\Lib"), Some(r"LP16\a.songprj"));
        assert_eq!(relative_to(r"C:\Lib\\LP16\a.songprj", r"c:\lib\"), Some(r"LP16\a.songprj"));
        assert_eq!(relative_to(r"D:\LP16\a.songprj", r"D:\"), Some(r"LP16\a.songprj"));
        assert_eq!(relative_to(r"C:\Library\a.songprj", r"C:\Lib"), None, "only whole folder names");
        assert_eq!(relative_to(r"C:\Lib", r"C:\Lib"), None);
        assert_eq!(relative_to(r"X:\a", ""), None);
    }
}
