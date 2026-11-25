use mil_js::{DomMutation, JsDomMirror, JsEngine, apply_report, apply_report_with_skipped};

const BASE: &str = "https://example.com/dir/index.html";

const HTML: &str = r#"
<html>
  <head><title>Old title</title></head>
  <body>
    <div id="x">old text</div>
    <p class="note">para</p>
    <a href="/next">link</a>
  </body>
</html>
"#;

fn run(scripts: &[&str]) -> mil_js::JsReport {
    let scripts: Vec<String> = scripts.iter().map(|script| (*script).to_string()).collect();
    JsEngine::new()
        .expect("engine")
        .run(HTML, &scripts, BASE)
        .expect("run")
}

#[test]
fn title_and_console_log() {
    let report = run(&[r#"document.title = "New"; console.log("hi");"#]);
    assert_eq!(report.title.as_deref(), Some("New"));
    assert!(
        report.logs.iter().any(|line| line == "hi"),
        "{:?}",
        report.logs
    );
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

#[test]
fn set_text_content_records_correct_node_id() {
    let expected_id = JsDomMirror::from_html(HTML, BASE)
        .expect("mirror")
        .element_by_id("x")
        .expect("#x");
    let report = run(&[r#"document.getElementById("x").textContent = "changed";"#]);

    let mutation = report
        .mutations
        .iter()
        .find_map(|mutation| match mutation {
            DomMutation::SetText { node_id, text } if text == "changed" => Some(*node_id),
            _ => None,
        })
        .expect("SetText mutation");
    assert_eq!(mutation, expected_id);
}

#[test]
fn anchor_click_records_navigation() {
    let report = run(&[r#"document.querySelector("a").click()"#]);
    assert!(
        report
            .navigations
            .iter()
            .any(|url| url == "https://example.com/next"),
        "{:?}",
        report.navigations
    );
}

#[test]
fn timeout_runs_and_loop_terminates() {
    let report = run(&[r#"setTimeout(() => { document.title = "late"; }, 10);"#]);
    assert_eq!(report.title.as_deref(), Some("late"));
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

#[test]
fn syntax_error_is_captured_but_later_scripts_run() {
    let report = run(&[
        "let = ;",
        r#"document.title = "still works"; console.log("ok");"#,
    ]);
    assert!(!report.errors.is_empty(), "syntax error was not recorded");
    assert_eq!(report.title.as_deref(), Some("still works"));
    assert!(report.logs.iter().any(|line| line == "ok"));
}

#[test]
fn apply_report_updates_real_document() {
    let id = JsDomMirror::from_html(HTML, BASE)
        .expect("mirror")
        .element_by_id("x")
        .expect("#x");
    let report = mil_js::JsReport {
        title: Some("Applied".to_string()),
        mutations: vec![
            DomMutation::SetText {
                node_id: id,
                text: "new text".to_string(),
            },
            DomMutation::SetStyle {
                node_id: id,
                property: "display".to_string(),
                value: "none".to_string(),
            },
        ],
        ..Default::default()
    };

    let config = blitz_dom::DocumentConfig::default();
    let mut doc = blitz_html::HtmlDocument::from_html(HTML, config).into_inner();
    apply_report(&mut doc, &report).expect("apply");

    assert_eq!(
        doc.find_title_node().expect("title").text_content(),
        "Applied"
    );
    let node = doc.get_node(id).expect("node");
    assert_eq!(node.text_content(), "new text");
    assert!(
        node.element_data()
            .and_then(|element| element.style_attribute.as_ref())
            .is_some(),
        "inline style was not applied"
    );
}

#[test]
fn skipped_mutations_are_reported_not_fatal() {
    let report = mil_js::JsReport {
        mutations: vec![DomMutation::SetText {
            node_id: 999_999,
            text: "nope".to_string(),
        }],
        ..Default::default()
    };
    let mut doc = blitz_html::HtmlDocument::from_html(HTML, Default::default()).into_inner();
    let skipped = apply_report_with_skipped(&mut doc, &report);
    assert_eq!(skipped.len(), 1);
    assert!(skipped[0].contains("999999"));
}

#[test]
fn query_selector_all_is_array_like() {
    let report = run(&[r#"
        var nodes = document.querySelectorAll("p");
        console.log("count=" + nodes.length);
        nodes.forEach(function(node) { console.log("item:" + node.textContent); });
        "#]);
    assert!(
        report.logs.iter().any(|line| line == "count=1"),
        "{:?}",
        report.logs
    );
    assert!(
        report.logs.iter().any(|line| line == "item:para"),
        "{:?}",
        report.logs
    );
}

#[test]
fn style_class_and_attribute_surface() {
    let report = run(&[r#"
        var el = document.getElementById("x");
        el.style.display = "none";
        el.style.setProperty("color", "red");
        el.classList.add("added");
        el.setAttribute("data-k", "v");
        console.log(el.classList.contains("added") + ":" + el.getAttribute("data-k"));
    "#]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(
        report.logs.iter().any(|line| line == "true:v"),
        "{:?}",
        report.logs
    );
    assert!(report.mutations.iter().any(|m| matches!(
        m,
        DomMutation::SetStyle { property, value, .. } if property == "display" && value == "none"
    )));
    assert!(report.mutations.iter().any(|m| matches!(
        m,
        DomMutation::SetAttribute { name, value, .. } if name == "data-k" && value == "v"
    )));
}

#[test]
fn document_write_appends_to_body() {
    let report = run(&[r#"document.write("hello "); document.write("world");"#]);
    let mutation = report
        .mutations
        .iter()
        .find_map(|mutation| match mutation {
            DomMutation::SetHtml { html, .. } => Some(html.as_str()),
            _ => None,
        })
        .expect("SetHtml mutation");
    assert_eq!(mutation, "hello world");
}

#[test]
fn misc_globals_are_available() {
    let report = run(&[r#"
        window.alert("hi");
        console.warn("careful");
        console.error("bad");
        window.location.href = "/other";
        console.log(navigator.userAgent);
        fetch("/data").then(function () { console.log("never"); });
    "#]);
    assert!(report.logs.iter().any(|line| line == "alert: hi"));
    assert!(report.logs.iter().any(|line| line == "warn: careful"));
    assert!(report.logs.iter().any(|line| line == "error: bad"));
    assert!(
        report
            .navigations
            .iter()
            .any(|url| url == "https://example.com/other"),
        "{:?}",
        report.navigations
    );
    assert!(
        report
            .navigations
            .iter()
            .any(|url| url == "https://example.com/data"),
        "{:?}",
        report.navigations
    );
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

#[test]
fn click_listeners_fire_and_stay_registered() {
    let report = run(&[r#"
        var el = document.querySelector("a");
        el.addEventListener("click", function (event) {
            console.log("clicked:" + event.type);
        });
        el.click();
        el.click();
    "#]);
    let clicks = report
        .logs
        .iter()
        .filter(|line| line.as_str() == "clicked:click")
        .count();
    assert_eq!(clicks, 2, "{:?}", report.logs);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
}

#[test]
fn inner_html_getter_is_escaped_setter_records_html() {
    let report = run(&[r#"
        var el = document.getElementById("x");
        console.log(el.innerHTML);
        el.innerHTML = "<b>&amp;</b>";
    "#]);
    assert!(
        report.logs.iter().any(|line| line == "old text"),
        "{:?}",
        report.logs
    );
    assert!(report.mutations.iter().any(|m| matches!(
        m,
        DomMutation::SetHtml { html, .. } if html == "<b>&amp;</b>"
    )));
}

#[test]
fn interval_is_bounded() {
    let report = run(&[r#"
        var n = 0;
        setInterval(function () { n += 1; document.title = "tick " + n; }, 100);
        "#]);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    // 16 iterations at 100 ms would exceed the 1000 ms budget: at most 10.
    let ticks = report
        .title
        .as_deref()
        .expect("title")
        .strip_prefix("tick ")
        .expect("tick value")
        .parse::<u32>()
        .expect("number");
    assert!(ticks <= 10, "too many interval ticks: {ticks}");
}
