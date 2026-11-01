//! An application's interface tree on Linux, read and acted on through
//! AT-SPI2 — the accessibility bus GTK, Qt, Chromium and Firefox publish to.
//!
//! AT-SPI is a D-Bus protocol on a bus of its own: the session bus names its
//! address (`org.a11y.Bus.GetAddress`), and on that bus the registry's root
//! lists every application as a bus name. An application is matched to the
//! pids the node was given by asking the bus who owns each name
//! (`GetConnectionUnixProcessID`), never by its accessible name, which is
//! whatever the toolkit chose and is shared by every instance.
//!
//! Extents are asked in *window* coordinates. Screen coordinates are what
//! UI Automation gives, but on Wayland a client cannot know where its window
//! is, and toolkits answer screen extents with window-relative numbers or
//! zeros; window coordinates divided by the frame's own size are right on
//! both display servers.
//!
//! Roles are read by name (`GetRoleName`), not number: the numbering grew
//! over the years and toolkits disagree about the newer ones, while the names
//! are what Orca prints and are stable.
//!
//! Every D-Bus call runs on a thread of its own, for the reason
//! [`crate::linux_app`] gives: zbus's blocking calls start a runtime, which
//! panics inside the node's Tokio runtime.
//!
//! GTK registers its applications whether or not assistive technology is on.
//! Chromium and Electron check `org.a11y.Status.IsEnabled` once at start and
//! publish nothing otherwise, so a miss with the bus off says how the person
//! turns it on; Cursio never changes that setting itself.

use std::collections::HashMap;

use crate::ui_tree::{self, Limits, Pick, UiNode};
pub use cursio_protocol::diag::ElementAct as Act;

/// A snapshot of the window's interface.
pub struct Snapshot {
    /// The frame's accessible name: its title.
    pub title: String,
    pub root: UiNode,
    pub nodes: u32,
    /// The walk stopped at its limits; deeper or later elements are left out.
    pub truncated: bool,
}

/// AT-SPI state bits (`AtspiStateType`), as positions in the two-word set
/// `GetState` answers.
mod state {
    pub const ACTIVE: u32 = 1;
    pub const CHECKED: u32 = 4;
    pub const DEFUNCT: u32 = 6;
    pub const EDITABLE: u32 = 7;
    pub const ENABLED: u32 = 8;
    pub const EXPANDABLE: u32 = 9;
    pub const EXPANDED: u32 = 10;
    pub const FOCUSED: u32 = 12;
    pub const PRESSED: u32 = 20;
    pub const SELECTED: u32 = 23;
    pub const SENSITIVE: u32 = 24;
    pub const SHOWING: u32 = 25;
    pub const INDETERMINATE: u32 = 32;
    pub const CHECKABLE: u32 = 41;
}

fn has(states: [u32; 2], bit: u32) -> bool {
    states[(bit / 32) as usize] & (1 << (bit % 32)) != 0
}

/// What one accessible says about itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Facts {
    pub role: String,
    pub name: String,
    pub states: [u32; 2],
    /// Left, top, width, height in window coordinates, when it is a
    /// component on screen.
    pub extents: Option<[i32; 4]>,
    pub value: Option<String>,
}

