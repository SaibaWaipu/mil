//! Plain, serialisable mirror of a parsed document.
//!
//! rquickjs callbacks capture an `Rc<RefCell<JsDomMirror>>` instead of a
//! `&mut BaseDocument`. Every JS write is recorded here as a [`DomMutation`]
//! and later applied by [`crate::apply_report`].

use std::collections::{BTreeMap, HashMap};

use blitz_dom::{BaseDocument, DocumentConfig, Node};
use blitz_html::HtmlDocument;
use rquickjs::Persistent;

use crate::{DomMutation, JsReport};

/// A single node of the mirrored DOM.
///
/// Node ids are the ids of the real Blitz document, so a mutation recorded
/// against a mirror node can be applied to the same node in the live document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorNode {
    /// Blitz node id (also the index in [`JsDomMirror::nodes`]).
    pub id: usize,
    /// Lower-case element name; empty for text and other nodes.
    pub tag: String,
    /// `id` attribute, when present.
    pub id_attr: Option<String>,
    /// Value of the `class` attribute.
    pub class_name: String,
    /// Flattened text content (or the raw text for a text node).
    pub text: String,
    /// All attributes of the element.
    pub attrs: BTreeMap<String, String>,
    /// Declarative inline style, normalised to kebab-case property names.
    pub style: BTreeMap<String, String>,
    /// `false` when `display: none`, `visibility: hidden` or `hidden` is set.
    pub visible: bool,
    /// `href` attribute, when present.
    pub href: Option<String>,
    /// `value` attribute (or the text of a text node).
    pub value: String,
    /// Parent node id, when present.
    pub parent: Option<usize>,
    /// Child node ids in document order.
    pub children: Vec<usize>,
}

impl MirrorNode {
    /// Whether the node is an element that can carry attributes and styles.
    pub fn is_element(&self) -> bool {
        !self.tag.is_empty()
    }

    /// Read an attribute by name.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.get(name).map(String::as_str)
    }

    /// Whether the node matches one of the deliberately small set of supported
    /// selectors: `#id`, `.class`, a bare tag name, `a[href]` and `*`.
    pub fn matches(&self, selector: &str) -> bool {
        let selector = selector.trim();
        if selector.is_empty() || selector == "*" {
            return self.is_element();
        }
        if let Some(id) = selector.strip_prefix('#') {
            return self.id_attr.as_deref() == Some(id);
        }
        if let Some(class) = selector.strip_prefix('.') {
            return self.class_name.split_whitespace().any(|item| item == class);
        }
        if selector.eq_ignore_ascii_case("a[href]") {
            return self.tag.eq_ignore_ascii_case("a") && self.attrs.contains_key("href");
        }
        if let Some((name, _)) = selector.split_once('[') {
            let name = name.trim();
            return name.eq_ignore_ascii_case(&self.tag)
                && selector.ends_with(']')
                && self.attrs.contains_key(
                    selector[name.len() + 1..selector.len() - 1]
                        .trim()
                        .trim_matches(|c| c == '\'' || c == '"'),
                );
        }
        self.tag.eq_ignore_ascii_case(selector)
    }
}

/// Everything the JavaScript façade needs to know about the document.
#[derive(Debug)]
pub struct JsDomMirror {
    /// Nodes indexed by their Blitz node id.
    pub nodes: Vec<MirrorNode>,
    /// Current `document.title`.
    pub title: String,
    /// Current `document.cookie` (never persisted outside this run).
    pub cookie: String,
    /// Current `window.location.href`.
    pub href: String,
    /// Document base URL, used to resolve relative references even after a
    /// script changes `window.location.href`.
    pub base_url: String,
    /// Node id of the `<body>` element, if any.
    pub body_id: Option<usize>,
    /// Accumulated `document.write` output, flushed at the end of a run.
    pub pending_body_html: String,
    /// Mutations, logs and errors collected so far.
    pub report: JsReport,
    /// Scheduled callbacks using a virtual clock.
    pub timers: Vec<Timer>,
    /// `addEventListener` callbacks, keyed by node id and event type.
    pub listeners: HashMap<usize, HashMap<String, Vec<Persistent<rquickjs::Function<'static>>>>>,
}

