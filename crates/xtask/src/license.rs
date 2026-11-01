//! `cargo xtask license-check`: which copyleft code a build links.
//!
//! Cursio is GPL because of two pieces of other people's code
//! (`THIRD-PARTY.md`): playfair, vendored C compiled into `cursio-airplay`,
//! and WinFsp's Rust bindings, behind `cursio-files/mount`. A product
//! embedding Cursio without them must link neither, and a new dependency can
//! bring GPL back without anyone noticing — a crate's license is one line in a
//! registry nobody reads. So this resolves the dependency graph exactly as one
//! build would (package, features, target), follows only what is linked
//! (normal dependencies, not build scripts or dev-dependencies), and fails on
//! a strong-copyleft license that offers no permissive alternative.
//!
//! This workspace's own crates are left out of the verdict: their declared
//! license is a choice the project makes, not something third-party code
//! forces — except where one compiles copyleft source inside itself
//! ([`VENDORED`]), which no license field can show.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::Args;

#[derive(Args)]
pub struct LicenseArgs {
    /// The package whose build is checked.
    #[arg(long, default_value = "cursio-core")]
    package: String,
    /// Features to enable, comma-separated, as cargo takes them.
    #[arg(long)]
    features: Option<String>,
    /// Leave the package's default features off.
    #[arg(long)]
    no_default_features: bool,
    /// The target triple to resolve for; the host's when absent.
    #[arg(long)]
    target: Option<String>,
    /// Report copyleft without failing.
    #[arg(long)]
    allow_gpl: bool,
}

/// How a license expression binds a program that links the crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Copyleft {
    /// Some alternative of the expression is permissive.
    None,
    /// LGPL-like: binds the library, not the program linking it.
    Weak,
    /// GPL or AGPL with no alternative: the linking program must be too.
    Strong,
}

/// Classify an SPDX expression. An `OR` gives the linker a choice, so the
/// expression is as strict as its least strict alternative; an `AND` binds by
/// its strictest term. The old `MIT/Apache-2.0` spelling is an `OR`.
pub fn classify(expression: &str) -> Copyleft {
    let cleaned = expression.replace(['(', ')'], " ").replace('/', " OR ");
    cleaned
        .split(" OR ")
        .map(|alternative| {
            alternative
                .split(" AND ")
                .map(|term| term_copyleft(term.trim()))
                .max()
                .unwrap_or(Copyleft::None)
        })
        .min()
        .unwrap_or(Copyleft::None)
}

fn term_copyleft(term: &str) -> Copyleft {
    let id = term
        .split(" WITH ")
        .next()
        .unwrap_or(term)
        .trim()
        .to_ascii_uppercase();
    if id.starts_with("LGPL") || id.starts_with("MPL") || id.starts_with("EPL") {
        Copyleft::Weak
    } else if id.starts_with("GPL") || id.starts_with("AGPL") {
        Copyleft::Strong
    } else {
        Copyleft::None
    }
}

/// Workspace crates that compile third-party copyleft source in themselves,
/// where no dependency's license field can say so.
const VENDORED: &[(&str, &str)] = &[(
    "cursio-airplay",
    "playfair, the FairPlay setup, vendored C under GPL-3.0",
)];

/// One line of `cargo tree --prefix none -f '{p}|{l}'`.
#[derive(Debug, PartialEq)]
struct Entry {
    name: String,
    /// `name vX.Y.Z`, plus the path for a local crate.
    package: String,
    license: Option<String>,
    member: bool,
}

fn parse_entry(line: &str, checkout: &str) -> Option<Entry> {
    let line = line.trim().trim_end_matches(" (*)");
    let (package, license) = line.rsplit_once('|')?;
    let name = package.split_whitespace().next()?.to_owned();
    Some(Entry {
        name,
        package: package.to_owned(),
        license: (!license.trim().is_empty()).then(|| license.trim().to_owned()),
        member: package.contains(checkout),
    })
}

