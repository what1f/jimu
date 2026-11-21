use bytes::Bytes;
use pikra_ai::{CancelToken, HttpRequest, HttpTransport, TransportError};
use pikra_http::ReqwestTransport;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn spawn_single_response_server(status_line: &'static str, body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut req_buf = [0u8; 1024];
            let _ = socket.read(&mut req_buf).await;
            let header = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(header.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        }
    });
    format!("http://{addr}/")
}

#[tokio::test]
async fn error_response_body_is_capped_at_64_kib_with_truncation_marker() {
    let transport = ReqwestTransport::new().unwrap();

    // 1. Oversized 200 KiB error body (with multi-byte UTF-8 across the 64 KiB cut boundary) is capped.
    let mut big_body = "x".repeat(65_535).into_bytes();
    big_body.extend_from_slice("你好世界".repeat(35_000).as_bytes());
    let big_url = spawn_single_response_server("502 Bad Gateway", big_body).await;

    let err = transport
        .send(
            HttpRequest {
                method: "GET".into(),
                url: big_url,
                headers: Vec::new(),
                body: Bytes::new(),
                timeout_ms: Some(5_000),
            },
            CancelToken::new(),
        )
        .await
        .err()
        .expect("502 must return TransportError::HttpStatus");

    match err {
        TransportError::HttpStatus { status, body, .. } => {
            assert_eq!(status, 502);
            assert!(
                body.ends_with("...[truncated]"),
                "oversized body must end with truncation marker"
            );
            assert!(
                body.len() <= 65_536 + "...[truncated]".len(),
                "body length {} exceeded 64 KiB cap",
                body.len()
            );
            assert!(!body.contains('\u{FFFD}'));
        }
        other => panic!("expected HttpStatus, got {other:?}"),
    }

    // 2. Small non-2xx error body passes through verbatim without truncation marker.
    let small_url =
        spawn_single_response_server("400 Bad Request", b"invalid_request_error".to_vec()).await;
    let small_err = transport
        .send(
            HttpRequest {
                method: "GET".into(),
                url: small_url,
                headers: Vec::new(),
                body: Bytes::new(),
                timeout_ms: Some(5_000),
            },
            CancelToken::new(),
        )
        .await
        .err()
        .expect("400 must return TransportError::HttpStatus");

    match small_err {
        TransportError::HttpStatus { status, body, .. } => {
            assert_eq!(status, 400);
            assert_eq!(body, "invalid_request_error");
        }
        other => panic!("expected HttpStatus, got {other:?}"),
    }
}

#[tokio::test]
async fn resolve_addrs_pins_tcp_destination_and_strips_internal_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();

    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut req_buf = [0u8; 4096];
            let n = socket.read(&mut req_buf).await.unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&req_buf[..n]).into_owned());
            let body = b"pinned-ok";
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(header.as_bytes()).await;
            let _ = socket.write_all(body).await;
            let _ = socket.shutdown().await;
        }
    });

    let transport = ReqwestTransport::new().unwrap();
    let response = transport
        .send(
            HttpRequest {
                method: "GET".into(),
                url: format!("http://pinned.example.invalid:{}/hello", addr.port()),
                headers: vec![
                    ("x-no-redirect".into(), "1".into()),
                    ("x-resolve-addrs".into(), addr.to_string()),
                    ("x-custom-header".into(), "kept".into()),
                ],
                body: Bytes::new(),
                timeout_ms: Some(5_000),
            },
            CancelToken::new(),
        )
        .await
        .expect("pinned domain request should connect to supplied SocketAddr");
    assert_eq!(response.status, 200);

    let raw_request = rx.await.unwrap();
    let lower = raw_request.to_ascii_lowercase();
    assert!(
        lower.contains("host: pinned.example.invalid"),
        "Host header should retain original domain: {raw_request}"
    );
    assert!(
        lower.contains("x-custom-header: kept"),
        "normal headers must pass through: {raw_request}"
    );
    assert!(
        !lower.contains("x-no-redirect"),
        "internal redirect header must be stripped: {raw_request}"
    );
    assert!(
        !lower.contains("x-resolve-addrs"),
        "internal resolve header must be stripped: {raw_request}"
    );
}
