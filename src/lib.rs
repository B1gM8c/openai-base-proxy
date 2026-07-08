use std::collections::HashSet;
use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{
    CloseFrame as AxumCloseFrame, Message as AxumMessage, WebSocket, WebSocketUpgrade,
};
use axum::http::{HeaderMap, HeaderName, Method, Response, StatusCode, Uri, header};
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::StreamExt;
use futures_util::{SinkExt, stream::SplitSink};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::Request as WebSocketRequest;
use tokio_tungstenite::tungstenite::protocol::frame::{
    CloseFrame as TungsteniteCloseFrame, coding::CloseCode as TungsteniteCloseCode,
};
use tokio_tungstenite::tungstenite::{Error as TungsteniteError, Message as TungsteniteMessage};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use url::Url;

const DEFAULT_UPSTREAM_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_BIND_ADDR: &str = "0.0.0.0:3000";
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 30;
const X_PROXY_TOKEN: &str = "x-proxy-token";
const CLOSE_INTERNAL_ERROR: u16 = 1011;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyConfig {
    pub upstream_base_url: String,
    pub bind_addr: SocketAddr,
    pub connect_timeout: Duration,
    pub proxy_token: Option<String>,
}

impl ProxyConfig {
    pub fn new(upstream_base_url: impl AsRef<str>) -> Result<Self, ConfigError> {
        Ok(Self {
            upstream_base_url: normalize_upstream_base_url(upstream_base_url.as_ref())?,
            bind_addr: DEFAULT_BIND_ADDR
                .parse()
                .map_err(|_| ConfigError::InvalidBindAddress(DEFAULT_BIND_ADDR.to_owned()))?,
            connect_timeout: Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
            proxy_token: None,
        })
    }

    pub fn from_env() -> Result<Self, ConfigError> {
        let upstream_base_url = env::var("UPSTREAM_BASE_URL")
            .or_else(|_| env::var("OPENAI_BASE_URL"))
            .unwrap_or_else(|_| DEFAULT_UPSTREAM_BASE_URL.to_owned());
        let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_owned());
        let connect_timeout_secs = env::var("CONNECT_TIMEOUT_SECS")
            .ok()
            .map(|raw| {
                raw.parse::<u64>()
                    .map_err(|_| ConfigError::InvalidConnectTimeout(raw.clone()))
                    .and_then(|secs| {
                        if secs == 0 {
                            Err(ConfigError::InvalidConnectTimeout(raw))
                        } else {
                            Ok(secs)
                        }
                    })
            })
            .transpose()?
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS);

        Ok(Self {
            upstream_base_url: normalize_upstream_base_url(&upstream_base_url)?,
            bind_addr: bind_addr
                .parse()
                .map_err(|_| ConfigError::InvalidBindAddress(bind_addr.clone()))?,
            connect_timeout: Duration::from_secs(connect_timeout_secs),
            proxy_token: env::var("PROXY_TOKEN")
                .or_else(|_| env::var("OPENAI_PROXY_TOKEN"))
                .ok()
                .filter(|token| !token.is_empty()),
        })
    }
}

#[derive(Clone)]
pub struct AppState {
    client: reqwest::Client,
    config: ProxyConfig,
}

impl AppState {
    pub fn new(config: ProxyConfig) -> Result<Self, ConfigError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .connect_timeout(config.connect_timeout)
            .build()
            .map_err(ConfigError::BuildHttpClient)?;

        Ok(Self { client, config })
    }

    pub fn config(&self) -> &ProxyConfig {
        &self.config
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/__healthz", get(healthz))
        .route("/docs", get(scalar_docs))
        .route("/scalar", get(scalar_docs))
        .route("/openapi.json", get(openapi_json))
        .fallback(proxy)
        .with_state(state)
}

pub async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

pub async fn scalar_docs() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        SCALAR_DOCS_HTML,
    )
}

pub async fn openapi_json() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        OPENAPI_JSON,
    )
}

