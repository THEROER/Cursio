//! The dialog a capture app's "share your screen" opens when Cursio is the
//! ScreenCast portal: this computer, or one of a peer's windows and screens.
//!
//! GNOME's own picker cannot be asked to list anything but this machine, so
//! Cursio's portal backend shows this first and hands "this computer" on to
//! GNOME's dialog unchanged. It is only ever shown when a peer actually has
//! something to offer; with no peer, the backend goes straight to GNOME's.
//!
//! It runs in a **child process** (this same executable, started with
//! [`CHOOSER_ARG`]) rather than on a thread. GTK can be initialised on one
//! thread per process, ever; the daemon may already have spent that on a
//! transfer surrogate, and a chooser that cannot open would leave every screen
//! share on the machine hanging. A process of its own always can, and a crash
//! in it is a cancelled share rather than a dead daemon.

use serde::{Deserialize, Serialize};

use crate::WindowError;

/// The argument that makes this executable run the chooser and nothing else.
pub const CHOOSER_ARG: &str = "__cursio-screen-chooser";

/// One line of the dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    /// Heading the line is grouped under ("This computer", a peer's name).
    pub section: String,
    pub label: String,
}

#[derive(Serialize, Deserialize)]
struct Request {
    app: String,
    choices: Vec<Choice>,
}

/// Ask the user; `Ok(None)` when they cancelled.
pub async fn choose(app: &str, choices: Vec<Choice>) -> Result<Option<usize>, WindowError> {
    use tokio::io::AsyncWriteExt;

    let exe = std::env::current_exe()
        .map_err(|e| WindowError::Backend(format!("cannot find this executable: {e}")))?;
    let mut child = tokio::process::Command::new(exe)
        .arg(CHOOSER_ARG)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| WindowError::Backend(format!("start the screen chooser: {e}")))?;
    let request = serde_json::to_vec(&Request {
        app: app.to_string(),
        choices,
    })
    .map_err(|e| WindowError::Backend(format!("encode the chooser request: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&request)
            .await
            .map_err(|e| WindowError::Backend(format!("talk to the screen chooser: {e}")))?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| WindowError::Backend(format!("screen chooser failed: {e}")))?;
    Ok(parse_answer(&String::from_utf8_lossy(&output.stdout)))
}

/// The chooser prints the chosen index, or nothing for a cancel.
fn parse_answer(stdout: &str) -> Option<usize> {
    stdout.trim().parse().ok()
}

/// The child's whole life: read the request from stdin, show the dialog,
/// print the answer. Returns the process exit code.
pub fn run_chooser_process() -> i32 {
    use std::io::Read;

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 2;
    }
    let Ok(request) = serde_json::from_str::<Request>(&input) else {
        return 2;
    };
    match show_dialog(&request) {
        Ok(Some(index)) => {
            println!("{index}");
            0
        }
        Ok(None) => 0,
        Err(e) => {
            eprintln!("screen chooser: {e}");
            1
        }
    }
}

fn show_dialog(request: &Request) -> Result<Option<usize>, String> {
    use gtk::glib;
    use gtk::prelude::*;
    use std::cell::Cell;
    use std::rc::Rc;

    gtk::init().map_err(|e| format!("GTK: {e}"))?;
    let window = gtk::Window::builder()
        .title("Share a screen")
        .default_width(440)
        .default_height(520)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    let app = if request.app.is_empty() {
        "An application".to_string()
    } else {
        request.app.clone()
    };
    let intro = gtk::Label::new(Some(&format!("{app} wants to share a screen or a window.")));
    intro.set_wrap(true);
    intro.set_xalign(0.0);
    content.append(&intro);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    // Row index → choice index; headings map to nothing.
    let mut rows: Vec<Option<usize>> = Vec::new();
    let mut section = None;
    for (index, choice) in request.choices.iter().enumerate() {
        if section.as_ref() != Some(&choice.section) {
            section = Some(choice.section.clone());
            let heading = gtk::Label::new(None);
            heading.set_markup(&format!(
                "<b>{}</b>",
                glib::markup_escape_text(&choice.section)
            ));
            heading.set_xalign(0.0);
            heading.set_margin_top(8);
            heading.set_margin_start(8);
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&heading));
            row.set_activatable(false);
            list.append(&row);
            rows.push(None);
        }
        let label = gtk::Label::new(Some(&choice.label));
        label.set_xalign(0.0);
        label.set_margin_top(6);
        label.set_margin_bottom(6);
        label.set_margin_start(20);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        list.append(&label);
        rows.push(Some(index));
    }
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&list));
    content.append(&scroll);

    let cancel = gtk::Button::with_label("Cancel");
    cancel.set_halign(gtk::Align::End);
    content.append(&cancel);
    window.set_child(Some(&content));

    let chosen = Rc::new(Cell::new(None));
    let quit = glib::MainLoop::new(None, false);
    {
        let (chosen, quit, window) = (chosen.clone(), quit.clone(), window.clone());
        list.connect_row_activated(move |_, row| {
            if let Some(Some(index)) = rows.get(row.index() as usize) {
                chosen.set(Some(*index));
                window.close();
                quit.quit();
            }
        });
    }
    {
        let (quit, window) = (quit.clone(), window.clone());
        cancel.connect_clicked(move |_| {
            window.close();
            quit.quit();
        });
    }
    {
        let quit = quit.clone();
        window.connect_close_request(move |_| {
            quit.quit();
            glib::Propagation::Proceed
        });
    }
    window.present();
    quit.run();
    Ok(chosen.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_printed_index_is_the_choice_and_silence_is_a_cancel() {
        assert_eq!(parse_answer("3\n"), Some(3));
        assert_eq!(parse_answer(""), None);
        assert_eq!(parse_answer("garbage"), None);
    }
}
