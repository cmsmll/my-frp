#!/usr/bin/env python3
"""Verify four successful Actions build artifacts and publish a GitHub Release.

Requires Python 3.11+ and an authenticated gh CLI. Downloads use a temporary
directory by default; neither local Cargo output nor a local dist is trusted.
"""

from __future__ import annotations

import argparse
import base64
from contextlib import nullcontext
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib
from urllib.parse import quote
import zipfile


WORKFLOW_PATH = ".github/workflows/build-release.yml"
TARGETS = {
    "x86_64-pc-windows-msvc": "windows-x86_64.zip",
    "x86_64-unknown-linux-musl": "linux-x86_64.tar.gz",
    "x86_64-apple-darwin": "macos-x86_64.tar.gz",
    "aarch64-apple-darwin": "macos-aarch64.tar.gz",
}
SAFE_VERSION = re.compile(r"[A-Za-z0-9_]+(?:[-.][A-Za-z0-9_]+)*\Z")
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
SHA1 = re.compile(r"[0-9a-f]{40}\Z")


class ReleaseError(RuntimeError):
    pass


def gh(arguments: list[str], destination: Path | None = None, optional: bool = False):
    """Capture gh output; credentials are inherited and never inspected or printed."""
    output = destination.open("wb") if destination is not None else nullcontext(subprocess.PIPE)
    with output as stream:
        result = subprocess.run(
            ["gh", *arguments], stdout=stream, stderr=subprocess.PIPE, check=False,
            env={**os.environ, "GH_PROMPT_DISABLED": "1", "GH_PAGER": "cat"},
        )
    if result.returncode:
        error = result.stderr.decode("utf-8", errors="replace").strip()
        if optional and "HTTP 404" in error:
            return None
        raise ReleaseError(f"GitHub CLI failed: {error}")
    if destination is not None:
        return destination
    return result.stdout.decode("utf-8")


def api(endpoint: str, optional: bool = False):
    output = gh(["api", "--method", "GET", endpoint], optional=optional)
    return None if output is None else json.loads(output)


def pages(endpoint: str, key: str | None = None) -> list:
    result = []
    for page in range(1, 1001):
        separator = "&" if "?" in endpoint else "?"
        response = api(f"{endpoint}{separator}per_page=100&page={page}")
        items = response[key] if key is not None else response
        result.extend(items)
        if len(items) < 100:
            return result
    raise ReleaseError(f"Too many pages from {endpoint}")


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verify_run(repo: str, run_id: int, allow_current_run: bool) -> dict:
    run = api(f"repos/{repo}/actions/runs/{run_id}")
    if (run.get("repository", {}).get("full_name", "").lower() != repo.lower()
            or run.get("head_repository", {}).get("full_name", "").lower() != repo.lower()):
        raise ReleaseError("The source run does not belong to the requested repository")
    if run.get("path") != WORKFLOW_PATH:
        raise ReleaseError(f"The source run must use {WORKFLOW_PATH}")
    if run.get("event") not in {"workflow_dispatch", "push"}:
        raise ReleaseError("Only a repository push or manually dispatched build may be published")
    if not SHA1.fullmatch(run.get("head_sha", "")):
        raise ReleaseError("The source run has an invalid build commit SHA")
    current = (allow_current_run and os.environ.get("GITHUB_RUN_ID") == str(run_id)
               and os.environ.get("GITHUB_REPOSITORY", "").lower() == repo.lower())
    successful = run.get("status") == "completed" and run.get("conclusion") == "success"
    running_here = current and run.get("status") == "in_progress" and run.get("conclusion") is None
    if not (successful or running_here):
        raise ReleaseError("The source run must have completed successfully; only this workflow's "
                           "own publishing job may use a run still in progress")
    jobs = pages(f"repos/{repo}/actions/runs/{run_id}/jobs", "jobs")
    builds = [job for job in jobs if job.get("name") in TARGETS]
    if len(builds) != len(TARGETS) or {job["name"] for job in builds} != set(TARGETS):
        raise ReleaseError("The source run must contain exactly the four expected platform build jobs")
    if any(job.get("status") != "completed" or job.get("conclusion") != "success"
           or job.get("head_sha") != run["head_sha"] for job in builds):
        raise ReleaseError("All four platform build and test jobs must have succeeded at the same commit")
    return run


