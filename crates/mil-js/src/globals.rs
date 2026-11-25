//! Installation of the small browser-like JavaScript global surface.
//!
//! Every callback closes over the shared [`JsDomMirror`] rather than over a
//! mutable borrow of the real Blitz document. The context is received as an
//! implicit callback parameter (`FromParam for Ctx`) rather than captured:
//! capturing a `Ctx` inside a JS-owned function would keep the context alive
//! forever and abort QuickJS at runtime teardown.

use std::{cell::RefCell, rc::Rc};

use rquickjs::{
    CaughtError, Ctx, Function, Object, Persistent, Undefined, Value, function::Rest,
    object::Accessor,
};

use crate::{
    DomMutation, JsDomMirror,
    mirror::{escape_html, normalise_property, unescape_html},
};

type Shared = Rc<RefCell<JsDomMirror>>;

/// Install `document`, `window`, `console`, `navigator` and timer globals.
pub fn install(ctx: &Ctx<'_>, state: &Shared) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    let document = document_object(ctx, state)?;
    globals.set("document", document.clone())?;

    let window = window_object(ctx, state, &document)?;
    globals.set("window", window)?;

    globals.set("console", console_object(ctx, state)?)?;
    globals.set("navigator", navigator_object(ctx)?)?;
    globals.set("alert", alert_function(ctx, state)?)?;
    globals.set("setTimeout", timer_function(ctx, state, false)?)?;
    globals.set("setInterval", timer_function(ctx, state, true)?)?;
    globals.set("fetch", fetch_function(ctx, state)?)?;

    Ok(())
}

fn document_object<'js>(ctx: &Ctx<'js>, state: &Shared) -> rquickjs::Result<Object<'js>> {
    let document = Object::new(ctx.clone())?;

    let get_state = state.clone();
    let set_state = state.clone();
    document.prop(
        "title",
        Accessor::new(
            move || -> String { get_state.borrow().title.clone() },
            move |value: rquickjs::Coerced<String>| {
                let mut mirror = set_state.borrow_mut();
                mirror.title = value.0.clone();
                mirror.report.title = Some(value.0);
            },
        ),
    )?;

    let get_state = state.clone();
    let set_state = state.clone();
    document.prop(
        "cookie",
        Accessor::new(
            move || -> String { get_state.borrow().cookie.clone() },
            move |value: rquickjs::Coerced<String>| {
                set_state.borrow_mut().cookie = value.0;
            },
        ),
    )?;

    let write_state = state.clone();
    document.set(
        "write",
        Function::new(
            ctx.clone(),
            move |parts: Rest<rquickjs::Coerced<String>>| {
                let text = parts
                    .0
                    .into_iter()
                    .map(|part| part.0)
                    .collect::<Vec<_>>()
                    .join("");
                write_state.borrow_mut().pending_body_html.push_str(&text);
            },
        )?,
    )?;

    let by_id_state = state.clone();
    document.set(
        "getElementById",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, id: String| -> Option<Object<'js>> {
                let node_id = by_id_state.borrow().element_by_id(&id);
                node_id.and_then(|id| element_object(&ctx, &by_id_state, id).ok().flatten())
            },
        )?,
    )?;

    let query_state = state.clone();
    document.set(
        "querySelector",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, selector: String| -> Option<Object<'js>> {
                let node_id = query_state.borrow().query_selector(&selector);
                node_id.and_then(|id| element_object(&ctx, &query_state, id).ok().flatten())
            },
        )?,
    )?;

    let query_all_state = state.clone();
    document.set(
        "querySelectorAll",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, selector: String| -> rquickjs::Result<Object<'js>> {
                let ids = query_all_state.borrow().query_selector_all(&selector);
                node_list(&ctx, &query_all_state, ids)
            },
        )?,
    )?;

    Ok(document)
}

fn window_object<'js>(
    ctx: &Ctx<'js>,
    state: &Shared,
    document: &Object<'js>,
) -> rquickjs::Result<Object<'js>> {
    let window = Object::new(ctx.clone())?;
    window.set("document", document.clone())?;

    let location = Object::new(ctx.clone())?;
    let get_state = state.clone();
    let set_state = state.clone();
    location.prop(
        "href",
        Accessor::new(
            move || -> String { get_state.borrow().href.clone() },
            move |value: rquickjs::Coerced<String>| {
                set_state.borrow_mut().navigate(&value.0);
            },
        ),
    )?;
    let assign_state = state.clone();
    location.set(
        "assign",
        Function::new(ctx.clone(), move |url: rquickjs::Coerced<String>| {
            assign_state.borrow_mut().navigate(&url.0);
        })?,
    )?;
    window.set("location", location.clone())?;

    ctx.globals().set("location", location)?;

    window.set("alert", alert_function(ctx, state)?)?;
    window.set("setTimeout", timer_function(ctx, state, false)?)?;
    window.set("setInterval", timer_function(ctx, state, true)?)?;

    Ok(window)
}

