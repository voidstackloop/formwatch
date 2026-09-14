# syntax=docker/dockerfile:1

# --- build stage -------------------------------------------------------
FROM rust:1.98-bookworm AS builder
WORKDIR /src

# Compile dependencies against a placeholder so a source-only change
# doesn't invalidate the (slow) dependency layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --locked

COPY . .
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

# --- runtime stage -----------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        chromium \
        ca-certificates \
        fonts-liberation \
        libnss3 \
        libx11-xcb1 \
        libxcomposite1 \
        libxdamage1 \
        libxrandr2 \
        libgbm1 \
        libasound2 \
        libatk1.0-0 \
        libatk-bridge2.0-0 \
        libcups2 \
        libdrm2 \
        libpangocairo-1.0-0 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/formwatch /usr/local/bin/formwatch

# Container kernels commonly lack the user-namespace sandbox Chromium
# wants, so default to --no-sandbox here. On a normal host, use the
# binary without this.
ENV FORMWATCH_NO_SANDBOX=1

# Run as a non-root user. formwatch downloads a Chrome build itself if no
# system Chrome is found; here Debian's chromium is already on PATH.
RUN useradd --create-home --uid 10001 formwatch
USER formwatch
WORKDIR /home/formwatch

ENTRYPOINT ["formwatch"]
CMD ["--help"]

# `formwatch serve` binds here (map it with `-p 8080:8080` if used).
EXPOSE 8080
