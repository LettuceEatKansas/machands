//! The accessibility tree, flattened into a short list of lines a model can act on.
//!
//! `Accessibility.getFullAXTree` returns every node of the page. The reader keeps what a person
//! would use: interactive elements (each with a short `eN` ref), headings, landmarks, images with
//! text and the visible text itself, indented by nesting, and stops at a character budget. A ref
//! names the element's `backendDOMNodeId`, so the same element keeps the same ref across reads of
//! one page. Refs are cleared when the page navigates.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

/// One node of the raw tree.
#[derive(Debug, Clone, Default)]
pub struct AxNode {
    /// The node id (a string in the protocol).
    pub id: String,
    /// Left out of the accessibility tree on purpose (hidden, presentational).
    pub ignored: bool,
    /// The role, e.g. `button`, `StaticText`.
    pub role: String,
    /// The accessible name.
    pub name: String,
    /// The value (text fields, sliders).
    pub value: String,
    /// The properties we show, by name.
    pub props: Vec<(String, String)>,
    /// Child node ids.
    pub children: Vec<String>,
    /// The DOM node this one stands for.
    pub backend: Option<i64>,
}

fn inner(v: &Value) -> String {
    match v.get("value") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Read the `nodes` array of a `getFullAXTree` answer.
pub fn parse_nodes(result: &Value) -> Vec<AxNode> {
    let Some(list) = result.get("nodes").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .map(|n| {
            let mut node = AxNode {
                id: n
                    .get("nodeId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                ignored: n.get("ignored").and_then(Value::as_bool).unwrap_or(false),
                role: n.get("role").map(inner).unwrap_or_default(),
                name: n.get("name").map(inner).unwrap_or_default(),
                value: n.get("value").map(inner).unwrap_or_default(),
                children: n
                    .get("childIds")
                    .and_then(Value::as_array)
                    .map(|c| {
                        c.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                backend: n.get("backendDOMNodeId").and_then(Value::as_i64),
                props: Vec::new(),
            };
            if let Some(props) = n.get("properties").and_then(Value::as_array) {
                for p in props {
                    let name = p.get("name").and_then(Value::as_str).unwrap_or("");
                    if SHOWN_PROPS.contains(&name) {
                        let v = p.get("value").map(inner).unwrap_or_default();
                        node.props.push((name.to_string(), v));
                    }
                }
            }
            node
        })
        .collect()
}

/// Properties worth their characters.
const SHOWN_PROPS: [&str; 10] = [
    "checked", "disabled", "expanded", "selected", "required", "invalid", "readonly", "level",
    "url", "focused",
];

/// Roles a person clicks, types into or toggles.
pub const INTERACTIVE: [&str; 20] = [
    "button",
    "link",
    "textbox",
    "searchbox",
    "combobox",
    "checkbox",
    "radio",
    "switch",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "tab",
    "option",
    "slider",
    "spinbutton",
    "listbox",
    "treeitem",
    "textarea",
    "PopUpButton",
    "DisclosureTriangle",
];

/// Roles that give a page its outline.
const LANDMARKS: [&str; 9] = [
    "navigation",
    "main",
    "banner",
    "contentinfo",
    "form",
    "search",
    "dialog",
    "alertdialog",
    "complementary",
];

/// What a ref stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefInfo {
    /// The DOM node.
    pub backend: i64,
    /// The role it had when it was read.
    pub role: String,
    /// The accessible name it had.
    pub name: String,
}

/// The refs handed out for one page.
#[derive(Debug, Default)]
pub struct Refs {
    by_backend: HashMap<i64, u32>,
    by_ref: HashMap<u32, RefInfo>,
    next: u32,
}

impl Refs {
    /// Forget everything (the page navigated).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The ref for `backend`, new if unseen. Refreshes the stored role and name.
    pub fn assign(&mut self, backend: i64, role: &str, name: &str) -> u32 {
        let n = match self.by_backend.get(&backend) {
            Some(n) => *n,
            None => {
                self.next += 1;
                self.by_backend.insert(backend, self.next);
                self.next
            }
        };
        self.by_ref.insert(
            n,
            RefInfo {
                backend,
                role: role.to_string(),
                name: name.to_string(),
            },
        );
        n
    }

    /// Look up `e12` (or `12`).
    pub fn lookup(&self, text: &str) -> Option<&RefInfo> {
        let n: u32 = text.trim().trim_start_matches(['e', 'E']).parse().ok()?;
        self.by_ref.get(&n)
    }
}

/// One output line before refs are handed out.
#[derive(Debug)]
struct Draft {
    depth: usize,
    body: String,
    target: Option<(i64, String, String)>,
}

/// The rendered tree.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    /// The lines, joined with newlines.
    pub text: String,
    /// How many elements fit.
    pub shown: usize,
    /// How many there were.
    pub total: usize,
    /// Every line, as `(ref number if interactive, role, name)`, for `find` and the banner hints.
    pub entries: Vec<Entry>,
}

/// One shown element.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    /// `eN` when interactive.
    pub reference: Option<String>,
    /// Its role.
    pub role: String,
    /// Its name or text.
    pub name: String,
    /// The line as printed.
    pub line: String,
}

fn clip(s: &str, max: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max {
        one
    } else {
        let mut t: String = one.chars().take(max).collect();
        t.push('~');
        t
    }
}

fn is_interactive(role: &str) -> bool {
    INTERACTIVE.contains(&role)
}

/// A link target with the query and fragment cut off, kept short.
fn short_url(u: &str) -> String {
    let base = u.split(['?', '#']).next().unwrap_or(u);
    let base = base
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"))
        .unwrap_or(base);
    clip(base, 70)
}

fn attrs(node: &AxNode) -> String {
    let mut out = String::new();
    if !node.value.is_empty() && is_interactive(&node.role) && node.role != "link" {
        out.push_str(&format!(" value=\"{}\"", clip(&node.value, 60)));
    }
    for (k, v) in &node.props {
        match k.as_str() {
            "url" => out.push_str(&format!(" ->{}", short_url(v))),
            "level" => {}
            "disabled" | "required" | "readonly" | "invalid" | "focused" | "selected"
            | "expanded" | "checked" => {
                if v == "true" {
                    out.push_str(&format!(" {k}"));
                } else if v == "mixed" {
                    out.push_str(&format!(" {k}=mixed"));
                }
            }
            _ => {}
        }
    }
    out
}

struct Walk<'a> {
    by_id: HashMap<&'a str, &'a AxNode>,
    out: Vec<Draft>,
    seen: HashSet<&'a str>,
}

