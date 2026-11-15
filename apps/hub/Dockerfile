# The hub service. Build from the repository root:
#   docker build -f apps/hub/Dockerfile -t licash-hub .
# --platform linux/arm64 or linux/arm/v7 cross-compiles (apps/image-build.sh).
FROM --platform=$BUILDPLATFORM rust:1-slim-trixie AS build
ARG TARGETPLATFORM
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/apps/target,sharing=locked \
    apps/image-build.sh licash-hub-service:licash-hub

FROM debian:trixie-slim
# Root certificates, for notifications and Esplora over HTTPS.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin licash \
    && mkdir -p /data && chown licash /data && chmod 700 /data
COPY --from=build /usr/local/bin/licash-hub /usr/local/bin/licash-hub
USER 10001
ENV LICASH_HUB_DATA=/data LICASH_HUB_LISTEN=0.0.0.0:8338
VOLUME /data
EXPOSE 8338
# Unhealthy once the hub halts (see the README).
HEALTHCHECK --interval=30s --timeout=5s \
    CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/${LICASH_HUB_LISTEN##*:} && printf 'GET /health HTTP/1.0\\r\\n\\r\\n' >&3 && head -n1 <&3 | grep -q ' 200 '"]
ENTRYPOINT ["licash-hub"]
CMD ["run"]
