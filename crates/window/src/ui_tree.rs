//! A window's interface as a tree of elements — roles, names, states — the
//! way screen readers see it, for a program on another machine to read and
//! act on by meaning rather than by where things are drawn.
//!
//! A click at a fraction of a screenshot is what a person does, and the most
//! brittle thing an agent can do: one changed layout and it lands elsewhere,
//! silently. Every desktop already publishes what is on screen to assistive
//! technology (UI Automation on Windows, AT-SPI on Linux), web views included,
//! so "the button named Play" can be found and pressed whatever the layout.
//!
//! This module is the platform-neutral half: the tree a backend builds, its
//! compact text form (the shape of Playwright's ARIA snapshot, a few
//! kilobytes where a screenshot is a megabyte), and finding one element in it
//! by role and name, or by the id a snapshot printed. Backends:
//! [`crate::windows_uia`].

/// One element of a window's interface.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UiNode {
    /// Stable while the element exists (UI Automation's runtime id), so an
    /// agent can act on an element it saw in an earlier snapshot.
    pub id: String,
    /// Lowercase: `button`, `edit`, `checkbox`, `tabitem`, `document`, …
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    /// `disabled`, `focused`, `selected`, `checked`, `mixed`, `expanded`,
    /// `collapsed`, `offscreen`.
    pub states: Vec<&'static str>,
    /// Left, top, width, height as fractions of the window (0 to 1), when the
    /// element is on screen.
    pub bounds: Option<[f32; 4]>,
    pub children: Vec<UiNode>,
}

/// How much of a tree a snapshot walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub depth: u16,
    pub nodes: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            depth: cursio_protocol::diag::DEFAULT_TREE_DEPTH,
            nodes: cursio_protocol::diag::DEFAULT_TREE_NODES,
        }
    }
}

impl Limits {
    /// The most a request may ask for: what acting on an element walks, so
    /// any element a tree could show is found.
    pub fn most() -> Self {
        Limits {
            depth: cursio_protocol::diag::MAX_TREE_DEPTH,
            nodes: cursio_protocol::diag::MAX_TREE_NODES,
        }
    }
}

/// Elements that carry nothing to read or act on: a nameless group or pane
/// with exactly one child is folded into that child, which is most of a web
/// view's nesting.
fn is_wrapper(node: &UiNode) -> bool {
    matches!(node.role.as_str(), "group" | "pane" | "custom")
        && node.name.is_empty()
        && node.value.is_none()
        && node.children.len() == 1
}

/// The tree as indented lines, one element each:
/// `- button "Play" [focused] @0.82,0.12 #42-7-1`.
pub fn render(root: &UiNode) -> String {
    fn line(out: &mut String, node: &UiNode, depth: usize) {
        if is_wrapper(node) {
            line(out, &node.children[0], depth);
            return;
        }
        out.push_str(&"  ".repeat(depth));
        out.push_str("- ");
        out.push_str(&node.role);
        if !node.name.is_empty() {
            out.push_str(&format!(" {:?}", clip(&node.name, 120)));
        }
        if let Some(value) = node.value.as_deref().filter(|v| !v.is_empty()) {
            out.push_str(&format!(" = {:?}", clip(value, 120)));
        }
        if !node.states.is_empty() {
            out.push_str(&format!(" [{}]", node.states.join(", ")));
        }
        if let Some([x, y, w, h]) = node.bounds {
            // The element's centre: where a click by coordinates would go.
            out.push_str(&format!(" @{:.2},{:.2}", x + w / 2.0, y + h / 2.0));
        }
        if !node.id.is_empty() {
            out.push_str(&format!(" #{}", node.id));
        }
        out.push('\n');
        for child in &node.children {
            line(out, child, depth + 1);
        }
    }
    let mut out = String::new();
    line(&mut out, root, 0);
    out
}

fn clip(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    match flat.chars().count() > max {
        true => flat.chars().take(max).chain(['…']).collect(),
        false => flat,
    }
}

/// Every element, depth first.
fn walk<'a>(node: &'a UiNode, out: &mut Vec<&'a UiNode>) {
    out.push(node);
    for child in &node.children {
        walk(child, out);
    }
}

pub use cursio_protocol::diag::ElementPick as Pick;