fn console_object<'js>(ctx: &Ctx<'js>, state: &Shared) -> rquickjs::Result<Object<'js>> {
    let console = Object::new(ctx.clone())?;
    for level in ["log", "warn", "error"] {
        let log_state = state.clone();
        let level_name = level.to_string();
        console.set(
            level,
            Function::new(
                ctx.clone(),
                move |parts: Rest<rquickjs::Coerced<String>>| {
                    let message = parts
                        .0
                        .into_iter()
                        .map(|part| part.0)
                        .collect::<Vec<_>>()
                        .join(" ");
                    log_state.borrow_mut().log(&level_name, &message);
                },
            )?,
        )?;
    }
    Ok(console)
}

fn navigator_object<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let navigator = Object::new(ctx.clone())?;
    navigator.prop(
        "userAgent",
        Accessor::from(|| -> String {
            "Mozilla/5.0 (compatible; Mil/0.1; terminal browser)".to_string()
        }),
    )?;
    Ok(navigator)
}

fn alert_function<'js>(ctx: &Ctx<'js>, state: &Shared) -> rquickjs::Result<Function<'js>> {
    let alert_state = state.clone();
    Function::new(
        ctx.clone(),
        move |parts: Rest<rquickjs::Coerced<String>>| {
            let message = parts
                .0
                .into_iter()
                .map(|part| part.0)
                .collect::<Vec<_>>()
                .join(" ");
            alert_state.borrow_mut().log("alert", &message);
        },
    )
}

fn timer_function<'js>(
    ctx: &Ctx<'js>,
    state: &Shared,
    interval: bool,
) -> rquickjs::Result<Function<'js>> {
    let timer_state = state.clone();
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, callback: Function<'js>, delay: rquickjs::function::Opt<f64>| -> u32 {
            let delay = delay.0.unwrap_or(0.0);
            let delay_ms = if delay.is_finite() && delay > 0.0 {
                delay as u64
            } else {
                0
            };
            let saved = Persistent::save(&ctx, callback);
            let mut mirror = timer_state.borrow_mut();
            let id = mirror.timers.len() + 1;
            mirror.timers.push(crate::mirror::Timer {
                id,
                callback: saved,
                due_ms: delay_ms,
                interval_ms: if interval {
                    Some(delay_ms.max(1))
                } else {
                    None
                },
            });
            id as u32
        },
    )
}

fn fetch_function<'js>(ctx: &Ctx<'js>, state: &Shared) -> rquickjs::Result<Function<'js>> {
    let fetch_state = state.clone();
    Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, url: rquickjs::Coerced<String>| {
            fetch_state.borrow_mut().navigate(&url.0);
            let placeholder = Object::new(ctx.clone())?;
            let noop = Function::new(ctx, || -> Undefined { Undefined })?;
            placeholder.set("then", noop.clone())?;
            placeholder.set("catch", noop)?;
            Ok::<Object<'js>, rquickjs::Error>(placeholder)
        },
    )
}

fn node_list<'js>(
    ctx: &Ctx<'js>,
    state: &Shared,
    ids: Vec<usize>,
) -> rquickjs::Result<Object<'js>> {
    let list = Object::new(ctx.clone())?;
    list.set("length", ids.len() as u32)?;
    for (index, id) in ids.iter().enumerate() {
        if let Some(element) = element_object(ctx, state, *id)? {
            list.set(index as u32, element)?;
        }
    }

    let for_each_state = state.clone();
    let for_each_ids = ids.clone();
    list.set(
        "forEach",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, callback: Function<'js>| -> rquickjs::Result<()> {
                for id in &for_each_ids {
                    let Some(element) = element_object(&ctx, &for_each_state, *id)? else {
                        continue;
                    };
                    let _: Value = callback.call((element,))?;
                }
                Ok(())
            },
        )?,
    )?;

    Ok(list)
}

