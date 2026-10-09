# syntax=docker/dockerfile:1.7

# Supported build matrix: native builds only, on an amd64 host (produces a
# linux/amd64 image) or an arm64 host (produces a linux/arm64 image, e.g.
# Apple Silicon's Docker Desktop, which defaults to building linux/arm64).
# BuildKit sets TARGETARCH to the *target* platform, which only matches the
# host's own arch — and therefore what musl-tools' musl-gcc can actually
# target — when the builder itself runs natively as that arch. Cross-arch
# builds (e.g. building a linux/arm64 image on an amd64 host, or vice versa)
# are not supported here: that would need a cross toolchain, which this
# Dockerfile doesn't set up.

# ---- Stage 1: chef base (cargo-chef for layer-cacheable builds) -------------
FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app
ARG TARGETARCH
# Pick the musl target for a fully static binary that can run in `FROM
# scratch`, keyed off TARGETARCH; add it for rustup and record it in a file
# so later RUN steps (in other stages built FROM this one) can read it back
# without re-deriving it.
RUN case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-musl ;; \
      arm64) echo aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac > /rust_target.txt \
    && rustup target add "$(cat /rust_target.txt)" \
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
    --target "$(cat /rust_target.txt)" \
    --recipe-path recipe.json
COPY . .
# Build, then copy the binary to a fixed, arch-independent path: `COPY
# --from=builder` in the next stage can't itself substitute $(cat
# /rust_target.txt) into a source path (COPY paths are resolved at build
# time, not by the shell), so this `cp` is what makes that COPY work the
# same regardless of TARGETARCH.
RUN cargo build --release \
    --target "$(cat /rust_target.txt)" \
    --bin ferryman \
    && mkdir -p /out \
    && cp "target/$(cat /rust_target.txt)/release/ferryman" /out/ferryman

# ---- Stage 4: scratch runtime -----------------------------------------------
FROM scratch AS runtime
WORKDIR /app
COPY --from=builder /out/ferryman /usr/local/bin/ferryman
COPY config.toml /app/config.toml
EXPOSE 8080 9090
# Exec form: scratch has no shell or curl. Probes the admin /healthz on the
# metrics port (override with FERRYMAN_METRICS_BIND at run time).
HEALTHCHECK --interval=10s --timeout=5s --start-period=5s --retries=3 \
  CMD ["/usr/local/bin/ferryman", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/ferryman"]
CMD ["--config", "/app/config.toml"]
