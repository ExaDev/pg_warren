#!/usr/bin/env python3
"""Renders the Homebrew tap's formula for a release (CONTRIBUTING.md,
"Packages") from packaging/homebrew/pg_warren.rb.in: the URL and sha256 of
the release's source tarball (an asset of its GitHub Release) and the
cargo-pgrx its Cargo.lock pins, all read from the tarball.

    release/homebrew.py TARBALL > pg_warren.rb
    release/homebrew.py TARBALL --bottle-from BUILT.rb > pg_warren.rb

The bottle job renders the formula, builds the bottle and lets `brew bottle
--merge --write` add its `bottle do ... end` block. The publish job renders
the formula again and takes from the bottle job's only that block, and only
in its expected shape: this release's root_url and sha256 lines.
"""

import argparse
import hashlib
import re
import sys
import tarfile
from pathlib import Path

from packages import PackagingError, pgrx_version

TEMPLATE = Path(__file__).resolve().parents[1] / "packaging" / "homebrew" / "pg_warren.rb.in"
RELEASES = "https://github.com/ExaDev/pg_warren/releases/download"
BOTTLE_SHA = re.compile(r'    sha256 (?:cellar: (?::any|:any_skip_relocation|"/opt/homebrew/Cellar"|"/usr/local/Cellar"), )?'
                        r'[a-z0-9_]+: "[0-9a-f]{64}"\n')


class HomebrewError(Exception):
    """The formula cannot be rendered from these values."""


def render(version, sha256, pgrx_version, bottle=None):
    for name, value, pattern in (("version", version, r"\d+\.\d+\.\d+"), ("sha256", sha256, r"[0-9a-f]{64}"),
                                 ("cargo-pgrx version", pgrx_version, r"\d+\.\d+\.\d+")):
        if not re.fullmatch(pattern, value):
            raise HomebrewError(f"malformed {name}: {value!r}")
    formula = TEMPLATE.read_text()
    for placeholder, value in (("@VERSION@", version), ("@SHA256@", sha256), ("@PGRX_VERSION@", pgrx_version)):
        formula = formula.replace(placeholder, value)
    return formula.replace("@BOTTLE@\n", f"\n{bottle}" if bottle else "")


def bottle_block(formula, root_url):
    """The formula's one `bottle do ... end` block, if it is this release's
    root_url and sha256 lines alone."""
    blocks = re.findall(r"^  bottle do\n.*?^  end\n", formula, re.M | re.S)
    if len(blocks) != 1:
        raise HomebrewError(f"the built formula has {len(blocks)} bottle blocks, not one")
    lines = blocks[0].splitlines(keepends=True)
    body = lines[1:-1]
    if not body or body[0] != f'    root_url "{root_url}"\n':
        raise HomebrewError(f"the bottle block's root_url is not {root_url}")
    rest = body[1:]
    if rest and re.fullmatch(r"    rebuild \d+\n", rest[0]):
        rest = rest[1:]
    if not rest or not all(BOTTLE_SHA.fullmatch(line) for line in rest):
        raise HomebrewError("the bottle block holds more than sha256 lines")
    return blocks[0]


def from_source(tarball):
    """The release's version, the tarball's sha256 and its pinned cargo-pgrx."""
    tarball = Path(tarball)
    found = re.fullmatch(r"pg_warren-(\d+\.\d+\.\d+)\.tar\.gz", tarball.name)
    if not found:
        raise HomebrewError(f"{tarball.name} is not pg_warren-X.Y.Z.tar.gz")
    version = found.group(1)
    with tarfile.open(tarball) as archive:
        lock = archive.extractfile(f"pg_warren-{version}/Cargo.lock")
        if lock is None:
            raise HomebrewError(f"{tarball.name} has no Cargo.lock")
        pinned = pgrx_version(lock.read().decode())
    return version, hashlib.sha256(tarball.read_bytes()).hexdigest(), pinned


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("tarball", type=Path)
    parser.add_argument("--bottle-from", type=Path)
    args = parser.parse_args(argv)
    try:
        version, sha256, pgrx = from_source(args.tarball)
        bottle = None
        if args.bottle_from:
            bottle = bottle_block(args.bottle_from.read_text(), f"{RELEASES}/v{version}")
        sys.stdout.write(render(version, sha256, pgrx, bottle))
    except (HomebrewError, PackagingError, OSError, KeyError, tarfile.TarError) as error:
        print(f"release/homebrew.py: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
