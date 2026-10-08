//! Streaming response cap (spec §8). Uses a local TCP stub only — never the
//! public internet.

use search_core::{AppError, Limits};
use search_fetcher::http::read_body_capped;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve one HTTP/1.1 response with a `body_len`-byte body, then exit.
async fn spawn_stub(body_len: usize) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut scratch = [0u8; 2048];
            let _ = sock.read(&mut scratch).await;
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {body_len}\r\n\r\n"
            );
            if sock.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            let chunk = vec![b'a'; 64 * 1024];
            let mut sent = 0usize;
            while sent < body_len {
                let n = (body_len - sent).min(chunk.len());
                if sock.write_all(&chunk[..n]).await.is_err() {
                    break;
                }
                sent += n;
            }
        }
    });
    addr
}

#[tokio::test]
async fn response_cap_triggers_limit_exceeded() {
    // Body far larger than the configured cap.
    let addr = spawn_stub(3 * 1024 * 1024).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .unwrap();

    let limits = Limits {
        max_response_bytes: 512 * 1024,
        max_decompressed_bytes: 1024 * 1024,
        ..Limits::default()
    };

    let err = read_body_capped(resp, &limits).await.unwrap_err();
    assert!(
        matches!(err, AppError::LimitExceeded(_)),
        "expected LimitExceeded, got {err:?}"
    );
}

#[tokio::test]
async fn small_body_under_cap_is_read_whole() {
    let addr = spawn_stub(1024).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .unwrap();

    let body = read_body_capped(resp, &Limits::default()).await.unwrap();
    assert_eq!(body.len(), 1024);
}
