# The swap provider service, with the wallet CLI to fund it. Build from the
# repository root:
#   docker build -f apps/swaps/Dockerfile -t licash-swaps .
FROM rust:1-slim-trixie AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/apps/target \
    cargo build --locked --release --manifest-path apps/Cargo.toml \
        -p licash-swaps-service -p licash-wallet-cli \
    && cp apps/target/release/licash-swaps apps/target/release/licash-wallet /usr/local/bin/

FROM debian:trixie-slim
# Root certificates, for hubs and notifications over HTTPS.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin licash \
    && mkdir -p /data && chown licash /data && chmod 700 /data
COPY --from=build /usr/local/bin/licash-swaps /usr/local/bin/licash-wallet /usr/local/bin/
USER 10001
ENV LICASH_SWAPS_DATA=/data LICASH_SWAPS_LISTEN=0.0.0.0:8340 LICASH_WALLET=/data/wallet.sqlite
VOLUME /data
EXPOSE 8340
HEALTHCHECK --interval=30s --timeout=5s \
    CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/${LICASH_SWAPS_LISTEN##*:} && printf 'GET /health HTTP/1.0\\r\\n\\r\\n' >&3 && head -n1 <&3 | grep -q ' 200 '"]
ENTRYPOINT ["licash-swaps"]
CMD ["run"]
