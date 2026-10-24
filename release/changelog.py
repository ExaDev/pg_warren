#!/usr/bin/env python3
"""CHANGELOG.md's release edits (CONTRIBUTING.md, "Releases and upgrades"):
the version in development has the section `## X.Y.Z — in development`; a
release dates it (`## X.Y.Z — title (YYYY-MM-DD)`), the next version opens a
new one above it, and a release's notes are its section's body.

    release/changelog.py notes X.Y.Z   # print X.Y.Z's notes
"""

import re
import sys
from pathlib import Path

HEADING = re.compile(r"^## (\S+)(?: |$)", re.M)


class ChangelogError(Exception):
    """CHANGELOG.md does not have the section a release edit needs."""


def in_development(version):
    return f"## {version} — in development"


def finalise(text, version, date, title=None):
    """`text` with `version`'s section in development dated `date` (and
    named `title`)."""
    heading = in_development(version)
    if not re.search(rf"^{re.escape(heading)}$", text, re.M):
        raise ChangelogError(f'CHANGELOG.md has no section "{heading}"')
    released = f"## {version} — {title} ({date})" if title else f"## {version} ({date})"
    return re.sub(rf"^{re.escape(heading)}$", lambda _: released, text, count=1, flags=re.M)


def open_next(text, version):
    """`text` with a section in development for `version` above the first
    version's."""
    if any(found == version for found in HEADING.findall(text)):
        raise ChangelogError(f"CHANGELOG.md's {version} already has a section")
    first = HEADING.search(text)
    at = first.start() if first else len(text)
    return f"{text[:at]}{in_development(version)}\n\n{text[at:]}"


def notes(text, version):
    """The body of `version`'s section, without its heading."""
    headings = list(HEADING.finditer(text))
    for index, heading in enumerate(headings):
        if heading.group(1) == version:
            end = headings[index + 1].start() if index + 1 < len(headings) else len(text)
            body = text[text.index("\n", heading.start()) + 1:end].strip("\n")
            return f"{body}\n" if body else ""
    raise ChangelogError(f"CHANGELOG.md has no section for {version}")


def main(argv):
    if len(argv) != 2 or argv[0] != "notes":
        print(__doc__, file=sys.stderr)
        return 2
    try:
        sys.stdout.write(notes(Path("CHANGELOG.md").read_text(), argv[1]))
    except ChangelogError as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
