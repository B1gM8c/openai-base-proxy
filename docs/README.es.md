# OpenAI Base Proxy

[English](../README.md) | [简体中文](README.zh-CN.md) | [日本語](README.ja.md) | [Español](README.es.md)

OpenAI Base Proxy is a transparent Rust/Axum proxy for OpenAI-compatible APIs. It gives clients a nearby `base_url` while keeping request and response behavior close to direct OpenAI API calls.

The proxy does not validate or rewrite OpenAI-specific request fields. This is intentional: new upstream parameters should continue to pass through without requiring proxy changes.

## Features

- Transparent forwarding of `Authorization: Bearer ...`.
- HTTP forwarding for `/v1/...` OpenAI-compatible endpoints.
- Streaming request and response bodies.
- SSE, multipart uploads, binary downloads, audio, and file content.
- WebSocket forwarding for Realtime, Realtime translation, sideband controls, and Responses WebSocket mode.
- HTTP forwarding for WebRTC setup endpoints.
- Optional `x-proxy-token` protection.
- Scalar API documentation at `/docs`, `/scalar`, and `/openapi.json`.

## Architecture

```mermaid
flowchart LR
    Client["Client / SDK"] --> Proxy["OpenAI Base Proxy"]
    Proxy --> Upstream["OpenAI-compatible upstream"]
```

HTTP requests are streamed to the configured upstream. WebSocket frames are forwarded in both directions after the upstream connection is established.

## Supported OpenAI API Areas

| Area | Status |
| --- | --- |
| Responses API | Supported, including SSE and WebSocket mode |
| Chat Completions | Supported |
| Embeddings | Supported |
| Images | Supported |
| Audio | Supported |
| Files and Uploads | Supported |
| Batches | Supported |
| Fine-tuning | Supported |
| Moderations and Models | Supported |
| Realtime WebSocket | Supported |
| Realtime translation WebSocket | Supported |
| WebRTC setup HTTP endpoints | Supported |
| WebRTC media, SIP media, webhooks | Out of scope |

## Run Locally

```bash
cp .env.example .env
cargo run
```

```bash
curl http://127.0.0.1:3000/v1/models \
  -H "Authorization: Bearer $OPENAI_API_KEY"
```

## Docker

```bash
docker build -t openai-base-proxy .
docker run --rm -p 3000:3000 -e PROXY_TOKEN=proxy-secret openai-base-proxy
```

## Production Notes

- Enable `PROXY_TOKEN` for public deployments.
- Put the proxy behind TLS.
- Redact `Authorization`, `x-proxy-token`, and `Sec-WebSocket-Protocol` from logs.
- Do not log request or response bodies.
- Add rate limits and connection limits at the infrastructure layer.

## Verification

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```
