//! HTML form discovery, mutation and successful-control serialization.
//!
//! Blitz already owns the live state of text inputs and checkboxes (in
//! `ElementData::special_data`), so this module reads that state when it is
//! available and falls back to the HTML attributes before layout has run.

use std::collections::HashMap;

use blitz_dom::{BaseDocument, DocumentMutator, ElementData, Node, local_name, qual_name};
use url::Url;

use crate::Page;

/// The purpose of a form control, derived from its tag and `type` attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Password,
    Email,
    Number,
    Search,
    Tel,
    Url,
    Hidden,
    Checkbox,
    Radio,
    Textarea,
    Select,
    Submit,
    Button,
    Other,
}

/// A single control associated with a `<form>`.
#[derive(Debug, Clone)]
pub struct FormField {
    /// Id of the control's element in the Blitz DOM.
    pub node_id: usize,
    /// Value of the `name` attribute, or an empty string.
    pub name: String,
    /// Current value of the control (live editor text when available).
    pub value: String,
    /// Kind of control.
    pub kind: FieldKind,
    /// Whether a checkbox/radio is checked; `false` for other controls.
    pub checked: bool,
    /// Whether the control has a `disabled` attribute.
    pub disabled: bool,
}

/// A `<form>` and the controls that belong to it.
#[derive(Debug, Clone)]
pub struct FormInfo {
    /// Id of the `<form>` element in the Blitz DOM.
    pub node_id: usize,
    /// Resolved `action` (the page URL when the attribute is absent or empty).
    pub action: Url,
    /// Lowercased `method`: `"get"` (the default) or `"post"`.
    pub method: String,
    /// `enctype` attribute or `application/x-www-form-urlencoded`.
    pub enctype: String,
    /// Controls in tree order, including submit buttons.
    pub fields: Vec<FormField>,
}

