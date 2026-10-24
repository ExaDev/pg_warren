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
debhelper renders it; its control file has only the fields
packaging/debian's control.in gives a binary package, with control.in's
values, a Depends of postgresql-NN and the C libraries a pgrx library
links, and the name, Version and Architecture of a package of X.Y.Z for a
supported major and distribution; and its files are pg_warren's library,
extension files and docs, regular files of mode 0644, in directories of
mode 0755 on their paths only, all owned by root.
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
CONTROL_IN = ROOT / "packaging" / "debian" / "control.in"
ARCHITECTURES = ("amd64", "arm64")
CONTROL_MEMBERS = {"control", "md5sums", "postinst"}
# The fields dh_gencontrol writes for control.in's binary package: those
# control.in gives it or its source, and those debhelper computes.
FROM_CONTROL_IN = ("Source", "Maintainer", "Section", "Priority", "Homepage", "Description")
FIELDS = {"Package", "Version", "Architecture", "Installed-Size", "Depends", *FROM_CONTROL_IN}
# ${shlibs:Depends} of a pgrx library: libc (with libm) and libgcc_s.
LIBRARIES = ("libc6", "libgcc-s1")
ROOT_OWNED = (0, 0, "root", "root")
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
    found = {}
    for member in archive.getmembers():
        name = member.name.removeprefix("./").rstrip("/")
        name = "" if name == "." else name
        if name in found:
            raise AssetError(f"{deb.name}: {name or '.'} is in its {part} archive twice")
        found[name] = (member, archive)
    return found


def fields(deb, control):
    """A control file's fields, each value with its continuation lines."""
    values, name = {}, None
    for line in control.splitlines():
        if line.startswith(" ") and name:
            values[name] += "\n" + line
            continue
        name, _, value = line.partition(": ")
        if name in values:
            raise AssetError(f"{deb.name}: control gives {name} twice")
        values[name] = value
    return values


def control_in(major):
    """control.in's fields for the binary package of `major` as
    dh_gencontrol writes them: the binary stanza's over the source's."""
    source, binary = (fields(CONTROL_IN, stanza) for stanza in
                      CONTROL_IN.read_text().replace("PGVERSION", major).split("\n\n"))
    return {**source, **binary}


def check_depends(deb, depends, major):
    """Depends as debhelper renders control.in's: postgresql-NN, and the
    C libraries ${shlibs:Depends} names, at a minimum version."""
    entries = [entry.strip() for entry in depends.split(",")]
    libraries = [e for e in entries if e != f"postgresql-{major}"]
    library = rf"(?:{'|'.join(map(re.escape, LIBRARIES))}) \(>= [0-9][0-9A-Za-z.+~-]*\)"
    if (len(libraries) != len(entries) - 1 or len({e.partition(" ")[0] for e in libraries}) != len(libraries)
            or not all(re.fullmatch(library, e) for e in libraries)):
        raise AssetError(f"{deb.name}: Depends {depends!r} is not postgresql-{major} and {', '.join(LIBRARIES)}")


def check_fields(deb, values, major):
    """Only the fields of control.in's binary package, with its values."""
    if set(values) != FIELDS:
        unexpected = sorted(set(values) ^ FIELDS)
        raise AssetError(f"{deb.name}: control gives or lacks {', '.join(unexpected)}, unlike control.in")
    expected = control_in(major)
    for name in FROM_CONTROL_IN:
        if values[name] != expected[name]:
            raise AssetError(f"{deb.name}: {name} {values[name]!r} is not control.in's")
    if not re.fullmatch(r"[0-9]+", values["Installed-Size"]):
        raise AssetError(f"{deb.name}: Installed-Size {values['Installed-Size']!r} is not a size")
    check_depends(deb, values["Depends"], major)


def check_control(deb, version):
    control = {name: found for name, found in members(deb, "ctrl").items() if name}
    names = set(control)
    if names != CONTROL_MEMBERS:
        raise AssetError(f"{deb.name}: control archive holds {sorted(names)}, not {sorted(CONTROL_MEMBERS)}")
    if not all(member.isreg() for member, _ in control.values()):
        raise AssetError(f"{deb.name}: its control archive holds other than regular files")
    read = {name: archive.extractfile(member).read() for name, (member, archive) in control.items()}
    values = fields(deb, read["control"].decode())
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
    check_fields(deb, values, major.group(1))
    return values, major.group(1)


def allowed(path, major):
    library = f"usr/lib/postgresql/{major}/lib"
    extension = f"usr/share/postgresql/{major}/extension"
    doc = f"usr/share/doc/postgresql-{major}-pg-warren"
    return (path == f"{library}/pg_warren.so"
            or re.fullmatch(rf"{re.escape(extension)}/pg_warren(--[0-9.]+(--[0-9.]+)?\.sql|\.control)", path)
            or re.fullmatch(rf"{re.escape(doc)}/[A-Za-z0-9][A-Za-z0-9._-]*", path))


def parents(path):
    """The directories `path` is in, the root ("") included."""
    parts = path.split("/")
    return {"/".join(parts[:i]) for i in range(len(parts))}


def check_files(deb, major):
    """Regular files of mode 0644 on pg_warren's paths, in directories of
    mode 0755 on those paths, all owned by root (by name, as dpkg takes the
    owner by name where the install host knows it, and by id)."""
    found = members(deb, "fsys")
    files = {path for path, (member, _) in found.items() if not member.isdir()}
    for path in files:
        member = found[path][0]
        if not member.isreg():
            raise AssetError(f"{deb.name}: {path} is not a regular file")
        if not allowed(path, major):
            raise AssetError(f"{deb.name}: path {path} is not one of pg_warren's")
    directories = {parent for path in files for parent in parents(path)}
    for path, (member, _) in found.items():
        if member.isdir() and path not in directories:
            raise AssetError(f"{deb.name}: directory {path} holds none of pg_warren's files")
        mode = 0o755 if member.isdir() else 0o644
        if member.mode & 0o7777 != mode:
            raise AssetError(f"{deb.name}: {path or '.'} has mode {member.mode & 0o7777:04o}, not {mode:04o}")
        if (member.uid, member.gid, member.uname, member.gname) != ROOT_OWNED:
            raise AssetError(f"{deb.name}: {path or '.'} is owned by {member.uname or member.uid}:"
                             f"{member.gname or member.gid}, not root:root")
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
