"""Pages always deploys the brochure and the newest published package tree together."""

import json
import os
import subprocess
from pathlib import Path

import pytest

import pages


FINGERPRINT = "0123456789ABCDEF0123456789ABCDEF01234567"


def published(version, *, assets=True, **extra):
    release = {"tag_name": f"v{version}", "draft": False, "prerelease": False,
               "assets": [{"name": name} for name in pages.snapshot_names(version)] if assets else []}
    release.update(extra)
    return release


@pytest.fixture
def tree(tmp_path):
    root = tmp_path / "repo"
    (root / "sql").mkdir(parents=True)
    (root / "sql/released.commits").write_text("a" * 40 + "  0.6.0\n" + "b" * 40 + "  0.7.0\n" + "c" * 40 + "  0.8.0\n")
    (root / "README.md").write_text(f"`{FINGERPRINT}` <!-- apt key -->\n")
    site = root / "site/dist"
    site.mkdir(parents=True)
    (site / "index.html").write_bytes(b"<h1>pg_warren</h1>")
    (site / "docs").mkdir()
    (site / "docs/index.html").write_bytes(b"<p>Documentation</p>")
    return root, site, tmp_path / "pages"


def test_before_first_published_packaged_release_brochure_only_is_allowed(tree):
    root, site, output = tree
    pages.assemble(site, output, "ExaDev/pg_warren", source_root=root,
                   releases=[published("0.6.0", assets=False)], source_sha="abc")
    assert (output / "index.html").read_bytes() == (site / "index.html").read_bytes()
    assert not (output / "apt").exists()
    assert (output / ".nojekyll").is_file()
    assert json.loads((output / "deployment.json").read_text()) == {
        "source_sha": "abc", "apt_version": None, "apt_archive_sha256": None}


def test_selection_uses_numeric_recorded_version_not_date_or_latest_flags():
    releases = [published("0.9.0"), published("0.10.0"),
                published("0.7.0", published_at="2099-01-01", latest=True)]
    assert pages.select_release(releases, {"0.7.0", "0.9.0", "0.10.0"}).version == "0.10.0"


def test_draft_prerelease_and_unrecorded_releases_cannot_select_a_package_tree():
    releases = [published("0.8.0", draft=True), published("0.9.0", prerelease=True),
                published("9.0.0"), published("0.7.0")]
    assert pages.select_release(releases, {"0.7.0", "0.8.0", "0.9.0"}).version == "0.7.0"


def test_historical_source_only_release_is_ignored_but_signed_snapshot_is_selected():
    source_only = published("0.6.0", assets=False)
    source_only["assets"] = [{"name": "pg_warren-0.6.0.tar.gz"}]
    assert pages.select_release([source_only], {"0.6.0"}) is None
    assert pages.select_release([published("0.6.0")], {"0.6.0"}).version == "0.6.0"


@pytest.mark.parametrize("asset", ["postgresql-16-pg-warren_0.6.0_amd64.deb", "pg_warren-apt-0.6.0.tar.gz"])
def test_historical_packaged_release_with_partial_or_missing_snapshot_fails(asset):
    historical = published("0.6.0", assets=False)
    historical["assets"] = [{"name": asset}]
    with pytest.raises(pages.PagesError, match="snapshot"):
        pages.select_release([historical], {"0.6.0"})


@pytest.mark.parametrize("count", [0, 1, 2])
def test_partial_newest_release_fails_instead_of_falling_back(tree, count):
    root, site, output = tree
    latest = published("0.8.0")
    latest["assets"] = latest["assets"][:count]
    with pytest.raises(pages.PagesError, match="snapshot"):
        pages.assemble(site, output, "ExaDev/pg_warren", source_root=root,
                       releases=[published("0.7.0"), latest], source_sha="abc")
    assert not output.exists()


def test_duplicate_or_missing_snapshot_assets_fail_closed():
    release = published("0.7.0")
    release["assets"].append(release["assets"][0])
    with pytest.raises(pages.PagesError, match="snapshot"):
        pages.select_release([release], {"0.7.0"})


def test_verified_apt_bytes_are_combined_with_site_without_signing(tree):
    root, site, output = tree
    calls = []
    def download(repository, tag, names, destination):
        calls.append((repository, tag, names))
        for name in names:
            (destination / name).write_bytes(b"archive-or-signature")
    def restore(archive, checksum, signature, fingerprint, destination):
        assert fingerprint == FINGERPRINT
        assert archive.name == pages.snapshot_names("0.8.0")[0]
        assert checksum.is_file() and signature.is_file()
        (destination / "apt/dists/bookworm").mkdir(parents=True)
        (destination / "apt/dists/bookworm/InRelease").write_bytes(b"signed-index\x00unchanged")
        (destination / "apt/pg_warren.asc").write_bytes(b"public-key")
    pages.assemble(site, output, "ExaDev/pg_warren", source_root=root,
                   releases=[published("0.8.0"), published("0.7.0")],
                   downloader=download, restorer=restore, source_sha="abc")
    assert calls == [("ExaDev/pg_warren", "v0.8.0", pages.snapshot_names("0.8.0"))]
    assert (output / "apt/dists/bookworm/InRelease").read_bytes() == b"signed-index\x00unchanged"
    assert (output / "docs/index.html").read_bytes() == b"<p>Documentation</p>"
    metadata = json.loads((output / "deployment.json").read_text())
    assert metadata["apt_version"] == "0.8.0"
    assert len(metadata["apt_archive_sha256"]) == 64


