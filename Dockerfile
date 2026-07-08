FROM rust:1-slim AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:stable-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/openai-base-proxy /usr/local/bin/openai-base-proxy

ENV BIND_ADDR=0.0.0.0:3000
EXPOSE 3000

CMD ["openai-base-proxy"]
