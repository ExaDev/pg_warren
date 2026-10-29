"""Tests of release/source.py, which makes a release's source assets beside
the `git archive` tarball (CONTRIBUTING.md, "Building from source"): the
vendored source tarball, which builds without network access, and the
CycloneDX SBOM; and of their checks in release/assets.py, which the publish
job runs on them as data."""

import gzip
import hashlib
import io
import json
import shutil
import tarfile
from pathlib import Path

import pytest

import assets
import source
from source import CARGO_CONFIG, SourceError, normalise_sbom, vendored, vendored_name

PREFIX = "pg_warren-1.2.3"
EPOCH = 1_793_267_268
COMMIT = "8df8c62bf01c2fb6ef6ae6f708f06fb971b76bad"
ROOT = "/tmp/build/pg_warren-1.2.3"
CHECKSUM = hashlib.sha256(b"the crate").hexdigest()

MANIFEST = b"""[workspace]
members = ["crates/a"]

[workspace.package]
version = "1.2.3"
"""

LOCK = f"""version = 4

[[package]]
name = "a"
version = "1.2.3"
dependencies = ["dep"]

[[package]]
name = "dep"
version = "0.4.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "{CHECKSUM}"
""".encode()


def archive(path, members, comment=COMMIT):
    """A tarball as `git archive --prefix` writes one: a global pax header
    naming the commit, every member dated by it."""
    with open(path, "wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", mtime=EPOCH, filename="") as zipped, \
            tarfile.open(fileobj=zipped, mode="w", format=tarfile.PAX_FORMAT,
                         pax_headers={"comment": comment}) as tar:
        for name, data in members:
            info = tarfile.TarInfo(name)
            info.mtime, info.uname, info.gname = EPOCH, "root", "root"
            if data is None:
                info.type, info.mode = tarfile.DIRTYPE, 0o775
                tar.addfile(info)
            elif isinstance(data, str):
                info.type, info.mode, info.linkname = tarfile.SYMTYPE, 0o777, data
                tar.addfile(info)
            else:
                info.size, info.mode = len(data), 0o664
                tar.addfile(info, io.BytesIO(data))
    return path


def source_members(lock=LOCK):
    return [
        (PREFIX, None),
        (f"{PREFIX}/Cargo.lock", lock),
        (f"{PREFIX}/Cargo.toml", MANIFEST),
        (f"{PREFIX}/crates", None),
        (f"{PREFIX}/crates/a", None),
        (f"{PREFIX}/crates/a/sql", "../../sql"),
        (f"{PREFIX}/crates/a/src", None),
        (f"{PREFIX}/crates/a/src/lib.rs", b"pub fn f() {}\n"),
    ]


def fake_vendor(directory):
    """What `cargo vendor --locked --versioned-dirs vendor` leaves for LOCK."""
    crate = Path(directory) / "vendor" / "dep-0.4.1"
    crate.mkdir(parents=True)
    manifest = b'[package]\nname = "dep"\nversion = "0.4.1"\n'
    (crate / "Cargo.toml").write_bytes(manifest)
    files = {"Cargo.toml": hashlib.sha256(manifest).hexdigest()}
    (crate / ".cargo-checksum.json").write_text(json.dumps({"files": files, "package": CHECKSUM}))


@pytest.fixture
def source_tarball(tmp_path):
    return archive(tmp_path / f"{PREFIX}.tar.gz", source_members())


def made(tmp_path, source_tarball, vendor=fake_vendor):
    out = tmp_path / "out"
    out.mkdir(exist_ok=True)
    return vendored(source_tarball, out, vendor=vendor)


def names(tarball):
    with tarfile.open(tarball) as tar:
        return [member.name for member in tar.getmembers()]


def test_the_vendored_tarball_is_named_after_the_source_s_version(tmp_path, source_tarball):
    assert made(tmp_path, source_tarball).name == "pg_warren-1.2.3-vendored.tar.gz" == vendored_name("1.2.3")


