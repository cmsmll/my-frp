#!/usr/bin/env python3
"""Package already-built Cargo binaries without collecting local runtime data."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import stat
import sys
import tarfile
import tempfile
import tomllib
import zipfile


ROOT = Path(__file__).resolve().parent.parent
SAFE_COMPONENT = re.compile(r"[A-Za-z0-9_]+(?:[-.][A-Za-z0-9_]+)*\Z")


def platform_for(target: str) -> tuple[str, str]:
    if not SAFE_COMPONENT.fullmatch(target):
        raise ValueError("target must be a Rust target triple, without path separators")
    arch = target.split("-", 1)[0]
    if arch not in {"x86_64", "aarch64"}:
        raise ValueError(f"unsupported architecture: {arch}")
    if "-windows-" in target:
        return "windows", arch
    if "-linux-" in target:
        return "linux", arch
    if target.endswith("-apple-darwin"):
        return "macos", arch
    raise ValueError(f"unsupported target platform: {target}")


def default_config(production: bool = False) -> bytes:
    # Deliberately generate clean templates: the working server.json can contain
    # a developer's password, domain, certificate paths, and other private data.
    value = {
        "admin_listen": "127.0.0.1:10086",
        "tunnel_listen": "0.0.0.0:10088" if production else "127.0.0.1:10088",
        "public_url": "wss://frp.example.com:10088" if production else "ws://127.0.0.1:10088",
        "data_dir": "../data/server" if production else "data/server",
        "admin_password": None,
        "reset_password": False,
        "tls_cert": "../certs/fullchain.pem" if production else None,
        "tls_key": "../certs/privkey.pem" if production else None,
        "allow_insecure": not production,
        "port_min": 10000,
        "port_max": 60000,
    }
    return (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode("utf-8")


def quickstart(version: str, platform: str, arch: str, has_demo: bool) -> bytes:
    windows = platform == "windows"
    server = r".\myfrps.exe" if windows else "./myfrps"
    client = r".\myfrpc.exe" if windows else "./myfrpc"
    demo = r".\examples\stream_service.exe" if windows else "./examples/stream_service"
    production = r".\examples\server.production.json" if windows else "./examples/server.production.json"
    text = f"""my-frp {version} — {platform} / {arch}

一、解压并启动（无需 Rust、Cargo、Node.js 或外部数据库）

完整解压后，进入本 QUICKSTART.txt 所在目录。在两个终端分别执行：

服务端：
  {server}

客户端（本机明文联调）：
  {client} --allow-insecure

服务端默认读取当前目录的 server.json。也可以将 JSON 配置路径作为唯一参数。
Windows 使用 PowerShell；Linux / macOS 使用终端。关闭终端或 Ctrl+C 会停止程序。
Web 页面关闭不会停止穿透服务。

二、首次登录和连接

服务端管理页面：http://127.0.0.1:10086
客户端管理页面：http://127.0.0.1:10087
两端首次初始化的密码均为 admin，登录后必须分别改成至少 12 个字符的新密码。
密码修改后重新登录；后续启动沿用已有密码，不会重置。请自行妥善保存新密码。

1. 在服务端创建隧道，例如映射端口 18080，复制生成的 JWT。
2. 在客户端新建连接，填写本地服务地址（例如 127.0.0.1:8080）和 JWT。
3. 等待在线后访问 http://127.0.0.1:18080，即可访问本地服务。

默认 server.json 仅监听本机，public_url 为 ws://127.0.0.1:10088。
10086 / 10087 为两端管理页面端口；10088 为穿透接入端口。
隧道业务端口需在 10000—60000 范围内，且不得已被占用。
默认模板仅用于同一台机器联调；客户端 --allow-insecure 允许明文 WS。
程序会按需创建 data/server 和 data/client，持久保存 SQLite、管理密码和签名密钥。
请保留这些数据以继续使用原有配置；发布包不含任何预置运行数据或私钥。

三、公网部署（TLS）

编辑 examples/server.production.json，把 frp.example.com 换成客户端可访问的域名，
并在 certs/fullchain.pem、certs/privkey.pem 放置与域名匹配的有效 TLS 证书和私钥。
这些路径和 data_dir 都以 JSON 配置文件所在目录为基准，包内模板已按此设置。
配置 public_url 为实际可达的 wss://域名:端口，allow_insecure 保持 false。

服务端：
  {server} {production}

客户端：
  {client}

放行穿透接入端口（默认 10088）及各隧道业务端口；管理页面默认保留在本机回环地址。
通过服务器本机浏览器或 SSH 端口转发管理。public_url 不会自动配置 DNS / 防火墙。
修改服务端配置需重启；更换 public_url 后应刷新隧道 JWT 并更新客户端。
穿透通道 TLS 不会自动为业务入口提供 HTTPS / WSS，业务加密按本地服务配置。
完整配置和部署说明见 README.md。
"""
    if has_demo:
        text += f"""
四、内置 WebSocket / SSE 测试服务

第三个终端执行：
  {demo} 127.0.0.1:18090