/// A scheduled `setTimeout` / `setInterval` callback.
#[derive(Debug, Clone)]
pub struct Timer {
    /// Stable id, used to remove fired timeouts.
    pub id: usize,
    /// The saved JavaScript callback.
    pub callback: rquickjs::Persistent<rquickjs::Function<'static>>,
    /// Virtual time at which the callback is next due.
    pub due_ms: u64,
    /// Repeat interval, or `None` for a one-shot timer.
    pub interval_ms: Option<u64>,
}

impl JsDomMirror {
    /// Parse `html` with Blitz and build a mirror whose ids match the real
    /// document built from the same markup.
    pub fn from_html(html: &str, base_url: &str) -> anyhow::Result<Self> {
        let config = DocumentConfig {
            base_url: Some(base_url.to_string()),
            ..DocumentConfig::default()
        };
        let doc = HtmlDocument::from_html(html, config).into_inner();
        Ok(Self::from_document(&doc, base_url))
    }

    /// Build a mirror from an already parsed Blitz document.
    pub fn from_document(doc: &BaseDocument, base_url: &str) -> Self {
        let mut nodes: Vec<MirrorNode> = Vec::with_capacity(doc.tree().len());
        let mut body_id = None;
        for (id, node) in doc.tree().iter() {
            let mirror = mirror_node(id, node);
            if mirror.tag.eq_ignore_ascii_case("body") && body_id.is_none() {
                body_id = Some(id);
            }
            nodes.push(mirror);
        }

        let title = doc
            .find_title_node()
            .map(Node::text_content)
            .unwrap_or_default();

        Self {
            nodes,
            title,
            cookie: String::new(),
            href: base_url.to_string(),
            base_url: base_url.to_string(),
            body_id,
            pending_body_html: String::new(),
            report: JsReport::default(),
            timers: Vec::new(),
            listeners: HashMap::new(),
        }
    }

    /// Borrow a node by id.
    pub fn node(&self, id: usize) -> Option<&MirrorNode> {
        self.nodes.get(id)
    }

    /// Mutably borrow a node by id.
    pub fn node_mut(&mut self, id: usize) -> Option<&mut MirrorNode> {
        self.nodes.get_mut(id)
    }

    /// Find the first element matching a (possibly comma-separated) selector.
    pub fn query_selector(&self, selector: &str) -> Option<usize> {
        self.query_selector_all(selector).into_iter().next()
    }

    /// Find all elements matching a (possibly comma-separated) selector.
    pub fn query_selector_all(&self, selector: &str) -> Vec<usize> {
        let selectors: Vec<&str> = selector
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        self.nodes
            .iter()
            .filter(|node| node.is_element())
            .filter(|node| selectors.iter().any(|sel| node.matches(sel)))
            .map(|node| node.id)
            .collect()
    }

    /// Find an element by its `id` attribute.
    pub fn element_by_id(&self, id: &str) -> Option<usize> {
        self.nodes
            .iter()
            .find(|node| node.id_attr.as_deref() == Some(id))
            .map(|node| node.id)
    }

    /// Resolve a possibly relative URL against the document URL.
    pub fn resolve_url(&self, url: &str) -> String {
        match url::Url::parse(url) {
            Ok(parsed) => parsed.to_string(),
            Err(_) => match url::Url::parse(&self.base_url) {
                Ok(base) => base
                    .join(url)
                    .map(|joined| joined.to_string())
                    .unwrap_or_else(|_| url.to_string()),
                Err(_) => url.to_string(),
            },
        }
    }

    /// Record a mutation, coalescing repeated writes to the same property.
    pub fn record_mutation(&mut self, mutation: DomMutation) {
        let replace = self
            .report
            .mutations
            .iter()
            .position(|existing| same_target(existing, &mutation));
        if let Some(index) = replace {
            self.report.mutations[index] = mutation;
        } else {
            self.report.mutations.push(mutation);
        }
    }

    /// Record a navigation to `url`, resolving relative references.
    pub fn navigate(&mut self, url: &str) {
        let resolved = self.resolve_url(url);
        if !self.report.navigations.contains(&resolved) {
            self.report.navigations.push(resolved.clone());
        }
        self.href = resolved;
    }