impl Page {
    /// All forms in the document, with their associated controls.
    pub fn forms(&self) -> Vec<FormInfo> {
        let doc = &self.doc;
        let form_ids = doc.query_selector_all("form").unwrap_or_default();
        let control_ids = doc
            .query_selector_all("input, textarea, select, button")
            .unwrap_or_default();

        let mut by_form: HashMap<usize, Vec<FormField>> = HashMap::new();
        for id in control_ids {
            let Some(owner) = form_owner(doc, id) else {
                continue;
            };
            let Some(node) = doc.get_node(id) else {
                continue;
            };
            if let Some(field) = form_field(node) {
                by_form.entry(owner).or_default().push(field);
            }
        }

        form_ids
            .iter()
            .filter_map(|&id| {
                let element = doc.get_node(id)?.element_data()?;
                let raw_action = element.attr(local_name!("action")).unwrap_or_default();
                let action = if raw_action.trim().is_empty() {
                    self.url.clone()
                } else {
                    self.url
                        .join(raw_action)
                        .unwrap_or_else(|_| self.url.clone())
                };
                let method = match element
                    .attr(local_name!("method"))
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "post" => "post",
                    _ => "get",
                };
                let enctype = element
                    .attr(local_name!("enctype"))
                    .filter(|value| !value.is_empty())
                    .unwrap_or("application/x-www-form-urlencoded");

                Some(FormInfo {
                    node_id: id,
                    action,
                    method: method.to_owned(),
                    enctype: enctype.to_owned(),
                    fields: by_form.remove(&id).unwrap_or_default(),
                })
            })
            .collect()
    }

    /// The name/value pairs of the successful controls of `form_node_id`, in
    /// submission (tree) order. Values are not percent-encoded; use
    /// [`Page::serialize_form_encoded`] for an `application/x-www-form-urlencoded`
    /// string.
    pub fn serialize_form(
        &self,
        form_node_id: usize,
        submitter_node_id: Option<usize>,
    ) -> Vec<(String, String)> {
        let Some(form) = self
            .forms()
            .into_iter()
            .find(|form| form.node_id == form_node_id)
        else {
            return Vec::new();
        };

        let mut pairs = Vec::new();
        for field in form.fields {
            if field.disabled || field.name.is_empty() {
                continue;
            }
            match field.kind {
                FieldKind::Checkbox | FieldKind::Radio if !field.checked => continue,
                // Only a submit button contributes its own name/value when it
                // is the submitter; ordinary buttons never do.
                FieldKind::Button => continue,
                FieldKind::Submit if Some(field.node_id) != submitter_node_id => continue,
                _ => {}
            }
            pairs.push((
                normalize_line_endings(&field.name),
                normalize_line_endings(&field.value),
            ));
        }
        pairs
    }

    /// Like [`Page::serialize_form`], but returns the escaped query/body string.
    pub fn serialize_form_encoded(
        &self,
        form_node_id: usize,
        submitter_node_id: Option<usize>,
    ) -> String {
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(self.serialize_form(form_node_id, submitter_node_id))
            .finish()
    }

    /// Set the current value of an input, textarea or select. Returns `false`
    /// for other nodes. Blitz's mutator keeps the `value` attribute and the
    /// live text editor in sync.
    pub fn set_field_value(&mut self, node_id: usize, value: &str) -> bool {
        let is_field = self
            .doc
            .get_node(node_id)
            .and_then(Node::element_data)
            .is_some_and(|element| {
                [
                    local_name!("input"),
                    local_name!("textarea"),
                    local_name!("select"),
                ]
                .contains(&element.name.local)
            });
        if !is_field {
            return false;
        }
        let mut mutator = DocumentMutator::new(&mut self.doc);
        mutator.set_attribute(node_id, qual_name!("value"), value);
        true
    }

    /// Toggle a checkbox or radio using Blitz's own checked-state logic.
    /// Returns the new checked state, or `false` for other nodes.
    pub fn toggle_field(&mut self, node_id: usize) -> bool {
        let Some((kind, name)) =
            self.doc
                .get_node(node_id)
                .and_then(Node::element_data)
                .map(|element| {
                    (
                        element_kind(element),
                        element.attr(local_name!("name")).map(str::to_owned),
                    )
                })
        else {
            return false;
        };
        let Some(kind) = kind else {
            return false;
        };

        match kind {
            FieldKind::Checkbox => {
                ensure_checkbox_state(&mut self.doc, node_id);
                let Some(element) = self
                    .doc
                    .get_node_mut(node_id)
                    .and_then(Node::element_data_mut)
                else {
                    return false;
                };
                BaseDocument::toggle_checkbox(element)
            }
            FieldKind::Radio => {
                ensure_checkbox_state(&mut self.doc, node_id);
                if let Some(name) = name {
                    self.doc.toggle_radio(name, node_id);
                } else {
                    let Some(element) = self
                        .doc
                        .get_node_mut(node_id)
                        .and_then(Node::element_data_mut)
                    else {
                        return false;
                    };
                    BaseDocument::toggle_checkbox(element);
                }
                self.doc
                    .get_node(node_id)
                    .and_then(Node::element_data)
                    .and_then(ElementData::checkbox_input_checked)
                    .unwrap_or(false)
            }
            _ => false,
        }
    }
}

/// The form that owns a control: an explicit `form="id"` reference, otherwise
/// the nearest ancestor `<form>`.
fn form_owner(doc: &BaseDocument, id: usize) -> Option<usize> {
    let node = doc.get_node(id)?;
    let element = node.element_data()?;

    if let Some(owner_id) = element.attr(local_name!("form")) {
        let owner = doc.get_element_by_id(owner_id)?;
        return doc
            .get_node(owner)
            .and_then(Node::element_data)
            .filter(|element| element.name.local == local_name!("form"))
            .map(|_| owner);
    }

    let mut parent = node.parent;
    while let Some(id) = parent {
        let ancestor = doc.get_node(id)?;
        if ancestor
            .element_data()
            .is_some_and(|element| element.name.local == local_name!("form"))
        {
            return Some(id);
        }
        parent = ancestor.parent;
    }
    None
}

