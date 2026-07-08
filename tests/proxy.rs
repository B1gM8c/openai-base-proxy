use std::convert::Infallible;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::extract::ws::{Message as AxumWsMessage, WebSocketUpgrade};
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::{Router, routing::any};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use openai_base_proxy::{
    AppState, ProxyConfig, app, build_upstream_url, build_upstream_websocket_url,
    filter_request_headers,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;

#[derive(Debug)]
struct CapturedRequest {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
}

#[derive(Debug)]
struct CapturedWebSocketRequest {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
}

#[tokio::test]
async fn build_upstream_url_preserves_path_query_and_base_prefix() {
    let config = ProxyConfig::new("https://upstream.example.com/openai/").unwrap();
    let uri: Uri = "/v1/chat/completions?stream=true&model=gpt-4.1"
        .parse()
        .unwrap();

    let url = build_upstream_url(&config, &uri).unwrap();

    assert_eq!(
        url,
        "https://upstream.example.com/openai/v1/chat/completions?stream=true&model=gpt-4.1"
    );
}

#[tokio::test]
async fn build_upstream_websocket_url_preserves_path_query_and_converts_scheme() {
    let config = ProxyConfig::new("https://api.openai.com").unwrap();
    let uri: Uri = "/v1/realtime?model=gpt-realtime-2.1".parse().unwrap();

    let url = build_upstream_websocket_url(&config, &uri).unwrap();

    assert_eq!(
        url,
        "wss://api.openai.com/v1/realtime?model=gpt-realtime-2.1"
    );

    let local_config = ProxyConfig::new("http://127.0.0.1:40123/openai").unwrap();
    let local_uri: Uri = "/v1/responses".parse().unwrap();

    let local_url = build_upstream_websocket_url(&local_config, &local_uri).unwrap();

    assert_eq!(local_url, "ws://127.0.0.1:40123/openai/v1/responses");
}

#[tokio::test]
async fn filter_request_headers_removes_hop_by_hop_host_length_and_proxy_token() {
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, "Bearer sk-test".parse().unwrap());
    headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    headers.insert(header::HOST, "proxy.local".parse().unwrap());
    headers.insert(header::CONTENT_LENGTH, "16".parse().unwrap());
    headers.insert(
        header::CONNECTION,
        "keep-alive, x-debug-hop".parse().unwrap(),
    );
    headers.insert("x-debug-hop", "remove-me".parse().unwrap());
    headers.insert("x-proxy-token", "proxy-secret".parse().unwrap());
    headers.insert("openai-beta", "assistants=v2".parse().unwrap());

    let filtered = filter_request_headers(&headers);

    assert_eq!(
        filtered.get(header::AUTHORIZATION).unwrap(),
        "Bearer sk-test"
    );
    assert_eq!(
        filtered.get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    assert_eq!(filtered.get("openai-beta").unwrap(), "assistants=v2");
    assert!(!filtered.contains_key(header::HOST));
    assert!(!filtered.contains_key(header::CONTENT_LENGTH));
    assert!(!filtered.contains_key(header::CONNECTION));
    assert!(!filtered.contains_key("x-debug-hop"));
    assert!(!filtered.contains_key("x-proxy-token"));
}

#[tokio::test]
async fn scalar_docs_routes_are_served_locally() {
    let state = AppState::new(ProxyConfig::new("https://api.openai.com").unwrap()).unwrap();

    for uri in ["/docs", "/scalar"] {
        let response = app(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("Scalar.createApiReference"));
        assert!(html.contains("https://cdn.jsdelivr.net/npm/@scalar/api-reference"));
        assert!(html.contains("url: '/openapi.json'"));
        assert!(html.contains("persistAuth: false"));
    }
}

#[tokio::test]
async fn openapi_json_documents_the_transparent_proxy_surface() {
    let state = AppState::new(ProxyConfig::new("https://api.openai.com").unwrap()).unwrap();
    let response = app(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let spec = std::str::from_utf8(&body).unwrap();
    assert!(spec.contains(r#""openapi":"3.1.0""#));
    assert!(spec.contains(r#""/v1/{openai_path}""#));
    assert!(spec.contains(r#""/v1/realtime""#));
    assert!(spec.contains(r#""/v1/realtime/translations""#));
    assert!(spec.contains(r#""/v1/responses""#));
    assert!(spec.contains(r#""x-transparent-proxy":true"#));
    assert!(spec.contains(r#""Authorization""#));
    assert!(spec.contains(r#""x-proxy-token""#));
}

#[tokio::test]
async fn proxy_forwards_method_path_query_headers_and_body() {
    let (captured_tx, mut captured_rx) = mpsc::channel(1);
    let upstream_url = spawn_upstream(captured_tx).await;
    let state = AppState::new(ProxyConfig::new(upstream_url).unwrap()).unwrap();
    let response = app(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/chat/completions?stream=false")
                .header(header::AUTHORIZATION, "Bearer sk-live")
                .header(header::CONTENT_TYPE, "application/json")
                .header("openai-organization", "org_123")
                .header(header::CONNECTION, "keep-alive")
                .body(Body::from(r#"{"model":"gpt-4.1"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers().get("x-upstream").unwrap(), "ok");
    assert!(!response.headers().contains_key(header::CONNECTION));
    assert!(!response.headers().contains_key(header::TRANSFER_ENCODING));
    assert_eq!(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
        Bytes::from_static(br#"{"proxied":true}"#)
    );

    let captured = captured_rx.recv().await.unwrap();
    assert_eq!(captured.method, Method::POST);
    assert_eq!(
        captured.uri.path_and_query().unwrap().as_str(),
        "/v1/chat/completions?stream=false"
    );
    assert_eq!(
        captured.headers.get(header::AUTHORIZATION).unwrap(),
        "Bearer sk-live"
    );
    assert_eq!(
        captured.headers.get("openai-organization").unwrap(),
        "org_123"
    );
    assert!(!captured.headers.contains_key(header::CONNECTION));
    assert_eq!(captured.body, Bytes::from_static(br#"{"model":"gpt-4.1"}"#));
}

#[tokio::test]
async fn proxy_streams_upstream_response_without_waiting_for_the_full_body() {
    let upstream_url = spawn_streaming_upstream().await;
    let state = AppState::new(ProxyConfig::new(upstream_url).unwrap()).unwrap();
    let started = Instant::now();

    let response = app(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/responses")
                .header(header::AUTHORIZATION, "Bearer sk-live")
                .body(Body::from(r#"{"stream":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        started.elapsed() < Duration::from_millis(200),
        "proxy waited for the full upstream stream before returning headers"
    );
    assert_eq!(response.status(), StatusCode::OK);

    let mut body = response.into_body();
    let first = timeout(Duration::from_millis(200), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    assert_eq!(first, Bytes::from_static(b"data: first\n\n"));

    assert!(
        timeout(Duration::from_millis(100), body.frame())
            .await
            .is_err(),
        "second chunk arrived before upstream delay, which would make the stream timing test invalid"
    );

    let second = timeout(Duration::from_millis(500), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    assert_eq!(second, Bytes::from_static(b"data: second\n\n"));
}

#[tokio::test]
async fn proxy_token_is_enforced_when_configured_and_not_forwarded() {
    let (captured_tx, mut captured_rx) = mpsc::channel(1);
    let upstream_url = spawn_upstream(captured_tx).await;
    let mut config = ProxyConfig::new(upstream_url).unwrap();
    config.proxy_token = Some("proxy-secret".to_owned());
    let state = AppState::new(config).unwrap();

    let unauthorized = app(state.clone())
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let authorized = app(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/v1/models")
                .header("x-proxy-token", "proxy-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(authorized.status(), StatusCode::CREATED);
    let captured = captured_rx.recv().await.unwrap();
    assert!(!captured.headers.contains_key("x-proxy-token"));
}

#[tokio::test]
async fn multipart_upload_body_boundary_and_repeated_fields_are_forwarded_verbatim() {
    let (captured_tx, mut captured_rx) = mpsc::channel(1);
    let upstream_url = spawn_upstream(captured_tx).await;
    let state = AppState::new(ProxyConfig::new(upstream_url).unwrap()).unwrap();
    let multipart_body = Bytes::from_static(
        b"--proxy-boundary\r\n\
Content-Disposition: form-data; name=\"purpose\"\r\n\
\r\n\
batch\r\n\
--proxy-boundary\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"input.jsonl\"\r\n\
Content-Type: application/jsonl\r\n\
\r\n\
{\"custom_id\":\"1\",\"method\":\"POST\",\"url\":\"/v1/responses\"}\n\r\n\
--proxy-boundary\r\n\
Content-Disposition: form-data; name=\"include[]\"\r\n\
\r\n\
output_text\r\n\
--proxy-boundary\r\n\
Content-Disposition: form-data; name=\"include[]\"\r\n\
\r\n\
usage\r\n\
--proxy-boundary--\r\n",
    );

    let response = app(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/uploads/upload_123/parts")
                .header(header::AUTHORIZATION, "Bearer sk-live")
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=proxy-boundary",
                )
                .body(Body::from(multipart_body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let captured = captured_rx.recv().await.unwrap();
    assert_eq!(captured.method, Method::POST);
    assert_eq!(
        captured.uri.path_and_query().unwrap().as_str(),
        "/v1/uploads/upload_123/parts"
    );
    assert_eq!(
        captured.headers.get(header::CONTENT_TYPE).unwrap(),
        "multipart/form-data; boundary=proxy-boundary"
    );
    assert!(!captured.headers.contains_key(header::CONTENT_LENGTH));
    assert_eq!(captured.body, multipart_body);
}

#[tokio::test]
async fn binary_range_download_preserves_status_headers_and_encoded_bytes() {
    let (captured_tx, mut captured_rx) = mpsc::channel(1);
    let upstream_url = spawn_binary_range_upstream(captured_tx).await;
    let state = AppState::new(ProxyConfig::new(upstream_url).unwrap()).unwrap();
    let response = app(state)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/v1/files/file_123/content")
                .header(header::AUTHORIZATION, "Bearer sk-live")
                .header(header::RANGE, "bytes=10-15")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
    assert_eq!(
        response.headers().get(header::CONTENT_RANGE).unwrap(),
        "bytes 10-15/64"
    );
    assert_eq!(
        response.headers().get(header::ACCEPT_RANGES).unwrap(),
        "bytes"
    );
    assert_eq!(
        response.headers().get(header::CONTENT_ENCODING).unwrap(),
        "gzip"
    );
    assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    assert_eq!(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
        Bytes::from_static(b"\x1f\x8braw-gzip-bytes\x00\xff")
    );

    let captured = captured_rx.recv().await.unwrap();
    assert_eq!(captured.method, Method::GET);
    assert_eq!(
        captured.uri.path_and_query().unwrap().as_str(),
        "/v1/files/file_123/content"
    );
    assert_eq!(captured.headers.get(header::RANGE).unwrap(), "bytes=10-15");
}

#[tokio::test]
async fn request_bodies_are_streamed_to_upstream_without_waiting_for_full_upload() {
    let (first_chunk_seen_tx, first_chunk_seen_rx) = watch::channel(false);
    let upstream_url = spawn_request_streaming_upstream(first_chunk_seen_tx).await;
    let state = AppState::new(ProxyConfig::new(upstream_url).unwrap()).unwrap();
    let body_stream = futures_util::stream::unfold(
        (0_u8, first_chunk_seen_rx),
        |(step, mut first_chunk_seen_rx)| async move {
            match step {
                0 => Some((
                    Ok::<_, Infallible>(Bytes::from_static(b"first-")),
                    (1, first_chunk_seen_rx),
                )),
                1 => {
                    while !*first_chunk_seen_rx.borrow() {
                        first_chunk_seen_rx.changed().await.unwrap();
                    }
                    Some((
                        Ok::<_, Infallible>(Bytes::from_static(b"second")),
                        (2, first_chunk_seen_rx),
                    ))
                }
                _ => None,
            }
        },
    );

    let response = timeout(
        Duration::from_secs(2),
        app(state).oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/files")
                .header(header::AUTHORIZATION, "Bearer sk-live")
                .header(
                    header::CONTENT_TYPE,
                    "multipart/form-data; boundary=streaming-boundary",
                )
                .body(Body::from_stream(body_stream))
                .unwrap(),
        ),
    )
    .await
    .expect("proxy buffered the full request body instead of streaming it")
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
        Bytes::from_static(b"first-second")
    );
}

#[tokio::test]
async fn realtime_websocket_proxy_forwards_headers_path_query_and_messages() {
    let (captured_tx, mut captured_rx) = mpsc::channel(1);
    let upstream_url = spawn_websocket_upstream(captured_tx).await;
    let mut config = ProxyConfig::new(upstream_url).unwrap();
    config.proxy_token = Some("proxy-secret".to_owned());
    let proxy_url = spawn_server(app(AppState::new(config).unwrap())).await;
    let ws_url = proxy_url.replacen("http://", "ws://", 1) + "/v1/realtime?model=gpt-realtime-2.1";

    let mut request = ws_url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, "Bearer sk-live".parse().unwrap());
    request.headers_mut().insert(
        "openai-safety-identifier",
        "hashed-user-id".parse().unwrap(),
    );
    request
        .headers_mut()
        .insert("x-proxy-token", "proxy-secret".parse().unwrap());
    let (mut client_ws, _) = connect_async(request).await.unwrap();

    assert_eq!(
        client_ws.next().await.unwrap().unwrap(),
        TungsteniteMessage::Text("upstream-ready".into())
    );
    client_ws
        .send(TungsteniteMessage::Text(
            r#"{"type":"session.update"}"#.into(),
        ))
        .await
        .unwrap();
    assert_eq!(
        client_ws.next().await.unwrap().unwrap(),
        TungsteniteMessage::Text(r#"echo:{"type":"session.update"}"#.into())
    );
    client_ws
        .send(TungsteniteMessage::Binary(Bytes::from_static(
            b"audio-bytes",
        )))
        .await
        .unwrap();
    assert_eq!(
        client_ws.next().await.unwrap().unwrap(),
        TungsteniteMessage::Binary(Bytes::from_static(b"audio-bytes"))
    );

    let captured = captured_rx.recv().await.unwrap();
    assert_eq!(captured.method, Method::GET);
    assert_eq!(
        captured.uri.path_and_query().unwrap().as_str(),
        "/v1/realtime?model=gpt-realtime-2.1"
    );
    assert_eq!(
        captured.headers.get(header::AUTHORIZATION).unwrap(),
        "Bearer sk-live"
    );
    assert_eq!(
        captured.headers.get("openai-safety-identifier").unwrap(),
        "hashed-user-id"
    );
    assert!(!captured.headers.contains_key("x-proxy-token"));
}

#[tokio::test]
async fn responses_websocket_mode_is_proxied_on_v1_responses() {
    let (captured_tx, mut captured_rx) = mpsc::channel(1);
    let upstream_url = spawn_websocket_upstream(captured_tx).await;
    let proxy_url = spawn_server(app(
        AppState::new(ProxyConfig::new(upstream_url).unwrap()).unwrap()
    ))
    .await;
    let ws_url = proxy_url.replacen("http://", "ws://", 1) + "/v1/responses";

    let mut request = ws_url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, "Bearer sk-live".parse().unwrap());
    let (mut client_ws, _) = connect_async(request).await.unwrap();

    assert_eq!(
        client_ws.next().await.unwrap().unwrap(),
        TungsteniteMessage::Text("upstream-ready".into())
    );
    client_ws
        .send(TungsteniteMessage::Text(
            r#"{"type":"response.create","response":{"model":"gpt-5.5"}}"#.into(),
        ))
        .await
        .unwrap();
    assert_eq!(
        client_ws.next().await.unwrap().unwrap(),
        TungsteniteMessage::Text(
            r#"echo:{"type":"response.create","response":{"model":"gpt-5.5"}}"#.into()
        )
    );

    let captured = captured_rx.recv().await.unwrap();
    assert_eq!(
        captured.uri.path_and_query().unwrap().as_str(),
        "/v1/responses"
    );
}

#[tokio::test]
async fn realtime_translation_websocket_path_is_proxied() {
    let (captured_tx, mut captured_rx) = mpsc::channel(1);
    let upstream_url = spawn_websocket_upstream(captured_tx).await;
    let proxy_url = spawn_server(app(
        AppState::new(ProxyConfig::new(upstream_url).unwrap()).unwrap()
    ))
    .await;
    let ws_url = proxy_url.replacen("http://", "ws://", 1)
        + "/v1/realtime/translations?model=gpt-realtime-translate";

    let mut request = ws_url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, "Bearer sk-live".parse().unwrap());
    let (mut client_ws, _) = connect_async(request).await.unwrap();

    assert_eq!(
        client_ws.next().await.unwrap().unwrap(),
        TungsteniteMessage::Text("upstream-ready".into())
    );

    let captured = captured_rx.recv().await.unwrap();
    assert_eq!(
        captured.uri.path_and_query().unwrap().as_str(),
        "/v1/realtime/translations?model=gpt-realtime-translate"
    );
}

#[tokio::test]
async fn v1_websocket_upgrades_are_forwarded_to_openai_instead_of_path_allowlisted() {
    let (captured_tx, mut captured_rx) = mpsc::channel(1);
    let upstream_url = spawn_websocket_upstream(captured_tx).await;
    let proxy_url = spawn_server(app(
        AppState::new(ProxyConfig::new(upstream_url).unwrap()).unwrap()
    ))
    .await;
    let ws_url = proxy_url.replacen("http://", "ws://", 1) + "/v1/future-websocket?preview=true";

    let mut request = ws_url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, "Bearer sk-live".parse().unwrap());
    let (mut client_ws, _) = connect_async(request).await.unwrap();

    assert_eq!(
        client_ws.next().await.unwrap().unwrap(),
        TungsteniteMessage::Text("upstream-ready".into())
    );

    let captured = captured_rx.recv().await.unwrap();
    assert_eq!(
        captured.uri.path_and_query().unwrap().as_str(),
        "/v1/future-websocket?preview=true"
    );
}

#[tokio::test]
async fn websocket_subprotocols_are_forwarded_and_selected_protocol_is_returned() {
    let (captured_tx, mut captured_rx) = mpsc::channel(1);
    let upstream_url = spawn_subprotocol_websocket_upstream(captured_tx).await;
    let proxy_url = spawn_server(app(
        AppState::new(ProxyConfig::new(upstream_url).unwrap()).unwrap()
    ))
    .await;
    let ws_url = proxy_url.replacen("http://", "ws://", 1) + "/v1/realtime?model=gpt-realtime-2.1";

    let mut request = ws_url.into_client_request().unwrap();
    request.headers_mut().insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        "realtime, openai-insecure-api-key.sk-live".parse().unwrap(),
    );
    let (_client_ws, response) = connect_async(request).await.unwrap();

    assert_eq!(
        response
            .headers()
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .unwrap(),
        "realtime"
    );
    let captured = captured_rx.recv().await.unwrap();
    assert_eq!(
        captured
            .headers
            .get(header::SEC_WEBSOCKET_PROTOCOL)
            .unwrap(),
        "realtime, openai-insecure-api-key.sk-live"
    );
}

#[tokio::test]
async fn websocket_upstream_handshake_errors_preserve_status_headers_and_body() {
    let upstream_url = spawn_rejecting_websocket_upstream().await;
    let proxy_url = spawn_server(app(
        AppState::new(ProxyConfig::new(upstream_url).unwrap()).unwrap()
    ))
    .await;
    let ws_url = proxy_url.replacen("http://", "ws://", 1) + "/v1/responses";

    let error = connect_async(ws_url).await.unwrap_err();
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("expected HTTP websocket handshake error, got {error}");
    };

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers().get("x-request-id").unwrap(), "req_123");
    assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "2");
    assert_eq!(
        response.body().as_deref(),
        Some(br#"{"error":{"type":"rate_limit_error"}}"#.as_slice())
    );
}

#[tokio::test]
async fn realtime_sideband_websocket_preserves_call_id_query() {
    let (captured_tx, mut captured_rx) = mpsc::channel(1);
    let upstream_url = spawn_websocket_upstream(captured_tx).await;
    let proxy_url = spawn_server(app(
        AppState::new(ProxyConfig::new(upstream_url).unwrap()).unwrap()
    ))
    .await;
    let ws_url = proxy_url.replacen("http://", "ws://", 1) + "/v1/realtime?call_id=rtc_123";

    let mut request = ws_url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, "Bearer sk-live".parse().unwrap());
    let (mut client_ws, _) = connect_async(request).await.unwrap();

    assert_eq!(
        client_ws.next().await.unwrap().unwrap(),
        TungsteniteMessage::Text("upstream-ready".into())
    );

    let captured = captured_rx.recv().await.unwrap();
    assert_eq!(
        captured.uri.path_and_query().unwrap().as_str(),
        "/v1/realtime?call_id=rtc_123"
    );
}

#[tokio::test]
async fn realtime_webrtc_sdp_call_creation_is_covered_by_http_proxy() {
    let (captured_tx, mut captured_rx) = mpsc::channel(1);
    let upstream_url = spawn_sdp_upstream(captured_tx).await;
    let state = AppState::new(ProxyConfig::new(upstream_url).unwrap()).unwrap();
    let response = app(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/v1/realtime/calls")
                .header(header::AUTHORIZATION, "Bearer sk-live")
                .header(header::CONTENT_TYPE, "application/sdp")
                .body(Body::from("offer-sdp"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response.headers().get(header::LOCATION).unwrap(),
        "/v1/realtime/calls/rtc_123"
    );
    assert_eq!(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
        Bytes::from_static(b"answer-sdp")
    );

    let captured = captured_rx.recv().await.unwrap();
    assert_eq!(captured.method, Method::POST);
    assert_eq!(
        captured.uri.path_and_query().unwrap().as_str(),
        "/v1/realtime/calls"
    );
    assert_eq!(
        captured.headers.get(header::CONTENT_TYPE).unwrap(),
        "application/sdp"
    );
    assert_eq!(captured.body, Bytes::from_static(b"offer-sdp"));
}

async fn capture_handler(
    State(captured_tx): State<mpsc::Sender<CapturedRequest>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    captured_tx
        .send(CapturedRequest {
            method,
            uri,
            headers,
            body,
        })
        .await
        .unwrap();

    let mut response = (StatusCode::CREATED, r#"{"proxied":true}"#).into_response();
    response
        .headers_mut()
        .insert("x-upstream", "ok".parse().unwrap());
    response
        .headers_mut()
        .insert(header::CONNECTION, "close".parse().unwrap());
    response
}

async fn streaming_handler() -> Response {
    let stream = futures_util::stream::unfold(0, |step| async move {
        match step {
            0 => Some((
                Ok::<_, Infallible>(Bytes::from_static(b"data: first\n\n")),
                1,
            )),
            1 => {
                sleep(Duration::from_millis(250)).await;
                Some((
                    Ok::<_, Infallible>(Bytes::from_static(b"data: second\n\n")),
                    2,
                ))
            }
            _ => None,
        }
    });

    let mut response = Body::from_stream(stream).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    response
}

async fn binary_range_handler(
    State(captured_tx): State<mpsc::Sender<CapturedRequest>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    captured_tx
        .send(CapturedRequest {
            method,
            uri,
            headers,
            body,
        })
        .await
        .unwrap();

    let mut response = (
        StatusCode::PARTIAL_CONTENT,
        b"\x1f\x8braw-gzip-bytes\x00\xff",
    )
        .into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    response
        .headers_mut()
        .insert(header::CONTENT_RANGE, "bytes 10-15/64".parse().unwrap());
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    response
        .headers_mut()
        .insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
    response
}

async fn request_streaming_handler(
    State(first_chunk_seen_tx): State<watch::Sender<bool>>,
    body: Body,
) -> Response {
    let mut body = body;
    let Some(first_frame) = body.frame().await else {
        return (StatusCode::BAD_REQUEST, "missing first chunk").into_response();
    };
    let first_chunk = match first_frame
        .map_err(|_| ())
        .and_then(|frame| frame.into_data().map_err(|_| ()))
    {
        Ok(chunk) => chunk,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid first chunk").into_response(),
    };

    if first_chunk != Bytes::from_static(b"first-") {
        return (StatusCode::BAD_REQUEST, "unexpected first chunk").into_response();
    }

    first_chunk_seen_tx.send(true).unwrap();
    let rest = body.collect().await.unwrap().to_bytes();
    if rest != Bytes::from_static(b"second") {
        return (StatusCode::BAD_REQUEST, "unexpected remaining body").into_response();
    }

    (StatusCode::OK, Bytes::from_static(b"first-second")).into_response()
}

async fn websocket_handler(
    State(captured_tx): State<mpsc::Sender<CapturedWebSocketRequest>>,
    ws: WebSocketUpgrade,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    captured_tx
        .send(CapturedWebSocketRequest {
            method,
            uri,
            headers,
        })
        .await
        .unwrap();

    ws.on_upgrade(|mut socket| async move {
        socket
            .send(AxumWsMessage::Text("upstream-ready".into()))
            .await
            .unwrap();

        while let Some(message) = socket.recv().await {
            match message.unwrap() {
                AxumWsMessage::Text(text) => {
                    socket
                        .send(AxumWsMessage::Text(format!("echo:{text}").into()))
                        .await
                        .unwrap();
                }
                AxumWsMessage::Binary(bytes) => {
                    socket.send(AxumWsMessage::Binary(bytes)).await.unwrap();
                }
                AxumWsMessage::Close(frame) => {
                    let _ = socket.send(AxumWsMessage::Close(frame)).await;
                    break;
                }
                AxumWsMessage::Ping(bytes) => {
                    socket.send(AxumWsMessage::Pong(bytes)).await.unwrap();
                }
                AxumWsMessage::Pong(_) => {}
            }
        }
    })
}

async fn subprotocol_websocket_handler(
    State(captured_tx): State<mpsc::Sender<CapturedWebSocketRequest>>,
    ws: WebSocketUpgrade,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    captured_tx
        .send(CapturedWebSocketRequest {
            method,
            uri,
            headers,
        })
        .await
        .unwrap();

    ws.protocols(["realtime"])
        .on_upgrade(|_socket| async move {})
}

async fn rejecting_websocket_handler() -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":{"type":"rate_limit_error"}}"#,
    )
        .into_response();
    response
        .headers_mut()
        .insert("x-request-id", "req_123".parse().unwrap());
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, "2".parse().unwrap());
    response
}

async fn sdp_handler(
    State(captured_tx): State<mpsc::Sender<CapturedRequest>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    captured_tx
        .send(CapturedRequest {
            method,
            uri,
            headers,
            body,
        })
        .await
        .unwrap();

    let mut response = (StatusCode::CREATED, "answer-sdp").into_response();
    response.headers_mut().insert(
        header::LOCATION,
        "/v1/realtime/calls/rtc_123".parse().unwrap(),
    );
    response
}

async fn spawn_upstream(captured_tx: mpsc::Sender<CapturedRequest>) -> String {
    let app = Router::new()
        .fallback(any(capture_handler))
        .with_state(captured_tx);
    spawn_server(app).await
}

async fn spawn_streaming_upstream() -> String {
    let app = Router::new().fallback(any(streaming_handler));
    spawn_server(app).await
}

async fn spawn_binary_range_upstream(captured_tx: mpsc::Sender<CapturedRequest>) -> String {
    let app = Router::new()
        .fallback(any(binary_range_handler))
        .with_state(captured_tx);
    spawn_server(app).await
}

async fn spawn_request_streaming_upstream(first_chunk_seen_tx: watch::Sender<bool>) -> String {
    let app = Router::new()
        .fallback(any(request_streaming_handler))
        .with_state(first_chunk_seen_tx);
    spawn_server(app).await
}

async fn spawn_websocket_upstream(captured_tx: mpsc::Sender<CapturedWebSocketRequest>) -> String {
    let app = Router::new()
        .fallback(any(websocket_handler))
        .with_state(captured_tx);
    spawn_server(app).await
}

async fn spawn_subprotocol_websocket_upstream(
    captured_tx: mpsc::Sender<CapturedWebSocketRequest>,
) -> String {
    let app = Router::new()
        .fallback(any(subprotocol_websocket_handler))
        .with_state(captured_tx);
    spawn_server(app).await
}

async fn spawn_rejecting_websocket_upstream() -> String {
    let app = Router::new().fallback(any(rejecting_websocket_handler));
    spawn_server(app).await
}

async fn spawn_sdp_upstream(captured_tx: mpsc::Sender<CapturedRequest>) -> String {
    let app = Router::new()
        .fallback(any(sdp_handler))
        .with_state(captured_tx);
    spawn_server(app).await
}

async fn spawn_server(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    std::mem::forget(shutdown_tx);
    format!("http://{addr}")
}
