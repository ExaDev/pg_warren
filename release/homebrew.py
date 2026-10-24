#!/usr/bin/env python3
"""Renders the Homebrew tap's formula for a release (CONTRIBUTING.md,
"Packages") from packaging/homebrew/pg_warren.rb.in: the URL and sha256 of
the release's source tarball (an asset of its GitHub Release) and the
cargo-pgrx its Cargo.lock pins. The release's bottles are then added by
`brew bottle --merge --write`.

    release/homebrew.py X.Y.Z SHA256 PGRX_VERSION > pg_warren.rb
"""

import re
import sys
from pathlib import Path

TEMPLATE = Path(__file__).resolve().parents[1] / "packaging" / "homebrew" / "pg_warren.rb.in"


class HomebrewError(Exception):
    """The formula cannot be rendered from these values."""


def render(version, sha256, pgrx_version):
    for name, value, pattern in (("version", version, r"\d+\.\d+\.\d+"), ("sha256", sha256, r"[0-9a-f]{64}"),
                                 ("cargo-pgrx version", pgrx_version, r"\d+\.\d+\.\d+")):
        if not re.fullmatch(pattern, value):
            raise HomebrewError(f"malformed {name}: {value!r}")
    formula = TEMPLATE.read_text()
    for placeholder, value in (("@VERSION@", version), ("@SHA256@", sha256), ("@PGRX_VERSION@", pgrx_version)):
        formula = formula.replace(placeholder, value)
    return formula


def main(argv):
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    try:
        sys.stdout.write(render(*argv))
    except HomebrewError as error:
        print(f"release/homebrew.py: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
