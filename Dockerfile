# syntax=docker/dockerfile:1.7
#
# Jimoto GPU Pod の実行イメージ（中央サーバー + 提供PCエージェント + 補助ツール）。
# 使い方は docs/deploy.md と README のクイックスタートを参照してください。

# ---- ビルドステージ ----
# rust-version の MSRV は 1.85 ですが、依存クレートの解決を考えると最新 stable で
# ビルドするのが安全です。固定したい場合は --build-arg RUST_VERSION=1.85.1 を渡します。
ARG RUST_VERSION=1
FROM rust:${RUST_VERSION}-bookworm AS builder

WORKDIR /src

# 依存の取得をキャッシュし、ソース変更時の再ビルドを短くします。
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --workspace --bins \
    && mkdir -p /out \
    && cp target/release/gpupod-control target/release/gpupod-agent target/release/gpupod-tools /out/

# ---- 実行ステージ ----
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="Jimoto GPU Pod" \
      org.opencontainers.image.description="Rust 製の GPU 貸出サービス（中央サーバー / 提供PCエージェント）" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.source="https://github.com/jimoto-no-llm/jimoto-no-gpupod-rs"

# ca-certificates: HTTPS クライアント
# curl: ヘルスチェック（/healthz）
# sqlite3: docker-compose のデモ用ノード登録
# openssh-client: デモ用に中継ホスト鍵の公開鍵を取り出す（ssh-keygen -y）
# tini: PID 1 のシグナル転送とゾンビ回収
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        sqlite3 \
        openssh-client \
        tini \
    && rm -rf /var/lib/apt/lists/*

# 非 root で動かします（UID/GID 10001）。
RUN groupadd --gid 10001 gpupod \
    && useradd --uid 10001 --gid 10001 --home-dir /var/lib/gpupod --shell /usr/sbin/nologin gpupod

COPY --from=builder /out/gpupod-control /usr/local/bin/gpupod-control
COPY --from=builder /out/gpupod-agent /usr/local/bin/gpupod-agent
COPY --from=builder /out/gpupod-tools /usr/local/bin/gpupod-tools

COPY examples/templates.json /opt/gpupod/examples/templates.json
COPY scripts/docker/control-entrypoint.sh /usr/local/bin/gpupod-control-entrypoint.sh
COPY scripts/docker/provider-entrypoint.sh /usr/local/bin/gpupod-provider-entrypoint.sh
COPY scripts/docker/bootstrap-provider.sh /usr/local/bin/gpupod-bootstrap-provider.sh

RUN chmod 0755 /usr/local/bin/gpupod-control-entrypoint.sh \
        /usr/local/bin/gpupod-provider-entrypoint.sh \
        /usr/local/bin/gpupod-bootstrap-provider.sh \
    && mkdir -p /var/lib/gpupod \
    && chown -R gpupod:gpupod /var/lib/gpupod /opt/gpupod \
    && chmod 0700 /var/lib/gpupod

USER gpupod:gpupod
WORKDIR /opt/gpupod

ENV GPUPOD_CONTROL_STATE=/var/lib/gpupod/control \
    RUST_LOG=info

# 8000: WebUI / API, 19000: 中継制御, 19001-19999: 貸出ごとの接続ポート
EXPOSE 8000 19000 19001-19999

HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=6 \
    CMD curl -fsS http://127.0.0.1:8000/healthz || exit 1

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/gpupod-control-entrypoint.sh"]
