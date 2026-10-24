#!/usr/bin/env bash
# Runs a pg_warren image (packaging/docker/Dockerfile) and the smoke test in
# it, as the superuser and as a database owner. release/packages.py image.
set -euo pipefail
tag="$1"
name="pg-warren-check-$$"
trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT
docker run -d --name "$name" --shm-size=256m -e POSTGRES_PASSWORD=postgres \
  -v "$(cd "$(dirname "$0")" && pwd)/smoke.sql:/smoke.sql:ro" "$tag" >/dev/null
# The entrypoint's init server listens only on the Unix socket and then
# restarts, so wait for TCP, which only the final server opens.
for _ in $(seq 1 60); do
  if docker exec "$name" pg_isready -q -h 127.0.0.1 -U postgres; then break; fi
  sleep 1
done
psql() { docker exec "$name" psql -h 127.0.0.1 -v ON_ERROR_STOP=1 "$@"; }
psql -U postgres -At -c 'SHOW shared_preload_libraries' | grep -qx pg_warren
psql -U postgres -f /smoke.sql
psql -U postgres -c 'CREATE ROLE app LOGIN' -c 'CREATE DATABASE app OWNER app'
psql -U app -d app -f /smoke.sql
echo "ok: $tag preloads pg_warren and runs the smoke test"
