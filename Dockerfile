# syntax=docker/dockerfile:1

# ---- build ----
FROM rust:1.96-slim-bookworm AS build
WORKDIR /app

# Build the dependencies on their own first, so editing the source does not
# recompile them: this layer is only rebuilt when Cargo.toml/Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src src
COPY web web
# Make sure cargo sees the real sources as newer than the placeholder build.
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

# ---- run ----
FROM debian:bookworm-slim
RUN useradd --system --uid 10001 plaza \
    && mkdir /data \
    && chown plaza /data
COPY --from=build /app/target/release/plaza /usr/local/bin/plaza

USER plaza
VOLUME /data
# 8080: web frontend + WebSocket. 8081: raw line protocol.
EXPOSE 8080 8081

# The slim image has no curl, so probe /healthz with bash's /dev/tcp.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s \
    CMD bash -c 'exec 3<>/dev/tcp/127.0.0.1/8080 \
        && printf "GET /healthz HTTP/1.0\r\n\r\n" >&3 \
        && grep -q ok <&3'

ENTRYPOINT ["plaza"]
CMD ["0.0.0.0:8080", "--tcp", "0.0.0.0:8081", "--data", "/data", "--cooldown", "250"]
