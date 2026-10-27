"""Signed apt snapshots: bounded, data-only archives and pinned signing trust."""

import hashlib
import io
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile

import pytest

import pages_archive as snapshots
from pages_archive import PagesArchiveError, pack_apt, restore_apt, snapshot_names


VERSION = "9.8.7"
FINGERPRINT = "A" * 40


def apt_tree(directory):
    apt = directory / "input"
    apt.mkdir()
    (apt / "pg_warren.asc").write_bytes(b"public key placeholder")
    for codename in ("bookworm", "trixie", "jammy", "noble"):
        index = apt / "dists" / codename / "InRelease"
        index.parent.mkdir(parents=True)
        index.write_bytes(b"signed index " + codename.encode())
    pool = apt / "pool" / "main" / "p" / "pg-warren"
    pool.mkdir(parents=True)
    (pool / "package.deb").write_bytes(b"\x00\xffexample package\n")
    return apt


def unsigned_snapshot(tmp_path):
    apt = apt_tree(tmp_path)
    archive, checksum = pack_apt(apt, VERSION, tmp_path / "assets")
    signature = checksum.with_name(checksum.name + ".asc")
    signature.write_bytes(b"signature placeholder")
    return apt, archive, checksum, signature


@pytest.fixture
def structure_only(monkeypatch):
    # Structural unit tests do not require GPG. Real trust failures are covered
    # separately with signatures produced by a disposable test key below.
    monkeypatch.setattr(snapshots, "_verify_authenticity", lambda *args: None)


def test_names_are_versioned_and_reject_path_injection():
    assert snapshot_names(VERSION) == (
        "pg_warren-apt-9.8.7.tar.gz", "pg_warren-apt-9.8.7.sha256", "pg_warren-apt-9.8.7.sha256.asc")
    for version in ("../escape", "1.2", "v1.2.3", "1.2.3\n", "1.2.3/evil"):
        with pytest.raises(PagesArchiveError, match="version"):
            snapshot_names(version)


def test_pack_is_deterministic_and_dereferences_hardlinks(tmp_path):
    apt = apt_tree(tmp_path)
    os.link(apt / "pg_warren.asc", apt / "same-key.asc")
    first, sums = pack_apt(apt, VERSION, tmp_path / "first")
    second, _ = pack_apt(apt, VERSION, tmp_path / "second")
    assert first.read_bytes() == second.read_bytes()
    assert sums.read_text() == f"{hashlib.sha256(first.read_bytes()).hexdigest()}  {first.name}\n"
    with tarfile.open(first) as archive:
        assert all(member.isdir() or member.isreg() for member in archive)


def test_pack_rejects_symlinks_and_existing_outputs(tmp_path):
    apt = apt_tree(tmp_path)
    (apt / "link").symlink_to(apt / "pg_warren.asc")
    with pytest.raises(PagesArchiveError, match="regular|symbolic|symlink"):
        pack_apt(apt, VERSION, tmp_path / "bad")
    (apt / "link").unlink()
    pack_apt(apt, VERSION, tmp_path / "assets")
    with pytest.raises(PagesArchiveError, match="exist"):
        pack_apt(apt, VERSION, tmp_path / "assets")


def test_restore_preserves_bytes_and_website_files(tmp_path, structure_only):
    apt, archive, checksum, signature = unsigned_snapshot(tmp_path)
    destination = tmp_path / "site"
    destination.mkdir()
    (destination / "index.html").write_text("website")
    restore_apt(archive, checksum, signature, FINGERPRINT, destination)
    assert (destination / "index.html").read_text() == "website"
    for original in apt.rglob("*"):
        if original.is_file():
            assert (destination / "apt" / original.relative_to(apt)).read_bytes() == original.read_bytes()
    with pytest.raises(PagesArchiveError, match="exist|collision"):
        restore_apt(archive, checksum, signature, FINGERPRINT, destination)


def test_rejects_wrong_hash_and_nonexact_checksum_manifest(tmp_path, structure_only):
    _, archive, checksum, signature = unsigned_snapshot(tmp_path)
    original = checksum.read_text()
    for manifest in ("0" * 64 + f"  {archive.name}\n", original + original,
                     original.replace(archive.name, "elsewhere.tar.gz")):
        checksum.write_text(manifest)
        destination = tmp_path / "site"
        with pytest.raises(PagesArchiveError, match="checksum|manifest"):
            restore_apt(archive, checksum, signature, FINGERPRINT, destination)
        assert not destination.exists()


