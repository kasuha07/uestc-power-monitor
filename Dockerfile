# === Stage 1: Planner ===
# Calculate dependency fingerprint to optimize build caching
FROM rust:1-bookworm AS planner
WORKDIR /usr/src/app
RUN cargo install cargo-chef
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# === Stage 2: Cacher ===
# Build only dependencies (including vendored OpenSSL) and cache them
FROM rust:1-bookworm AS cacher
WORKDIR /usr/src/app
RUN cargo install cargo-chef
COPY --from=planner /usr/src/app/recipe.json recipe.json
# This step compiles dependencies. As long as Cargo.toml doesn't change, this layer is cached
RUN cargo chef cook --release --recipe-path recipe.json

# === Stage 3: Builder ===
FROM rust:1-bookworm AS builder
WORKDIR /usr/src/app
COPY . .
# Copy pre-built dependencies from cacher stage
COPY --from=cacher /usr/src/app/target target
COPY --from=cacher $CARGO_HOME $CARGO_HOME

# Build the application code (fast since dependencies are already compiled)
RUN cargo build --release

# Strip debug symbols to reduce binary size by 30%+
RUN strip target/release/uestc-power-monitor

# === Stage 4: Runtime ===
# Use Google Distroless CC image (includes glibc/libgcc/libm)
FROM gcr.io/distroless/cc-debian12

# Copy timezone data for TZ environment variable support
COPY --from=builder /usr/share/zoneinfo /usr/share/zoneinfo

# Copy the final binary from builder stage
COPY --from=builder /usr/src/app/target/release/uestc-power-monitor /usr/local/bin/uestc-power-monitor

# Set working directory
WORKDIR /app

# The application looks for config.toml in the working directory
# It can also be configured via environment variables (UPM_*)

# Run the application
CMD ["/usr/local/bin/uestc-power-monitor"]