def source_version(repo: str, sha: str) -> str:
    source = api(f"repos/{repo}/contents/Cargo.toml?ref={sha}")
    if source.get("encoding") != "base64":
        raise ReleaseError("Cannot read Cargo.toml at the actual build commit")
    content = base64.b64decode(source["content"], validate=False).decode("utf-8")
    version = tomllib.loads(content)["package"]["version"]
    if not isinstance(version, str) or not SAFE_VERSION.fullmatch(version):
        raise ReleaseError("The source Cargo.toml has an unsupported version")
    return version


def tag_commit(repo: str, tag: str) -> str | None:
    ref = api(f"repos/{repo}/git/ref/tags/{quote(tag, safe='')}", optional=True)
    if ref is None:
        return None
    obj = ref["object"]
    for _ in range(8):
        if obj.get("type") == "commit":
            return obj["sha"]
        if obj.get("type") != "tag":
            break
        obj = api(f"repos/{repo}/git/tags/{obj['sha']}")["object"]
    raise ReleaseError(f"Tag {tag} does not resolve to a commit")


def require_tag_target(repo: str, tag: str, sha: str, required: bool = False) -> str | None:
    target = tag_commit(repo, tag)
    if target is not None and target != sha:
        raise ReleaseError(f"Tag {tag} points to {target}, but the packages were built at {sha}; "
                           "refusing to move an existing tag")
    if required and target is None:
        raise ReleaseError(f"Published release tag {tag} is missing")
    return target


def checksum_manifest(content: str, expected_name: str) -> str:
    lines = content.splitlines()
    if len(lines) != 1:
        raise ReleaseError(f"Each platform SHA256SUMS must contain exactly {expected_name}")
    match = re.fullmatch(r"([0-9a-fA-F]{64})  ([^/\\\r\n]+)", lines[0])
    if match is None or match.group(2) != expected_name:
        raise ReleaseError(f"The platform checksum manifest must name exactly {expected_name}")
    return match.group(1).lower()


def download_packages(repo: str, run: dict, version: str, output: Path) -> dict[str, Path]:
    artifacts = pages(f"repos/{repo}/actions/runs/{run['id']}/artifacts", "artifacts")
    expected_artifacts = {f"my-frp-{target}" for target in TARGETS}
    if len(artifacts) != 4 or {item.get("name") for item in artifacts} != expected_artifacts:
        raise ReleaseError("The source run must have exactly the four expected package artifacts")
    result = {}
    checksums = {}
    downloads = output / "actions-artifacts"
    downloads.mkdir()
    for artifact in sorted(artifacts, key=lambda item: item["name"]):
        if artifact.get("expired"):
            raise ReleaseError(f"Artifact {artifact['name']} has expired; build this version again")
        source = artifact.get("workflow_run", {})
        if source.get("id") != run["id"] or source.get("head_sha") != run["head_sha"]:
            raise ReleaseError(f"Artifact {artifact['name']} does not match the source run commit")
        target = artifact["name"].removeprefix("my-frp-")
        name = f"my-frp-{version}-{TARGETS[target]}"
        zip_path = downloads / (artifact["name"] + ".zip")
        # The artifact endpoint redirects to ZIP storage; its REST API accepts
        # JSON, not application/octet-stream. gh follows the binary redirect.
        gh(["api", "--method", "GET", f"repos/{repo}/actions/artifacts/{artifact['id']}/zip"],
           destination=zip_path)
        artifact_digest = artifact.get("digest", "")
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", artifact_digest):
            raise ReleaseError(f"Artifact {artifact['name']} has no GitHub SHA-256 digest")
        if "sha256:" + digest(zip_path) != artifact_digest:
            raise ReleaseError(f"The downloaded Actions artifact {artifact['name']} failed its digest check")
        with zipfile.ZipFile(zip_path) as archive:
            members = archive.infolist()
            if len(members) != 2 or {entry.filename for entry in members} != {name, "SHA256SUMS"}:
                raise ReleaseError(f"Artifact {artifact['name']} must contain only its package and SHA256SUMS")
            manifest = archive.getinfo("SHA256SUMS")
            package = archive.getinfo(name)
            if manifest.file_size > 4096 or package.file_size > 1024 ** 3:
                raise ReleaseError(f"Artifact {artifact['name']} exceeds expected file size limits")
            expected_digest = checksum_manifest(archive.read(manifest).decode("utf-8"), name)
            path = output / name
            with archive.open(package) as source_stream, path.open("wb") as destination:
                while chunk := source_stream.read(1024 * 1024):
                    destination.write(chunk)
        if digest(path) != expected_digest:
            raise ReleaseError(f"Package {name} failed its SHA256SUMS check")
        checksums[name] = expected_digest
        result[name] = path
        print(f"Verified package: {name}", flush=True)
    manifest_path = output / "SHA256SUMS"
    manifest_path.write_text("".join(f"{checksums[name]}  {name}\n" for name in sorted(checksums)),
                             encoding="utf-8", newline="\n")
    result[manifest_path.name] = manifest_path
    return result