def custom_snapshot(tmp_path, entries, archive_format=tarfile.USTAR_FORMAT):
    archive_name, checksum_name, signature_name = snapshot_names(VERSION)
    archive = tmp_path / archive_name
    with tarfile.open(archive, "w:gz", format=archive_format) as target:
        for name, kind, value in entries:
            info = tarfile.TarInfo(name)
            info.type = kind
            if kind == tarfile.REGTYPE:
                info.size = len(value)
                target.addfile(info, io.BytesIO(value))
            else:
                info.linkname = value
                target.addfile(info)
    checksum = tmp_path / checksum_name
    checksum.write_text(f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive_name}\n")
    signature = tmp_path / signature_name
    signature.write_bytes(b"signature placeholder")
    return archive, checksum, signature


@pytest.mark.parametrize("name,kind,value", [
    ("apt/../outside", tarfile.REGTYPE, b"bad"),
    ("/apt/absolute", tarfile.REGTYPE, b"bad"),
    ("apt\\backslash", tarfile.REGTYPE, b"bad"),
    ("index.html", tarfile.REGTYPE, b"bad"),
    ("apt/link", tarfile.SYMTYPE, "../outside"),
    ("apt/hardlink", tarfile.LNKTYPE, "apt/pg_warren.asc"),
    ("apt/fifo", tarfile.FIFOTYPE, ""),
])
def test_rejects_unsafe_paths_and_member_types(tmp_path, structure_only, name, kind, value):
    archive, checksum, signature = custom_snapshot(tmp_path, [(name, kind, value)])
    destination = tmp_path / "site"
    with pytest.raises(PagesArchiveError):
        restore_apt(archive, checksum, signature, FINGERPRINT, destination)
    assert not destination.exists()


def test_rejects_duplicates_and_pax_headers(tmp_path, structure_only):
    entries = [("apt/key", tarfile.REGTYPE, b"a"), ("apt/key", tarfile.REGTYPE, b"b")]
    archive, checksum, signature = custom_snapshot(tmp_path, entries)
    with pytest.raises(PagesArchiveError, match="duplicate"):
        restore_apt(archive, checksum, signature, FINGERPRINT, tmp_path / "site")
    # A long path forces a PAX extended header which must be rejected even
    # though tarfile ordinarily consumes that header before exposing members.
    entries = [("apt/" + "x" * 260, tarfile.REGTYPE, b"a")]
    archive, checksum, signature = custom_snapshot(tmp_path, entries, tarfile.PAX_FORMAT)
    with pytest.raises(PagesArchiveError, match="header|PAX|pax"):
        restore_apt(archive, checksum, signature, FINGERPRINT, tmp_path / "site")


def test_rejects_expansion_and_member_count_limits(tmp_path, structure_only, monkeypatch):
    _, archive, checksum, signature = unsigned_snapshot(tmp_path)
    monkeypatch.setattr(snapshots, "MAX_UNPACKED_BYTES", 1024)
    with pytest.raises(PagesArchiveError, match="size|limit|large"):
        restore_apt(archive, checksum, signature, FINGERPRINT, tmp_path / "site")
    monkeypatch.setattr(snapshots, "MAX_UNPACKED_BYTES", 1 << 30)
    monkeypatch.setattr(snapshots, "MAX_MEMBERS", 1)
    with pytest.raises(PagesArchiveError, match="member|limit"):
        restore_apt(archive, checksum, signature, FINGERPRINT, tmp_path / "site")


def test_missing_required_indexes_is_rejected_before_publication(tmp_path, structure_only):
    apt = apt_tree(tmp_path)
    (apt / "dists/noble/InRelease").unlink()
    with pytest.raises(PagesArchiveError, match="InRelease|noble"):
        pack_apt(apt, VERSION, tmp_path / "assets")


def test_missing_packages_is_rejected_before_publication(tmp_path):
    apt = apt_tree(tmp_path)
    shutil.rmtree(apt / "pool")
    with pytest.raises(PagesArchiveError, match="pool|package"):
        pack_apt(apt, VERSION, tmp_path / "assets")


