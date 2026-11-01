//! Other apps on this phone reaching the node through `cursio-sdk`.
//!
//! The node listens on the abstract socket `@cursio-apps`
//! ([`cursio_app_protocol::ABSTRACT_NAME`]): Cursio's private directory is out
//! of every other app's reach, so a socket file there would be no use to
//! them. Any app can connect to an abstract socket, which is why each
//! connection passes [`cursio_core::apps::access`]: the kernel's uid for the
//! peer, the packages Android says run as it, and the person's answer.

use std::sync::Arc;

use cursio_core::apps::access::{package_gate, Package};
use jni::objects::{JString, JValue};

/// Start taking applications on this phone, as `machine`.
pub fn start(machine: String) {
    let config_path = match cursio_core::config::default_path() {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(%error, "no config path; other apps on this phone cannot reach the node");
            return;
        }
    };
    let gate = package_gate(Arc::new(packages_for_uid), config_path);
    cursio_core::apps::serve::start_abstract(machine, cursio_app_protocol::ABSTRACT_NAME, gate);
}

/// The packages running as `uid`, with their labels; empty when Android does
/// not say, which the gate refuses.
fn packages_for_uid(uid: u32) -> Vec<Package> {
    let answer = crate::jvm::with_native(|env, class| {
        let lines = env
            .call_static_method(
                class,
                "packagesForUid",
                "(I)Ljava/lang/String;",
                &[JValue::Int(uid as i32)],
            )?
            .l()?;
        let lines = JString::from(lines);
        let lines: String = env.get_string(&lines)?.into();
        Ok(lines)
    });
    match answer {
        Ok(lines) => parse(&lines),
        Err(why) => {
            tracing::warn!(uid, %why, "cannot ask Android which app is connecting");
            Vec::new()
        }
    }
}

/// `package\tlabel` per line, as `CursioNative.packagesForUid` writes them.
fn parse(lines: &str) -> Vec<Package> {
    lines
        .lines()
        .filter_map(|line| {
            let (name, label) = line.split_once('\t').unwrap_or((line, line));
            (!name.is_empty()).then(|| Package {
                name: name.to_owned(),
                label: label.to_owned(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn androids_answer_reads_as_packages_with_their_labels() {
        let found = parse("dev.leavepulse.launcher\tLeavePulse\ncom.example.shared\n");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "dev.leavepulse.launcher");
        assert_eq!(found[0].label, "LeavePulse");
        assert_eq!(found[1].label, "com.example.shared", "no label: its name");
        assert!(parse("").is_empty());
    }
}
