//! Matching a name against the pattern a directory query carries, the way NT
//! matches it: case-insensitively, with its DOS wildcards.

/// Match a name against a Windows enumeration pattern.
///
/// `*` stands for any run of characters, `?` for exactly one, and the match is
/// case-insensitive because the volume declares itself so. Backtracking is
/// iterative rather than recursive: a pattern of many `*` against a long name
/// would otherwise recurse once per position.
///
/// The pattern does not arrive as the user typed it. NT rewrites it into the
/// DOS wildcards its own matcher speaks, upper-cased — `*.o` reaches this
/// function as `<.O`. Those three characters are the reason a build on the
/// drive failed: `<` was compared literally, matched nothing, and cargo took
/// the empty listing for `ERROR_INVALID_NAME`.
///
/// - `<` (`DOS_STAR`) — like `*`, except it will not consume the final `.`
///   of the name; treating it as a plain `*` is close enough here, since the
///   difference only shows on names ending in a dot, which the peer does not
///   produce.
/// - `>` (`DOS_QM`) — like `?`, but also matches nothing at the end of a name.
/// - `"` (`DOS_DOT`) — matches a literal `.`, or nothing at the end of a name.
pub(super) fn matches_pattern(pattern: &str, name: &str) -> bool {
    let pattern: String = pattern
        .chars()
        .map(|c| match c {
            '<' => '*',
            '>' => '?',
            '"' => '.',
            other => other,
        })
        .collect();
    let pat: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
    let txt: Vec<char> = name.chars().flat_map(char::to_lowercase).collect();

    let (mut p, mut t) = (0usize, 0usize);
    // Where to resume if the current `*` turns out to have consumed too little.
    let (mut star, mut retry) = (None, 0usize);

    while t < txt.len() {
        match pat.get(p) {
            Some('*') => {
                star = Some(p);
                retry = t;
                p += 1;
            }
            Some('?') => {
                p += 1;
                t += 1;
            }
            Some(&c) if c == txt[t] => {
                p += 1;
                t += 1;
            }
            // A mismatch is fatal only if no `*` can absorb one more character.
            _ => match star {
                Some(s) => {
                    p = s + 1;
                    retry += 1;
                    t = retry;
                }
                None => return false,
            },
        }
    }
    // Trailing `*`s may still match the empty remainder.
    pat[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::matches_pattern;

    #[test]
    fn a_bare_star_matches_everything() {
        assert!(matches_pattern("*", "anything.o"));
        assert!(matches_pattern("*", ""));
    }

    #[test]
    fn an_extension_pattern_selects_only_that_extension() {
        assert!(matches_pattern("*.o", "build.o"));
        assert!(!matches_pattern("*.o", "build.rmeta"));
        // The one that mattered: cargo's temporary directories.
        assert!(matches_pattern("*.temp-archive", ".tmpd6Z2gF.temp-archive"));
    }

    #[test]
    fn matching_ignores_case_as_the_volume_promises() {
        assert!(matches_pattern("*.O", "build.o"));
        assert!(matches_pattern("LIB*", "libcore.rlib"));
    }

    #[test]
    fn a_question_mark_stands_for_exactly_one_character() {
        assert!(matches_pattern("a?c", "abc"));
        assert!(!matches_pattern("a?c", "ac"));
        assert!(!matches_pattern("a?c", "abbc"));
    }

    #[test]
    fn several_stars_backtrack_without_running_away() {
        assert!(matches_pattern("*b*d", "abcd"));
        assert!(!matches_pattern("*b*e", "abcd"));
        // Pathological for a naive recursive matcher; must simply return.
        assert!(!matches_pattern("*a*a*a*a*b", &"a".repeat(64)));
    }

    #[test]
    fn dos_wildcards_are_understood_as_nt_sends_them() {
        // Exactly what the log showed arriving for `*.o` and `*.temp-archive`.
        assert!(matches_pattern("<.O", "a.o"));
        assert!(matches_pattern("<.TEMP-ARCHIVE", ".tmpXX.temp-archive"));
        assert!(!matches_pattern("<.O", "a.rmeta"));
        // `>` for a single character, `"` for the dot.
        assert!(matches_pattern("a>c", "abc"));
        assert!(matches_pattern("a\"o", "a.o"));
    }

    #[test]
    fn a_literal_name_matches_only_itself() {
        assert!(matches_pattern("Cargo.toml", "cargo.toml"));
        assert!(!matches_pattern("Cargo.toml", "Cargo.lock"));
    }
}
