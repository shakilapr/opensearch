//! Live Chromium test: the driver renders a **client-rendered** page and the text exists only
//! after a script runs.
//!
//! Ignored by default because it launches a real browser process; run it with
//! `cargo test -p search-browser --features chromium --test chromium_live -- --ignored --nocapture`
//! and point `SEARCH_CHROMIUM` at the binary when it is not discoverable.
//!
//! This is the evidence that §21 escalation is not merely wired: the page served over HTTP
//! contains no text in its HTML — only a `<script>` that writes it — and the driver must return
//! the rendered text anyway.

#![cfg(feature = "chromium")]

use std::io::{Read, Write};
use std::net::TcpListener;

use search_browser::{BrowserFetcher, BrowserLimits};

/// A JS-only page: the visible article exists only after the script runs.
const JS_ONLY_PAGE: &str = "<!doctype html><html><head><title>Shell</title></head><body>\
<div id=\"root\"></div>\
<script>document.getElementById('root').innerHTML = \
'<article><h1>Paged Attention</h1><p>Paged attention stores keys and values in separate \
blocks, reducing KV cache memory in vLLM.</p></article>';</script></body></html>";

/// Serve the page on an ephemeral loopback port; returns (url, join handle).
fn serve_page() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = std::thread::spawn(move || {
        // Serve every request, not just the first: Chromium opens more than one connection
        // (a preconnect and a favicon fetch are common), and answering only the first makes the
        // real navigation fail with a cancelled request — which is exactly how this test failed
        // before the loop was added.
        let body = JS_ONLY_PAGE.as_bytes();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}/shell"), handle)
}

#[tokio::test]
#[ignore = "launches a real Chromium process"]
async fn the_driver_returns_text_that_only_exists_after_javascript_runs() {
    let (url, _server) = serve_page();
    let browser = search_browser::chromium::ChromiumBrowser::launch()
        .await
        .expect("Chromium must launch");

    let limits = BrowserLimits { max_ms: 20_000, ..BrowserLimits::default() };
    let rendered = browser.fetch(&url, limits).await.expect("render");

    assert_eq!(rendered.status, 200);
    assert!(
        rendered.html.contains("Paged attention stores keys and values"),
        "the rendered DOM must contain the script-written text; got {} bytes",
        rendered.html.len()
    );
    // The point of the escalation: without the script there is no such text in the document at
    // all. The phrase does appear inside the script's own *source*, so the check strips the
    // script block rather than looking for the string in the file.
    let start = JS_ONLY_PAGE.find("<script>").expect("the shell has a script");
    let end = JS_ONLY_PAGE.find("</script>").expect("the script closes") + "</script>".len();
    let shell_without_script = format!("{}{}", &JS_ONLY_PAGE[..start], &JS_ONLY_PAGE[end..]);
    assert!(
        !shell_without_script.contains("Paged attention"),
        "the server-rendered shell must carry no text: {shell_without_script}"
    );

    // Deliberately no `join()`: the server thread serves until the process exits (Chromium
    // fetches more than one URL), so joining it blocks forever. That is exactly how this test
    // hung after the render had already succeeded.
}
