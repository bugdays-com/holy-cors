# Build stage
FROM rust:1.75-alpine AS builder

# Install build dependencies
RUN apk add --no-cache musl-dev pkgconfig openssl-dev

# Create app directory
WORKDIR /app

# Copy manifests
COPY Cargo.toml Cargo.lock* ./

# Create a dummy main.rs to cache dependencies
RUN mkdir -p src && \
    echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src

# Copy actual source code
COPY src ./src

# Build for release
RUN touch src/main.rs && cargo build --release

# Runtime stage
FROM alpine:3.19

# Install CA certificates for TLS
RUN apk add --no-cache ca-certificates

# Copy the binary
COPY --from=builder /app/target/release/holy-cors /usr/local/bin/holy-cors

# Set default port
ENV HOLY_CORS_PORT=2345
ENV HOLY_CORS_BIND=0.0.0.0

# Expose port
EXPOSE 2345

# Run the binary
ENTRYPOINT ["holy-cors"]