/// The `ui_tree` role an AT-SPI role name maps to: UI Automation's
/// vocabulary, so a request names a button the same way on either system.
pub(crate) fn role(atspi: &str, states: [u32; 2]) -> String {
    let mapped = match atspi {
        "push button" | "toggle button" | "push button menu" => "button",
        "check box" | "switch" => "checkbox",
        "radio button" => "radiobutton",
        "entry" | "password text" => "edit",
        "text" if has(states, state::EDITABLE) => "edit",
        "text" | "label" | "static" | "paragraph" | "heading" | "caption" => "text",
        "page tab" => "tabitem",
        "page tab list" => "tab",
        "document web" | "document frame" | "document text" => "document",
        "frame" | "window" | "dialog" | "alert" | "file chooser" => "window",
        "panel" | "filler" | "section" | "grouping" | "form" | "landmark" | "article"
        | "block quote" | "generic" => "group",
        "list" | "list box" => "list",
        "list item" => "listitem",
        "menu item" | "check menu item" | "radio menu item" | "tearoff menu item" => "menuitem",
        "menu" => "menu",
        "menu bar" => "menubar",
        "image" | "icon" => "image",
        "link" => "hyperlink",
        "combo box" => "combobox",
        "slider" => "slider",
        "spin button" => "spinner",
        "tree" | "tree table" => "tree",
        "tree item" => "treeitem",
        "table" => "table",
        "table cell" => "dataitem",
        "tool bar" => "toolbar",
        "status bar" => "statusbar",
        "scroll bar" => "scrollbar",
        "progress bar" => "progressbar",
        "separator" => "separator",
        "tool tip" => "tooltip",
        other => return other.replace(' ', ""),
    };
    mapped.to_owned()
}

/// The `ui_tree` states an AT-SPI state set maps to.
pub(crate) fn states(atspi: &str, set: [u32; 2]) -> Vec<&'static str> {
    let mut out = Vec::new();
    // GTK 4 says SENSITIVE and never ENABLED; GTK 3, Qt and Chromium say
    // both. Either one means the element takes input.
    if !has(set, state::ENABLED) && !has(set, state::SENSITIVE) {
        out.push("disabled");
    }
    if has(set, state::FOCUSED) {
        out.push("focused");
    }
    if has(set, state::SELECTED) {
        out.push("selected");
    }
    if has(set, state::INDETERMINATE) && has(set, state::CHECKABLE) {
        out.push("mixed");
    } else if has(set, state::CHECKED) || (atspi == "toggle button" && has(set, state::PRESSED)) {
        out.push("checked");
    }
    if has(set, state::EXPANDABLE) {
        out.push(match has(set, state::EXPANDED) {
            true => "expanded",
            false => "collapsed",
        });
    }
    if !has(set, state::SHOWING) {
        out.push("offscreen");
    }
    out
}

/// Where the accessibles come from: the bus, or a fake in tests.
pub(crate) trait Source {
    type Ref: Clone;
    fn facts(&mut self, element: &Self::Ref) -> Result<Facts, String>;
    fn children(&mut self, element: &Self::Ref) -> Result<Vec<Self::Ref>, String>;
    /// Stable while the element exists: the last part of its object path.
    fn id(&self, element: &Self::Ref) -> String;
}

struct Walk<'s, S: Source> {
    source: &'s mut S,
    frame: [f32; 2],
    limits: Limits,
    nodes: u32,
    truncated: bool,
    elements: HashMap<String, S::Ref>,
}

impl<S: Source> Walk<'_, S> {
    fn node(&mut self, element: &S::Ref, facts: Facts, depth: u16) -> UiNode {
        let id = self.source.id(element);
        let showing = has(facts.states, state::SHOWING);
        let [fw, fh] = self.frame;
        let bounds = facts
            .extents
            .filter(|[_, _, w, h]| showing && *w > 0 && *h > 0 && fw > 0.0 && fh > 0.0)
            .map(|[x, y, w, h]| [x as f32 / fw, y as f32 / fh, w as f32 / fw, h as f32 / fh]);
        let mut node = UiNode {
            id: id.clone(),
            role: role(&facts.role, facts.states),
            name: facts.name.trim().to_owned(),
            value: facts.value,
            states: states(&facts.role, facts.states),
            bounds,
            children: Vec::new(),
        };
        self.elements.insert(id, element.clone());
        if depth >= self.limits.depth {
            return node;
        }
        let Ok(children) = self.source.children(element) else {
            return node;
        };
        for child in children {
            if self.nodes >= self.limits.nodes {
                self.truncated = true;
                break;
            }
            // An element that went away mid-walk, or one the toolkit marks
            // defunct, is left out rather than failing the whole tree.
            let Ok(facts) = self.source.facts(&child) else {
                continue;
            };
            if has(facts.states, state::DEFUNCT) {
                continue;
            }
            self.nodes += 1;
            let child_node = self.node(&child, facts, depth + 1);
            node.children.push(child_node);
        }
        node
    }
}

