# The published image (ghcr.io/exadev/pg_warren:<version>-pg<NN>): PGDG's
# official postgres:<NN>-<codename> image with the files of
# postgresql-<NN>-pg-warren and pg_warren preloaded. Built by
# release/packages.py image from a package (the build context is the
# package's files, unpacked); CONTRIBUTING.md, "Packages". It runs nothing,
# so the publish job builds it, for either arch, from the package it
# checked. No org.opencontainers.image.source label: it would link the GHCR
# package to the repository, whose workflows it would then let push.
#
#   docker run --rm --shm-size=1g -e POSTGRES_PASSWORD=postgres -p 5432:5432 \
#     ghcr.io/exadev/pg_warren:pg17
# BASE: postgres:<NN>-<codename>@sha256:<digest>, pinned in release/packages.py.
ARG BASE
FROM ${BASE}
COPY usr/ /usr/
# Loaded graphs are shared between backends (DESIGN.md §5.6).
CMD ["postgres", "-c", "shared_preload_libraries=pg_warren"]
