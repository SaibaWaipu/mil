use mil_dom::{ColorScheme, FieldKind, Page, Viewport};
use url::Url;

fn page(html: &str) -> Page {
    let url = Url::parse("https://example.com/dir/index.html").unwrap();
    let viewport = Viewport::new(800, 600, 1.0, ColorScheme::Light);
    Page::from_html(html, &url, viewport).unwrap()
}

fn id(page: &Page, id: &str) -> usize {
    page.doc.get_element_by_id(id).expect("missing test node")
}

fn strs(pairs: &[(String, String)]) -> Vec<(&str, &str)> {
    pairs
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

#[test]
fn get_form_serializes_successful_controls_in_tree_order() {
    let mut page = page(
        r#"
        <form id="f" action="/submit" method="GET">
          <input id="q" type="text" name="q" value="hello world">
          <input type="hidden" name="token" value="abc">
          <input id="cb" type="checkbox" name="opt" value="yes">
          <input id="first" type="submit" name="go" value="first">
          <input id="second" type="submit" name="go" value="second">
        </form>
        "#,
    );
    page.resolve(0.0);

    let forms = page.forms();
    assert_eq!(forms.len(), 1);
    let form = &forms[0];
    assert_eq!(form.action.as_str(), "https://example.com/submit");
    assert_eq!(form.method, "get");
    assert_eq!(form.enctype, "application/x-www-form-urlencoded");
    assert_eq!(form.fields.len(), 5);
    assert_eq!(form.fields[0].kind, FieldKind::Text);
    assert_eq!(form.fields[0].value, "hello world");
    assert_eq!(form.fields[1].kind, FieldKind::Hidden);
    assert_eq!(form.fields[2].kind, FieldKind::Checkbox);
    assert!(!form.fields[2].checked);
    assert_eq!(form.fields[3].kind, FieldKind::Submit);
    assert_eq!(form.fields[4].kind, FieldKind::Submit);

    assert_eq!(
        strs(&page.serialize_form(form.node_id, Some(id(&page, "first")))),
        [("q", "hello world"), ("token", "abc"), ("go", "first")]
    );
    assert_eq!(
        strs(&page.serialize_form(form.node_id, Some(id(&page, "second")))),
        [("q", "hello world"), ("token", "abc"), ("go", "second")]
    );
    assert_eq!(
        strs(&page.serialize_form(form.node_id, None)),
        [("q", "hello world"), ("token", "abc")]
    );
    assert_eq!(
        page.serialize_form_encoded(form.node_id, Some(id(&page, "first"))),
        "q=hello+world&token=abc&go=first"
    );
}

#[test]
fn post_form_uses_enctype_and_live_field_values() {
    let mut page = page(
        r#"
        <form id="f" method="POST" action="submit" enctype="multipart/form-data">
          <input id="email" type="email" name="email" value="old@example.com">
          <textarea id="bio" name="bio">hello</textarea>
        </form>
        "#,
    );
    page.resolve(0.0);

    let forms = page.forms();
    assert_eq!(forms.len(), 1);
    assert_eq!(forms[0].action.as_str(), "https://example.com/dir/submit");
    assert_eq!(forms[0].method, "post");
    assert_eq!(forms[0].enctype, "multipart/form-data");

    let form_id = forms[0].node_id;
    let email = id(&page, "email");
    assert!(page.set_field_value(email, "new@example.com"));
    assert_eq!(page.forms()[0].fields[0].value, "new@example.com");
    assert_eq!(
        strs(&page.serialize_form(form_id, None)),
        [("email", "new@example.com"), ("bio", "hello")]
    );
    assert_eq!(
        page.serialize_form_encoded(form_id, None),
        "email=new%40example.com&bio=hello"
    );

    assert!(!page.set_field_value(id(&page, "f"), "nope"));
}

#[test]
fn toggling_a_checkbox_changes_serialization() {
    let mut page = page(
        r#"
        <form id="f">
          <input id="cb" type="checkbox" name="opt" value="yes">
        </form>
        "#,
    );
    page.resolve(0.0);
    let form_id = page.forms()[0].node_id;
    let cb = id(&page, "cb");

    assert!(page.serialize_form(form_id, None).is_empty());
    assert!(page.toggle_field(cb));
    assert!(page.forms()[0].fields[0].checked);
    assert_eq!(strs(&page.serialize_form(form_id, None)), [("opt", "yes")]);
    assert!(!page.toggle_field(cb));
    assert!(page.serialize_form(form_id, None).is_empty());
    assert!(!page.toggle_field(form_id));
}

#[test]
fn toggling_a_radio_unchecks_its_siblings() {
    let mut page = page(
        r#"
        <form id="f">
          <input id="a" type="radio" name="pick" value="one" checked>
          <input id="b" type="radio" name="pick" value="two">
        </form>
        "#,
    );
    page.resolve(0.0);
    let form_id = page.forms()[0].node_id;

    assert_eq!(strs(&page.serialize_form(form_id, None)), [("pick", "one")]);
    assert!(page.toggle_field(id(&page, "b")));
    let fields = &page.forms()[0].fields;
    assert!(!fields[0].checked);
    assert!(fields[1].checked);
    assert_eq!(strs(&page.serialize_form(form_id, None)), [("pick", "two")]);
}

#[test]
fn excludes_fields_outside_the_form_disabled_and_unnamed() {
    let page = page(
        r#"
        <form id="f">
          <input id="keep" name="keep" value="yes">
          <input id="disabled" name="disabled" value="no" disabled>
          <input id="unnamed" value="no">
          <button id="reset" type="reset" name="reset" value="no">Reset</button>
        </form>
        <input id="outside" name="outside" value="no">
        <form id="other"><input id="other-field" name="other" value="no"></form>
        "#,
    );

    let forms = page.forms();
    assert_eq!(forms.len(), 2);
    let form = &forms[0];
    let ids: Vec<usize> = form.fields.iter().map(|field| field.node_id).collect();
    assert_eq!(
        ids,
        [
            id(&page, "keep"),
            id(&page, "disabled"),
            id(&page, "unnamed"),
            id(&page, "reset")
        ]
    );

    assert_eq!(
        strs(&page.serialize_form(form.node_id, None)),
        [("keep", "yes")]
    );
    assert_eq!(
        strs(&page.serialize_form(form.node_id, Some(id(&page, "reset")))),
        [("keep", "yes")]
    );
    assert_eq!(
        strs(&page.serialize_form(forms[1].node_id, None)),
        [("other", "no")]
    );
}

#[test]
fn explicit_form_attribute_assigns_ownership() {
    let page = page(
        r#"
        <form id="f"><input id="inside" name="inside" value="1"></form>
        <input id="attached" form="f" name="attached" value="2">
        <input id="orphan" name="orphan" value="3">
        "#,
    );

    let forms = page.forms();
    assert_eq!(forms.len(), 1);
    let ids: Vec<usize> = forms[0].fields.iter().map(|field| field.node_id).collect();
    assert_eq!(ids, [id(&page, "inside"), id(&page, "attached")]);
    assert_eq!(
        strs(&page.serialize_form(forms[0].node_id, None)),
        [("inside", "1"), ("attached", "2")]
    );
}
