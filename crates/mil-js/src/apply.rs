//! Application of a [`JsReport`] back to a real Blitz document.

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use blitz_dom::{BaseDocument, DocumentMutator, LocalName, Namespace, QualName};
use blitz_html::HtmlProvider;

use crate::{
    DomMutation, JsReport,
    mirror::{normalise_property, unescape_html},
};

/// Apply a report to `doc`.
///
/// Mutations that cannot be applied (unknown node id, wrong node kind) are
/// skipped with a `tracing::warn!` instead of failing. Use
/// [`apply_report_with_skipped`] to collect the reasons yourself.
pub fn apply_report(doc: &mut BaseDocument, report: &JsReport) -> Result<()> {
    for reason in apply_report_with_skipped(doc, report) {
        tracing::warn!("mil-js: skipped mutation: {reason}");
    }
    Ok(())
}

/// Apply a report and return one message per skipped mutation.
pub fn apply_report_with_skipped(doc: &mut BaseDocument, report: &JsReport) -> Vec<String> {
    if report.mutations.is_empty() && report.title.is_none() {
        return Vec::new();
    }

    let mut skipped = Vec::new();

    // The dummy provider installed by BaseDocument would silently drop parsed
    // markup, so install a real one only when inserted HTML needs it.
    if report
        .mutations
        .iter()
        .any(|mutation| matches!(mutation, DomMutation::SetHtml { html, .. } if html.contains('<')))
    {
        doc.set_html_parser_provider(Arc::new(HtmlProvider));
    }

    let kinds: HashMap<usize, Kind> = report
        .mutations
        .iter()
        .map(DomMutation::node_id)
        .filter_map(|id| {
            doc.get_node(id).map(|node| {
                let kind = if node.is_element() {
                    Kind::Element
                } else if node.is_text_node() {
                    Kind::Text
                } else {
                    Kind::Other
                };
                (id, kind)
            })
        })
        .collect();

    let title_text_id = doc
        .find_title_node()
        .and_then(|node| node.children.first().copied());

    {
        let mut mutator = doc.mutate();

        if let Some(title) = &report.title {
            match title_text_id {
                Some(id) => mutator.set_node_text(id, title),
                None => skipped.push("document.title: no <title> text node".to_string()),
            }
        }

        for mutation in &report.mutations {
            let id = mutation.node_id();
            let Some(kind) = kinds.get(&id) else {
                skipped.push(format!("{mutation:?}: node {id} does not exist"));
                continue;
            };

            match mutation {
                DomMutation::SetText { text, .. } => match kind {
                    Kind::Text => mutator.set_node_text(id, text),
                    Kind::Element => replace_children_with_text(&mut mutator, id, text),
                    Kind::Other => {
                        skipped.push(format!("{mutation:?}: node {id} is not text-capable"))
                    }
                },
                DomMutation::SetHtml { html, .. } => {
                    if *kind == Kind::Element {
                        if html.contains('<') {
                            mutator.set_inner_html(id, html);
                        } else {
                            // Plain text does not need an HTML parser provider.
                            replace_children_with_text(&mut mutator, id, &unescape_html(html));
                        }
                    } else {
                        skipped.push(format!("{mutation:?}: node {id} is not an element"));
                    }
                }
                DomMutation::SetStyle {
                    property, value, ..
                } => {
                    if *kind == Kind::Element {
                        mutator.set_style_property(id, &normalise_property(property), value);
                    } else {
                        skipped.push(format!("{mutation:?}: node {id} is not an element"));
                    }
                }
                DomMutation::Hide { .. } => {
                    if *kind == Kind::Element {
                        mutator.set_style_property(id, "display", "none");
                    } else {
                        skipped.push(format!("{mutation:?}: node {id} is not an element"));
                    }
                }
                DomMutation::SetAttribute { name, value, .. } => {
                    if *kind == Kind::Element {
                        let name = QualName::new(
                            None,
                            Namespace::default(),
                            LocalName::from(name.as_str()),
                        );
                        mutator.set_attribute(id, name, value);
                    } else {
                        skipped.push(format!("{mutation:?}: node {id} is not an element"));
                    }
                }
            }
        }
    }

    skipped
}

fn replace_children_with_text(mutator: &mut DocumentMutator<'_>, id: usize, text: &str) {
    for child in mutator.child_ids(id) {
        mutator.remove_and_drop_node(child);
    }
    let text_id = mutator.create_text_node(text);
    mutator.append_children(id, &[text_id]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Element,
    Text,
    Other,
}