pub async fn proxy(
    State(state): State<AppState>,
    maybe_websocket: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    if is_websocket_request(&headers) {
        let Ok(websocket) = maybe_websocket else {
            return proxy_error(
                StatusCode::BAD_REQUEST,
                "invalid_websocket_upgrade",
                "invalid websocket upgrade request",
            );
        };

        return proxy_websocket(state, websocket, method, uri, headers).await;
    }

    proxy_http(state, method, uri, headers, body).await
}

const SCALAR_DOCS_HTML: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <title>OpenAI Base Proxy API Reference</title>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <style>
      body { margin: 0; }
      #app { min-height: 100vh; }
    </style>
  </head>
  <body>
    <div id="app"></div>
    <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
    <script>
      Scalar.createApiReference('#app', {
        url: '/openapi.json',
        theme: 'default',
        layout: 'modern',
        defaultHttpClient: { target: 'shell', client: 'curl' },
        persistAuth: false,
        hideClientButton: false,
      })
    </script>
  </body>
</html>
"#;

const OPENAPI_JSON: &str = r##"{"openapi":"3.1.0","info":{"title":"OpenAI Base Proxy","version":"0.1.0","description":"Transparent OpenAI-compatible API proxy. The proxy forwards method, path, query, headers, request body, status, response headers, and response body without validating or restricting OpenAI-specific request fields."},"jsonSchemaDialect":"https://json-schema.org/draft/2020-12/schema","servers":[{"url":"/","description":"This proxy"}],"tags":[{"name":"Proxy","description":"Transparent OpenAI-compatible HTTP proxy surface."},{"name":"WebSocket","description":"OpenAI /v1 WebSocket Upgrade surface. Payload events are passed through without parsing."},{"name":"Utility","description":"Local utility endpoints exposed by this proxy."}],"x-transparent-proxy":true,"security":[{"Authorization":[]},{"Authorization":[],"ProxyToken":[]}],"components":{"securitySchemes":{"Authorization":{"type":"http","scheme":"bearer","bearerFormat":"OpenAI API key","description":"Forwarded to the upstream API as Authorization: Bearer ..."},"ProxyToken":{"type":"apiKey","in":"header","name":"x-proxy-token","description":"Optional proxy-side token, required only when PROXY_TOKEN or OPENAI_PROXY_TOKEN is configured."}},"parameters":{"OpenAIPath":{"name":"openai_path","in":"path","required":true,"schema":{"type":"string"},"description":"Represents an OpenAI /v1 path. Runtime forwarding is greedy for all /v1/... subpaths; OpenAPI path templates cannot fully express that greedy matching."}},"requestBodies":{"TransparentOpenAIRequest":{"description":"Any OpenAI request body. The proxy streams it to the upstream API without parsing or validation.","required":false,"content":{"application/json":{"schema":{}},"multipart/form-data":{"schema":{"type":"object","additionalProperties":true}},"application/sdp":{"schema":{"type":"string"}},"application/octet-stream":{"schema":{"type":"string","format":"binary"}},"text/plain":{"schema":{"type":"string"}}}}},"responses":{"TransparentOpenAIResponse":{"description":"The upstream OpenAI-compatible status, headers, and body are streamed back to the client.","headers":{"x-request-id":{"schema":{"type":"string"},"description":"Upstream request id when returned by OpenAI."},"x-ratelimit-limit-requests":{"schema":{"type":"string"},"description":"Upstream rate-limit header when returned by OpenAI."},"x-ratelimit-remaining-requests":{"schema":{"type":"string"},"description":"Upstream rate-limit header when returned by OpenAI."},"retry-after":{"schema":{"type":"string"},"description":"Upstream retry hint when returned by OpenAI."}},"content":{"application/json":{"schema":{}},"text/event-stream":{"schema":{"type":"string"}},"application/octet-stream":{"schema":{"type":"string","format":"binary"}},"audio/mpeg":{"schema":{"type":"string","format":"binary"}},"text/plain":{"schema":{"type":"string"}}}},"ProxyError":{"description":"Proxy-local error returned before the request reaches the upstream API.","content":{"application/json":{"schema":{"type":"object","properties":{"error":{"type":"object","properties":{"type":{"const":"proxy_error"},"code":{"type":"string"},"message":{"type":"string"}},"required":["type","code","message"]}},"required":["error"]}}}}}},"paths":{"/__healthz":{"get":{"tags":["Utility"],"summary":"Health check","operationId":"healthz","security":[],"responses":{"200":{"description":"Proxy is running.","content":{"text/plain":{"schema":{"type":"string","example":"ok\n"}}}}}}},"/docs":{"get":{"tags":["Utility"],"summary":"Scalar API reference UI","operationId":"scalarDocs","security":[],"responses":{"200":{"description":"HTML page that renders /openapi.json with Scalar.","content":{"text/html":{"schema":{"type":"string"}}}}}}},"/scalar":{"get":{"tags":["Utility"],"summary":"Scalar API reference UI alias","operationId":"scalarDocsAlias","security":[],"responses":{"200":{"description":"Alias of /docs.","content":{"text/html":{"schema":{"type":"string"}}}}}}},"/openapi.json":{"get":{"tags":["Utility"],"summary":"OpenAPI document","operationId":"openapiJson","security":[],"responses":{"200":{"description":"OpenAPI 3.1 description for the proxy.","content":{"application/json":{"schema":{}}}}}}},"/v1/{openai_path}":{"parameters":[{"$ref":"#/components/parameters/OpenAIPath"}],"get":{"tags":["Proxy"],"summary":"Transparent OpenAI GET proxy","operationId":"proxyGet","description":"Forwards any GET request under /v1/... to the configured OpenAI-compatible upstream. Query parameters and end-to-end headers are preserved.","x-transparent-proxy":true,"responses":{"default":{"$ref":"#/components/responses/TransparentOpenAIResponse"}}},"post":{"tags":["Proxy"],"summary":"Transparent OpenAI POST proxy","operationId":"proxyPost","description":"Forwards any POST request under /v1/... to the configured OpenAI-compatible upstream. JSON, multipart, SDP, text, binary, and streaming bodies are passed through without schema validation.","x-transparent-proxy":true,"requestBody":{"$ref":"#/components/requestBodies/TransparentOpenAIRequest"},"responses":{"default":{"$ref":"#/components/responses/TransparentOpenAIResponse"}}},"put":{"tags":["Proxy"],"summary":"Transparent OpenAI PUT proxy","operationId":"proxyPut","description":"Forwards any PUT request under /v1/... to the configured OpenAI-compatible upstream.","x-transparent-proxy":true,"requestBody":{"$ref":"#/components/requestBodies/TransparentOpenAIRequest"},"responses":{"default":{"$ref":"#/components/responses/TransparentOpenAIResponse"}}},"patch":{"tags":["Proxy"],"summary":"Transparent OpenAI PATCH proxy","operationId":"proxyPatch","description":"Forwards any PATCH request under /v1/... to the configured OpenAI-compatible upstream.","x-transparent-proxy":true,"requestBody":{"$ref":"#/components/requestBodies/TransparentOpenAIRequest"},"responses":{"default":{"$ref":"#/components/responses/TransparentOpenAIResponse"}}},"delete":{"tags":["Proxy"],"summary":"Transparent OpenAI DELETE proxy","operationId":"proxyDelete","description":"Forwards any DELETE request under /v1/... to the configured OpenAI-compatible upstream.","x-transparent-proxy":true,"responses":{"default":{"$ref":"#/components/responses/TransparentOpenAIResponse"}}}},"/v1/realtime":{"get":{"tags":["WebSocket"],"summary":"Realtime WebSocket Upgrade","operationId":"realtimeWebSocket","description":"Upgrade to a Realtime WebSocket session, for example /v1/realtime?model=... or /v1/realtime?call_id=.... WebSocket frames are forwarded without parsing or validation.","x-websocket":true,"x-transparent-proxy":true,"responses":{"101":{"description":"WebSocket upgrade accepted by the upstream API."},"default":{"$ref":"#/components/responses/TransparentOpenAIResponse"}}}},"/v1/realtime/translations":{"get":{"tags":["WebSocket"],"summary":"Realtime translation WebSocket Upgrade","operationId":"realtimeTranslationWebSocket","description":"Upgrade to a Realtime translation WebSocket session, for example /v1/realtime/translations?model=gpt-realtime-translate. WebSocket frames are forwarded without parsing or validation.","x-websocket":true,"x-transparent-proxy":true,"responses":{"101":{"description":"WebSocket upgrade accepted by the upstream API."},"default":{"$ref":"#/components/responses/TransparentOpenAIResponse"}}}},"/v1/responses":{"get":{"tags":["WebSocket"],"summary":"Responses WebSocket Upgrade","operationId":"responsesWebSocket","description":"Upgrade to Responses WebSocket mode. WebSocket frames are forwarded without parsing or validation.","x-websocket":true,"x-transparent-proxy":true,"responses":{"101":{"description":"WebSocket upgrade accepted by the upstream API."},"default":{"$ref":"#/components/responses/TransparentOpenAIResponse"}}},"post":{"tags":["Proxy"],"summary":"Create a response through the transparent HTTP proxy","operationId":"responsesPost","description":"Representative HTTP proxy endpoint. All OpenAI request fields are accepted and forwarded without local schema validation.","x-transparent-proxy":true,"requestBody":{"$ref":"#/components/requestBodies/TransparentOpenAIRequest"},"responses":{"default":{"$ref":"#/components/responses/TransparentOpenAIResponse"}}}}}}"##;

