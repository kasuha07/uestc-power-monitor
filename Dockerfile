# Build stage
FROM rust:latest AS builder

# Set working directory to /usr/src to manage sibling dependencies
WORKDIR /usr/src

# Clone the uestc-client dependency required by Cargo.toml
# This ensures the build works standalone without requiring the user to manually provide the dependency folder.
RUN git clone https://github.com/kasuha07/uestc-client.git

# Copy the project files
COPY . uestc-power-monitor

# Switch to the project directory
WORKDIR /usr/src/uestc-power-monitor

# Build the application in release mode
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

# Install necessary runtime dependencies
# libssl3: Required for HTTPS requests and database connections
# ca-certificates: Required for verifying SSL certificates
RUN apt-get update && apt-get install -y \
    libssl3 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy the compiled binary from the builder stage
COPY --from=builder /usr/src/uestc-power-monitor/target/release/uestc-power-monitor /usr/local/bin/uestc-power-monitor

# Set the working directory
WORKDIR /app

# The application looks for config.toml in the working directory
# It can also be configured via environment variables (UPM_*)

# Run the application
CMD ["uestc-power-monitor"]

