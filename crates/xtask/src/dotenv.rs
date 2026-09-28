//! The checkout's `.env`: what describes this developer's machines rather
//! than the project — the Windows peer's ssh address, its user's home, its
//! name in the mesh. Those used to be flag defaults, which put one person's
//! LAN into every clone; now the flags read `CURSIO_PEER_*` and this file is
//! where a checkout keeps them. It is ignored by git; `.env.example` lists the
//! keys.
//!
//! A variable already set in the environment wins, so a one-off
//! `CURSIO_PEER_SSH=... cargo xtask deploy` needs no edit. The format is the
//! plain subset every dotenv reader agrees on — `KEY=value`, `#` comments,
//! optional matching quotes — rather than a dependency for a dozen lines.

use std::path::Path;

/// Export every key in `path` not already present in the environment. A
/// missing file is the ordinary case (nothing to load); an unreadable one is
/// reported, since a silently skipped `.env` shows up later as a flag clap
/// says is missing.
pub fn load(path: &Path) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            eprintln!("xtask: ignoring {}: {e}", path.display());
            return;
        }
    };
    for (key, value) in parse(&text) {
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
        }
    }
}

fn parse(text: &str) -> Vec<(&str, &str)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            Some((key.trim(), unquote(value.trim())))
        })
        .filter(|(key, _)| !key.is_empty())
        .collect()
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_keys_skipping_comments_and_blank_lines() {
        let text = "# peer\n\nCURSIO_PEER_SSH=me@10.0.0.2\n  CURSIO_PEER_SSH_PORT = 2222 \n";
        assert_eq!(
            parse(text),
            vec![
                ("CURSIO_PEER_SSH", "me@10.0.0.2"),
                ("CURSIO_PEER_SSH_PORT", "2222")
            ]
        );
    }

    #[test]
    fn strips_matching_quotes_and_export_prefix() {
        let text = "export A=\"/mnt/c/Users/Some One\"\nB='x'\nC=\"unbalanced'\n";
        assert_eq!(
            parse(text),
            vec![
                ("A", "/mnt/c/Users/Some One"),
                ("B", "x"),
                ("C", "\"unbalanced'")
            ]
        );
    }

    #[test]
    fn keeps_equals_signs_inside_the_value() {
        assert_eq!(parse("K=a=b"), vec![("K", "a=b")]);
    }

    #[test]
    fn ignores_lines_without_a_key() {
        assert_eq!(parse("=value\nnot a pair\n"), Vec::<(&str, &str)>::new());
    }
}