async fn proxy_http(
    state: AppState,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    if !proxy_token_is_valid(&state.config, &headers) {
        return proxy_error(
            StatusCode::UNAUTHORIZED,
            "proxy_auth_required",
            "proxy authentication is required",
        );
    }

    let upstream_url = match build_upstream_url(&state.config, &uri) {
        Ok(url) => url,
        Err(_) => {
            return proxy_error(
                StatusCode::BAD_GATEWAY,
                "invalid_upstream_url",
                "proxy could not build upstream URL",
            );
        }
    };

    let method = match to_reqwest_method(&method) {
        Ok(method) => method,
        Err(_) => {
            return proxy_error(
                StatusCode::BAD_REQUEST,
                "invalid_method",
                "request method is not supported",
            );
        }
    };

    let request_stream = body
        .into_data_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other));

    let upstream_response = state
        .client
        .request(method, upstream_url)
        .headers(filter_request_headers(&headers))
        .body(reqwest::Body::wrap_stream(request_stream))
        .send()
        .await;

    let upstream_response = match upstream_response {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            return proxy_error(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "upstream request timed out",
            );
        }
        Err(_) => {
            return proxy_error(
                StatusCode::BAD_GATEWAY,
                "upstream_connect_error",
                "upstream request failed",
            );
        }
    };

    let status = StatusCode::from_u16(upstream_response.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = filter_response_headers(upstream_response.headers());
    let response_stream = upstream_response
        .bytes_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other));

    build_response(status, headers, Body::from_stream(response_stream))
}