def command(*args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, **kwargs).stdout


@pytest.fixture(scope="module")
def signing_key():
    if not shutil.which("gpg") or not shutil.which("gpgv"):
        pytest.skip("needs gpg and gpgv")
    # macOS's default pytest paths exceed the Unix-domain socket name limit
    # for gpg-agent; use a short private directory on both Linux and macOS.
    temporary = tempfile.TemporaryDirectory(prefix="pwa-", dir="/tmp")
    home = Path(temporary.name)
    home.chmod(0o700)
    command("gpg", "--no-options", "--homedir", str(home), "--batch", "--pinentry-mode", "loopback",
            "--passphrase", "", "--quick-generate-key", "pg_warren test", "ed25519", "sign", "0")
    listing = command("gpg", "--no-options", "--homedir", str(home), "--with-colons", "--list-keys").decode()
    fingerprint = next(line.split(":")[9] for line in listing.splitlines() if line.startswith("fpr:"))
    key = command("gpg", "--no-options", "--homedir", str(home), "--armor", "--export", fingerprint)
    yield home, fingerprint, key
    subprocess.run(["gpgconf", "--homedir", str(home), "--kill", "gpg-agent"], capture_output=True)
    temporary.cleanup()


def signed_snapshot(tmp_path, signing_key):
    home, fingerprint, key = signing_key
    apt = apt_tree(tmp_path)
    (apt / "pg_warren.asc").write_bytes(key)
    for codename in ("bookworm", "trixie", "jammy", "noble"):
        index = apt / "dists" / codename / "InRelease"
        content = index.read_bytes()
        index.unlink()
        command("gpg", "--no-options", "--homedir", str(home), "--batch", "--yes", "--armor",
                "--local-user", fingerprint, "--output", str(index), "--clearsign", "-", input=content)
    archive, checksum = pack_apt(apt, VERSION, tmp_path / "assets")
    signature = checksum.with_name(checksum.name + ".asc")
    command("gpg", "--no-options", "--homedir", str(home), "--batch", "--yes", "--armor",
            "--local-user", fingerprint, "--output", str(signature), "--detach-sign", str(checksum))
    return apt, archive, checksum, signature


def test_real_gpg_verification_and_wrong_fingerprint(tmp_path, signing_key):
    apt, archive, checksum, signature = signed_snapshot(tmp_path, signing_key)
    _, fingerprint, _ = signing_key
    destination = tmp_path / "site"
    restore_apt(archive, checksum, signature, fingerprint, destination)
    assert (destination / "apt/pg_warren.asc").read_bytes() == (apt / "pg_warren.asc").read_bytes()
    with pytest.raises(PagesArchiveError, match="fingerprint|key"):
        restore_apt(archive, checksum, signature, FINGERPRINT, tmp_path / "wrong-key")
    assert not (tmp_path / "wrong-key").exists()


def test_real_gpg_rejects_tampered_signature(tmp_path, signing_key):
    _, archive, checksum, signature = signed_snapshot(tmp_path, signing_key)
    signature.write_bytes(b"not a valid signature")
    with pytest.raises(PagesArchiveError, match="signature|GPG|gpg"):
        restore_apt(archive, checksum, signature, signing_key[1], tmp_path / "site")
    assert not (tmp_path / "site").exists()


def test_real_gpg_rejects_invalid_index_even_when_snapshot_manifest_is_signed(tmp_path, signing_key):
    apt, _, _, _ = signed_snapshot(tmp_path, signing_key)
    (apt / "dists/noble/InRelease").write_bytes(b"an unsigned index")
    archive, checksum = pack_apt(apt, VERSION, tmp_path / "bad-index")
    signature = checksum.with_name(checksum.name + ".asc")
    home, fingerprint, _ = signing_key
    command("gpg", "--no-options", "--homedir", str(home), "--batch", "--yes", "--armor",
            "--local-user", fingerprint, "--output", str(signature), "--detach-sign", str(checksum))
    with pytest.raises(PagesArchiveError, match="GPG|signature"):
        restore_apt(archive, checksum, signature, fingerprint, tmp_path / "site")
    assert not (tmp_path / "site").exists()
