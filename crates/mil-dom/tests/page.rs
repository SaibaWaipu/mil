use mil_dom::{ColorScheme, Page, PaintConfig, Viewport, render_page};
use url::Url;

const HTML: &str = r#"
<html>
  <head><title>Tiny page</title></head>
  <body>
    <h1>Hello</h1>
    <p>A <a href="/x">link</a></p>
  </body>
</html>
"#;

fn page() -> Page {
    let url = Url::parse("https://example.com/dir/index.html").unwrap();
    let viewport = Viewport::new(800, 600, 1.0, ColorScheme::Light);
    let mut page = Page::from_html(HTML, &url, viewport).unwrap();
    page.resolve(0.0);
    page
}

#[test]
fn extracts_title_text_and_links() {
    let page = page();
    assert_eq!(page.title(), "Tiny page");

    let runs = page.text_runs();
    assert!(
        runs.iter().any(|run| run.text.contains("Hello")),
        "missing Hello run: {runs:?}"
    );
    assert!(
        runs.iter().any(|run| run.text.contains("link")),
        "missing link run: {runs:?}"
    );

    let links = page.links();
    assert!(
        links.iter().any(|link| link.url == "https://example.com/x"),
        "relative href was not resolved: {links:?}"
    );
}

#[test]
fn renders_source_text_into_cells() {
    let page = page();
    let surface = render_page(&page, &PaintConfig::default(), 80, 24).unwrap();
    assert_eq!(surface.width, 80);
    assert_eq!(surface.height, 24);
    assert!(
        surface.cells().iter().any(|cell| cell.symbol == "H"),
        "'H' was not painted: {:?}",
        &surface.cells()[..80]
    );
}

#[test]
fn hit_test_returns_a_node() {
    let page = page();
    assert!(page.hit_test(10.0, 10.0).is_some());
}

#[test]
fn inline_style_spans_do_not_duplicate_text() {
    let url = Url::parse("https://example.com/first").unwrap();
    let viewport = Viewport::new(800, 600, 1.0, ColorScheme::Light);
    let mut page = Page::from_html(
        r#"<p>This is a <strong>smoke</strong> page with a <a href="/second">link</a>.</p>"#,
        &url,
        viewport,
    )
    .unwrap();
    page.resolve(0.0);

    let runs = page.text_runs();
    let texts: Vec<&str> = runs.iter().map(|run| run.text.as_str()).collect();
    assert_eq!(texts, ["This is a ", "smoke", " page with a ", "link", "."]);
}