def test_it_holds_the_source_the_vendored_crates_and_the_cargo_config(tmp_path, source_tarball):
    listed = names(made(tmp_path, source_tarball))
    assert set(listed) == {name for name, _ in source_members()} | {
        f"{PREFIX}/.cargo", f"{PREFIX}/.cargo/config.toml", f"{PREFIX}/vendor", f"{PREFIX}/vendor/dep-0.4.1",
        f"{PREFIX}/vendor/dep-0.4.1/Cargo.toml", f"{PREFIX}/vendor/dep-0.4.1/.cargo-checksum.json"}
    assert listed == sorted(listed)


def test_the_cargo_config_points_crates_io_at_the_vendor_directory(tmp_path, source_tarball):
    with tarfile.open(made(tmp_path, source_tarball)) as tar:
        config = tar.extractfile(f"{PREFIX}/.cargo/config.toml").read().decode()
    assert config == CARGO_CONFIG
    assert 'replace-with = "vendored-sources"' in config and 'directory = "vendor"' in config


def test_the_tarball_is_the_same_bytes_every_time(tmp_path, source_tarball):
    first = made(tmp_path, source_tarball).read_bytes()
    shutil.rmtree(tmp_path / "out")
    assert made(tmp_path, source_tarball).read_bytes() == first


def test_it_is_dated_and_owned_as_the_source_and_names_its_commit(tmp_path, source_tarball):
    with tarfile.open(made(tmp_path, source_tarball)) as tar:
        assert tar.pax_headers == {"comment": COMMIT}
        assert {(m.mtime, m.uid, m.gid, m.uname, m.gname) for m in tar.getmembers()} == {(EPOCH, 0, 0, "root", "root")}


def test_a_source_with_its_own_cargo_config_is_refused(tmp_path):
    members = [*source_members(), (f"{PREFIX}/.cargo", None), (f"{PREFIX}/.cargo/config.toml", b"[build]\n")]
    tarball = archive(tmp_path / "s.tar.gz", members)
    with pytest.raises(SourceError, match="config.toml"):
        made(tmp_path, tarball)


def test_the_checks_pass_on_what_vendored_makes(tmp_path, source_tarball):
    assets.check_vendored(made(tmp_path, source_tarball), source_tarball)


def rewritten(tmp_path, tarball, change):
    """`tarball` with `change(members)` applied to its (name, data) list."""
    with tarfile.open(tarball) as tar:
        members = []
        for member in tar.getmembers():
            if member.isdir():
                members.append((member.name, None))
            elif member.issym():
                members.append((member.name, member.linkname))
            else:
                members.append((member.name, tar.extractfile(member).read()))
    return archive(tmp_path / "changed.tar.gz", change(members))


@pytest.mark.parametrize("change, error", [
    (lambda ms: [(n, b"pub fn g() {}\n" if n.endswith("lib.rs") else d) for n, d in ms], "lib.rs"),
    (lambda ms: [(n, d) for n, d in ms if not n.endswith("lib.rs")], "lib.rs"),
    (lambda ms: [*ms, (f"{PREFIX}/build.rs", b"fn main() {}\n")], "build.rs"),
    (lambda ms: [*ms, (f"{PREFIX}/vendor/evil-1.0.0", None)], "evil-1.0.0"),
    (lambda ms: [(n, json.dumps({"files": {}, "package": "0" * 64}).encode()
                  if n.endswith(".cargo-checksum.json") else d) for n, d in ms], "dep-0.4.1"),
    (lambda ms: [(n, b"[package]\n" if n.endswith("dep-0.4.1/Cargo.toml") else d) for n, d in ms],
     "dep-0.4.1/Cargo.toml"),
    (lambda ms: [*ms, (f"{PREFIX}/vendor/dep-0.4.1/build.rs", b"fn main() {}\n")], "dep-0.4.1's files"),
    (lambda ms: [(n, b"[source]\n" if n.endswith("config.toml") else d) for n, d in ms], "config.toml"),
    (lambda ms: [*ms, (f"{PREFIX}/vendor/dep-0.4.1/link", "/etc/passwd")], "link"),
    (lambda ms: [*ms, ("elsewhere/file", b"")], "elsewhere"),
])
def test_the_checks_refuse_a_vendored_tarball_that_is_not_the_source_and_its_locked_crates(
        tmp_path, source_tarball, change, error):
    changed = rewritten(tmp_path, made(tmp_path, source_tarball), change)
    with pytest.raises(assets.AssetError, match=error):
        assets.check_vendored(changed, source_tarball)