    /// Record a console-like log line.
    pub fn log(&mut self, level: &str, message: &str) {
        if level == "log" {
            self.report.logs.push(message.to_string());
        } else {
            self.report.logs.push(format!("{level}: {message}"));
        }
    }

    /// Record a JavaScript error without aborting the page.
    pub fn error(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.report.errors.push(message.clone());
        tracing::warn!("mil-js: {message}");
    }

    /// Flush accumulated `document.write` output into a `SetHtml` mutation.
    pub fn flush_writes(&mut self) {
        if self.pending_body_html.is_empty() {
            return;
        }
        let Some(body_id) = self.body_id else {
            return;
        };
        let html = std::mem::take(&mut self.pending_body_html);
        self.record_mutation(DomMutation::SetHtml {
            node_id: body_id,
            html,
        });
    }
}

fn mirror_node(id: usize, node: &Node) -> MirrorNode {
    let tag = node
        .element_data()
        .map(|element| element.name.local.to_string())
        .unwrap_or_default();
    let attrs: BTreeMap<String, String> = node
        .attrs()
        .map(|attrs| {
            attrs
                .iter()
                .map(|attr| (attr.name.local.to_string(), attr.value.clone()))
                .collect()
        })
        .unwrap_or_default();
    let id_attr = attrs.get("id").cloned();
    let class_name = attrs.get("class").cloned().unwrap_or_default();
    let href = attrs.get("href").cloned();
    let value = attrs
        .get("value")
        .cloned()
        .or_else(|| node.text_data().map(|text| text.content.clone()))
        .unwrap_or_default();
    let text = if tag.is_empty() {
        node.text_data()
            .map(|text| text.content.clone())
            .unwrap_or_default()
    } else {
        node.text_content()
    };
    let style = parse_style(attrs.get("style").map(String::as_str).unwrap_or(""));
    let visible = attrs.contains_key("hidden")
        || style.get("display").is_some_and(|value| value == "none")
        || style
            .get("visibility")
            .is_some_and(|value| value == "hidden");

    MirrorNode {
        id,
        tag,
        id_attr,
        class_name,
        text,
        attrs,
        style,
        visible,
        href,
        value,
        parent: node.parent,
        children: node.children.clone(),
    }
}

/// Parse a declarative `style` attribute into kebab-case properties.
pub fn parse_style(style: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for declaration in style.split(';') {
        if let Some((name, value)) = declaration.split_once(':') {
            let name = normalise_property(name.trim());
            if !name.is_empty() {
                out.insert(name, value.trim().to_string());
            }
        }
    }
    out
}

/// Convert a JS style property name (`backgroundColor`) to CSS
/// (`background-color`) and lower-case it.
pub fn normalise_property(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    for ch in name.chars() {
        if ch.is_uppercase() {
            out.push('-');
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
        } else {
            out.push(ch);
        }
    }
    out.to_ascii_lowercase()
}

/// Escape text for use as HTML text content.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Very small HTML unescape used for `innerHTML` getter/setter round trips.
pub fn unescape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        rest = &rest[index..];
        let Some(end) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        let entity = &rest[1..end];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "39" | "#39" | "apos" => out.push('\''),
            _ => {
                out.push('&');
                out.push_str(entity);
                out.push(';');
            }
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

fn same_target(existing: &DomMutation, new: &DomMutation) -> bool {
    match (existing, new) {
        (DomMutation::SetText { node_id: a, .. }, DomMutation::SetText { node_id: b, .. })
        | (DomMutation::SetHtml { node_id: a, .. }, DomMutation::SetHtml { node_id: b, .. })
        | (DomMutation::Hide { node_id: a }, DomMutation::Hide { node_id: b }) => a == b,
        (
            DomMutation::SetStyle {
                node_id: a,
                property: pa,
                ..
            },
            DomMutation::SetStyle {
                node_id: b,
                property: pb,
                ..
            },
        ) => a == b && pa == pb,
        (
            DomMutation::SetAttribute {
                node_id: a,
                name: na,
                ..
            },
            DomMutation::SetAttribute {
                node_id: b,
                name: nb,
                ..
            },
        ) => a == b && na == nb,
        _ => false,
    }
}
