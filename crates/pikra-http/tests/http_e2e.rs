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
