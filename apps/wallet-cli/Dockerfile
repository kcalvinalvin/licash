# The wallet CLI. Build from the repository root:
#   docker build -f apps/wallet-cli/Dockerfile -t licash-wallet .
# --platform linux/arm64 or linux/arm/v7 cross-compiles (apps/image-build.sh).
FROM --platform=$BUILDPLATFORM rust:1-slim-trixie AS build
ARG TARGETPLATFORM
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/apps/target,sharing=locked \
    apps/image-build.sh licash-wallet-cli:licash-wallet

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