/// Build the JS façade for a mirrored element.
pub fn element_object<'js>(
    ctx: &Ctx<'js>,
    state: &Shared,
    id: usize,
) -> rquickjs::Result<Option<Object<'js>>> {
    if !state
        .borrow()
        .node(id)
        .is_some_and(|node| node.is_element())
    {
        return Ok(None);
    }
    let element = Object::new(ctx.clone())?;

    let get_text = state.clone();
    let set_text = state.clone();
    element.prop(
        "textContent",
        Accessor::new(
            move || -> String {
                get_text
                    .borrow()
                    .node(id)
                    .map(|n| n.text.clone())
                    .unwrap_or_default()
            },
            move |value: rquickjs::Coerced<String>| {
                let mut mirror = set_text.borrow_mut();
                if mirror.node(id).is_none() {
                    return;
                }
                if let Some(node) = mirror.node_mut(id) {
                    node.text = value.0.clone();
                    node.children.clear();
                }
                mirror.record_mutation(DomMutation::SetText {
                    node_id: id,
                    text: value.0,
                });
            },
        ),
    )?;

    let get_html = state.clone();
    let set_html = state.clone();
    element.prop(
        "innerHTML",
        Accessor::new(
            move || -> String {
                get_html
                    .borrow()
                    .node(id)
                    .map(|n| escape_html(&n.text))
                    .unwrap_or_default()
            },
            move |value: rquickjs::Coerced<String>| {
                let text = unescape_html(&value.0);
                let mut mirror = set_html.borrow_mut();
                if mirror.node(id).is_none() {
                    return;
                }
                if let Some(node) = mirror.node_mut(id) {
                    node.text = text;
                    node.children.clear();
                }
                mirror.record_mutation(DomMutation::SetHtml {
                    node_id: id,
                    html: value.0,
                });
            },
        ),
    )?;

    element.set("style", style_object(ctx, state, id)?)?;
    element.set("classList", class_list_object(ctx, state, id)?)?;

    let get_attribute_state = state.clone();
    element.set(
        "getAttribute",
        Function::new(
            ctx.clone(),
            move |name: rquickjs::Coerced<String>| -> Option<String> {
                get_attribute_state
                    .borrow()
                    .node(id)
                    .and_then(|node| node.attr(&name.0))
                    .map(str::to_string)
            },
        )?,
    )?;

    let set_attribute_state = state.clone();
    element.set(
        "setAttribute",
        Function::new(
            ctx.clone(),
            move |name: rquickjs::Coerced<String>, value: rquickjs::Coerced<String>| {
                set_attribute(id, &set_attribute_state, &name.0, &value.0);
            },
        )?,
    )?;

    let get_value = state.clone();
    let set_value = state.clone();
    element.prop(
        "value",
        Accessor::new(
            move || -> String {
                get_value
                    .borrow()
                    .node(id)
                    .map(|n| n.value.clone())
                    .unwrap_or_default()
            },
            move |value: rquickjs::Coerced<String>| {
                set_attribute(id, &set_value, "value", &value.0);
            },
        ),
    )?;

    let add_listener_state = state.clone();
    element.set(
        "addEventListener",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  event_type: rquickjs::Coerced<String>,
                  callback: Function<'js>|
                  -> rquickjs::Result<()> {
                let saved = Persistent::save(&ctx, callback);
                let mut mirror = add_listener_state.borrow_mut();
                mirror
                    .listeners
                    .entry(id)
                    .or_default()
                    .entry(event_type.0)
                    .or_default()
                    .push(saved);
                Ok(())
            },
        )?,
    )?;

    let click_state = state.clone();
    element.set(
        "click",
        Function::new(ctx.clone(), move |ctx: Ctx<'js>| -> rquickjs::Result<()> {
            let href = click_state
                .borrow()
                .node(id)
                .and_then(|node| node.href.clone());
            if let Some(href) = href {
                click_state.borrow_mut().navigate(&href);
            }
            invoke_listeners(&ctx, &click_state, id, "click")
        })?,
    )?;

    Ok(Some(element))
}

fn style_object<'js>(ctx: &Ctx<'js>, state: &Shared, id: usize) -> rquickjs::Result<Object<'js>> {
    let style = Object::new(ctx.clone())?;
    for property in [
        "display",
        "color",
        "backgroundColor",
        "background-color",
        "visibility",
        "width",
        "height",
        "opacity",
    ] {
        let get_state = state.clone();
        let set_state = state.clone();
        let key = normalise_property(property);
        let get_key = key.clone();
        style.prop(
            property,
            Accessor::new(
                move || -> String {
                    get_state
                        .borrow()
                        .node(id)
                        .and_then(|node| node.style.get(&get_key))
                        .cloned()
                        .unwrap_or_default()
                },
                move |value: rquickjs::Coerced<String>| {
                    set_style(id, &set_state, &key, &value.0);
                },
            ),
        )?;
    }

    let set_property_state = state.clone();
    style.set(
        "setProperty",
        Function::new(
            ctx.clone(),
            move |property: rquickjs::Coerced<String>, value: rquickjs::Coerced<String>| {
                set_style(id, &set_property_state, &property.0, &value.0);
            },
        )?,
    )?;

    let get_property_state = state.clone();
    style.set(
        "getPropertyValue",
        Function::new(
            ctx.clone(),
            move |property: rquickjs::Coerced<String>| -> String {
                let key = normalise_property(&property.0);
                get_property_state
                    .borrow()
                    .node(id)
                    .and_then(|node| node.style.get(&key))
                    .cloned()
                    .unwrap_or_default()
            },
        )?,
    )?;

    Ok(style)
}