/// The tree under `frame`, within `limits`, with every element it holds by id.
pub(crate) fn walk<S: Source>(
    source: &mut S,
    frame: &S::Ref,
    limits: Limits,
) -> Result<(Snapshot, HashMap<String, S::Ref>), String> {
    let facts = source.facts(frame)?;
    let size = facts
        .extents
        .map(|[_, _, w, h]| [w as f32, h as f32])
        .unwrap_or_default();
    let title = facts.name.trim().to_owned();
    let mut walk = Walk {
        source,
        frame: size,
        limits,
        nodes: 1,
        truncated: false,
        elements: HashMap::new(),
    };
    let mut root = walk.node(frame, facts, 0);
    // The frame is the window itself: whatever its extents said, it spans it.
    if root.bounds.is_some() {
        root.bounds = Some([0.0, 0.0, 1.0, 1.0]);
    }
    let snapshot = Snapshot {
        title,
        root,
        nodes: walk.nodes,
        truncated: walk.truncated,
    };
    Ok((snapshot, walk.elements))
}

/// Which of an application's top-level accessibles is the window asked for:
/// the first whose name contains `title`, else the active one, else the
/// first on screen.
pub(crate) fn choose_frame(frames: &[(usize, Facts)], title: Option<&str>) -> Option<usize> {
    if let Some(title) = title.map(str::to_lowercase) {
        return frames
            .iter()
            .find(|(_, f)| f.name.to_lowercase().contains(&title))
            .map(|(i, _)| *i);
    }
    frames
        .iter()
        .find(|(_, f)| has(f.states, state::ACTIVE))
        .or_else(|| frames.iter().find(|(_, f)| has(f.states, state::SHOWING)))
        .or(frames.first())
        .map(|(i, _)| *i)
}

/// The index of the action among `names` that does `act`, by the names
/// toolkits give them: GTK says "click" and "activate", Qt "Press" and
/// "Toggle", Chromium "click", "jump", "expand or contract".
pub(crate) fn action_for(act: &Act, names: &[String]) -> Result<usize, String> {
    let wanted: &[&str] = match act {
        Act::Invoke => &["click", "press", "activate", "jump", "open"],
        Act::Toggle => &["toggle", "click", "press", "activate"],
        Act::Select => &["select", "click", "activate"],
        Act::Expand => &["expand", "expand or contract", "open", "toggle", "click"],
        Act::Collapse => &["collapse", "expand or contract", "close", "toggle", "click"],
        Act::Focus | Act::SetValue(_) => &[],
    };
    let lower: Vec<String> = names.iter().map(|n| n.trim().to_lowercase()).collect();
    for want in wanted {
        if let Some(index) = lower.iter().position(|n| n == want) {
            return Ok(index);
        }
    }
    Err(match names.is_empty() {
        true => "it offers no actions".to_owned(),
        false => format!("it offers only {}", names.join(", ")),
    })
}

fn verb(act: &Act) -> &'static str {
    match act {
        Act::Invoke => "invoked",
        Act::Toggle => "toggled",
        Act::Select => "selected",
        Act::Expand => "expanded",
        Act::Collapse => "collapsed",
        Act::Focus => "focused",
        Act::SetValue(_) => "set",
    }
}

const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT: &str = "/org/a11y/atspi/accessible/root";
const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const COMPONENT: &str = "org.a11y.atspi.Component";
const ACTION: &str = "org.a11y.atspi.Action";
const TEXT: &str = "org.a11y.atspi.Text";
const EDITABLE_TEXT: &str = "org.a11y.atspi.EditableText";
const VALUE: &str = "org.a11y.atspi.Value";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
/// `ATSPI_COORD_TYPE_WINDOW`: relative to the element's top-level window.
const COORD_WINDOW: u32 = 1;
/// A value longer than this is cut: a document's whole text is not a value.
const MAX_VALUE: i32 = 1000;
/// One unresponsive application must not hold a request for D-Bus's default
/// 25 seconds per call.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