fn element_kind(element: &ElementData) -> Option<FieldKind> {
    if element.name.local == local_name!("textarea") {
        return Some(FieldKind::Textarea);
    }
    if element.name.local == local_name!("select") {
        return Some(FieldKind::Select);
    }
    if element.name.local == local_name!("button") {
        return Some(
            match element
                .attr(local_name!("type"))
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str()
            {
                "" | "submit" => FieldKind::Submit,
                _ => FieldKind::Button,
            },
        );
    }
    if element.name.local != local_name!("input") {
        return None;
    }

    Some(
        match element
            .attr(local_name!("type"))
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "text" => FieldKind::Text,
            "password" => FieldKind::Password,
            "email" => FieldKind::Email,
            "number" => FieldKind::Number,
            "search" => FieldKind::Search,
            "tel" => FieldKind::Tel,
            "url" => FieldKind::Url,
            "hidden" => FieldKind::Hidden,
            "checkbox" => FieldKind::Checkbox,
            "radio" => FieldKind::Radio,
            "submit" | "image" => FieldKind::Submit,
            "button" | "reset" => FieldKind::Button,
            _ => FieldKind::Other,
        },
    )
}

fn form_field(node: &Node) -> Option<FormField> {
    let element = node.element_data()?;
    let kind = element_kind(element)?;
    let checked = matches!(kind, FieldKind::Checkbox | FieldKind::Radio)
        && element
            .checkbox_input_checked()
            .unwrap_or_else(|| element.has_attr(local_name!("checked")));
    Some(FormField {
        node_id: node.id,
        name: element
            .attr(local_name!("name"))
            .unwrap_or_default()
            .to_owned(),
        value: field_value(node, element, &kind),
        kind,
        checked,
        disabled: element.has_attr(local_name!("disabled")),
    })
}

/// Live editor text takes precedence over the `value` attribute. Blitz seeds
/// an editor with a single space for inputs that have no `value`, so that
/// placeholder is treated as an empty value.
fn field_value(node: &Node, element: &ElementData, kind: &FieldKind) -> String {
    let attr = element.attr(local_name!("value"));

    if matches!(
        kind,
        FieldKind::Text
            | FieldKind::Password
            | FieldKind::Email
            | FieldKind::Number
            | FieldKind::Search
            | FieldKind::Tel
            | FieldKind::Url
            | FieldKind::Textarea
    ) {
        if let Some(editor) = element.text_input_data() {
            let text = editor.editor.text();
            if !(text == " " && attr.is_none()) {
                return text.to_string();
            }
        }
        if let Some(value) = attr {
            return value.to_owned();
        }
        if *kind == FieldKind::Textarea {
            let text = node.text_content();
            return text.strip_prefix('\n').unwrap_or(&text).to_owned();
        }
        return String::new();
    }

    if *kind == FieldKind::Select {
        if let Some(value) = attr {
            return value.to_owned();
        }
        return selected_option_value(node);
    }

    attr.unwrap_or_default().to_owned()
}

fn selected_option_value(node: &Node) -> String {
    let mut first = None;
    for id in &node.children {
        let Some(child) = node.tree().get(*id) else {
            continue;
        };
        let Some(option) = child.element_data() else {
            continue;
        };
        if option.name.local != local_name!("option") {
            continue;
        }
        let value = option
            .attr(local_name!("value"))
            .map(str::to_owned)
            .unwrap_or_else(|| child.text_content());
        if option.has_attr(local_name!("selected")) {
            return value;
        }
        if first.is_none() {
            first = Some(value);
        }
    }
    first.unwrap_or_default()
}

/// Blitz only creates `CheckboxInput` state during layout. Seed it from the
/// `checked` attribute so toggling works even before `Page::resolve`.
fn ensure_checkbox_state(doc: &mut BaseDocument, id: usize) {
    let Some(element) = doc.get_node_mut(id).and_then(Node::element_data_mut) else {
        return;
    };
    if element.checkbox_input_checked().is_none() {
        let checked = element.has_attr(local_name!("checked"));
        element.special_data = blitz_dom::node::SpecialElementData::CheckboxInput(checked);
    }
}

fn normalize_line_endings(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match (ch, chars.peek()) {
            ('\r', Some('\n')) => {
                out.push_str("\r\n");
                chars.next();
            }
            ('\r' | '\n', _) => out.push_str("\r\n"),
            _ => out.push(ch),
        }
    }
    out
}
