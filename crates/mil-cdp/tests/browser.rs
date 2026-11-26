//! Real-browser smoke test. Run explicitly with:
//! `cargo test -p mil-cdp -- --ignored --nocapture`

use std::path::Path;

use mil_cdp::{CdpConfig, CdpEngine};
use url::Url;

#[tokio::test(flavor = "current_thread")]
#[ignore = "launches a real browser and needs network access"]
async fn renders_example_com() {
    let executable = Path::new(
        "/home/waipu/.cache/puppeteer/chrome-headless-shell/linux-155.0.8059.79/chrome-headless-shell-linux64/chrome-headless-shell",
    );
    if !executable.is_file() {
        eprintln!("skipping: cached chrome-headless-shell not found");
        return;
    }

    let mut engine = CdpEngine::new(CdpConfig {
        executable: Some(executable.to_path_buf()),
        ..CdpConfig::default()
    })
    .await
    .expect("launch chrome-headless-shell");

    let url = Url::parse("https://example.com/").expect("parse example.com URL");
    let page = engine
        .navigate(&url, 80, 24)
        .await
        .expect("navigate to example.com");

    println!(
        "title={:?} url={} document={}x{} cells with glyphs={}",
        page.title,
        page.url,
        page.document_width,
        page.document_height,
        page.surface
            .cells()
            .iter()
            .filter(|cell| cell.symbol != " ")
            .count()
    );
    assert!(!page.title.is_empty(), "page title should not be empty");
    assert!(page.document_width > 0 && page.document_height > 0);
    assert!(
        page.surface
            .cells()
            .iter()
            .any(|cell| cell.symbol != " " && !cell.continuation),
        "surface should contain at least one rendered grapheme"
    );

    engine.close().await.expect("close browser cleanly");
}