async fn proxy_websocket(
    state: AppState,
    websocket: WebSocketUpgrade,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response<Body> {
    if method != Method::GET {
        return proxy_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "invalid_websocket_method",
            "websocket requests must use GET",
        );
    }

    if !is_supported_openai_websocket_path(&uri) {
        return proxy_error(
            StatusCode::BAD_REQUEST,
            "unsupported_websocket_path",
            "websocket proxying is limited to OpenAI /v1 API paths",
        );
    }

    if !proxy_token_is_valid(&state.config, &headers) {
        return proxy_error(
            StatusCode::UNAUTHORIZED,
            "proxy_auth_required",
            "proxy authentication is required",
        );
    }

    let upstream_url = match build_upstream_websocket_url(&state.config, &uri) {
        Ok(url) => url,
        Err(_) => {
            return proxy_error(
                StatusCode::BAD_GATEWAY,
                "invalid_upstream_url",
                "proxy could not build upstream websocket URL",
            );
        }
    };

    let upstream_request = match build_upstream_websocket_request(&upstream_url, &headers) {
        Ok(request) => request,
        Err(_) => {
            return proxy_error(
                StatusCode::BAD_GATEWAY,
                "invalid_upstream_websocket_request",
                "proxy could not build upstream websocket request",
            );
        }
    };

    let upstream_connection = match timeout(
        state.config.connect_timeout,
        connect_async(upstream_request),
    )
    .await
    {
        Ok(Ok(connection)) => connection,
        Ok(Err(error)) => return websocket_handshake_error_response(error),
        Err(_) => {
            return proxy_error(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "upstream websocket connection timed out",
            );
        }
    };

    let (upstream_websocket, upstream_response) = upstream_connection;
    let selected_protocol = upstream_response
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let websocket = if let Some(protocol) = selected_protocol {
        websocket.protocols([protocol])
    } else {
        websocket
    };

    websocket.on_upgrade(move |client_websocket| async move {
        bridge_websockets(client_websocket, upstream_websocket).await;
    })
}