const TURN_ON: &str = "the accessibility bus is off — the person at that machine turns it on \
     with `gsettings set org.gnome.desktop.interface toolkit-accessibility true` and restarts \
     the application (Chromium and Electron also publish when started with \
     --force-renderer-accessibility)";

/// An accessible: the bus name of its application and its object path.
type Element = (String, zbus::zvariant::OwnedObjectPath);

struct Bus {
    conn: zbus::blocking::Connection,
}

impl Bus {
    fn connect() -> Result<Bus, String> {
        let session = zbus::blocking::Connection::session()
            .map_err(|e| format!("no D-Bus session bus: {e}"))?;
        let address: String = session
            .call_method(
                Some("org.a11y.Bus"),
                "/org/a11y/bus",
                Some("org.a11y.Bus"),
                "GetAddress",
                &(),
            )
            .and_then(|reply| reply.body().deserialize::<String>())
            .map_err(|e| {
                format!("the accessibility bus is not running (at-spi-bus-launcher): {e}")
            })?;
        let conn = zbus::blocking::connection::Builder::address(address.as_str())
            .and_then(|b| b.method_timeout(CALL_TIMEOUT).build())
            .map_err(|e| format!("could not reach the accessibility bus at {address}: {e}"))?;
        Ok(Bus { conn })
    }

    fn call<B, R>(
        &self,
        element: &Element,
        iface: &str,
        method: &str,
        body: &B,
    ) -> Result<R, String>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: for<'de> serde::Deserialize<'de> + zbus::zvariant::Type,
    {
        self.conn
            .call_method(
                Some(element.0.as_str()),
                element.1.as_str(),
                Some(iface),
                method,
                body,
            )
            .and_then(|reply| reply.body().deserialize::<R>())
            .map_err(|e| format!("{method}: {e}"))
    }

    fn property<R>(&self, element: &Element, iface: &str, name: &str) -> Result<R, String>
    where
        R: TryFrom<zbus::zvariant::OwnedValue>,
    {
        let value: zbus::zvariant::OwnedValue =
            self.call(element, PROPERTIES, "Get", &(iface, name))?;
        R::try_from(value).map_err(|_| format!("{iface}.{name} has an unexpected type"))
    }

    fn interfaces(&self, element: &Element) -> Vec<String> {
        self.call(element, ACCESSIBLE, "GetInterfaces", &())
            .unwrap_or_default()
    }

    /// Whether assistive technology is switched on, which is what Chromium
    /// waits for before publishing anything.
    fn enabled() -> bool {
        zbus::blocking::Connection::session()
            .and_then(|session| {
                session.call_method(
                    Some("org.a11y.Bus"),
                    "/org/a11y/bus",
                    Some(PROPERTIES),
                    "Get",
                    &("org.a11y.Status", "IsEnabled"),
                )
            })
            .and_then(|reply| reply.body().deserialize::<zbus::zvariant::OwnedValue>())
            .ok()
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(false)
    }

    /// The top-level accessibles of every application one of `pids` owns.
    fn frames_of(&self, pids: &[u32]) -> Result<Vec<Element>, String> {
        let root: Element = (REGISTRY.to_owned(), owned_path(ROOT));
        let apps: Vec<Element> = self
            .call(&root, ACCESSIBLE, "GetChildren", &())
            .map_err(|e| format!("the accessibility registry did not answer: {e}"))?;
        let mut frames = Vec::new();
        for app in apps {
            let pid = self
                .conn
                .call_method(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    Some("org.freedesktop.DBus"),
                    "GetConnectionUnixProcessID",
                    &(app.0.as_str(),),
                )
                .and_then(|reply| reply.body().deserialize::<u32>());
            if pid.is_ok_and(|pid| pids.contains(&pid)) {
                let children: Vec<Element> = self
                    .call(&app, ACCESSIBLE, "GetChildren", &())
                    .unwrap_or_default();
                frames.extend(children);
            }
        }
        Ok(frames)
    }

