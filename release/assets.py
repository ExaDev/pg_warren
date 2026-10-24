#!/usr/bin/env python3
"""The publish job's checks of the packages before anything is signed or
published (CONTRIBUTING.md, "Packages"). The read-only jobs that built them
ran the release's code; these checks bound what a compromised build can
get signed into the apt repository.

    release/assets.py check-deb DEB X.Y.Z      # pg_warren's files, our postinst, X.Y.Z-1~<suffix>
    release/assets.py listed-debs DIR          # the .deb files DIR/SHA256SUMS lists, each checked
    release/assets.py readme-fingerprint       # the apt key's fingerprint, as README.md states it

A package passes `check-deb` only if its control archive holds `control`,
`md5sums` and a `postinst` byte-equal to packaging/debian's template as
debhelper renders it, its files are pg_warren's library, extension files
and docs, all regular files, and its name, Version and Architecture are
those of a package of X.Y.Z for a supported major and distribution.
"""

import hashlib
import io
import re
import subprocess
import sys
import tarfile
from pathlib import Path

from packages import DISTRIBUTIONS, MAJORS, ROOT

POSTINST = ROOT / "packaging" / "debian" / "pg-warren.postinst.in"
ARCHITECTURES = ("amd64", "arm64")
CONTROL_MEMBERS = {"control", "md5sums", "postinst"}
DEB_NAME = re.compile(r"postgresql-\d+-pg-warren_\d+\.\d+\.\d+-1\.[a-z0-9.]+_[a-z0-9]+\.deb")
FINGERPRINT = re.compile(r"`([0-9A-F ]{40,60})` <!-- apt key -->")


class AssetError(Exception):
    """A package is not one the publish job may sign or publish."""


def asset_name(package, version, arch):
    """The file name of a package as a GitHub Release asset, which turns
    the revision's tilde into a dot."""
    return f"{package}_{version}_{arch}.deb".replace("~", ".")


def expected_postinst(major):
    """packaging/debian's postinst for `major`, as debhelper builds it: its
    #DEBHELPER# marker left empty (the package has no other snippets)."""
    return POSTINST.read_text().replace("PGVERSION", major).replace("#DEBHELPER#", "").encode()


def members(deb, part):
    done = subprocess.run(["dpkg-deb", f"--{part}-tarfile", str(deb)], capture_output=True)
    if done.returncode != 0:
        raise AssetError(f"{deb.name} is not a package: {done.stderr.decode().strip()}")
    archive = tarfile.open(fileobj=io.BytesIO(done.stdout))
    named = ((m.name.removeprefix("./").rstrip("/"), m) for m in archive.getmembers())
    return {name: (member, archive) for name, member in named if name not in ("", ".")}


def fields(control):
    return dict(line.split(": ", 1) for line in control.splitlines() if ": " in line and not line.startswith(" "))


def check_control(deb, version):
    control = members(deb, "ctrl")
    names = set(control)
    if names != CONTROL_MEMBERS:
        raise AssetError(f"{deb.name}: control archive holds {sorted(names)}, not {sorted(CONTROL_MEMBERS)}")
    read = {name: archive.extractfile(member).read() for name, (member, archive) in control.items()}
    values = fields(read["control"].decode())
    major = re.fullmatch(r"postgresql-(\d+)-pg-warren", values.get("Package", ""))
    if not major or major.group(1) not in MAJORS:
        raise AssetError(f"{deb.name}: Package {values.get('Package')!r} is not pg_warren's for PG 15-18")
    suffixes = "|".join(re.escape(d.suffix) for d in DISTRIBUTIONS)
    if not re.fullmatch(rf"{re.escape(version)}-1~(?:{suffixes})", values.get("Version", "")):
        raise AssetError(f"{deb.name}: Version {values.get('Version')!r} is not {version}-1~<distribution>")
    if values.get("Architecture") not in ARCHITECTURES:
        raise AssetError(f"{deb.name}: Architecture {values.get('Architecture')!r} is not amd64 or arm64")
    if deb.name != asset_name(values["Package"], values["Version"], values["Architecture"]):
        raise AssetError(f"{deb.name}: the file name is not the package's")
    if read["postinst"] != expected_postinst(major.group(1)):
        raise AssetError(f"{deb.name}: postinst is not packaging/debian's")
    return values, major.group(1)


def allowed(path, major):
    library = f"usr/lib/postgresql/{major}/lib"
    extension = f"usr/share/postgresql/{major}/extension"
    doc = f"usr/share/doc/postgresql-{major}-pg-warren"
    return (path == f"{library}/pg_warren.so"
            or re.fullmatch(rf"{re.escape(extension)}/pg_warren(--[0-9.]+(--[0-9.]+)?\.sql|\.control)", path)
            or re.fullmatch(rf"{re.escape(doc)}/[A-Za-z0-9][A-Za-z0-9._-]*", path))


def check_files(deb, major):
    files = {}
    for path, (member, _) in members(deb, "fsys").items():
        if member.isdir():
            continue
        if not member.isreg() or member.mode & 0o7000:
            raise AssetError(f"{deb.name}: {path} is not a regular file without special mode bits")
        if not allowed(path, major):
            raise AssetError(f"{deb.name}: path {path} is not one of pg_warren's")
        files[path] = member
    for required in (f"usr/lib/postgresql/{major}/lib/pg_warren.so",
                     f"usr/share/postgresql/{major}/extension/pg_warren.control"):
        if required not in files:
            raise AssetError(f"{deb.name}: no {Path(required).name}")


def check_deb(deb, version):
    """`deb`'s control fields, once it is shown to be a package of `version`
    as packaging/debian builds it."""
    deb = Path(deb)
    values, major = check_control(deb, version)
    check_files(deb, major)
    return values


def listed_debs(sums):
    """`{name: sha256}` of the .deb files a SHA256SUMS lists."""
    listed = {}
    for line in sums.splitlines():
        digest, _, name = line.partition("  ")
        if not name.endswith(".deb"):
            continue
        if not DEB_NAME.fullmatch(name) or not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise AssetError(f"SHA256SUMS lists {name!r}, which is not a package's file name")
        listed[name] = digest
    return listed


def verify_listed(directory, sums):
    """The listed .deb files in `directory`, each matching its sha256."""
    paths = []
    for name, digest in sorted(listed_debs(sums).items()):
        path = Path(directory) / name
        if not path.is_file():
            raise AssetError(f"{name} is listed in SHA256SUMS but missing")
        if hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise AssetError(f"{name} does not match its sha256 in SHA256SUMS")
        paths.append(path)
    return paths


def readme_fingerprint(readme):
    """The apt key's fingerprint, from README.md's line marked `<!-- apt key -->`."""
    found = FINGERPRINT.search(readme)
    digits = found.group(1).replace(" ", "") if found else ""
    if not re.fullmatch(r"[0-9A-F]{40}", digits):
        raise AssetError("README.md states no apt key fingerprint (`<40 hex digits>` <!-- apt key -->)")
    return digits


def main(argv):
    try:
        if len(argv) == 3 and argv[0] == "check-deb":
            values = check_deb(argv[1], argv[2])
            print(f"ok: {values['Package']} {values['Version']} {values['Architecture']}")
        elif len(argv) == 2 and argv[0] == "listed-debs":
            for path in verify_listed(argv[1], (Path(argv[1]) / "SHA256SUMS").read_text()):
                print(path)
        elif argv == ["readme-fingerprint"]:
            print(readme_fingerprint((ROOT / "README.md").read_text()))
        else:
            print(__doc__, file=sys.stderr)
            return 2
    except (AssetError, OSError) as error:
        print(f"release/assets.py: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
