//! Finding a path's real spelling on an owner that compares names with case.
//!
//! A Windows drive compares names without case, and Windows programs rely on
//! it: Unreal Engine games open their own executable lowercased. A Linux
//! owner does not, so the Windows mount of its disk looks a path up component
//! by component in each folder's listing when the owner does not know it as
//! given. Kept apart from the mount so it is tested where WinFsp is not.

/// `path` as the owner spells it.
///
/// `known` says whether the owner knows a path exactly as given; `names`
/// lists a directory (`""` is the volume's root). An exact name is preferred
/// over one matching only without case, so of `a` and `A` side by side each
/// keeps its own. What matches nothing is kept as given from there on: a file
/// being created gets the name it was given, under its parent's real
/// spelling.
pub fn true_case(
    path: &str,
    known: impl Fn(&str) -> bool,
    names: impl Fn(&str) -> Option<Vec<String>>,
) -> String {
    if path.is_empty() || known(path) {
        return path.to_owned();
    }
    let join = |dir: &str, name: &str| match dir.is_empty() {
        true => name.to_owned(),
        false => format!("{dir}/{name}"),
    };
    let mut real = String::new();
    let mut rest = path.split('/');
    while let Some(component) = rest.next() {
        let given = join(&real, component);
        if known(&given) {
            real = given;
            continue;
        }
        let folded = component.to_lowercase();
        let found =
            names(&real).and_then(|names| names.into_iter().find(|n| n.to_lowercase() == folded));
        match found {
            Some(name) => real = join(&real, &name),
            None => {
                let mut kept = given;
                for tail in rest {
                    kept.push('/');
                    kept.push_str(tail);
                }
                return kept;
            }
        }
    }
    real
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn tree() -> HashMap<&'static str, Vec<String>> {
        HashMap::from([
            ("", vec!["Games".to_owned()]),
            ("Games", vec!["Drive Beyond Horizons".to_owned()]),
            (
                "Games/Drive Beyond Horizons",
                vec![
                    "Game-Shipping.exe".to_owned(),
                    "a".to_owned(),
                    "A".to_owned(),
                ],
            ),
        ])
    }

    fn resolve(path: &str) -> String {
        let tree = tree();
        let known = |p: &str| {
            let (dir, name) = p.rsplit_once('/').unwrap_or(("", p));
            tree.get(dir)
                .is_some_and(|names| names.iter().any(|n| n == name))
        };
        true_case(path, known, |dir| tree.get(dir).cloned())
    }

    #[test]
    fn a_lowercased_path_finds_the_file_the_owner_spells_otherwise() {
        assert_eq!(
            resolve("games/drive beyond horizons/game-shipping.exe"),
            "Games/Drive Beyond Horizons/Game-Shipping.exe"
        );
    }

    #[test]
    fn an_exact_name_wins_over_one_matching_without_case() {
        assert_eq!(
            resolve("games/drive beyond horizons/A"),
            "Games/Drive Beyond Horizons/A"
        );
        assert_eq!(
            resolve("games/drive beyond horizons/a"),
            "Games/Drive Beyond Horizons/a"
        );
    }

    #[test]
    fn a_new_file_keeps_its_name_under_its_parents_real_spelling() {
        assert_eq!(
            resolve("games/drive beyond horizons/Saved/new.ini"),
            "Games/Drive Beyond Horizons/Saved/new.ini"
        );
    }

    #[test]
    fn a_path_the_owner_knows_is_not_listed_at_all() {
        let listed = std::cell::Cell::new(false);
        let out = true_case(
            "Games/x",
            |_| true,
            |_| {
                listed.set(true);
                None
            },
        );
        assert_eq!(out, "Games/x");
        assert!(!listed.get());
    }
}