/// The element `pick` names in `root`, or why there is none — with the
/// names it could have meant, so the asker's next try can be right. A name
/// matches exactly, ignoring case; failing that, the only element whose name
/// contains it does.
pub fn find<'a>(root: &'a UiNode, pick: &Pick) -> Result<&'a UiNode, String> {
    let mut all = Vec::new();
    walk(root, &mut all);
    match pick {
        Pick::Id(id) => all.into_iter().find(|n| n.id == *id).ok_or_else(|| {
            format!("no element #{id} in the window now; take a new tree, ids change when the interface is rebuilt")
        }),
        Pick::Match { role, name, nth } => {
            let role_l = role.trim().to_lowercase();
            let name_l = name.trim().to_lowercase();
            let of_role: Vec<&UiNode> = all
                .into_iter()
                .filter(|n| n.role == role_l)
                .collect();
            let exact: Vec<&UiNode> = of_role
                .iter()
                .copied()
                .filter(|n| n.name.trim().to_lowercase() == name_l)
                .collect();
            if let Some(node) = exact.get(*nth as usize) {
                return Ok(node);
            }
            let containing: Vec<&UiNode> = of_role
                .iter()
                .copied()
                .filter(|n| n.name.to_lowercase().contains(&name_l))
                .collect();
            if exact.is_empty() && *nth == 0 {
                if let [only] = containing.as_slice() {
                    return Ok(only);
                }
            }
            Err(no_match(&role_l, name, *nth, &exact, &containing, &of_role))
        }
    }
}

fn no_match(
    role: &str,
    name: &str,
    nth: u32,
    exact: &[&UiNode],
    containing: &[&UiNode],
    of_role: &[&UiNode],
) -> String {
    let names = |nodes: &[&UiNode]| -> String {
        let mut seen: Vec<String> = Vec::new();
        for node in nodes {
            let shown = format!("{:?}", clip(&node.name, 60));
            if !seen.contains(&shown) {
                seen.push(shown);
            }
        }
        let more = seen.len().saturating_sub(15);
        let mut list = seen.into_iter().take(15).collect::<Vec<_>>().join(", ");
        if more > 0 {
            list.push_str(&format!(" and {more} more"));
        }
        list
    };
    if !exact.is_empty() {
        return format!(
            "only {} {role} named {name:?}; nth {nth} counts from 0",
            exact.len()
        );
    }
    if containing.len() > 1 {
        return format!(
            "{} {role}s have {name:?} in their name: {}; name one exactly",
            containing.len(),
            names(containing)
        );
    }
    if of_role.is_empty() {
        return format!("no {role} in the window; take a tree to see its roles");
    }
    format!("no {role} named {name:?}; {role}s: {}", names(of_role))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: &str, name: &str, children: Vec<UiNode>) -> UiNode {
        UiNode {
            id: format!("{role}-{name}"),
            role: role.into(),
            name: name.into(),
            children,
            ..UiNode::default()
        }
    }

    fn launcher() -> UiNode {
        node(
            "window",
            "LeavePulse",
            vec![node(
                "group",
                "",
                vec![node(
                    "document",
                    "Library",
                    vec![
                        node("button", "Play", vec![]),
                        node("button", "Play now", vec![]),
                        node("button", "Install selected version", vec![]),
                        node("tabitem", "Library", vec![]),
                    ],
                )],
            )],
        )
    }

    #[test]
    fn a_tree_renders_one_line_per_element_with_its_role_name_states_and_id() {
        let mut tree = launcher();
        tree.children[0].children[0].children[0].states = vec!["focused"];
        tree.children[0].children[0].children[0].bounds = Some([0.8, 0.1, 0.1, 0.04]);
        let text = render(&tree);
        assert!(text.starts_with("- window \"LeavePulse\" #window-LeavePulse\n"));
        assert!(
            text.contains("    - button \"Play\" [focused] @0.85,0.12 #button-Play\n"),
            "{text}"
        );
        assert!(
            !text.contains("- group"),
            "a nameless wrapper with one child is folded away"
        );
    }

    #[test]
    fn an_element_is_found_by_exact_name_ignoring_case_before_a_partial_one() {
        let tree = launcher();
        let pick = |name: &str| Pick::Match {
            role: "Button".into(),
            name: name.into(),
            nth: 0,
        };
        assert_eq!(find(&tree, &pick("play")).unwrap().name, "Play");
        assert_eq!(
            find(&tree, &pick("install")).unwrap().name,
            "Install selected version",
            "a single partial match is enough"
        );
        assert_eq!(
            find(&tree, &Pick::Id("tabitem-Library".into()))
                .unwrap()
                .role,
            "tabitem"
        );
    }

    #[test]
    fn a_miss_names_the_elements_it_could_have_meant() {
        let tree = launcher();
        let miss = |role: &str, name: &str| {
            find(
                &tree,
                &Pick::Match {
                    role: role.into(),
                    name: name.into(),
                    nth: 0,
                },
            )
            .unwrap_err()
        };
        let error = miss("button", "Stop");
        assert!(error.contains("no button named \"Stop\""), "{error}");
        assert!(error.contains("\"Play now\""), "{error}");
        let error = miss("button", "pla");
        assert!(error.contains("2 buttons have \"pla\""), "{error}");
        assert!(miss("slider", "x").contains("no slider in the window"));
        assert!(find(&tree, &Pick::Id("gone".into()))
            .unwrap_err()
            .contains("take a new tree"));
    }
}
