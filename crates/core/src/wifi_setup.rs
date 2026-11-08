//! Giving the Wi-Fi card a second station interface for direct links.
//!
//! A direct link joins a nearby machine's group while this machine stays on
//! its own network, which takes a second station interface on the same card
//! (see `cursio_wifidirect`). The card may allow one; nothing creates it, and
//! creating it is root's. So this is an explicit step a person takes once,
//! never something a running node does by itself: a udev rule that adds
//! `wlcursio0` whenever the card appears, and the interface added now so no
//! reboot is needed. NetworkManager then manages it like any Wi-Fi device,
//! and the node keeps it from joining anything but a direct link.
//!
//! The interface gets an address of its own, derived from the card's: two
//! stations sharing one address are refused by the kernel, and the card's
//! P2P device already holds the address with only the local bit set.

use std::path::Path;

use anyhow::Context;

const RULE: &str = "/etc/udev/rules.d/90-cursio-wifi.rules";
const MARKER: &str = "# Written by `cursio wifi install`; `cursio wifi uninstall` removes it.";
const INTERFACE: &str = "wlcursio0";

/// The address the second interface takes: locally administered, and apart
/// from the one the card's P2P device derives the same way.
fn second_address(card: &str) -> anyhow::Result<String> {
    let mut octets = card
        .trim()
        .split(':')
        .map(|o| u8::from_str_radix(o, 16))
        .collect::<Result<Vec<u8>, _>>()
        .with_context(|| format!("unreadable card address {card:?}"))?;
    anyhow::ensure!(octets.len() == 6, "unreadable card address {card:?}");
    octets[0] = (octets[0] | 0x02) ^ 0x0c;
    Ok(octets
        .iter()
        .map(|o| format!("{o:02x}"))
        .collect::<Vec<_>>()
        .join(":"))
}

/// Whether `iw phy … info` lists a combination with two or more stations.
fn allows_two_stations(info: &str) -> bool {
    info.lines().any(|line| {
        let Some(at) = line.find("#{ managed") else {
            return false;
        };
        let rest = &line[at..];
        rest.split_once("} <=")
            .and_then(|(_, n)| {
                n.trim()
                    .split(|c: char| !c.is_ascii_digit())
                    .next()?
                    .parse::<u32>()
                    .ok()
            })
            .is_some_and(|n| n >= 2)
    })
}

fn rule(card: &str, iw: &str, address: &str) -> String {
    format!(
        "{MARKER}\n\
         # A second Wi-Fi station on this card for Cursio's direct links.\n\
         ACTION==\"add\", SUBSYSTEM==\"ieee80211\", ATTR{{macaddress}}==\"{card}\", \
         RUN+=\"{iw} phy %k interface add {INTERFACE} type managed addr {address}\"\n"
    )
}

/// The card's phy, its address, and what `iw` says it allows.
fn card() -> anyhow::Result<(String, String)> {
    let dir = Path::new("/sys/class/ieee80211");
    let mut phys: Vec<_> = std::fs::read_dir(dir)
        .context("this machine has no Wi-Fi card")?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    phys.sort();
    let phy = phys
        .into_iter()
        .next()
        .context("this machine has no Wi-Fi card")?;
    let address = std::fs::read_to_string(dir.join(&phy).join("macaddress"))
        .with_context(|| format!("read {phy}'s address"))?;
    Ok((phy, address.trim().to_string()))
}

fn iw() -> anyhow::Result<String> {
    ["/usr/sbin/iw", "/sbin/iw", "/usr/bin/iw"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .map(str::to_string)
        .context("`iw` is not installed; install it (apt install iw) and run this again")
}

fn as_root(script: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new("pkexec")
        .args(["/bin/sh", "-c", script, "sh"])
        .args(args)
        .status()
        .context("run pkexec (polkit) to act as root")?;
    anyhow::ensure!(status.success(), "the change as root was refused or failed");
    Ok(())
}

pub fn install() -> anyhow::Result<()> {
    let (phy, card) = card()?;
    let iw = iw()?;
    let info = std::process::Command::new(&iw)
        .args(["phy", &phy, "info"])
        .output()
        .context("run iw")?;
    anyhow::ensure!(
        allows_two_stations(&String::from_utf8_lossy(&info.stdout)),
        "this Wi-Fi card ({phy}) cannot be on two networks at once, so it cannot keep its own network while on a direct link"
    );
    let address = second_address(&card)?;
    let exists = Path::new("/sys/class/net").join(INTERFACE).exists();
    println!("adding {INTERFACE} ({address}) to {phy}, now and whenever the card appears");
    as_root(
        r#"printf '%s' "$1" > "$2" && { [ "$5" = yes ] || "$3" phy "$4" interface add wlcursio0 type managed addr "$6"; }"#,
        &[
            &rule(&card, &iw, &address),
            RULE,
            &iw,
            &phy,
            if exists { "yes" } else { "no" },
            &address,
        ],
    )?;
    println!("done; machines near this one can now be reached over a direct Wi-Fi link");
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    let ours = std::fs::read_to_string(RULE).is_ok_and(|t| t.starts_with(MARKER));
    let exists = Path::new("/sys/class/net").join(INTERFACE).exists();
    if !ours && !exists {
        println!("nothing to remove");
        return Ok(());
    }
    let iw = iw()?;
    as_root(
        r#"{ [ "$1" = no ] || rm -f "$2"; } && { [ "$3" = no ] || "$4" dev wlcursio0 del; }"#,
        &[
            if ours { "yes" } else { "no" },
            RULE,
            if exists { "yes" } else { "no" },
            &iw,
        ],
    )?;
    println!("removed {INTERFACE} and its rule");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_second_address_is_local_and_apart_from_the_p2p_device() {
        assert_eq!(
            second_address("34:6f:24:da:0e:13").unwrap(),
            "3a:6f:24:da:0e:13"
        );
        // The P2P device takes the local bit alone: 36:...
        assert_ne!(
            second_address("34:6f:24:da:0e:13").unwrap(),
            "36:6f:24:da:0e:13"
        );
    }

    #[test]
    fn a_card_allowing_two_stations_is_recognised() {
        let mt7921 = "\tvalid interface combinations:\n\
             \t\t * #{ managed, P2P-client } <= 2, #{ P2P-GO } <= 1, #{ P2P-device } <= 1,\n";
        assert!(allows_two_stations(mt7921));
        let one = "\t\t * #{ managed } <= 1, #{ AP } <= 1,\n";
        assert!(!allows_two_stations(one));
    }

    #[test]
    fn the_rule_adds_the_interface_for_this_card_only() {
        let r = rule("34:6f:24:da:0e:13", "/usr/sbin/iw", "3a:6f:24:da:0e:13");
        assert!(r.starts_with(MARKER));
        assert!(r.contains("ATTR{macaddress}==\"34:6f:24:da:0e:13\""));
        assert!(r.contains("interface add wlcursio0 type managed addr 3a:6f:24:da:0e:13"));
    }
}
