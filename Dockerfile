# syntax=docker/dockerfile:1

########################################################################
# community — one image running the durable node (ankurah websocket
# server + Postgres storage) AND serving the Leptos SPA same-origin.
# Mirrors idp.to's Cloud Run image: cargo-chef for server dep caching,
# a trunk stage for the wasm SPA, a slim runtime.
########################################################################

# ---- server: dependency planner / builder (cargo-chef) ---------------
FROM rust:1.88-bookworm AS chef
WORKDIR /workspace
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && cargo install cargo-chef --locked

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY model/ model/
COPY server/ server/
COPY leptos-app/Cargo.toml leptos-app/Cargo.toml
RUN mkdir -p leptos-app/src && echo 'fn main() {}' > leptos-app/src/main.rs
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS server-builder
COPY --from=planner /workspace/recipe.json recipe.json
# Cache the (native, postgres) dependency build for community-server.
RUN cargo chef cook --locked --release -p community-server --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY model/ model/
COPY server/ server/
COPY leptos-app/Cargo.toml leptos-app/Cargo.toml
RUN mkdir -p leptos-app/src && echo 'fn main() {}' > leptos-app/src/main.rs
RUN cargo build --locked --release -p community-server

# ---- SPA: trunk build to wasm ----------------------------------------
FROM rust:1.88-bookworm AS spa-builder
ARG TARGETARCH
WORKDIR /workspace
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add wasm32-unknown-unknown \
    && case "${TARGETARCH:-amd64}" in \
         amd64) TRUNK_ARCH=x86_64 ;; \
         arm64) TRUNK_ARCH=aarch64 ;; \
         *) echo "unsupported TARGETARCH=${TARGETARCH:-?}" >&2; exit 1 ;; \
       esac \
    && curl -fsSL "https://github.com/trunk-rs/trunk/releases/download/v0.21.14/trunk-${TRUNK_ARCH}-unknown-linux-gnu.tar.gz" \
       | tar -xzf - -C /usr/local/bin trunk \
    && trunk --version
COPY Cargo.toml Cargo.lock ./
COPY model/ model/
COPY server/ server/
COPY leptos-app/ leptos-app/
WORKDIR /workspace/leptos-app
# Same-origin by default. A cross-origin build can bake a backend URL here.
ARG BACKEND_WS_URL=""
ENV BACKEND_WS_URL=${BACKEND_WS_URL}
RUN trunk build --release

# ---- runtime ---------------------------------------------------------
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=server-builder /workspace/target/release/community-server /app/community-server
COPY --from=spa-builder /workspace/leptos-app/dist /app/static
# The JWT policy is baked into the image; the server loads it at POLICY_PATH and
# the `watcher` publishes it into the `jwtpolicy` collection for clients to sync.
COPY policy.json /app/policy.json
ENV STATIC_DIR=/app/static
ENV POLICY_PATH=/app/policy.json
# Cloud Run overrides PORT at runtime; 8080 is the default for local runs.
ENV PORT=8080
EXPOSE 8080
CMD ["/app/community-server"]
