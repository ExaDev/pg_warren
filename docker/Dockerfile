# Builds pg_warren against one PostgreSQL major and installs it into the
# official postgres image for that major.
#
#   docker build -f docker/Dockerfile --build-arg PG_MAJOR=17 -t pg_warren:pg17 .
#   docker run --rm -e POSTGRES_PASSWORD=postgres -p 5432:5432 pg_warren:pg17
#   psql ... -c 'CREATE EXTENSION pg_warren; SELECT warren.version();'
#
# Supported majors: 15, 16, 17, 18 (DESIGN.md §11).

ARG PG_MAJOR=16
ARG PGRX_VERSION=0.19.3

FROM postgres:${PG_MAJOR}-bookworm AS build
ARG PG_MAJOR
ARG PGRX_VERSION

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential ca-certificates clang curl libclang-dev pkg-config \
      "postgresql-server-dev-${PG_MAJOR}" \
 && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
WORKDIR /src
# Install the toolchain pinned in rust-toolchain.toml before the cached layers.
COPY rust-toolchain.toml .
RUN rustup toolchain install
RUN cargo install cargo-pgrx --version "${PGRX_VERSION}" --locked

# Point pgrx at the image's pg_config instead of running `cargo pgrx init`.
ENV PGRX_PG_CONFIG_PATH=/usr/lib/postgresql/${PG_MAJOR}/bin/pg_config

COPY . .
RUN cargo pgrx package \
      --manifest-path crates/pg_warren/Cargo.toml \
      --pg-config "${PGRX_PG_CONFIG_PATH}" \
      --no-default-features --features "pg${PG_MAJOR}" \
      --out-dir /pkg

FROM postgres:${PG_MAJOR}-bookworm
COPY --from=build /pkg/ /
