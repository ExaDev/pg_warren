#!/usr/bin/env bash
# Installs a pg_warren .deb with apt in a clean container of its
# distribution, with PostgreSQL from PGDG, adds pg_warren to
# shared_preload_libraries, restarts and runs smoke.sql, as a superuser and
# as a database owner (a trusted install). release/packages.py check.
set -euo pipefail
deb="$1"
major="$(dpkg-deb --field "$deb" Package | sed -E 's/^postgresql-([0-9]+)-pg-warren$/\1/')"

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends ca-certificates curl gnupg postgresql-common >/dev/null
/usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y >/dev/null
# apt resolves the package's Depends (postgresql-NN from PGDG, libc6, ...).
apt-get install -y --no-install-recommends "$deb" | tee /tmp/install.log
grep -q "shared_preload_libraries" /tmp/install.log || { echo "the postinst does not say to preload pg_warren"; exit 1; }
grep -q "^shared_preload_libraries" "/etc/postgresql/$major/main/postgresql.conf" \
  && { echo "the package edited postgresql.conf"; exit 1; }

pg_ctlcluster "$major" main start
as_postgres() { su postgres -c "$*"; }
as_postgres "psql -v ON_ERROR_STOP=1 -c \"ALTER SYSTEM SET shared_preload_libraries = 'pg_warren'\""
pg_ctlcluster "$major" main restart
as_postgres "psql -v ON_ERROR_STOP=1 -At -c 'SHOW shared_preload_libraries'" | grep -qx pg_warren
as_postgres "psql -v ON_ERROR_STOP=1 -f /packaging/smoke.sql"
as_postgres "psql -v ON_ERROR_STOP=1 -c 'CREATE ROLE app LOGIN' -c 'CREATE DATABASE app OWNER app'"
# A trusted install: a non-superuser that owns its database (peer auth).
useradd --no-create-home app
su app -c "psql -v ON_ERROR_STOP=1 -d app -f /packaging/smoke.sql"
echo "ok: $(basename "$deb") installs, preloads and runs on PostgreSQL $major"