    /// The window `title` names among `pids`' applications.
    fn frame(&mut self, pids: &[u32], title: Option<&str>) -> Result<Element, String> {
        let frames = self.frames_of(pids)?;
        if frames.is_empty() {
            return Err(match Bus::enabled() {
                true => "the application has published no window on the accessibility bus; \
                         Chromium and Electron publish only when started after the bus was on \
                         or with --force-renderer-accessibility"
                    .to_owned(),
                false => format!("the application is not on the accessibility bus: {TURN_ON}"),
            });
        }
        let facts: Vec<(usize, Facts)> = frames
            .iter()
            .enumerate()
            .filter_map(|(i, frame)| self.facts(frame).ok().map(|f| (i, f)))
            .collect();
        choose_frame(&facts, title)
            .map(|i| frames[i].clone())
            .ok_or_else(|| match title {
                Some(title) => {
                    let names: Vec<String> =
                        facts.iter().map(|(_, f)| format!("{:?}", f.name)).collect();
                    format!(
                        "no window titled like {title:?} on the accessibility bus; it has {}",
                        names.join(", ")
                    )
                }
                None => "the application's windows did not answer".to_owned(),
            })
    }
}

fn owned_path(path: &str) -> zbus::zvariant::OwnedObjectPath {
    zbus::zvariant::OwnedObjectPath::try_from(path).expect("a constant object path is valid")
}

/// Text is read as a value only where it is one: what a person typed into
/// a field, never a password, never a whole document.
fn reads_text(role: &str, states: [u32; 2]) -> bool {
    matches!(role, "entry" | "combo box" | "spin button")
        || (role == "text" && has(states, state::EDITABLE))
}

impl Source for Bus {
    type Ref = Element;

    fn facts(&mut self, element: &Element) -> Result<Facts, String> {
        let role: String = self.call(element, ACCESSIBLE, "GetRoleName", &())?;
        let name: String = self
            .property(element, ACCESSIBLE, "Name")
            .unwrap_or_default();
        let set: Vec<u32> = self.call(element, ACCESSIBLE, "GetState", &())?;
        let states = [
            set.first().copied().unwrap_or(0),
            set.get(1).copied().unwrap_or(0),
        ];
        let interfaces = self.interfaces(element);
        let offers = |iface: &str| interfaces.iter().any(|i| i == iface);
        let extents = match offers(COMPONENT) && has(states, state::SHOWING) {
            true => self
                .call::<_, (i32, i32, i32, i32)>(element, COMPONENT, "GetExtents", &(COORD_WINDOW,))
                .ok()
                .map(|(x, y, w, h)| [x, y, w, h]),
            false => None,
        };
        let value = if offers(VALUE) {
            self.property::<f64>(element, VALUE, "CurrentValue")
                .ok()
                .map(|v| v.to_string())
        } else if offers(TEXT) && reads_text(&role, states) {
            let count: i32 = self.property(element, TEXT, "CharacterCount").unwrap_or(0);
            self.call::<_, String>(element, TEXT, "GetText", &(0i32, count.min(MAX_VALUE)))
                .ok()
        } else {
            None
        };
        Ok(Facts {
            role,
            name,
            states,
            extents,
            value,
        })
    }

    fn children(&mut self, element: &Element) -> Result<Vec<Element>, String> {
        self.call(element, ACCESSIBLE, "GetChildren", &())
    }

    fn id(&self, element: &Element) -> String {
        element
            .1
            .as_str()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned()
    }
}

