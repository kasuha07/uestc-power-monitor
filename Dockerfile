# 所有基础镜像按摘要固定、构建工具固定版本并使用 --locked，避免可变上游
# 静默改变发布镜像。升级时同时更新标签与摘要：
#   docker buildx imagetools inspect rust:<版本>-bookworm
#   docker buildx imagetools inspect gcr.io/distroless/cc-debian12:latest
ARG RUST_IMAGE=rust:1.99.1-bookworm@sha256:9181b56c2a21ab1d5e73cffe04fc762426d30aa7fac3c91141dd033f49a7fc8b
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12@sha256:e5d81ddde149641e2a9ba55be4545bc125c67de07508b03ba4c22e6eb0ded5aa
ARG CARGO_CHEF_VERSION=0.1.78

# === Stage 1: Planner ===
# Calculate dependency fingerprint to optimize build caching
FROM ${RUST_IMAGE} AS planner
WORKDIR /usr/src/app
ARG CARGO_CHEF_VERSION
RUN cargo install cargo-chef --version ${CARGO_CHEF_VERSION} --locked
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# === Stage 2: Cacher ===
# Build only dependencies (including vendored OpenSSL) and cache them
FROM ${RUST_IMAGE} AS cacher
WORKDIR /usr/src/app
ARG CARGO_CHEF_VERSION
RUN cargo install cargo-chef --version ${CARGO_CHEF_VERSION} --locked
COPY --from=planner /usr/src/app/recipe.json recipe.json
# cargo-chef's recipe references local path dependencies, but it does not
# embed their source trees. Copy vendored path dependencies before `cook` so
# Cargo can resolve `uestc-client = { path = "vendor/uestc-client" }`.
COPY --from=planner /usr/src/app/vendor vendor
# This step compiles dependencies. As long as Cargo.toml doesn't change, this layer is cached
RUN cargo chef cook --release --recipe-path recipe.json

# === Stage 3: Builder ===
FROM ${RUST_IMAGE} AS builder
WORKDIR /usr/src/app
COPY . .
# Copy pre-built dependencies from cacher stage
COPY --from=cacher /usr/src/app/target target
COPY --from=cacher $CARGO_HOME $CARGO_HOME

# Build the application code (fast since dependencies are already compiled)
RUN cargo build --release

# Strip debug symbols to reduce binary size by 30%+
RUN strip target/release/uestc-power-monitor
RUN mkdir -p /runtime/app

# === Stage 4: Runtime ===
# Use Google Distroless CC image (includes glibc/libgcc/libm)
FROM ${RUNTIME_IMAGE}

# Copy timezone data for TZ environment variable support
COPY --from=builder /usr/share/zoneinfo /usr/share/zoneinfo

# Copy the final binary from builder stage
COPY --from=builder --chown=nonroot:nonroot /usr/src/app/target/release/uestc-power-monitor /usr/local/bin/uestc-power-monitor
COPY --from=builder --chown=nonroot:nonroot /runtime/app /app

# Set working directory
WORKDIR /app

# Default runtime timezone (application-level + libc-level)
ENV TZ=Asia/Shanghai
ENV UPM_TIMEZONE=Asia/Shanghai

# The application looks for config.toml in the working directory
# It can also be configured via environment variables (UPM_*)

# Run as distroless non-root user
USER nonroot:nonroot

EXPOSE 8080

# Run the application
CMD ["/usr/local/bin/uestc-power-monitor"]
