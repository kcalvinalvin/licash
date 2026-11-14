# The wallet CLI. Build from the repository root:
#   docker build -f apps/wallet-cli/Dockerfile -t licash-wallet .
FROM rust:1-slim-trixie AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/apps/target,sharing=locked \
    cargo build --locked --release --manifest-path apps/Cargo.toml -p licash-wallet-cli \
    && cp apps/target/release/licash-wallet /usr/local/bin/licash-wallet

FROM debian:trixie-slim
# Root certificates, for hubs over HTTPS.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin licash \
    && mkdir -p /data && chown licash /data && chmod 700 /data
COPY --from=build /usr/local/bin/licash-wallet /usr/local/bin/licash-wallet
USER 10001
ENV LICASH_WALLET=/data/wallet.sqlite
VOLUME /data
ENTRYPOINT ["licash-wallet"]