在服务端创建业务端口 18080 的隧道，客户端本地服务设为 127.0.0.1:18090 并填写 JWT。
通过 http://127.0.0.1:18080 打开测试页面，验证 WS 回显和 SSE 逐条事件。
页面静态资源已内嵌在示例程序中，无需额外 Web 服务器。
"""
    if platform == "linux":
        text += "\nLinux 的 deploy/ 含 systemd 单元模板，安装路径与权限设置参见 README.md。\n"
    return text.encode("utf-8")


def collect_files(binary_dir: Path, version: str, platform: str, arch: str) -> list[tuple[str, bytes, int]]:
    suffix = ".exe" if platform == "windows" else ""
    files = []
    for binary in ("myfrps", "myfrpc"):
        path = binary_dir / (binary + suffix)
        if not path.is_file():
            raise FileNotFoundError(f"missing binary: {path}; build this target first")
        files.append((path.name, path.read_bytes(), 0o755))
    demo_path = binary_dir / "examples" / ("stream_service" + suffix)
    if demo_path.is_file():
        files.append((f"examples/{demo_path.name}", demo_path.read_bytes(), 0o755))
    files.extend([
        ("server.json", default_config(), 0o644),
        ("examples/server.production.json", default_config(production=True), 0o644),
        ("README.md", (ROOT / "README.md").read_bytes(), 0o644),
        ("QUICKSTART.txt", quickstart(version, platform, arch, demo_path.is_file()), 0o644),
    ])
    if platform == "linux":
        for name in ("myfrps.service", "myfrpc.service"):
            files.append((f"deploy/{name}", (ROOT / "deploy" / name).read_bytes(), 0o644))
    return sorted(files)


def write_archive(destination: Path, root_name: str, files: list[tuple[str, bytes, int]], windows: bool) -> None:
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=destination.parent, suffix=".tmp", delete=False) as stream:
            temporary = Path(stream.name)
            if windows:
                with zipfile.ZipFile(stream, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
                    for name, content, mode in files:
                        entry = zipfile.ZipInfo(f"{root_name}/{name}", date_time=(1980, 1, 1, 0, 0, 0))
                        entry.create_system = 3
                        entry.external_attr = (stat.S_IFREG | mode) << 16
                        entry.compress_type = zipfile.ZIP_DEFLATED
                        archive.writestr(entry, content)
            else:
                with gzip.GzipFile(fileobj=stream, mode="wb", filename="", mtime=0) as compressed:
                    with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                        directory = tarfile.TarInfo(root_name)
                        directory.type = tarfile.DIRTYPE
                        directory.mode = 0o755
                        archive.addfile(directory)
                        for name, content, mode in files:
                            entry = tarfile.TarInfo(f"{root_name}/{name}")
                            entry.size = len(content)
                            entry.mode = mode
                            archive.addfile(entry, io.BytesIO(content))
        os.replace(temporary, destination)
        temporary = None
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def update_checksums(output_dir: Path, artifact: Path) -> str:
    with artifact.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    manifest = output_dir / "SHA256SUMS"
    entries = {}
    if manifest.exists():
        for line in manifest.read_text(encoding="utf-8").splitlines():
            match = re.fullmatch(r"([0-9a-fA-F]{64})  ([^/\\\r\n]+)", line)
            if not match:
                raise ValueError(f"invalid checksum entry in {manifest}: {line!r}")
            entries[match.group(2)] = match.group(1).lower()
    entries[artifact.name] = digest
    content = "".join(f"{value}  {name}\n" for name, value in sorted(entries.items()))
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", newline="\n", dir=output_dir, delete=False) as stream:
        temporary = Path(stream.name)
        stream.write(content)
    try:
        os.replace(temporary, manifest)
    finally:
        temporary.unlink(missing_ok=True)
    return digest


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, help="Rust target triple used for the Cargo build")
    parser.add_argument("--profile", default="release", help="Cargo output profile directory (default: release)")
    parser.add_argument("--output-dir", type=Path, default=Path("target/packages"))
    parser.add_argument("--target-dir", type=Path, default=Path("target"), help="Cargo target root directory")
    args = parser.parse_args()
    try:
        platform, arch = platform_for(args.target)
        if not SAFE_COMPONENT.fullmatch(args.profile):
            raise ValueError("profile must be one directory name")
        with (ROOT / "Cargo.toml").open("rb") as stream:
            version = str(tomllib.load(stream)["package"]["version"])
        if not SAFE_COMPONENT.fullmatch(version):
            raise ValueError(f"unsupported package version: {version!r}")
        binary_dir = args.target_dir.resolve() / args.target / args.profile
        files = collect_files(binary_dir, version, platform, arch)
        root_name = f"my-frp-{version}-{platform}-{arch}"
        extension = ".zip" if platform == "windows" else ".tar.gz"
        output_dir = args.output_dir.resolve()
        output_dir.mkdir(parents=True, exist_ok=True)
        artifact = output_dir / (root_name + extension)
        write_archive(artifact, root_name, files, platform == "windows")
        checksum = update_checksums(output_dir, artifact)
        print(f"Packaged: {artifact}")
        print(f"SHA256:   {checksum}")
        print(f"Manifest: {output_dir / 'SHA256SUMS'}")
        return 0
    except (OSError, ValueError, KeyError, tomllib.TOMLDecodeError) as error:
        print(f"Packaging failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