def test_the_checks_refuse_a_vendored_tarball_missing_a_locked_crate(tmp_path, source_tarball):
    def without_dep(directory):
        (Path(directory) / "vendor").mkdir()
    with pytest.raises(assets.AssetError, match="dep-0.4.1"):
        assets.check_vendored(made(tmp_path, source_tarball, vendor=without_dep), source_tarball)


def bom(**metadata):
    return {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {"timestamp": "2026-10-29T10:00:00.123Z", "component": {
            "type": "library", "name": "pg_warren", "version": "1.2.3", "bom-ref": f"path+file://{ROOT}/crates/pg_warren#1.2.3",
            "purl": "pkg:cargo/pg_warren@1.2.3?download_url=file://.",
            "externalReferences": [{"type": "vcs", "url": "https://github.com/ExaDev/pg_warren"}]}, **metadata},
        "components": [
            {"type": "library", "name": "dep", "version": "0.4.1",
             "bom-ref": "registry+https://github.com/rust-lang/crates.io-index#dep@0.4.1",
             "purl": "pkg:cargo/dep@0.4.1", "hashes": [{"alg": "SHA-256", "content": CHECKSUM}]},
        ],
    }


def test_the_sbom_is_dated_by_the_commit_and_its_serial_number_derived_from_the_release():
    first = normalise_sbom(bom(), "1.2.3", COMMIT, EPOCH, ROOT)
    assert first["metadata"]["timestamp"] == "2026-10-29T09:47:48Z"
    assert first["serialNumber"].startswith("urn:uuid:")
    again = normalise_sbom(bom(), "1.2.3", COMMIT, EPOCH, ROOT)
    assert again == first
    assert normalise_sbom(bom(), "1.2.4", COMMIT, EPOCH, ROOT)["serialNumber"] != first["serialNumber"]


def test_the_sbom_records_the_release_commit():
    component = normalise_sbom(bom(), "1.2.3", COMMIT, EPOCH, ROOT)["metadata"]["component"]
    assert component["externalReferences"] == [
        {"type": "vcs", "url": "https://github.com/ExaDev/pg_warren"},
        {"type": "vcs", "url": f"https://github.com/ExaDev/pg_warren/tree/{COMMIT}", "comment": "the release commit"}]


def test_the_sbom_carries_no_path_of_the_machine_that_made_it():
    normalised = normalise_sbom(bom(), "1.2.3", COMMIT, EPOCH, ROOT)
    assert ROOT not in json.dumps(normalised)
    assert normalised["metadata"]["component"]["bom-ref"] == "path+file:///crates/pg_warren#1.2.3"


def write_bom(tmp_path, document):
    path = tmp_path / "pg_warren-1.2.3.cdx.json"
    path.write_text(json.dumps(document))
    return path


def test_the_checks_pass_on_a_normalised_sbom_of_the_locked_crates(tmp_path, source_tarball):
    assets.check_sbom(write_bom(tmp_path, normalise_sbom(bom(), "1.2.3", COMMIT, EPOCH, ROOT)), source_tarball)


@pytest.mark.parametrize("change, error", [
    (lambda b: {**b, "bomFormat": "SPDX"}, "CycloneDX"),
    (lambda b: {**b, "metadata": {**b["metadata"], "component": {**b["metadata"]["component"], "version": "9.9.9"}}},
     "9.9.9"),
    (lambda b: {**b, "components": [{**b["components"][0], "version": "0.4.2", "purl": "pkg:cargo/dep@0.4.2"}]},
     "dep@0.4.2"),
    (lambda b: {**b, "components": [{**b["components"][0], "hashes": [{"alg": "SHA-256", "content": "0" * 64}]}]},
     "dep@0.4.1"),
])
def test_the_checks_refuse_an_sbom_that_is_not_the_release_s(tmp_path, source_tarball, change, error):
    document = change(normalise_sbom(bom(), "1.2.3", COMMIT, EPOCH, ROOT))
    with pytest.raises(assets.AssetError, match=error):
        assets.check_sbom(write_bom(tmp_path, document), source_tarball)
