//! Commands answered by the running node over its control socket, and the
//! few that act on this machine alone.

use crate::cli::BluetoothAction;
use crate::cli::CallCommand;
use crate::cli::PortalAction;
use crate::cli::SmsAction;
use crate::cli::WifiAction;

#[cfg(target_os = "linux")]
pub(crate) async fn portal(action: PortalAction) -> anyhow::Result<()> {
    match action {
        PortalAction::Install => cursio_core::portal_setup::install(),
        PortalAction::Uninstall => cursio_core::portal_setup::uninstall(),
        PortalAction::Relay => Ok(cursio_window::linux_portal::serve(None).await?),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn bluetooth(action: BluetoothAction) -> anyhow::Result<()> {
    match action {
        BluetoothAction::Install => cursio_core::bluetooth_setup::install(),
        BluetoothAction::Uninstall => cursio_core::bluetooth_setup::uninstall(),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn bluetooth(_action: BluetoothAction) -> anyhow::Result<()> {
    anyhow::bail!(
        "a phone's calls reach this computer's audio over bluetooth only on Linux so far; calls are still answered and placed from here over the mesh"
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn wifi(action: WifiAction) -> anyhow::Result<()> {
    match action {
        WifiAction::Install => cursio_core::wifi_setup::install(),
        WifiAction::Uninstall => cursio_core::wifi_setup::uninstall(),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn wifi(_action: WifiAction) -> anyhow::Result<()> {
    anyhow::bail!(
        "nothing to install here: Windows runs the group a nearby Linux machine joins, and needs no second interface for it"
    )
}

pub(crate) async fn call(command: CallCommand) -> anyhow::Result<()> {
    use cursio_core::calls_hub::CallAction;
    use cursio_core::control::{Command, Reply};
    let client = control_client()
        .await
        .map_err(|e| anyhow::anyhow!("no running node ({e}); start `cursio mesh` first"))?;
    let (phone, action) = match command {
        CallCommand::List => {
            let Reply::Status(status) = client.request(Command::Status).await? else {
                anyhow::bail!("the node did not answer with its status");
            };
            if status.calls.is_empty() {
                println!("no phone that reports calls is connected");
            }
            for phone in &status.calls {
                let route = if phone.hands_free {
                    "bluetooth"
                } else {
                    "mesh"
                };
                println!("{} ({route})", phone.name);
                if let Some(why) = &phone.unavailable {
                    println!("  calls off: {why}");
                }
                for c in &phone.calls {
                    println!("  #{} {:?} {:?} {}", c.id, c.direction, c.state, c.who());
                }
            }
            return Ok(());
        }
        CallCommand::Answer { phone } => (phone, CallAction::Answer),
        CallCommand::Decline { phone } => (phone, CallAction::Decline),
        CallCommand::Hangup { phone } => (phone, CallAction::HangUp),
        CallCommand::Dial { phone, number } => (phone, CallAction::Dial { number }),
        CallCommand::Mute { phone, off } => (phone, CallAction::Mute { muted: !off }),
        CallCommand::Audio { phone, phone_side } => {
            (phone, CallAction::AudioHere { here: !phone_side })
        }
    };
    match client.request(Command::Call { phone, action }).await? {
        Reply::Done => Ok(()),
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected reply from the node: {other:?}"),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) async fn portal(_action: PortalAction) -> anyhow::Result<()> {
    anyhow::bail!(
        "the ScreenCast portal exists only on Linux; on Windows `mesh --remote-screen` needs no setup"
    )
}

pub(crate) async fn control_client() -> anyhow::Result<cursio_core::control::transport::Client> {
    let endpoint = cursio_core::control::transport::endpoint()?;
    let (client, _pushes) = cursio_core::control::transport::connect(&endpoint).await?;
    Ok(client)
}

pub(crate) async fn print_status(json: bool) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Presence, Reply};
    let reply = control_client().await?.request(Command::Status).await?;
    let status = match reply {
        Reply::Status(status) => status,
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected reply {other:?}"),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }
    let at = |p: cursio_protocol::Position| format!("{},{}", p.x, p.y);
    let online = status
        .peers
        .iter()
        .filter(|p| p.presence == Presence::Online)
        .count();
    println!(
        "{} at {} — {online} of {} peers online",
        status.node.name,
        at(status.node.position),
        status.peers.len()
    );
    for peer in &status.peers {
        let presence = match (peer.presence, peer.link.as_ref().map(|l| l.path)) {
            (Presence::Online, Some(cursio_core::control::LinkPath::Bluetooth)) => "bluetooth",
            (Presence::Online, _) => "online",
            (Presence::Connecting, _) => "connecting",
            (Presence::Offline, _) => "offline",
        };
        match &peer.link {
            Some(link) => {
                // A phone's cell borders nothing; its kind is what to know.
                let place = if link.device_kind.takes_cursor() {
                    format!("at {}", at(link.position))
                } else {
                    format!("{:?}", link.device_kind).to_lowercase()
                };
                println!(
                    "  {:<16} {presence:<10} {place:<9} {}  modules: {}",
                    peer.name,
                    link.status,
                    link.modules.join(", ")
                )
            }
            None => println!("  {:<16} {presence}", peer.name),
        }
    }
    for r in &status.latency {
        println!(
            "input latency {}: p50 {:.1} ms, p99 {:.1} ms over {} events",
            r.path,
            r.p50_us as f64 / 1000.0,
            r.p99_us as f64 / 1000.0,
            r.events
        );
    }
    if status.problems.is_empty() {
        println!("no problems");
    }
    for problem in &status.problems {
        println!("{:?}: {}", problem.severity, problem.summary);
        if let Some(hint) = &problem.hint {
            println!("  {hint}");
        }
    }
    if !status.restart_needed.is_empty() {
        println!(
            "waiting for a restart: {}",
            status.restart_needed.join(", ")
        );
    }
    if !status.pinned.is_empty() {
        println!("pinned by flags: {}", status.pinned.join(", "));
    }
    Ok(())
}

/// Hand a share to the running node, which holds the links.
/// `cursio sms`: one request to the phone through the running node.
pub(crate) async fn sms(action: SmsAction) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Reply};
    let client = control_client().await.map_err(|e| {
        anyhow::anyhow!(
            "no running node to reach the phone through ({e}); start `cursio mesh` first"
        )
    })?;
    let command = match &action {
        SmsAction::List { phone, limit } => Command::ListConversations {
            peer: phone.clone(),
            limit: *limit,
            before_ms: None,
        },
        SmsAction::Read {
            phone,
            thread,
            limit,
        } => Command::ListMessages {
            peer: phone.clone(),
            thread_id: *thread,
            limit: *limit,
            before_ms: None,
        },
        SmsAction::Send {
            phone,
            number,
            text,
        } => Command::SendSms {
            peer: phone.clone(),
            addresses: vec![number.clone()],
            body: text.clone(),
        },
    };
    match client.request(command).await? {
        Reply::Conversations { conversations } => {
            for c in conversations {
                let who = if c.names.is_empty() {
                    c.addresses.join(", ")
                } else {
                    c.names.join(", ")
                };
                println!(
                    "{:>6}  {}{}  {}",
                    c.thread_id,
                    if c.unread { "* " } else { "" },
                    who,
                    c.snippet.replace('\n', " ")
                );
            }
            Ok(())
        }
        Reply::Messages { messages } => {
            for m in messages.iter().rev() {
                let arrow = match m.kind {
                    cursio_protocol::messages::MessageKind::Inbox => "<",
                    cursio_protocol::messages::MessageKind::Failed => "!",
                    _ => ">",
                };
                let attachment = if m.attachment {
                    " [attachment not shown]"
                } else {
                    ""
                };
                println!("{arrow} {}: {}{attachment}", m.address, m.body);
            }
            Ok(())
        }
        Reply::Done => {
            if let SmsAction::Send { number, .. } = action {
                println!("handed to the phone for {number}");
            }
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!(
            "the running node does not know a phone's messages ({other:?}); restart it with this version of Cursio"
        ),
    }
}

pub(crate) async fn send_share(
    target: &str,
    to: &str,
    copy: bool,
    save: bool,
) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Reply};
    use cursio_protocol::share::ShareAction;
    let action = match (copy, save) {
        (true, _) => ShareAction::Copy,
        (_, true) => ShareAction::Save,
        _ => ShareAction::Open,
    };
    // The node's working directory is not this shell's, so a relative path
    // would name some other file there, or none.
    let target = match std::fs::canonicalize(target) {
        Ok(path) if path.is_file() => path.to_string_lossy().into_owned(),
        _ => target.to_string(),
    };
    let client = control_client().await.map_err(|e| {
        anyhow::anyhow!("no running node to send through ({e}); start `cursio mesh` first")
    })?;
    match client
        .request(Command::Share {
            peer: to.to_string(),
            target,
            action,
        })
        .await?
    {
        Reply::Done => {
            println!("sent to {to}");
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!(
            "the running node does not know how to share ({other:?}); restart it with this version of Cursio"
        ),
    }
}

/// Ask the running node, which holds the links, to ring a phone.
pub(crate) async fn ring_phone(phone: &str) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Reply};
    let client = control_client().await.map_err(|e| {
        anyhow::anyhow!("no running node to ring through ({e}); start `cursio mesh` first")
    })?;
    match client
        .request(Command::Ring {
            peer: phone.to_string(),
        })
        .await?
    {
        Reply::Done => {
            println!("ringing {phone}");
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!(
            "the running node does not know how to ring ({other:?}); restart it with this version of Cursio"
        ),
    }
}

pub(crate) async fn phone_screen(phone: &str, show: bool) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Reply};
    let client = control_client().await.map_err(|e| {
        anyhow::anyhow!(
            "no running node to show the phone through ({e}); start `cursio mesh` first"
        )
    })?;
    match client
        .request(Command::PhoneScreen {
            peer: phone.to_string(),
            show,
        })
        .await?
    {
        Reply::Done if show => {
            println!("asked {phone}; its screen opens here once it is allowed on the phone");
            Ok(())
        }
        Reply::Done => {
            println!("{phone}'s screen is no longer shown here");
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!(
            "the running node does not know how to show a phone's screen ({other:?}); restart it with this version of Cursio"
        ),
    }
}

/// `cursio phone`: read or drive a phone's shared screen through the node.
pub(crate) async fn phone(phone: &str, action: crate::cli::PhoneCommand) -> anyhow::Result<()> {
    use crate::cli::{PhoneCommand, PhoneKeyArg};
    use cursio_core::control::{Command, Reply};
    use cursio_core::phone_action::{PhoneAction, PhoneKey};
    let client = control_client().await.map_err(|e| {
        anyhow::anyhow!(
            "no running node to reach the phone through ({e}); start `cursio mesh` first"
        )
    })?;
    let (command, out) = match action {
        PhoneCommand::Screenshot { out, max_side, ask } => (
            Command::PhoneScreenshot {
                phone: phone.to_string(),
                max_side,
                ask,
            },
            out,
        ),
        PhoneCommand::Tap { x, y, hold } => (
            Command::PhoneAct {
                phone: phone.to_string(),
                action: PhoneAction::Tap {
                    x,
                    y,
                    hold_ms: hold,
                },
            },
            None,
        ),
        PhoneCommand::Swipe { x1, y1, x2, y2, ms } => (
            Command::PhoneAct {
                phone: phone.to_string(),
                action: PhoneAction::Swipe { x1, y1, x2, y2, ms },
            },
            None,
        ),
        PhoneCommand::Key { key } => (
            Command::PhoneAct {
                phone: phone.to_string(),
                action: PhoneAction::Key {
                    key: match key {
                        PhoneKeyArg::Back => PhoneKey::Back,
                        PhoneKeyArg::Home => PhoneKey::Home,
                        PhoneKeyArg::Recents => PhoneKey::Recents,
                    },
                },
            },
            None,
        ),
        PhoneCommand::Text { text } => (
            Command::PhoneAct {
                phone: phone.to_string(),
                action: PhoneAction::Text { text },
            },
            None,
        ),
    };
    match client.request(command).await? {
        Reply::Screenshot {
            path,
            width,
            height,
        } => {
            match out {
                Some(out) => {
                    std::fs::copy(&path, &out)
                        .map_err(|e| anyhow::anyhow!("cannot copy to {}: {e}", out.display()))?;
                    println!("{} ({width}x{height})", out.display());
                }
                None => println!("{} ({width}x{height})", path.display()),
            }
            Ok(())
        }
        Reply::Acted { what } => {
            println!("{what}");
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!(
            "the running node does not know how to reach a phone's screen ({other:?}); restart it with this version of Cursio"
        ),
    }
}

/// Remove a machine: through the running node when there is one, so the link
/// closes at once, otherwise only in the files.
pub(crate) async fn forget_machine(
    machine: &str,
    config_path: &std::path::Path,
) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Reply};
    let (peer, _) = cursio_core::forget::resolve(machine)?;
    let (name, was_builder) = match control_client().await {
        Ok(client) => match client.request(Command::Forget { peer }).await? {
            Reply::Forgotten { name, was_builder } => (name, was_builder),
            Reply::Error { message } => anyhow::bail!("{message}"),
            other => anyhow::bail!(
                "the running node does not know how to remove a machine ({other:?}); \
                 restart it with this version of Cursio"
            ),
        },
        // No node to tell; the durable half is all there is.
        Err(_) => {
            let forgotten = cursio_core::forget::forget_on_disk(peer, config_path)?;
            (forgotten.name, forgotten.was_builder)
        }
    };
    println!("{name} is no longer part of this mesh");
    if was_builder {
        println!("its builds are no longer accepted here");
    }
    println!("to bring it back, pair the two machines again");
    Ok(())
}

/// `cursio window`: read or drive an application's window on a computer.
pub(crate) async fn devtools(
    machine: &str,
    app: &str,
    port: u16,
    close: bool,
    launch: bool,
) -> anyhow::Result<()> {
    use cursio_core::control::{Command, Reply};
    let (app, process) = match app.split_once('/') {
        Some((app, process)) => (app.to_owned(), Some(process.to_owned())),
        None => (app.to_owned(), None),
    };
    let command = Command::Devtools {
        machine: machine.to_owned(),
        app: app.clone(),
        process,
        port,
        close,
        launch,
    };
    if launch {
        println!("asking {machine} for {app}'s DevTools, starting it with them if it must…");
    }
    match control_client().await?.request(command).await? {
        Reply::Done => {
            println!("stopped listening for {app}'s DevTools on {machine}");
            Ok(())
        }
        Reply::Devtools {
            port,
            token,
            launched,
        } => {
            if let Some(launched) = launched {
                println!("{machine}: {launched}");
            }
            let http = format!("http://127.0.0.1:{port}/{token}");
            println!("{app} on {machine}: DevTools at {http}");
            println!("  the token in the path is the only way in; keep the address to yourself");
            println!();
            println!("  Playwright:  const browser = await chromium.connectOverCDP('{http}');");
            println!("  targets:     {http}/json/list");
            println!("  front end:   open a target's devtoolsFrontendUrl, prefixed with http://127.0.0.1:{port}");
            println!();
            println!("it stays open 15 minutes after its last connection; `--close` ends it now");
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("the running node answered {other:?}"),
    }
}

pub(crate) async fn window(
    machine: &str,
    app: &str,
    title: Option<String>,
    action: crate::cli::WindowCommand,
) -> anyhow::Result<()> {
    use crate::cli::WindowCommand;
    use cursio_core::control::{Command, Reply};
    use cursio_protocol::diag::{
        window_fraction, DesktopInput, ElementAct, ElementPick, WindowTarget,
    };
    let (app, process) = match app.split_once('/') {
        Some((app, process)) => (app.to_owned(), Some(process.to_owned())),
        None => (app.to_owned(), None),
    };
    let target = WindowTarget {
        app,
        process,
        title,
    };
    let act = |input| Command::WindowAct {
        machine: machine.to_owned(),
        target: target.clone(),
        input,
    };
    let element_command = |element: crate::cli::ElementArgs, act| Command::WindowElement {
        machine: machine.to_owned(),
        target: target.clone(),
        pick: match element.id {
            Some(id) => ElementPick::Id(id),
            None => ElementPick::Match {
                role: element.role.unwrap_or_default(),
                name: element.name.unwrap_or_default(),
                nth: element.nth,
            },
        },
        act,
    };
    let (command, out) = match action {
        WindowCommand::Screenshot { out, max_side } => (
            Command::WindowShot {
                machine: machine.to_owned(),
                target: target.clone(),
                max_side,
            },
            out,
        ),
        WindowCommand::Click { x, y, right } => (
            act(DesktopInput::Click {
                x: window_fraction(x).map_err(|e| anyhow::anyhow!("{e}"))?,
                y: window_fraction(y).map_err(|e| anyhow::anyhow!("{e}"))?,
                right,
            }),
            None,
        ),
        WindowCommand::Type { text } => (act(DesktopInput::Type { text }), None),
        WindowCommand::Key { key } => (act(DesktopInput::Key { key }), None),
        WindowCommand::Tree { depth, max } => (
            Command::WindowTree {
                machine: machine.to_owned(),
                target: target.clone(),
                depth,
                max_nodes: max,
            },
            None,
        ),
        WindowCommand::Invoke { element, action } => {
            use crate::cli::ElementActionArg as A;
            let act = match action {
                A::Invoke => ElementAct::Invoke,
                A::Toggle => ElementAct::Toggle,
                A::Select => ElementAct::Select,
                A::Expand => ElementAct::Expand,
                A::Collapse => ElementAct::Collapse,
                A::Focus => ElementAct::Focus,
            };
            (element_command(element, act), None)
        }
        WindowCommand::SetValue { element, value } => {
            (element_command(element, ElementAct::SetValue(value)), None)
        }
    };
    let client = control_client().await.map_err(|e| {
        anyhow::anyhow!(
            "no running node to reach {machine} through ({e}); start `cursio mesh` first"
        )
    })?;
    match client.request(command).await? {
        Reply::Screenshot {
            path,
            width,
            height,
        } => {
            let shown = match out {
                Some(out) => {
                    std::fs::copy(&path, &out)
                        .map_err(|e| anyhow::anyhow!("cannot copy to {}: {e}", out.display()))?;
                    out
                }
                None => path,
            };
            println!("{} ({width}x{height})", shown.display());
            Ok(())
        }
        Reply::Acted { what } => {
            println!("{machine}: {what}");
            Ok(())
        }
        Reply::WindowTree {
            title,
            text,
            nodes,
            truncated,
        } => {
            println!("{machine}: {title:?}, {nodes} elements");
            print!("{text}");
            if truncated {
                println!("(cut at the limits; --depth and --max reach further)");
            }
            Ok(())
        }
        Reply::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!(
            "the running node does not know how to reach an application's window ({other:?}); restart it with this version of Cursio"
        ),
    }
}