pub fn build_upstream_url(config: &ProxyConfig, uri: &Uri) -> Result<String, ProxyError> {
    let path_and_query = uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");

    if !path_and_query.starts_with('/') {
        return Err(ProxyError::InvalidPathAndQuery);
    }

    Ok(format!("{}{}", config.upstream_base_url, path_and_query))
}

pub fn build_upstream_websocket_url(config: &ProxyConfig, uri: &Uri) -> Result<String, ProxyError> {
    let upstream_url = build_upstream_url(config, uri)?;
    let url = match upstream_url.strip_prefix("https://") {
        Some(rest) => format!("wss://{rest}"),
        None => match upstream_url.strip_prefix("http://") {
            Some(rest) => format!("ws://{rest}"),
            None => return Err(ProxyError::InvalidWebSocketBaseUrl),
        },
    };

    Ok(url)
}

pub fn filter_request_headers(headers: &HeaderMap) -> HeaderMap {
    filter_headers(headers, HeaderDirection::Request)
}

pub fn filter_response_headers(headers: &HeaderMap) -> HeaderMap {
    filter_headers(headers, HeaderDirection::Response)
}

#[derive(Copy, Clone)]
enum HeaderDirection {
    Request,
    Response,
}

fn filter_headers(headers: &HeaderMap, direction: HeaderDirection) -> HeaderMap {
    let connection_headers = connection_header_names(headers);
    let mut filtered = HeaderMap::new();

    for (name, value) in headers {
        if should_filter_header(name, direction, &connection_headers) {
            continue;
        }
        filtered.append(name.clone(), value.clone());
    }

    filtered
}

fn should_filter_header(
    name: &HeaderName,
    direction: HeaderDirection,
    connection_headers: &HashSet<HeaderName>,
) -> bool {
    connection_headers.contains(name)
        || name == header::CONNECTION
        || name.as_str().eq_ignore_ascii_case("keep-alive")
        || name == header::PROXY_AUTHENTICATE
        || name == header::PROXY_AUTHORIZATION
        || name == header::TE
        || name == header::TRAILER
        || name == header::TRANSFER_ENCODING
        || name == header::UPGRADE
        || name.as_str().eq_ignore_ascii_case("proxy-connection")
        || name
            .as_str()
            .eq_ignore_ascii_case("proxy-authentication-info")
        || name.as_str().eq_ignore_ascii_case("trailers")
        || name.as_str().eq_ignore_ascii_case(X_PROXY_TOKEN)
        || matches!(direction, HeaderDirection::Request) && name == header::HOST
        || name == header::CONTENT_LENGTH
}

fn filter_websocket_handshake_headers(headers: &HeaderMap) -> HeaderMap {
    let mut filtered = filter_request_headers(headers);
    for name in [
        "sec-websocket-accept",
        "sec-websocket-extensions",
        "sec-websocket-key",
        "sec-websocket-version",
    ] {
        filtered.remove(name);
    }
    filtered
}

fn connection_header_names(headers: &HeaderMap) -> HashSet<HeaderName> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect()
}

fn proxy_token_is_valid(config: &ProxyConfig, headers: &HeaderMap) -> bool {
    let Some(expected_token) = &config.proxy_token else {
        return true;
    };

    headers
        .get(X_PROXY_TOKEN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|actual_token| actual_token == expected_token)
}

