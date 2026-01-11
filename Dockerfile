# Build stage
FROM rust:latest AS builder

WORKDIR /usr/src/app

# Copy the project files
COPY . .

# Build the application in release mode
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install necessary runtime dependencies
# libssl3: Required for HTTPS requests
# ca-certificates: Required for verifying SSL certificates
RUN apt-get update && apt-get install -y \
    libssl3 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy the compiled binary from the builder stage
COPY --from=builder /usr/src/app/target/release/uestc-power-monitor /usr/local/bin/uestc-power-monitor

# Set the working directory
WORKDIR /app

# The application looks for config.toml in the working directory
# It can also be configured via environment variables (UPM_*)

# Run the application
CMD ["uestc-power-monitor"]
