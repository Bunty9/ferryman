# syntax=docker/dockerfile:1.7

# ---- Stage 1: chef base (cargo-chef for layer-cacheable builds) -------------
FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app
# Add musl target for a fully static binary that can run in `FROM scratch`.
RUN rustup target add x86_64-unknown-linux-musl \
    && apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/*

# ---- Stage 2: planner (compute the recipe of dependencies) ------------------
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- Stage 3: builder (cook deps, then build the actual workspace) ----------
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release \
    --target x86_64-unknown-linux-musl \
    --recipe-path recipe.json
COPY . .
RUN cargo build --release \
    --target x86_64-unknown-linux-musl \
    --bin ferryman-server

# ---- Stage 4: scratch runtime -----------------------------------------------
FROM scratch AS runtime
WORKDIR /app
COPY --from=builder \
    /app/target/x86_64-unknown-linux-musl/release/ferryman-server \
    /usr/local/bin/ferryman-server
COPY config.toml /app/config.toml
EXPOSE 8080 9090
ENTRYPOINT ["/usr/local/bin/ferryman-server"]
CMD ["--config", "/app/config.toml"]
