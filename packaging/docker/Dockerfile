# The published image (ghcr.io/exadev/pg_warren:<version>-pg<NN>): PGDG's
# official postgres:<NN>-<codename> image with postgresql-<NN>-pg-warren
# installed from its .deb and pg_warren preloaded. Built by
# release/packages.py image from a built package (the build context holds
# only pg_warren.deb); CONTRIBUTING.md, "Packages".
#
#   docker run --rm --shm-size=1g -e POSTGRES_PASSWORD=postgres -p 5432:5432 \
#     ghcr.io/exadev/pg_warren:pg17
# BASE: postgres:<NN>-<codename>@sha256:<digest>, pinned in release/packages.py.
ARG BASE
FROM ${BASE}
COPY pg_warren.deb /tmp/pg_warren.deb
RUN apt-get update \
 && apt-get install -y --no-install-recommends /tmp/pg_warren.deb \
 && rm -rf /var/lib/apt/lists/* /tmp/pg_warren.deb
LABEL org.opencontainers.image.source="https://github.com/ExaDev/pg_warren"
# Loaded graphs are shared between backends (DESIGN.md §5.6).
CMD ["postgres", "-c", "shared_preload_libraries=pg_warren"]
