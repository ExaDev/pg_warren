#!/usr/bin/env bash
# Runs the benchmark matrix (bench/matrix.json) against a scratch cluster and
# writes results to bench/results/<pg_warren version>/<graph>.json.
#
#   bench/run.sh                      # every graph but "smoke"; installs a release build
#   bench/run.sh random-100k          # one graph
#   bench/run.sh smoke --no-install --results bench/tmp   # quick check, as CI does
#
# Options are bench/matrix.py's (--help). PYTHON overrides the interpreter.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "${PYTHON:-python3}" "$here/matrix.py" "$@"
