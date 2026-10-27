"""Pack and verify immutable, signed apt snapshots for the shared Pages site.

The release job signs the generated checksum manifest, not this helper. Restore
needs only the README-pinned primary fingerprint: the archive's bounded public
key bootstraps a private verification keyring, never the user's GPG configuration.
"""

import gzip
import hashlib
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tarfile
import tempfile


MAX_ARCHIVE_BYTES = 1 << 30
MAX_UNPACKED_BYTES = 1 << 30
MAX_MEMBERS = 100_000
MAX_KEY_BYTES = 1 << 20
MAX_MANIFEST_BYTES = 4096
MAX_SIGNATURE_BYTES = 1 << 20
DISTRIBUTIONS = ("bookworm", "trixie", "jammy", "noble")


class PagesArchiveError(ValueError):
    """An apt snapshot could not be packed or safely authenticated."""


def snapshot_names(version):
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise PagesArchiveError(f"invalid snapshot version: {version!r}")
    prefix = f"pg_warren-apt-{version}"
    return f"{prefix}.tar.gz", f"{prefix}.sha256", f"{prefix}.sha256.asc"


def _name(name, directory=False):
    if "\\" in name or any(ord(char) < 32 for char in name):
        raise PagesArchiveError(f"unsafe archive path: {name!r}")
    clean = name[:-1] if directory and name.endswith("/") else name
    parts = clean.split("/")
    if any(part in ("", ".", "..") for part in parts) or parts[0] != "apt":
        raise PagesArchiveError(f"archive path must stay within apt/: {name!r}")
    if len(parts) == 1 and not directory:
        raise PagesArchiveError("apt must be a directory")
    return clean


def _require_tree(apt):
    required = [apt / "pg_warren.asc", *(apt / "dists" / distro / "InRelease" for distro in DISTRIBUTIONS)]
    for file in required:
        if not file.is_file() or file.is_symlink():
            raise PagesArchiveError(f"missing required apt file: {file.relative_to(apt)}")
    if (apt / "pg_warren.asc").stat().st_size > MAX_KEY_BYTES:
        raise PagesArchiveError("apt public key exceeds size limit")
    pool = apt / "pool"
    if not pool.is_dir() or pool.is_symlink() or not any(
            file.is_file() and not file.is_symlink() for file in pool.rglob("*.deb")):
        raise PagesArchiveError("apt pool must contain at least one regular .deb package")


def _bounded_file(file, maximum, label):
    file = Path(file)
    if not file.is_file() or file.is_symlink() or file.stat().st_size > maximum:
        raise PagesArchiveError(f"{label} is missing, not regular, or exceeds size limit")
    with file.open("rb") as stream:
        value = stream.read(maximum + 1)
    if len(value) > maximum:
        raise PagesArchiveError(f"{label} exceeds size limit")
    return value