/// Run `f` against the accessibility bus on a thread of its own.
fn on_bus<T: Send + 'static>(
    f: impl FnOnce(&mut Bus) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    std::thread::Builder::new()
        .name("cursio-atspi".into())
        .spawn(move || f(&mut Bus::connect()?))
        .map_err(|e| format!("could not start the accessibility thread: {e}"))?
        .join()
        .map_err(|_| "the accessibility thread panicked".to_owned())?
}

/// The interface of the window `title` names (or the active one) among the
/// applications `pids` own, within `limits`.
pub fn tree(pids: Vec<u32>, title: Option<String>, limits: Limits) -> Result<Snapshot, String> {
    on_bus(move |bus| {
        let frame = bus.frame(&pids, title.as_deref())?;
        walk(bus, &frame, limits).map(|(snapshot, _)| snapshot)
    })
}

/// Do `act` to the element `pick` names in that window; answers what was
/// done, in a sentence.
pub fn act(
    pids: Vec<u32>,
    title: Option<String>,
    pick: Pick,
    act: Act,
    limits: Limits,
) -> Result<String, String> {
    on_bus(move |bus| {
        let frame = bus.frame(&pids, title.as_deref())?;
        let (snapshot, elements) = walk(bus, &frame, limits)?;
        let node = ui_tree::find(&snapshot.root, &pick)?;
        let element = elements
            .get(&node.id)
            .ok_or("the element went away while it was looked for")?;
        perform(bus, element, node, &act)
    })
}