impl<'a> Walk<'a> {
    fn visit(&mut self, id: &'a str, depth: usize, in_named: bool) {
        if !self.seen.insert(id) || depth > 40 {
            return;
        }
        let Some(&node) = self.by_id.get(id) else {
            return;
        };
        let role = node.role.as_str();
        let name = clip(&node.name, 120);
        let mut child_depth = depth;
        let mut child_named = in_named;
        if !node.ignored {
            if is_interactive(role) {
                let shown = if name.is_empty() {
                    clip(&node.value, 60)
                } else {
                    name.clone()
                };
                let body = format!("{role} \"{shown}\"{}", attrs(node));
                self.out.push(Draft {
                    depth,
                    body,
                    target: node.backend.map(|b| (b, role.to_string(), shown)),
                });
                child_depth = depth + 1;
                child_named = true;
            } else if role == "heading" {
                let level = node
                    .props
                    .iter()
                    .find(|(k, _)| k == "level")
                    .map_or("", |(_, v)| v.as_str());
                if !name.is_empty() {
                    self.out.push(Draft {
                        depth,
                        body: format!("h{level} \"{name}\""),
                        target: None,
                    });
                }
                child_named = true;
            } else if LANDMARKS.contains(&role) {
                let label = if name.is_empty() {
                    String::new()
                } else {
                    format!(" \"{name}\"")
                };
                self.out.push(Draft {
                    depth,
                    body: format!("{role}{label}"),
                    target: None,
                });
                child_depth = depth + 1;
            } else if role == "img" || role == "image" {
                if !name.is_empty() {
                    self.out.push(Draft {
                        depth,
                        body: format!("img \"{name}\""),
                        target: None,
                    });
                }
            } else if role == "StaticText" && !in_named && !name.is_empty() {
                self.out.push(Draft {
                    depth,
                    body: format!("\"{}\"", clip(&node.name, 200)),
                    target: None,
                });
            }
        }
        for c in &node.children {
            self.visit(c, child_depth, child_named);
        }
    }
}