fn tree(args: &LicenseArgs, extra: &[&str]) -> Result<String> {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.args([
        "tree",
        "-p",
        &args.package,
        "-e",
        "normal",
        "--prefix",
        "none",
    ]);
    if let Some(target) = &args.target {
        cmd.args(["--target", target]);
    }
    if let Some(features) = &args.features {
        cmd.args(["--features", features]);
    }
    if args.no_default_features {
        cmd.arg("--no-default-features");
    }
    cmd.args(extra);
    cmd.current_dir(crate::checkout_dir());
    let output = cmd.output().context("running cargo tree")?;
    if !output.status.success() {
        bail!(
            "cargo tree failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn license_check(args: LicenseArgs) -> Result<()> {
    // `cargo tree -p` resolves features for this one package's build, which
    // `cargo metadata` does not: it unifies them across the workspace, so
    // `cursio-app` depending on `cursio-core` with its defaults would put
    // AirPlay into every check.
    let checkout = crate::checkout_dir().display().to_string();
    let mut linked: BTreeMap<String, Entry> = BTreeMap::new();
    for line in tree(&args, &["-f", "{p}|{l}"])?.lines() {
        if let Some(entry) = parse_entry(line, &checkout) {
            linked.entry(entry.package.clone()).or_insert(entry);
        }
    }
    let third_party = linked.values().filter(|e| !e.member).count();
    println!(
        "{} with features [{}]{}{} links {third_party} third-party crates",
        args.package,
        args.features.as_deref().unwrap_or("default"),
        if args.no_default_features {
            ", no defaults"
        } else {
            ""
        },
        args.target
            .as_deref()
            .map(|t| format!(", for {t}"))
            .unwrap_or_default(),
    );

    let mut strong: Vec<String> = Vec::new();
    for entry in linked.values() {
        if entry.member {
            if let Some((_, what)) = VENDORED.iter().find(|(name, _)| *name == entry.name) {
                strong.push(format!("{} carries {what}", entry.name));
            }
            continue;
        }
        let license = entry.license.as_deref().unwrap_or("no license field");
        match entry.license.as_deref().map(classify) {
            Some(Copyleft::Strong) => {
                let via = linked_by(&args, &entry.name, &checkout)?;
                strong.push(format!("{} ({license}), linked by {via}", entry.package));
            }
            Some(Copyleft::Weak) => println!(
                "  weak copyleft (binds that crate only): {} ({license})",
                entry.package
            ),
            Some(Copyleft::None) => {}
            None => println!("  no license field, check by hand: {}", entry.package),
        }
    }
    if strong.is_empty() {
        println!("✓ no GPL or AGPL code is linked");
        return Ok(());
    }
    for line in &strong {
        println!("  ✗ strong copyleft: {line}");
    }
    if args.allow_gpl {
        println!("{} GPL/AGPL source(s) linked; allowed", strong.len());
        Ok(())
    } else {
        bail!(
            "{} GPL/AGPL source(s) linked; leave out the feature that brings them, or pass \
             --allow-gpl for a build that is GPL anyway",
            strong.len()
        )
    }
}

/// The workspace crates through which `name` comes into the build.
fn linked_by(args: &LicenseArgs, name: &str, checkout: &str) -> Result<String> {
    let inverted = tree(args, &["-i", name, "-f", "{p}"])?;
    let via: BTreeSet<String> = inverted
        .lines()
        .map(|line| line.trim().trim_end_matches(" (*)"))
        .filter(|line| line.contains(checkout))
        .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
        .collect();
    Ok(match via.is_empty() {
        true => "another dependency".to_owned(),
        false => via.into_iter().collect::<Vec<_>>().join(", "),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_choice_of_a_permissive_license_is_not_copyleft() {
        assert_eq!(classify("MIT OR Apache-2.0"), Copyleft::None);
        assert_eq!(classify("MIT/Apache-2.0"), Copyleft::None);
        assert_eq!(classify("GPL-3.0-or-later OR MIT"), Copyleft::None);
        assert_eq!(
            classify("(MIT OR Apache-2.0) AND Unicode-3.0"),
            Copyleft::None
        );
    }

    #[test]
    fn gpl_with_no_alternative_binds_the_program() {
        assert_eq!(classify("GPL-3.0"), Copyleft::Strong);
        assert_eq!(classify("GPL-3.0-only"), Copyleft::Strong);
        assert_eq!(classify("AGPL-3.0-or-later"), Copyleft::Strong);
        assert_eq!(classify("MIT AND GPL-2.0"), Copyleft::Strong);
        assert_eq!(
            classify("GPL-3.0 WITH Classpath-exception-2.0"),
            Copyleft::Strong
        );
    }

    #[test]
    fn lgpl_and_mpl_bind_only_the_library() {
        assert_eq!(classify("LGPL-2.1-or-later"), Copyleft::Weak);
        assert_eq!(classify("MPL-2.0"), Copyleft::Weak);
        assert_eq!(classify("LGPL-3.0 OR GPL-3.0"), Copyleft::Weak);
        assert_eq!(classify("MPL-2.0 OR MIT"), Copyleft::None);
    }

    #[test]
    fn a_tree_line_names_the_crate_its_license_and_whether_it_is_ours() {
        let ours = parse_entry(
            "cursio-files v0.9.0 (/src/cursio/crates/files)|GPL-3.0-or-later (*)",
            "/src/cursio",
        )
        .unwrap();
        assert_eq!(ours.name, "cursio-files");
        assert!(ours.member);
        let theirs = parse_entry("winfsp-sys v0.12.1+winfsp-2.1|GPL-3.0", "/src/cursio").unwrap();
        assert_eq!(theirs.license.as_deref(), Some("GPL-3.0"));
        assert!(!theirs.member);
        assert_eq!(
            parse_entry("odd v1.0.0|", "/src/cursio").unwrap().license,
            None
        );
    }
}