fn perform(bus: &Bus, element: &Element, node: &UiNode, act: &Act) -> Result<String, String> {
    let label = format!("{} {:?}", node.role, node.name);
    let refused = |e: String| format!("{label} would not be {}: {e}", verb(act));
    match act {
        Act::Focus => match bus.call::<_, bool>(element, COMPONENT, "GrabFocus", &()) {
            Ok(true) => Ok(format!("focused {label}")),
            Ok(false) => Err(format!("{label} cannot take focus")),
            Err(e) => Err(refused(e)),
        },
        Act::SetValue(value) => {
            let interfaces = bus.interfaces(element);
            if interfaces.iter().any(|i| i == EDITABLE_TEXT) {
                match bus.call::<_, bool>(
                    element,
                    EDITABLE_TEXT,
                    "SetTextContents",
                    &(value.as_str(),),
                ) {
                    Ok(true) => Ok(format!(
                        "set {label} to {} characters",
                        value.chars().count()
                    )),
                    Ok(false) => Err(format!("{label} refused the text")),
                    Err(e) => Err(refused(e)),
                }
            } else if interfaces.iter().any(|i| i == VALUE) {
                let number: f64 = value
                    .trim()
                    .parse()
                    .map_err(|_| format!("{label} takes a number, not {value:?}"))?;
                bus.call::<_, ()>(
                    element,
                    PROPERTIES,
                    "Set",
                    &(VALUE, "CurrentValue", zbus::zvariant::Value::F64(number)),
                )
                .map(|()| format!("set {label} to {number}"))
                .map_err(refused)
            } else {
                Err(format!("{label} takes no value"))
            }
        }
        // "expand or contract" toggles, so asking for the state an element
        // is already in must not flip it the other way.
        Act::Expand if node.states.contains(&"expanded") => {
            Ok(format!("{label} is already expanded"))
        }
        Act::Collapse if node.states.contains(&"collapsed") => {
            Ok(format!("{label} is already collapsed"))
        }
        _ => {
            let actions: Vec<(String, String, String)> = bus
                .call(element, ACTION, "GetActions", &())
                .unwrap_or_default();
            let names: Vec<String> = actions.into_iter().map(|(name, _, _)| name).collect();
            let index = action_for(act, &names).map_err(refused)?;
            match bus.call::<_, bool>(element, ACTION, "DoAction", &(index as i32,)) {
                Ok(true) => Ok(format!("{} {label}", verb(act))),
                Ok(false) => Err(refused(format!("its {:?} action failed", names[index]))),
                Err(e) => Err(refused(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(bits: &[u32]) -> [u32; 2] {
        let mut out = [0u32; 2];
        for bit in bits {
            out[(bit / 32) as usize] |= 1 << (bit % 32);
        }
        out
    }

    const NORMAL: &[u32] = &[state::ENABLED, state::SHOWING];

    struct Fake {
        elements: HashMap<String, (Facts, Vec<String>)>,
    }

    /// Id, role name, name, state bits, extents, child ids.
    type Item<'a> = (
        &'a str,
        &'a str,
        &'a str,
        Vec<u32>,
        Option<[i32; 4]>,
        Vec<&'a str>,
    );

    impl Fake {
        fn with(items: Vec<Item>) -> Fake {
            let elements = items
                .into_iter()
                .map(|(id, role, name, bits, extents, children)| {
                    let facts = Facts {
                        role: role.into(),
                        name: name.into(),
                        states: set(&bits),
                        extents,
                        value: None,
                    };
                    let children = children.into_iter().map(String::from).collect();
                    (id.to_owned(), (facts, children))
                })
                .collect();
            Fake { elements }
        }
    }

    impl Source for Fake {
        type Ref = String;
        fn facts(&mut self, element: &String) -> Result<Facts, String> {
            self.elements
                .get(element)
                .map(|(f, _)| f.clone())
                .ok_or_else(|| "gone".to_owned())
        }
        fn children(&mut self, element: &String) -> Result<Vec<String>, String> {
            Ok(self.elements[element].1.clone())
        }
        fn id(&self, element: &String) -> String {
            element.clone()
        }
    }

    fn player() -> Fake {
        let n = NORMAL.to_vec();
        Fake::with(vec![
            (
                "1",
                "frame",
                "Player",
                n.clone(),
                Some([0, 0, 800, 400]),
                vec!["2", "9"],
            ),
            (
                "2",
                "panel",
                "",
                n.clone(),
                Some([0, 0, 800, 400]),
                vec!["3", "4", "5", "6"],
            ),
            (
                "3",
                "push button",
                "Play",
                vec![state::ENABLED, state::SHOWING, state::FOCUSED],
                Some([400, 100, 80, 40]),
                vec![],
            ),
            (
                "4",
                "check box",
                "Loop",
                vec![state::ENABLED, state::SHOWING, state::CHECKED],
                Some([0, 0, 10, 10]),
                vec![],
            ),
            (
                "5",
                "label",
                "hidden",
                vec![state::ENABLED],
                Some([0, 0, 10, 10]),
                vec![],
            ),
            (
                "6",
                "push button",
                "dead",
                vec![state::DEFUNCT],
                None,
                vec![],
            ),
            ("9", "status bar", "", n, None, vec!["gone"]),
        ])
    }

    #[test]
    fn roles_map_to_the_vocabulary_ui_automation_uses() {
        let none = [0, 0];
        assert_eq!(role("push button", none), "button");
        assert_eq!(role("toggle button", none), "button");
        assert_eq!(role("check box", none), "checkbox");
        assert_eq!(role("page tab", none), "tabitem");
        assert_eq!(role("document web", none), "document");
        assert_eq!(role("text", set(&[state::EDITABLE])), "edit");
        assert_eq!(role("text", none), "text");
        assert_eq!(role("tree table", none), "tree");
        assert_eq!(role("generic", none), "group");
        assert_eq!(role("color chooser", none), "colorchooser");
    }

    #[test]
    fn states_map_to_the_tree_vocabulary_with_disabled_and_offscreen_as_absences() {
        assert_eq!(states("push button", set(NORMAL)), Vec::<&str>::new());
        assert_eq!(states("push button", [0, 0]), vec!["disabled", "offscreen"]);
        let gtk4 = set(&[state::SENSITIVE, state::SHOWING]);
        assert_eq!(states("push button", gtk4), Vec::<&str>::new());
        let mixed = set(&[
            state::ENABLED,
            state::SHOWING,
            state::CHECKABLE,
            state::INDETERMINATE,
            state::CHECKED,
        ]);
        assert_eq!(states("check box", mixed), vec!["mixed"]);
        let pressed = set(&[state::ENABLED, state::SHOWING, state::PRESSED]);
        assert_eq!(states("toggle button", pressed), vec!["checked"]);
        assert_eq!(states("push button", pressed), Vec::<&str>::new());
        let tree = set(&[
            state::ENABLED,
            state::SHOWING,
            state::EXPANDABLE,
            state::FOCUSED,
        ]);
        assert_eq!(states("tree item", tree), vec!["focused", "collapsed"]);
    }

    #[test]
    fn the_walk_builds_the_tree_with_window_fractions_and_skips_defunct_and_vanished_elements() {
        let mut fake = player();
        let (snapshot, elements) = walk(&mut fake, &"1".to_owned(), Limits::default()).unwrap();
        assert_eq!(snapshot.title, "Player");
        assert!(!snapshot.truncated);
        assert_eq!(
            snapshot.nodes, 6,
            "frame, panel, three children, status bar"
        );
        let panel = &snapshot.root.children[0];
        assert_eq!(panel.role, "group");
        let play = &panel.children[0];
        assert_eq!(
            (play.role.as_str(), play.name.as_str(), play.id.as_str()),
            ("button", "Play", "3")
        );
        assert_eq!(play.states, vec!["focused"]);
        assert_eq!(play.bounds, Some([0.5, 0.25, 0.1, 0.1]));
        assert_eq!(panel.children[1].states, vec!["checked"]);
        assert_eq!(
            panel.children[2].bounds, None,
            "an element not showing has no bounds"
        );
        assert!(panel.children.iter().all(|n| n.name != "dead"));
        assert!(elements.contains_key("4"));
        let text = ui_tree::render(&snapshot.root);
        assert!(
            text.contains("- button \"Play\" [focused] @0.55,0.30 #3"),
            "{text}"
        );
    }

    #[test]
    fn the_walk_stops_at_its_limits_and_says_so() {
        let mut fake = player();
        let (snapshot, _) =
            walk(&mut fake, &"1".to_owned(), Limits { depth: 8, nodes: 3 }).unwrap();
        assert!(snapshot.truncated);
        assert_eq!(snapshot.nodes, 3);
        let (shallow, _) = walk(
            &mut player(),
            &"1".to_owned(),
            Limits {
                depth: 1,
                nodes: 100,
            },
        )
        .unwrap();
        assert!(shallow.root.children[0].children.is_empty());
    }

    #[test]
    fn the_window_is_chosen_by_title_then_by_being_active() {
        let frame = |name: &str, bits: &[u32]| Facts {
            name: name.into(),
            states: set(bits),
            ..Facts::default()
        };
        let frames = vec![
            (0, frame("Preferences", &[state::SHOWING])),
            (1, frame("Main — Player", &[state::SHOWING, state::ACTIVE])),
        ];
        assert_eq!(choose_frame(&frames, Some("prefer")), Some(0));
        assert_eq!(choose_frame(&frames, Some("nothing")), None);
        assert_eq!(choose_frame(&frames, None), Some(1));
        assert_eq!(choose_frame(&frames[..1], None), Some(0));
    }

    #[test]
    fn an_action_is_found_by_the_names_toolkits_give_it() {
        let names = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            action_for(&Act::Invoke, &names(&["Press", "SetFocus"])),
            Ok(0)
        );
        assert_eq!(
            action_for(&Act::Toggle, &names(&["click", "toggle"])),
            Ok(1)
        );
        assert_eq!(
            action_for(&Act::Expand, &names(&["expand or contract"])),
            Ok(0)
        );
        assert_eq!(
            action_for(&Act::Select, &names(&["jump"])),
            Err("it offers only jump".to_owned())
        );
        assert_eq!(
            action_for(&Act::Invoke, &[]),
            Err("it offers no actions".to_owned())
        );
    }
}