def find_release(repo: str, tag: str) -> dict | None:
    matches = [release for release in pages(f"repos/{repo}/releases") if release["tag_name"] == tag]
    if len(matches) > 1:
        raise ReleaseError(f"Multiple releases use {tag}; resolve the conflict before publishing")
    return matches[0] if matches else None


def check_release_target(repo: str, release: dict, tag: str, sha: str) -> None:
    target = require_tag_target(repo, tag, sha)
    if target is None and (not release.get("draft") or release.get("target_commitish") != sha):
        raise ReleaseError(f"Existing release {tag} has no matching tag or exact build commit target")


def verify_assets(repo: str, release: dict, packages: dict[str, Path], output: Path,
                  allow_missing: bool = False) -> list[str]:
    assets = pages(f"repos/{repo}/releases/{release['id']}/assets")
    names = [asset.get("name") for asset in assets]
    if len(set(names)) != len(names) or set(names) - set(packages):
        raise ReleaseError("Existing release has unexpected or duplicate assets; refusing to overwrite it")
    missing = sorted(set(packages) - set(names))
    if missing and not allow_missing:
        raise ReleaseError(f"Existing release is missing assets: {', '.join(missing)}")
    for asset in assets:
        local = packages[asset["name"]]
        expected_digest = digest(local)
        if asset.get("state") != "uploaded" or asset.get("size") != local.stat().st_size:
            raise ReleaseError(f"Existing asset {asset['name']} has a different size or upload state")
        remote_digest = asset.get("digest")
        if remote_digest is None:
            # Older GitHub assets lack server-side digests. Download and verify
            # them instead of accepting size alone or overwriting blindly.
            downloaded = output / ("remote-" + str(asset["id"]))
            gh(["api", "--method", "GET", "-H", "Accept: application/octet-stream",
                f"repos/{repo}/releases/assets/{asset['id']}"], destination=downloaded)
            remote_digest = "sha256:" + digest(downloaded)
            downloaded.unlink()
        if remote_digest != "sha256:" + expected_digest:
            raise ReleaseError(f"Existing asset {asset['name']} differs from the verified build; "
                               "refusing to replace a published file")
    return missing


def release_notes(repo: str, run: dict, version: str) -> str:
    return f"""my-frp {version}：Rust 内网穿透服务端、客户端及 WebSocket / SSE 测试服务。

下载与运行：

- Windows x64：解压 ZIP，在 PowerShell 中启动 myfrps.exe 和 myfrpc.exe。
- Linux x64：musl 静态程序，解压 tar.gz 后启动 ./myfrps 和 ./myfrpc。
- macOS：按 Intel / Apple Silicon 选择对应 tar.gz；程序未签名或公证。
- 包内提供 server.json、QUICKSTART.txt、README.md 和部署配置模板。
- 两端管理页面首次密码为 admin，登录后必须修改；使用前核对 SHA256SUMS。

所有四个平台均已在对应原生运行环境通过发布版测试。

构建来源：[GitHub Actions #{run['id']}](https://github.com/{repo}/actions/runs/{run['id']})
源码提交：[{run['head_sha']}](https://github.com/{repo}/commit/{run['head_sha']})
"""


