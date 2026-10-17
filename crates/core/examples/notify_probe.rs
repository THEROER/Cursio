//! Show one notification the way a phone's would arrive, from inside a Tokio
//! runtime as a module's host thread calls it, and print what the user does
//! with it — so the desktop half can be checked without a phone.
//!
//! ```sh
//! cargo run -p cursio-core --example notify_probe
//! ```

#[cfg(target_os = "linux")]
fn main() {
    use cursio_core::notify_hub::NotifyDisplay;
    use cursio_protocol::notify::{Notification, NotifyAction, NotifyKind};
    use cursio_protocol::PeerId;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let display = cursio_core::notify_display::FdoDisplay::start().expect("a session bus");
        println!("user here: {}", cursio_core::presence::user_here());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        display.show(
            PeerId::from_u128(1),
            "probe",
            &Notification {
                key: "probe".into(),
                app_id: "dev.cursio.probe".into(),
                app_name: "Cursio".into(),
                title: "Cursio · проверка".into(),
                text: "Нажми кнопку или закрой уведомление".into(),
                icon: None,
                when_ms: 0,
                kind: NotifyKind::Message,
                actions: vec![
                    NotifyAction {
                        title: "Прочитано".into(),
                        reply: false,
                    },
                    NotifyAction {
                        title: "Ответить".into(),
                        reply: true,
                    },
                ],
            },
            tx,
        );
        println!("shown; waiting up to 60 s for what you do with it");
        match tokio::time::timeout(std::time::Duration::from_secs(60), rx.recv()).await {
            Ok(Some(event)) => println!("{event:?}"),
            _ => println!("nothing within 60 s"),
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("this probe drives the Linux notification server");
}