def _hash(file):
    digest = hashlib.sha256()
    with Path(file).open("rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def pack_apt(apt_directory, version, output_directory):
    """Create deterministic apt/... tar+checksum files; return their two Paths."""
    apt = Path(apt_directory)
    if not apt.is_dir() or apt.is_symlink():
        raise PagesArchiveError("apt source must be a real directory")
    _require_tree(apt)
    entries = [("apt", apt)]
    for directory, dirs, files in os.walk(apt, followlinks=False):
        for name in sorted(dirs + files):
            file = Path(directory) / name
            entry = "apt/" + file.relative_to(apt).as_posix()
            mode = file.lstat().st_mode
            if not stat.S_ISDIR(mode) and not stat.S_ISREG(mode):
                raise PagesArchiveError(f"apt source must contain regular files and directories only: {entry}")
            _name(entry, stat.S_ISDIR(mode))
            entries.append((entry, file))
    entries.sort(key=lambda entry: entry[0])
    if len(entries) > MAX_MEMBERS:
        raise PagesArchiveError("apt snapshot exceeds member count limit")
    # Include tar headers, file padding, end blocks, and the final record.
    tar_bytes = sum(512 + ((file.stat().st_size + 511) // 512) * 512
                    if file.is_file() else 512 for _, file in entries) + 1024
    tar_bytes = ((tar_bytes + 10239) // 10240) * 10240
    if tar_bytes > MAX_UNPACKED_BYTES:
        raise PagesArchiveError("apt snapshot exceeds unpacked size limit")
    archive_name, checksum_name, _ = snapshot_names(version)
    output = Path(output_directory)
    output.mkdir(parents=True, exist_ok=True)
    archive, checksum = output / archive_name, output / checksum_name
    if archive.exists() or archive.is_symlink() or checksum.exists() or checksum.is_symlink():
        raise PagesArchiveError("snapshot output already exists; immutable assets cannot be replaced")
    created = []
    try:
        with archive.open("xb") as raw:
            created.append(archive)
            with gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w|", format=tarfile.USTAR_FORMAT) as target:
                    for name, file in entries:
                        info = tarfile.TarInfo(name)
                        info.uid = info.gid = info.mtime = 0
                        info.uname = info.gname = "root"
                        if file.is_dir():
                            info.type, info.mode = tarfile.DIRTYPE, 0o755
                            target.addfile(info)
                        else:
                            info.mode, info.size = 0o644, file.stat().st_size
                            with file.open("rb") as contents:
                                target.addfile(info, contents)
        if archive.stat().st_size > MAX_ARCHIVE_BYTES:
            raise PagesArchiveError("compressed archive exceeds size limit")
        with checksum.open("x", encoding="ascii", newline="\n") as manifest:
            created.append(checksum)
            manifest.write(f"{_hash(archive)}  {archive_name}\n")
    except (OSError, ValueError, tarfile.TarError) as error:
        for file in created:
            file.unlink()
        if isinstance(error, PagesArchiveError):
            raise
        raise PagesArchiveError(f"cannot pack apt snapshot: {error}") from error
    return archive, checksum


class _StrictTarInfo(tarfile.TarInfo):
    def _proc_member(self, source):
        # Guard the dispatch point before tarfile consumes PAX/GNU metadata.
        # Overriding public frombuf is insufficient on Python 3.14, which uses
        # a private _frombuf directly. Both 3.9 and 3.14 dispatch raw headers
        # here before interpreting them or exposing a file to the caller.
        if self.type not in (tarfile.REGTYPE, tarfile.AREGTYPE, tarfile.DIRTYPE):
            raise PagesArchiveError(f"unsupported archive member/header type: {self.name!r}")
        return super()._proc_member(source)


class _LimitedReader:
    def __init__(self, source):
        self.source, self.count = source, 0

    def read(self, size=-1):
        remaining = MAX_UNPACKED_BYTES - self.count
        block = self.source.read(min(size, remaining + 1) if size >= 0 else remaining + 1)
        self.count += len(block)
        if self.count > MAX_UNPACKED_BYTES:
            raise PagesArchiveError("archive expansion exceeds unpacked size limit")
        return block


def _unpack(archive, staging):
    names, total = set(), 0
    with gzip.open(archive, "rb") as compressed:
        limited = _LimitedReader(compressed)
        with tarfile.open(fileobj=limited, mode="r|", tarinfo=_StrictTarInfo) as source:
            for member in source:
                name = _name(member.name, member.isdir())
                if name in names:
                    raise PagesArchiveError(f"duplicate archive member: {name}")
                names.add(name)
                if len(names) > MAX_MEMBERS:
                    raise PagesArchiveError("archive exceeds member count limit")
                if member.size < 0 or member.isdir() and member.size:
                    raise PagesArchiveError(f"invalid archive member size: {name}")
                total += member.size
                if total > MAX_UNPACKED_BYTES:
                    raise PagesArchiveError("archive exceeds unpacked size limit")
                if name == "apt/pg_warren.asc" and member.size > MAX_KEY_BYTES:
                    raise PagesArchiveError("apt public key exceeds size limit")
                destination = staging / name
                if member.isdir():
                    destination.mkdir(parents=True, exist_ok=True)
                    destination.chmod(0o755)
                else:
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    with destination.open("xb") as target:
                        contents = source.extractfile(member)
                        remaining = member.size
                        while remaining:
                            block = contents.read(min(1 << 20, remaining))
                            if not block:
                                raise PagesArchiveError(f"truncated archive member: {name}")
                            target.write(block)
                            remaining -= len(block)
                    destination.chmod(0o644)
            # Consume the rest, including compressed padding/trailing data, so
            # the expansion cap cannot be bypassed after tar's end marker.
            while limited.read(1 << 20):
                pass


def _gpg(*arguments):
    try:
        result = subprocess.run(arguments, capture_output=True, timeout=60)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise PagesArchiveError(f"GPG verification could not run: {error}") from error
    if result.returncode:
        raise PagesArchiveError("GPG signature/key verification failed: " + result.stderr.decode("utf8", "replace").strip())
    return result.stdout


def _verify_authenticity(apt, checksum, signature, expected_fingerprint):
    with tempfile.TemporaryDirectory(prefix="pg-warren-verify-") as temporary:
        home = Path(temporary)
        home.chmod(0o700)
        common = ("gpg", "--no-options", "--homedir", str(home), "--batch", "--no-autostart")
        _gpg(*common, "--import", str(apt / "pg_warren.asc"))
        listing = _gpg(*common, "--with-colons", "--fingerprint", "--list-keys").decode("ascii")
        primary, pending = [], False
        for line in listing.splitlines():
            columns = line.split(":")
            if columns[0] == "pub":
                pending = True
            elif columns[0] == "fpr" and pending:
                primary.append(columns[9])
                pending = False
            elif columns[0] == "sub":
                pending = False
        if primary != [expected_fingerprint]:
            raise PagesArchiveError("apt public key primary fingerprint does not match the configured trust anchor")
        keyring = home / "trusted.gpg"
        keyring.write_bytes(_gpg(*common, "--export", expected_fingerprint))
        verify = ("gpgv", "--homedir", str(home), "--keyring", str(keyring))
        _gpg(*verify, str(signature), str(checksum))
        for distro in DISTRIBUTIONS:
            _gpg(*verify, str(apt / "dists" / distro / "InRelease"))


def restore_apt(archive, checksum, signature, expected_fingerprint, destination):
    """Authenticate and stage apt, then install it without touching web files."""
    archive, checksum, signature, destination = map(Path, (archive, checksum, signature, destination))
    expected_fingerprint = re.sub(r"\s", "", expected_fingerprint).upper()
    if not re.fullmatch(r"[0-9A-F]{40}", expected_fingerprint):
        raise PagesArchiveError("expected fingerprint must be a 40-digit primary fingerprint")
    match = re.fullmatch(r"pg_warren-apt-([0-9]+\.[0-9]+\.[0-9]+)\.tar\.gz", archive.name)
    if not match:
        raise PagesArchiveError("invalid apt archive filename")
    names = snapshot_names(match[1])
    if checksum.name != names[1] or signature.name != names[2]:
        raise PagesArchiveError("checksum/signature filenames do not match the snapshot version")
    if not archive.is_file() or archive.is_symlink() or archive.stat().st_size > MAX_ARCHIVE_BYTES:
        raise PagesArchiveError("archive is missing, not regular, or exceeds size limit")
    manifest = _bounded_file(checksum, MAX_MANIFEST_BYTES, "checksum manifest")
    _bounded_file(signature, MAX_SIGNATURE_BYTES, "signature")
    expected_manifest = f"{_hash(archive)}  {archive.name}\n".encode("ascii")
    if manifest != expected_manifest:
        raise PagesArchiveError("checksum manifest must contain exactly the matching archive hash and filename")
    if destination.is_symlink() or (destination.exists() and not destination.is_dir()):
        raise PagesArchiveError("destination must be a real directory")
    if (destination / "apt").exists() or (destination / "apt").is_symlink():
        raise PagesArchiveError("destination apt/ already exists; refusing extraction collision")
    if not destination.parent.is_dir():
        raise PagesArchiveError("destination parent directory must exist")
    try:
        with tempfile.TemporaryDirectory(prefix=".pg-warren-apt-", dir=destination.parent) as temporary:
            staging = Path(temporary)
            _unpack(archive, staging)
            apt = staging / "apt"
            _require_tree(apt)
            _verify_authenticity(apt, checksum, signature, expected_fingerprint)
            destination.mkdir(exist_ok=True)
            # Same filesystem staging means no partial extraction is exposed.
            # mkdir gives a fail-closed collision check even for an empty apt/.
            target = destination / "apt"
            target.mkdir()
            try:
                apt.replace(target)
            except BaseException:
                target.rmdir()
                raise
    except (OSError, ValueError, tarfile.TarError, EOFError) as error:
        if isinstance(error, PagesArchiveError):
            raise
        raise PagesArchiveError(f"cannot restore apt snapshot: {error}") from error


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    try:
        if len(argv) == 4 and argv[0] == "pack":
            pack_apt(*argv[1:])
        elif len(argv) == 6 and argv[0] == "restore":
            restore_apt(*argv[1:])
        else:
            raise PagesArchiveError("usage: pages_archive.py pack APT_DIR VERSION OUT_DIR | restore ARCHIVE CHECKSUM SIGNATURE FINGERPRINT DESTINATION")
    except (PagesArchiveError, OSError) as error:
        print(f"release/pages_archive.py: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