def publish(args, output: Path) -> int:
    run = verify_run(args.repo, args.run_id, args.allow_current_run)
    version = source_version(args.repo, run["head_sha"])
    tag = args.tag or "v" + version
    if tag != "v" + version:
        raise ReleaseError(f"Release tag must be v{version}, matching Cargo.toml at {run['head_sha']}")
    if (os.environ.get("GITHUB_REF_TYPE") == "tag"
            and os.environ.get("GITHUB_REF_NAME") != tag):
        raise ReleaseError("The triggering tag does not match the version in the actual build commit")
    tag_target = require_tag_target(args.repo, tag, run["head_sha"])
    packages = download_packages(args.repo, run, version, output)
    release = find_release(args.repo, tag)
    missing = sorted(packages)
    if release is not None:
        check_release_target(args.repo, release, tag, run["head_sha"])
        missing = verify_assets(args.repo, release, packages, output, allow_missing=release.get("draft", False))
    print(f"Verified source: {args.repo}@{run['head_sha']} ({tag}), Actions run {run['id']}")
    if args.dry_run:
        print("Dry run passed: four packages and combined SHA256SUMS verified; no remote changes made.")
        return 0
    if release is not None and not release.get("draft"):
        print(f"Release already published with the same five verified assets: {release['html_url']}")
        return 0
    if release is None:
        notes = output / "release-notes.md"
        notes.write_text(release_notes(args.repo, run, version), encoding="utf-8", newline="\n")
        # Existing tags already identify the verified build commit. Omit
        # target_commitish in this case: it is unused for tag creation and a
        # historical workflow revision can require additional API permissions.
        # Check again after downloads so a concurrently created tag is checked.
        tag_target = require_tag_target(args.repo, tag, run["head_sha"])
        command = ["release", "create", tag, "--repo", args.repo, "--draft",
                   "--title", f"my-frp {tag}", "--notes-file", str(notes)]
        if tag_target is not None:
            command.append("--verify-tag")
        else:
            command.extend(["--target", run["head_sha"]])
        if "-" in version:
            command.append("--prerelease")
        gh(command)
        release = find_release(args.repo, tag)
        if release is None or not release.get("draft"):
            raise ReleaseError("Could not find the newly created draft release")
        check_release_target(args.repo, release, tag, run["head_sha"])
    if missing:
        gh(["release", "upload", tag, "--repo", args.repo, *[str(packages[name]) for name in missing]])
    release = find_release(args.repo, tag)
    if release is None or not release.get("draft"):
        raise ReleaseError("Draft release changed during upload; refusing to publish")
    check_release_target(args.repo, release, tag, run["head_sha"])
    verify_assets(args.repo, release, packages, output)
    gh(["release", "edit", tag, "--repo", args.repo, "--draft=false",
        "--latest=" + ("false" if "-" in version else "true")])
    release = find_release(args.repo, tag)
    if release is None or release.get("draft"):
        raise ReleaseError("The release was not published successfully")
    require_tag_target(args.repo, tag, run["head_sha"], required=True)
    verify_assets(args.repo, release, packages, output)
    print(f"Published five verified release assets: {release['html_url']}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, help="GitHub owner/repository")
    parser.add_argument("--run-id", required=True, type=int, help="Successful four-platform build run")
    parser.add_argument("--tag", help="Defaults to v + Cargo version read at the build commit")
    parser.add_argument("--output-dir", type=Path, help="Empty staging directory; defaults to an automatically removed temp directory")
    parser.add_argument("--allow-current-run", action="store_true",
                        help="Allow this Actions run's publishing job after all four builds succeed")
    parser.add_argument("--dry-run", action="store_true", help="Verify packages and any existing release without publishing")
    args = parser.parse_args()
    try:
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo) or args.run_id <= 0:
            raise ReleaseError("Supply a valid owner/repository and positive run ID")
        if args.output_dir is not None:
            output = args.output_dir.resolve()
            output.mkdir(parents=True, exist_ok=True)
            if any(output.iterdir()):
                raise ReleaseError("The staging output directory must be empty to avoid mixing builds")
            return publish(args, output)
        with tempfile.TemporaryDirectory(prefix="my-frp-release-") as temporary:
            return publish(args, Path(temporary))
    except (ReleaseError, OSError, ValueError, KeyError, zipfile.BadZipFile) as error:
        print(f"Release failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