fn class_list_object<'js>(
    ctx: &Ctx<'js>,
    state: &Shared,
    id: usize,
) -> rquickjs::Result<Object<'js>> {
    let list = Object::new(ctx.clone())?;

    let add_state = state.clone();
    list.set(
        "add",
        Function::new(ctx.clone(), move |class: rquickjs::Coerced<String>| {
            let mut classes = current_classes(&add_state, id);
            if !classes.iter().any(|item| item == &class.0) {
                classes.push(class.0);
                set_attribute(id, &add_state, "class", &classes.join(" "));
            }
        })?,
    )?;

    let remove_state = state.clone();
    list.set(
        "remove",
        Function::new(ctx.clone(), move |class: rquickjs::Coerced<String>| {
            let mut classes = current_classes(&remove_state, id);
            classes.retain(|item| item != &class.0);
            set_attribute(id, &remove_state, "class", &classes.join(" "));
        })?,
    )?;

    let contains_state = state.clone();
    list.set(
        "contains",
        Function::new(
            ctx.clone(),
            move |class: rquickjs::Coerced<String>| -> bool {
                current_classes(&contains_state, id)
                    .iter()
                    .any(|item| item == &class.0)
            },
        )?,
    )?;

    Ok(list)
}

fn current_classes(state: &Shared, id: usize) -> Vec<String> {
    state
        .borrow()
        .node(id)
        .map(|node| {
            node.class_name
                .split_whitespace()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn set_attribute(id: usize, state: &Shared, name: &str, value: &str) {
    let mut mirror = state.borrow_mut();
    if mirror.node(id).is_none() {
        return;
    }
    if let Some(node) = mirror.node_mut(id) {
        node.attrs.insert(name.to_string(), value.to_string());
        match name {
            "id" => node.id_attr = Some(value.to_string()),
            "class" => node.class_name = value.to_string(),
            "href" => node.href = Some(value.to_string()),
            _ => {}
        }
    }
    mirror.record_mutation(DomMutation::SetAttribute {
        node_id: id,
        name: name.to_string(),
        value: value.to_string(),
    });
}

fn set_style(id: usize, state: &Shared, property: &str, value: &str) {
    let key = normalise_property(property);
    let mut mirror = state.borrow_mut();
    if mirror.node(id).is_none() {
        return;
    }
    if let Some(node) = mirror.node_mut(id) {
        node.style.insert(key.clone(), value.to_string());
        let display_none = node.style.get("display").is_some_and(|v| v == "none");
        let visibility_hidden = node.style.get("visibility").is_some_and(|v| v == "hidden");
        node.visible = !node.attrs.contains_key("hidden") && !display_none && !visibility_hidden;
    }
    mirror.record_mutation(DomMutation::SetStyle {
        node_id: id,
        property: key,
        value: value.to_string(),
    });
}

fn invoke_listeners<'js>(
    ctx: &Ctx<'js>,
    state: &Shared,
    id: usize,
    event_type: &str,
) -> rquickjs::Result<()> {
    let callbacks = {
        let mut mirror = state.borrow_mut();
        mirror
            .listeners
            .get_mut(&id)
            .and_then(|events| events.get_mut(event_type))
            .map(std::mem::take)
            .unwrap_or_default()
    };

    for saved in callbacks {
        let callback = match saved.clone().restore(ctx) {
            Ok(callback) => callback,
            Err(error) => {
                state
                    .borrow_mut()
                    .error(format!("listener restore: {error}"));
                continue;
            }
        };
        let event = Object::new(ctx.clone())?;
        event.set("type", event_type)?;
        if let Err(error) = callback.call::<_, Value>((event,)) {
            let message = CaughtError::from_error(ctx, error).to_string();
            state.borrow_mut().error(message);
        }
        // Keep the listener registered for later clicks.
        if let Some(events) = state
            .borrow_mut()
            .listeners
            .get_mut(&id)
            .and_then(|events| events.get_mut(event_type))
        {
            events.push(saved);
        }
    }
    Ok(())
}