/// Flatten `nodes` into lines within `budget` characters, handing out refs in `refs`.
pub fn render(nodes: &[AxNode], refs: &mut Refs, budget: usize) -> Rendered {
    let by_id: HashMap<&str, &AxNode> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let children: HashSet<&str> = nodes
        .iter()
        .flat_map(|n| n.children.iter().map(String::as_str))
        .collect();
    let mut walk = Walk {
        by_id,
        out: Vec::new(),
        seen: HashSet::new(),
    };
    for n in nodes.iter().filter(|n| !children.contains(n.id.as_str())) {
        walk.visit(&n.id, 0, false);
    }
    let total = walk.out.len();
    let mut lines: Vec<String> = Vec::new();
    let mut entries = Vec::new();
    let mut used = 0usize;
    for d in &walk.out {
        let (reference, prefix) = match &d.target {
            Some((backend, role, name)) => {
                let n = refs.assign(*backend, role, name);
                (Some(format!("e{n}")), format!("[e{n}] "))
            }
            None => (None, String::new()),
        };
        let line = format!("{}{prefix}{}", "  ".repeat(d.depth.min(6)), d.body);
        if used + line.len() + 1 > budget {
            break;
        }
        used += line.len() + 1;
        let (role, name) = match &d.target {
            Some((_, r, n)) => (r.clone(), n.clone()),
            None => (String::new(), d.body.clone()),
        };
        entries.push(Entry {
            reference,
            role,
            name,
            line: line.clone(),
        });
        lines.push(line);
    }
    Rendered {
        shown: lines.len(),
        total,
        text: lines.join("\n"),
        entries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A small login-free page: heading, text, a link, a search form with a field and buttons.
    pub fn sample() -> Value {
        json!({"nodes": [
          {"nodeId":"1","ignored":false,"role":{"value":"RootWebArea"},"name":{"value":"Shop"},
           "childIds":["2","3","4","5","20"],"backendDOMNodeId":1},
          {"nodeId":"2","ignored":false,"role":{"value":"heading"},"name":{"value":"Welcome"},
           "properties":[{"name":"level","value":{"type":"integer","value":1}}],
           "childIds":["6"],"backendDOMNodeId":2},
          {"nodeId":"6","ignored":false,"role":{"value":"StaticText"},"name":{"value":"Welcome"},"childIds":[],"backendDOMNodeId":6},
          {"nodeId":"3","ignored":false,"role":{"value":"StaticText"},"name":{"value":"Free shipping over $50."},"childIds":[],"backendDOMNodeId":3},
          {"nodeId":"4","ignored":false,"role":{"value":"link"},"name":{"value":"Deals"},
           "properties":[{"name":"url","value":{"type":"string","value":"https://shop.test/deals?utm=zzz#top"}}],
           "childIds":["7"],"backendDOMNodeId":4},
          {"nodeId":"7","ignored":false,"role":{"value":"StaticText"},"name":{"value":"Deals"},"childIds":[],"backendDOMNodeId":7},
          {"nodeId":"5","ignored":false,"role":{"value":"form"},"name":{"value":""},"childIds":["8","9","10","11"],"backendDOMNodeId":5},
          {"nodeId":"8","ignored":false,"role":{"value":"textbox"},"name":{"value":"Search"},"value":{"value":"shoes"},
           "properties":[{"name":"required","value":{"type":"booleanOrUndefined","value":true}}],"childIds":[],"backendDOMNodeId":8},
          {"nodeId":"9","ignored":false,"role":{"value":"button"},"name":{"value":"Go"},"childIds":[],"backendDOMNodeId":9},
          {"nodeId":"10","ignored":true,"role":{"value":"none"},"childIds":["12"],"backendDOMNodeId":10},
          {"nodeId":"12","ignored":false,"role":{"value":"checkbox"},"name":{"value":"In stock only"},
           "properties":[{"name":"checked","value":{"type":"tristate","value":"true"}}],"childIds":[],"backendDOMNodeId":12},
          {"nodeId":"11","ignored":false,"role":{"value":"generic"},"childIds":[],"backendDOMNodeId":11},
          {"nodeId":"20","ignored":false,"role":{"value":"StaticText"},"name":{"value":"Footer text"},"childIds":[],"backendDOMNodeId":20}
        ]})
    }

    #[test]
    fn the_tree_flattens_to_refs_and_text() {
        let nodes = parse_nodes(&sample());
        let mut refs = Refs::default();
        let r = render(&nodes, &mut refs, 8000);
        let expect = "\
h1 \"Welcome\"
\"Free shipping over $50.\"
link \"Deals\" ->shop.test/deals
form
  [e2] textbox \"Search\" value=\"shoes\" required
  [e3] button \"Go\"
  [e4] checkbox \"In stock only\" checked
\"Footer text\"";
        // the link is e1: the first interactive element read
        let expect = expect.replace("link \"Deals\"", "[e1] link \"Deals\"");
        assert_eq!(r.text, expect);
        assert_eq!(r.shown, r.total);
        assert_eq!(refs.lookup("e3").unwrap().name, "Go");
        assert_eq!(refs.lookup("e3").unwrap().backend, 9);
    }

    #[test]
    fn link_text_children_are_not_repeated() {
        let r = render(&parse_nodes(&sample()), &mut Refs::default(), 8000);
        assert_eq!(r.text.matches("Deals").count(), 1);
        assert_eq!(r.text.matches("Welcome").count(), 1);
    }

    #[test]
    fn refs_are_stable_across_reads_of_one_page() {
        let nodes = parse_nodes(&sample());
        let mut refs = Refs::default();
        let a = render(&nodes, &mut refs, 8000);
        let b = render(&nodes, &mut refs, 8000);
        assert_eq!(a.text, b.text);
        refs.clear();
        assert!(refs.lookup("e1").is_none());
    }

    #[test]
    fn the_budget_cuts_the_list_and_reports_it() {
        let nodes = parse_nodes(&sample());
        let r = render(&nodes, &mut Refs::default(), 80);
        assert!(r.text.len() <= 80, "{}", r.text.len());
        assert!(r.shown < r.total);
        assert_eq!(r.total, 8);
    }

    #[test]
    fn an_empty_answer_renders_nothing() {
        let r = render(&parse_nodes(&json!({})), &mut Refs::default(), 8000);
        assert_eq!(r, Rendered::default());
    }

    #[test]
    fn ref_lookup_accepts_a_bare_number() {
        let mut refs = Refs::default();
        let n = refs.assign(77, "button", "Ok");
        assert_eq!(refs.lookup(&n.to_string()).unwrap().backend, 77);
        assert!(refs.lookup("e999").is_none());
        assert!(refs.lookup("nope").is_none());
    }
}