fn is_websocket_request(headers: &HeaderMap) -> bool {
    let has_upgrade = headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));

    let has_connection_upgrade = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| value.trim().eq_ignore_ascii_case("upgrade"));

    has_upgrade && has_connection_upgrade
}

fn is_supported_openai_websocket_path(uri: &Uri) -> bool {
    uri.path().starts_with("/v1/")
}

fn build_upstream_websocket_request(
    upstream_url: &str,
    headers: &HeaderMap,
) -> Result<WebSocketRequest<()>, TungsteniteError> {
    let mut request = upstream_url.into_client_request()?;
    let filtered_headers = filter_websocket_handshake_headers(headers);
    for (name, value) in filtered_headers.iter() {
        request.headers_mut().append(name.clone(), value.clone());
    }
    Ok(request)
}

async fn bridge_websockets(
    client_websocket: WebSocket,
    upstream_websocket: WebSocketStream<MaybeTlsStream<TcpStream>>,
) {
    let (mut client_sender, mut client_receiver) = client_websocket.split();
    let (mut upstream_sender, mut upstream_receiver) = upstream_websocket.split();

    loop {
        tokio::select! {
            client_message = client_receiver.next() => {
                match client_message {
                    Some(Ok(message)) => {
                        let is_close = matches!(message, AxumMessage::Close(_));
                        if let Err(error) = upstream_sender.send(axum_to_tungstenite_message(message)).await {
                            tracing::debug!(%error, "failed to forward websocket message to upstream");
                            close_axum_sender(&mut client_sender, CLOSE_INTERNAL_ERROR, "upstream websocket write failed").await;
                            break;
                        }
                        if is_close {
                            break;
                        }
                    }
                    Some(Err(error)) => {
                        tracing::debug!(%error, "failed to read websocket message from client");
                        let _ = upstream_sender
                            .send(TungsteniteMessage::Close(Some(TungsteniteCloseFrame {
                                code: TungsteniteCloseCode::Error,
                                reason: "client websocket read failed".into(),
                            })))
                            .await;
                        break;
                    }
                    None => break,
                }
            }
            upstream_message = upstream_receiver.next() => {
                match upstream_message {
                    Some(Ok(message)) => {
                        if matches!(message, TungsteniteMessage::Frame(_)) {
                            continue;
                        }
                        let is_close = matches!(message, TungsteniteMessage::Close(_));
                        if let Err(error) = client_sender.send(tungstenite_to_axum_message(message)).await {
                            tracing::debug!(%error, "failed to forward websocket message to client");
                            let _ = upstream_sender
                                .send(TungsteniteMessage::Close(Some(TungsteniteCloseFrame {
                                    code: TungsteniteCloseCode::Error,
                                    reason: "client websocket write failed".into(),
                                })))
                                .await;
                            break;
                        }
                        if is_close {
                            break;
                        }
                    }
                    Some(Err(error)) => {
                        tracing::debug!(%error, "failed to read websocket message from upstream");
                        close_axum_sender(&mut client_sender, CLOSE_INTERNAL_ERROR, "upstream websocket read failed").await;
                        break;
                    }
                    None => break,
                }
            }
        }
    }
}

fn axum_to_tungstenite_message(message: AxumMessage) -> TungsteniteMessage {
    match message {
        AxumMessage::Text(text) => TungsteniteMessage::Text(text.to_string().into()),
        AxumMessage::Binary(bytes) => TungsteniteMessage::Binary(bytes),
        AxumMessage::Ping(bytes) => TungsteniteMessage::Ping(bytes),
        AxumMessage::Pong(bytes) => TungsteniteMessage::Pong(bytes),
        AxumMessage::Close(frame) => {
            TungsteniteMessage::Close(frame.map(|frame| TungsteniteCloseFrame {
                code: TungsteniteCloseCode::from(frame.code),
                reason: frame.reason.to_string().into(),
            }))
        }
    }
}