def test_archive_verification_failure_leaves_no_deployable_output(tree):
    root, site, output = tree
    def download(repository, tag, names, destination):
        for name in names:
            (destination / name).write_bytes(b"invalid")
    def restore(*args):
        raise ValueError("signature invalid")
    with pytest.raises(pages.PagesError, match="signature invalid"):
        pages.assemble(site, output, "ExaDev/pg_warren", source_root=root,
                       releases=[published("0.7.0")], downloader=download,
                       restorer=restore, source_sha="abc")
    assert not output.exists()


def test_website_cannot_claim_apt_or_deployment_metadata_paths(tree):
    root, site, output = tree
    for reserved in ("apt", "deployment.json"):
        (site / reserved).write_text("collision")
        with pytest.raises(pages.PagesError, match="reserved"):
            pages.assemble(site, output, "ExaDev/pg_warren", source_root=root,
                           releases=[], source_sha="abc")
        (site / reserved).unlink()
    assert not output.exists()


@pytest.mark.parametrize("kind", ["symlink", "hardlink"])
def test_website_links_are_rejected(tree, kind):
    root, site, output = tree
    if kind == "symlink":
        (site / "linked").symlink_to(site / "index.html")
    else:
        os.link(site / "index.html", site / "linked")
    with pytest.raises(pages.PagesError, match="link"):
        pages.assemble(site, output, "ExaDev/pg_warren", source_root=root,
                       releases=[], source_sha="abc")
    assert not output.exists()


def test_combined_output_size_includes_apt(tree):
    root, site, output = tree
    def download(repository, tag, names, destination):
        for name in names:
            (destination / name).write_bytes(b"valid")
    def restore(archive, checksum, signature, fingerprint, destination):
        (destination / "apt").mkdir()
        (destination / "apt/large.deb").write_bytes(b"x" * 1024)
    with pytest.raises(pages.PagesError, match="size"):
        pages.assemble(site, output, "ExaDev/pg_warren", source_root=root,
                       releases=[published("0.7.0")], downloader=download,
                       restorer=restore, source_sha="abc", max_bytes=512)
    assert not output.exists()


def test_network_failure_is_not_treated_as_no_releases(tree, monkeypatch):
    root, site, output = tree
    def fail(*args, **kwargs):
        raise subprocess.CalledProcessError(1, "gh", stderr="network unavailable")
    monkeypatch.setattr(pages.subprocess, "run", fail)
    with pytest.raises(pages.PagesError, match="network unavailable"):
        pages.assemble(site, output, "ExaDev/pg_warren", source_root=root, source_sha="abc")
    assert not output.exists()


def test_release_query_collects_all_pages(monkeypatch):
    commands = []
    def run(command, **kwargs):
        commands.append(command)
        return subprocess.CompletedProcess(command, 0, json.dumps([[published("0.8.0")], [published("0.7.0")]]))
    monkeypatch.setattr(pages.subprocess, "run", run)
    assert len(pages.fetch_releases("ExaDev/pg_warren")) == 2
    assert "--paginate" in commands[0] and "--slurp" in commands[0]


def test_site_sha_comes_from_checked_out_source_not_event_sha(tree, monkeypatch):
    root, site, output = tree
    monkeypatch.setenv("GITHUB_SHA", "older-trigger-commit")
    commands = []
    def run(command, **kwargs):
        commands.append((command, kwargs["cwd"]))
        return subprocess.CompletedProcess(command, 0, "checked-out-main\n")
    monkeypatch.setattr(pages.subprocess, "run", run)
    metadata = pages.assemble(site, output, "ExaDev/pg_warren", source_root=root, releases=[])
    assert metadata["source_sha"] == "checked-out-main"
    assert commands == [(["git", "rev-parse", "HEAD"], root)]


def test_download_must_produce_every_required_snapshot_file(tree):
    root, site, output = tree
    def partial(repository, tag, names, destination):
        (destination / names[0]).write_bytes(b"only-the-archive")
    with pytest.raises(pages.PagesError, match="required assets"):
        pages.assemble(site, output, "ExaDev/pg_warren", source_root=root,
                       releases=[published("0.7.0")], downloader=partial, source_sha="abc")
    assert not output.exists()


def test_size_bound_includes_generated_deployment_metadata(tree):
    root, site, output = tree
    source_bytes = sum(item.stat().st_size for item in site.rglob("*") if item.is_file())
    with pytest.raises(pages.PagesError, match="size"):
        pages.assemble(site, output, "ExaDev/pg_warren", source_root=root,
                       releases=[], source_sha="abc", max_bytes=source_bytes)
    assert not output.exists()


@pytest.mark.parametrize("response", ["not-json", '{"message": "error"}', '[{"tag_name": "v0.7.0"}]'])
def test_invalid_api_response_is_never_treated_as_no_published_releases(monkeypatch, response):
    def run(command, **kwargs):
        return subprocess.CompletedProcess(command, 0, response)
    monkeypatch.setattr(pages.subprocess, "run", run)
    with pytest.raises(pages.PagesError):
        pages.fetch_releases("ExaDev/pg_warren")


def test_downloader_requests_only_exact_snapshot_names(monkeypatch, tmp_path):
    commands = []
    monkeypatch.setattr(pages.subprocess, "run", lambda command, **kwargs: commands.append(command))
    names = pages.snapshot_names("0.7.0")
    pages.download_snapshot("ExaDev/pg_warren", "v0.7.0", names, tmp_path)
    assert commands == [["gh", "release", "download", "v0.7.0", "--repo", "ExaDev/pg_warren", "--dir", str(tmp_path),
                         "--pattern", names[0], "--pattern", names[1], "--pattern", names[2]]]