fn tungstenite_to_axum_message(message: TungsteniteMessage) -> AxumMessage {
    match message {
        TungsteniteMessage::Text(text) => AxumMessage::Text(text.to_string().into()),
        TungsteniteMessage::Binary(bytes) => AxumMessage::Binary(bytes),
        TungsteniteMessage::Ping(bytes) => AxumMessage::Ping(bytes),
        TungsteniteMessage::Pong(bytes) => AxumMessage::Pong(bytes),
        TungsteniteMessage::Close(frame) => AxumMessage::Close(frame.map(|frame| AxumCloseFrame {
            code: u16::from(frame.code),
            reason: frame.reason.to_string().into(),
        })),
        TungsteniteMessage::Frame(_) => AxumMessage::Close(Some(AxumCloseFrame {
            code: CLOSE_INTERNAL_ERROR,
            reason: "unexpected websocket frame".into(),
        })),
    }
}

async fn close_axum_sender(
    client_sender: &mut SplitSink<WebSocket, AxumMessage>,
    code: u16,
    reason: &'static str,
) {
    let _ = client_sender
        .send(AxumMessage::Close(Some(AxumCloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

fn normalize_upstream_base_url(raw: &str) -> Result<String, ConfigError> {
    let url = Url::parse(raw).map_err(ConfigError::InvalidUpstreamBaseUrl)?;

    if url.query().is_some() || url.fragment().is_some() {
        return Err(ConfigError::UpstreamBaseUrlCannotContainQueryOrFragment);
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::UpstreamBaseUrlCannotContainCredentials);
    }

    match url.scheme() {
        "https" => {}
        "http" if is_loopback_host(url.host_str()) => {}
        scheme => return Err(ConfigError::UnsupportedUpstreamScheme(scheme.to_owned())),
    }

    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn is_loopback_host(host: Option<&str>) -> bool {
    matches!(host, Some("localhost") | Some("127.0.0.1") | Some("[::1]"))
}

fn to_reqwest_method(method: &Method) -> Result<reqwest::Method, ()> {
    reqwest::Method::from_bytes(method.as_str().as_bytes()).map_err(|_| ())
}

fn build_response(status: StatusCode, headers: HeaderMap, body: Body) -> Response<Body> {
    let mut builder = Response::builder().status(status);
    if let Some(response_headers) = builder.headers_mut() {
        *response_headers = headers;
    }

    builder.body(body).unwrap_or_else(|_| {
        proxy_error(
            StatusCode::BAD_GATEWAY,
            "response_build_error",
            "proxy failed to build response",
        )
    })
}

fn websocket_handshake_error_response(error: TungsteniteError) -> Response<Body> {
    match error {
        TungsteniteError::Http(response) => {
            let (parts, body) = (*response).into_parts();
            let status =
                StatusCode::from_u16(parts.status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let headers = filter_response_headers(&parts.headers);
            build_response(status, headers, Body::from(body.unwrap_or_default()))
        }
        _ => proxy_error(
            StatusCode::BAD_GATEWAY,
            "upstream_websocket_connect_error",
            "upstream websocket connection failed",
        ),
    }
}

fn proxy_error(status: StatusCode, code: &str, message: &str) -> Response<Body> {
    let body =
        format!(r#"{{"error":{{"type":"proxy_error","code":"{code}","message":"{message}"}}}}"#);

    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid upstream base URL: {0}")]
    InvalidUpstreamBaseUrl(url::ParseError),
    #[error("upstream base URL must use https, or http for localhost tests")]
    UnsupportedUpstreamScheme(String),
    #[error("upstream base URL must not contain query or fragment")]
    UpstreamBaseUrlCannotContainQueryOrFragment,
    #[error("upstream base URL must not contain credentials")]
    UpstreamBaseUrlCannotContainCredentials,
    #[error("invalid bind address: {0}")]
    InvalidBindAddress(String),
    #[error("CONNECT_TIMEOUT_SECS must be a positive integer, got {0}")]
    InvalidConnectTimeout(String),
    #[error("failed to build HTTP client: {0}")]
    BuildHttpClient(reqwest::Error),
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("request URI does not contain an origin-form path and query")]
    InvalidPathAndQuery,
    #[error("upstream base URL cannot be mapped to a websocket URL")]
    InvalidWebSocketBaseUrl,
}
