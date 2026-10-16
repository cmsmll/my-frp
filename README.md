# my-frp

Rust 编写的 TCP 内网穿透服务。服务端 `myfrps` 和客户端 `myfrpc` 都内嵌中文 Web 管理界面，运行时不依赖 Node.js、外部前端服务器或独立数据库服务。

## 当前功能

- 服务端：多隧道、公网端口配置、启用/禁用/删除、并发连接限制、JWT 签发和刷新、在线状态、流量与操作记录。
- 客户端：在页面填写本地地址和 JWT，连接多个不同服务端；支持编辑、停止、启用、删除与断线重连。
- SQLite 持久化配置、审计记录和运行日志；服务端 Ed25519 签名密钥持久保存。
- WSS/TLS 控制连接与数据连接；每条公网 TCP 连接使用独立数据通道，支持背压和 TCP 半关闭。
- 凭证刷新、禁用、删除或到期时终止现有连接；数据通道票据短时有效且只能使用一次。
- 管理页面使用独立管理密码、Argon2 密码哈希、8 小时会话、登录限速和内容安全策略；支持页面修改密码，默认密码首次登录必须修改。
- 两端均提供“运行日志”页面，可按级别和关键词筛选、刷新及导出文本日志，保留最近 10,000 条记录。

## 快速开始

在项目根目录分别打开两个终端。服务端只从 JSON 文件加载启动配置：不传参数时读取当前工作目录的 `server.json`，也可传入一个配置文件路径。仓库提供的 `server.json` 使用本机回环地址和明文连接，供以下本机联调使用；客户端也需明确开启明文模式。

### dev：通过 Cargo 启动

安装 Rust stable，Windows 还需要 Visual Studio C++ Build Tools。以下命令在 Windows PowerShell 和 Linux shell 中均可使用。

服务端：

```sh
cargo run --locked --bin myfrps
```

客户端（另一个终端）：

```sh
cargo run --locked --bin myfrpc -- --allow-insecure
```

指定其他服务端配置文件时，使用 `cargo run --locked --bin myfrps -- ./examples/server.production.json`。先按“公网部署”修改生产配置中的域名与证书路径。

### 打包后：直接运行程序

先编译发布版：

```sh
cargo build --release --locked
```

Windows PowerShell，在项目根目录分别启动：

```powershell
# 服务端，读取项目根目录的 server.json
.\target\release\myfrps.exe
```

```powershell
# 客户端，另一个终端
.\target\release\myfrpc.exe --allow-insecure
```

Linux / macOS，在项目根目录分别启动：

```sh
# 服务端，读取项目根目录的 server.json
./target/release/myfrps
```

```sh
# 客户端，另一个终端
./target/release/myfrpc --allow-insecure
```

分发时复制对应平台的程序，并将仓库的 `server.json` 放到服务端程序旁，在该目录运行；SQLite 与页面资源都编译进程序。若从其他目录启动服务端，将配置文件路径作为唯一位置参数，例如 `.\myfrps.exe C:\myfrp\server.json` 或 `./myfrps /etc/myfrp/server.json`。配置文件不存在时程序会报错退出，不会自动生成配置。

### 创建并连接第一条隧道

两个程序首次初始化时，Web 管理默认密码均为 `admin`。首次登录后必须设置至少 12 个字符的新密码，完成后使用新密码重新登录，才能管理隧道。两端密码独立保存，后续启动沿用数据库中的密码，升级不会将已有密码重置为 `admin`。

1. 打开服务端页面 <http://127.0.0.1:7400>，使用 `admin` 登录，按提示修改密码并重新登录。
2. 创建隧道，例如公网映射端口 `18080`，有效期 `30` 天；本机测试可将监听地址设为 `127.0.0.1`。
3. 复制弹出的 JWT。关闭弹窗后不会再次展示原文，需要时使用“刷新凭证”。
4. 打开客户端页面 <http://127.0.0.1:7401>，使用 `admin` 登录，按提示修改密码并重新登录。
5. 新建连接，填写运行中的本地服务地址，例如 `127.0.0.1:8080`，粘贴 JWT。
6. 当页面显示在线后，访问 `http://127.0.0.1:18080` 即可到达本地 HTTP 服务。

不要求本地服务是 HTTP，也可以转发 SSH、数据库等 TCP 服务。Web 页面关闭不会停止程序；关闭程序或按 Ctrl+C 才会停止穿透。

后续修改密码时，点击页面右上角“退出登录”旁的“修改密码”，输入当前密码、新密码和确认密码。新密码至少 12 个字符，修改成功后所有现有管理会话立即失效，需要重新登录。若初始化时已显式配置至少 12 个字符的自定义管理密码，则直接使用该密码登录。

## 多平台打包与下载

支持以下 4 个发布目标，每个包同时包含服务端、客户端和 WS / SSE 示例程序。文件名中的版本号来自 `Cargo.toml`，当前为 `0.1.0`。

| 平台 | Rust target | 压缩包 |
| --- | --- | --- |
| Windows x64 | `x86_64-pc-windows-msvc` | `my-frp-0.1.0-windows-x86_64.zip` |
| Linux x64（musl） | `x86_64-unknown-linux-musl` | `my-frp-0.1.0-linux-x86_64.tar.gz` |
| macOS Intel | `x86_64-apple-darwin` | `my-frp-0.1.0-macos-x86_64.tar.gz` |
| macOS Apple Silicon | `aarch64-apple-darwin` | `my-frp-0.1.0-macos-aarch64.tar.gz` |

### 从 GitHub Release 下载

在 [最新 Release](https://github.com/cmsmll/my-frp/releases/latest) 的 **Assets** 中直接下载对应平台压缩包及 `SHA256SUMS`。发布包保存在 GitHub Release，日常开发和发布不需要在本地创建 `dist/`。

压缩包内提供 `myfrps`、`myfrpc`、`server.json`、生产配置模板、`README.md`、中文 `QUICKSTART.txt` 和 `examples/stream_service`；Windows 程序带 `.exe` 后缀并静态链接 C 运行库，Linux 包另外包含 `deploy/` 下的 systemd 模板。包内不包含运行数据库、签名私钥或真实管理凭证。两个 macOS 包是对应架构的命令行程序，未进行 Apple Developer 签名或公证。

校验平台压缩包，Windows 使用 PowerShell 查看 SHA-256 并与同目录的 `SHA256SUMS` 对照：

```powershell
Get-FileHash .\my-frp-0.1.0-windows-x86_64.zip -Algorithm SHA256
Get-Content .\SHA256SUMS
```

Linux 和 macOS 分别使用：

```sh
# Linux
sha256sum --check SHA256SUMS
# macOS
shasum -a 256 --check SHA256SUMS
```

`SHA256SUMS` 包含全部 4 个平台包；使用上面的完整清单校验命令时，需将全部包下载到同一目录。只下载一个平台包时，可直接计算该文件的 SHA-256，并与清单中的对应行比较。

### 自动构建与发布

推送 `v*` 标签后，[Build release packages 工作流](https://github.com/cmsmll/my-frp/actions/workflows/build-release.yml) 会在 Windows、Linux 和两种架构的 macOS runner 上并行编译，执行发布模式的完整 Rust 测试，然后打包。Linux 使用 musl 工具链。全部 4 个目标成功后，才会将 4 个压缩包和合并的 `SHA256SUMS` 发布到 GitHub Release。

发布新版本时，先更新 `Cargo.toml` 和 `Cargo.lock` 中的包版本并提交，再推送对应标签，例如：

```sh
git tag v0.1.1
git push origin main
git push origin v0.1.1
```

也可以在 Actions 页面点击 **Run workflow** 手动发布；`tag` 留空时，使用所选源码的 Cargo 版本生成 `v版本号`，填写时必须与该版本一致。已有标签和正式发布资产受到校验保护，不会用不同构建覆盖已发布的版本；修改程序后应递增版本并创建新标签。

手动运行还可以通过 `source_run_id` 指定本仓库已经成功完成的多平台构建，复用其包，避免重复编译。此时标签指向该次构建实际使用的提交，而不是当前分支的新提交。工作流校验产物的完整性和版本后发布；不需要将包下载到本地。

复用历史构建时，如果该提交的工作流文件与当前默认分支不同，且对应版本标签尚不存在，`GITHUB_TOKEN` 无法创建指向该历史提交的 Release；GitHub 要求额外的工作流写权限。仓库维护者需先通过有仓库写权限的 SSH 执行 `git tag v版本号 构建提交SHA` 和 `git push origin v版本号`，确保标签指向实际构建提交，再使用 `source_run_id` 发布。普通推送标签发布时，标签已经存在，不需要这一步。[GitHub Release API 权限说明](https://docs.github.com/en/rest/releases/releases#create-a-release)

Actions 的 **Artifacts** 仅作为构建及发布过程的中间产物，保留 30 天；用户下载入口以 Release 为准。

### 解压后启动

在解压得到的 `my-frp-版本-平台-架构` 目录中运行，服务端和客户端各占一个终端。包内默认配置用于本机明文联调；公网部署应改用 TLS 配置。

```powershell
# Windows：终端 1，服务端读取同目录 server.json
.\myfrps.exe .\server.json
# Windows：终端 2，客户端
.\myfrpc.exe --allow-insecure
# 可选：终端 3，运行本地 WS / SSE 测试服务
.\examples\stream_service.exe 127.0.0.1:18090
```

```sh
# Linux / macOS：终端 1
./myfrps ./server.json
# Linux / macOS：终端 2
./myfrpc --allow-insecure
# 可选：终端 3
./examples/stream_service 127.0.0.1:18090
```

访问服务端 <http://127.0.0.1:7400> 和客户端 <http://127.0.0.1:7401>，按前文创建隧道。若运行了示例服务，本地地址填写 `127.0.0.1:18090`。首次管理密码为 `admin`，登录后必须修改。程序停止后配置和凭证仍保存在各自的数据目录中。

### 在本机编译打包

仅在需要本地产物时执行本节。安装 Rust stable、Python 3.11 或更新版本，以及目标平台的 C 编译器。打包脚本只收集已编译的程序，不代替编译；默认输出到已被 Git 忽略的 `target/packages/`，并更新其中的 `SHA256SUMS`。Linux 和 macOS 的 tar 包保留程序执行权限。

Windows x64（需要 Visual Studio C++ Build Tools）：

```powershell
rustup target add x86_64-pc-windows-msvc
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --locked --target x86_64-pc-windows-msvc --bins --example stream_service
python scripts/package-release.py --target x86_64-pc-windows-msvc --profile release --output-dir target/packages
```

Linux x64，以下依赖安装命令适用于 Ubuntu / Debian：

```sh
sudo apt-get update
sudo apt-get install --yes musl-tools
rustup target add x86_64-unknown-linux-musl
export CC_x86_64_unknown_linux_musl=musl-gcc
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc
cargo build --release --locked --target x86_64-unknown-linux-musl --bins --example stream_service
python3 scripts/package-release.py --target x86_64-unknown-linux-musl --profile release --output-dir target/packages
```

macOS 需要 Xcode Command Line Tools（首次安装运行 `xcode-select --install`），在对应架构的 Mac 上执行：

```sh
# Intel Mac 使用 x86_64-apple-darwin；Apple Silicon 使用 aarch64-apple-darwin
TARGET=aarch64-apple-darwin
rustup target add "$TARGET"
cargo build --release --locked --target "$TARGET" --bins --example stream_service
python3 scripts/package-release.py --target "$TARGET" --profile release --output-dir target/packages
```

跨系统编译还需要目标系统的链接器、C 工具链和 SDK，只有 `rustup target add` 不足以完成构建。统一生成全部平台包时使用上面的 GitHub Actions 工作流；它在 Windows、Linux 和两种架构的 macOS 环境中并行完成编译、测试和打包。

## WebSocket 与 SSE 示例

仓库提供一个同时包含 WebSocket 和 SSE 的本地 HTTP 服务：`examples/stream_service.rs`。浏览器访问首页即可打开中文测试页面，建立 WS 连接、发送文本或二进制消息，以及订阅 SSE 事件。两种协议共用一个 TCP 隧道，不需要分别创建隧道或修改穿透协议。

### Windows 一键配置并测试

使用 PowerShell 7，在项目根目录运行：

```powershell
pwsh -NoProfile -File .\examples\start-stream-demo.ps1
```

脚本编译并启动服务端、客户端和示例服务，自动创建隧道、配置客户端连接，并通过映射端口验证 WS 中文回显和 SSE 增量事件。默认地址如下：

| 地址 | 用途 |
| --- | --- |
| <http://127.0.0.1:18080> | 经过穿透访问的测试页面，WS 和 SSE 都通过此入口连接 |
| <http://127.0.0.1:18090> | 本地服务，供直接访问对照 |
| <http://127.0.0.1:17400> | 独立的演示服务端管理页面 |
| <http://127.0.0.1:17401> | 独立的演示客户端管理页面 |
| `ws://127.0.0.1:17443` | 演示穿透通道的接入地址 |

两端演示管理密码均为 `stream-demo-password-2026`。演示数据与配置保存在 `target/stream-demo/`，不修改根目录的 `server.json` 或正式数据目录。脚本保持前台运行，按 Ctrl+C 清理本次启动的进程。

需要发布版时添加 `-Release`；已编译对应模式的程序后，可添加 `-SkipBuild` 跳过编译。只运行自动验证并退出时添加 `-VerifyOnly`。默认端口被占用时，使用 `-ServerAdminPort`、`-ClientAdminPort`、`-TransportPort`、`-PublicPort`、`-LocalPort` 指定不同端口。

打开 **18080 端口**的页面，连接 WS 后发送消息，确认收到相同内容；订阅 SSE，确认事件逐条到达。也可以在另一个终端观察流式响应：

```powershell
curl.exe -N "http://127.0.0.1:18080/sse?count=3&interval_ms=1000&tag=demo"
```

### 单独启动示例并手动配置

开发时通过 Cargo 启动本地服务：

```sh
cargo run --locked --example stream_service -- 127.0.0.1:18090
```

打包后直接启动：

```sh
cargo build --release --locked --example stream_service
```

```powershell
# Windows
.\target\release\examples\stream_service.exe 127.0.0.1:18090
```

```sh
# Linux
./target/release/examples/stream_service 127.0.0.1:18090
```

按照前文“快速开始”另外启动 `myfrps` 和 `myfrpc`。在服务端管理页面创建公网端口为 `18080` 的 TCP 隧道，将 JWT 填入客户端，客户端本地服务地址设置为 `127.0.0.1:18090`。隧道在线后打开 <http://127.0.0.1:18080>。若已有隧道占用了该端口，请选择其他允许的端口，并同步修改访问地址。

示例接口：

| 路径 | 行为 |
| --- | --- |
| `/` | 浏览器交互测试页面 |
| `/health` | JSON 健康检查 |
| `/ws` | 原样返回收到的文本和二进制 WebSocket 消息 |
| `/sse?count=3&interval_ms=1000&tag=demo` | 每秒发送一条 `tick` 事件，JSON 数据包含 `seq`、`tag`，共发送 3 条 |

SSE 未指定 `count` 时持续发送；支持 `Last-Event-ID` 恢复序号，无事件可发送时每 15 秒发送注释心跳。网页的 WS 连接每 10 秒发送一条心跳文本并接收回显。Linux 可使用 `curl -N` 替代上述 `curl.exe -N` 命令；`-N` 用于关闭 curl 输出缓冲，便于观察每条事件的到达时间。

穿透数据连接连续约 300 秒没有业务数据会关闭。SSE 注释和业务 WS 的 ping/pong 都会作为 TCP 数据通过隧道，能够保持该连接活跃；穿透控制通道自身的心跳不会重置业务数据连接的空闲计时。示例仅在回环地址使用明文 WS，部署到公网时仍应按下一节配置穿透通道 TLS，并按业务需要配置 HTTPS/WSS。

## 公网部署

使用 `examples/server.production.json` 作为模板，为服务端配置公网域名、与域名匹配的 TLS 证书及私钥。主要字段示例：

```json
{
  "admin_listen": "127.0.0.1:7400",
  "tunnel_listen": "0.0.0.0:7443",
  "public_url": "wss://frp.example.com:7443",
  "data_dir": "/var/lib/myfrp-server",
  "tls_cert": "/etc/myfrp/fullchain.pem",
  "tls_key": "/etc/myfrp/privkey.pem",
  "allow_insecure": false
}
```

Windows 请将上述文件路径改为本机路径，例如 `"C:/myfrp/certs/fullchain.pem"`。JSON 中的相对路径以 **配置文件所在目录** 为基准，与启动程序的工作目录无关。仓库生产模板中的 `../data/server` 和 `../certs/` 分别指向项目根目录下的 `data/server` 和 `certs/`；将模板复制到其他目录部署时应同步修改这些路径。

配置完成后启动服务端：

```sh
# 开发环境，项目根目录
cargo run --locked --bin myfrps -- ./examples/server.production.json
```

```powershell
# Windows 发布版，项目根目录
.\target\release\myfrps.exe .\examples\server.production.json
```

```sh
# Linux 发布版，将生产配置保存到 /etc/myfrp/server.json
./target/release/myfrps /etc/myfrp/server.json
```

`public_url` 是写入 JWT 的客户端接入地址，**不是公网映射端口或管理地址**。必须包含客户端能访问的域名和端口，不能带路径、账号、查询参数或片段。TLS 与 `ws/wss` 配置必须匹配。

管理界面默认只绑定回环地址。启用上述 TLS 后管理页面为 `https://127.0.0.1:7400`，浏览器访问名称必须与证书匹配；可以通过 SSH 转发并使用匹配域名访问，或将 JSON 中的 `admin_listen` 设置为 `"0.0.0.0:7400"`，使用 `https://frp.example.com:7400` 管理。非回环管理地址必须启用 TLS。

客户端正常启动即可：

```sh
# 开发环境
cargo run --locked --bin myfrpc
```

```powershell
# Windows 发布版
.\target\release\myfrpc.exe
```

```sh
# Linux 发布版
./target/release/myfrpc
```

客户端使用内置的公共信任根校验证书。自建 CA 场景使用 `--ca-cert ca.pem` 添加可信证书，不提供关闭证书验证的选项。客户端页面本身也支持 `--tls-cert`、`--tls-key`；仅在需要远程管理客户端时配置非回环监听地址。

在服务器防火墙中放行接入端口和实际使用的映射端口。公网用户到映射端口这一段是否加密取决于被转发的业务协议；穿透链路的 TLS 不会自动把普通 HTTP 变成 HTTPS。

## 配置与运维

### 服务端 JSON 配置

服务端不使用 Clap，也不接受 `--config`、`--public-url` 等选项，只接受一个可选的 JSON 配置文件路径；默认读取 `./server.json`。字段使用下划线命名，拼写错误或未知字段会报错。修改配置后需重启服务端。

| JSON 字段 | 未填写时的默认值 | 含义 |
| --- | --- | --- |
| `admin_listen` | `"127.0.0.1:7400"` | 管理页面监听地址 |
| `tunnel_listen` | `"0.0.0.0:7443"` | 客户端接入监听地址 |
| `public_url` | `"wss://localhost:7443"` | 写入 JWT 的客户端接入地址 |
| `data_dir` | `"data/server"` | 数据目录，相对配置文件目录解析 |
| `admin_password` | `null` | 首次启动管理密码；未指定则为 `admin` 并强制修改，自定义密码至少 12 个字符 |
| `reset_password` | `false` | 本次启动是否重置管理密码 |
| `tls_cert` / `tls_key` | `null` | TLS 证书与私钥文件，需成对配置；相对配置文件目录解析 |
| `allow_insecure` | `false` | 是否允许明文 `ws`，仅本机联调时开启 |
| `port_min` / `port_max` | `10000` / `60000` | 允许创建隧道的公网端口范围 |

默认要求 TLS。仓库根目录的 `server.json` 为快速开始显式设置了本机明文模式；生产环境使用 TLS 模板。服务端不读取 `MYFRP_ADMIN_PASSWORD` 环境变量，管理密码由 JSON 中的 `admin_password` 指定。数据库初始化后，修改该字段不会自动覆盖已有密码。

服务端忘记密码时，停止程序，在 JSON 中将 `reset_password` 设为 `true`，并设置新的 `admin_password`；保留 `null` 则重置为 `admin`，登录后必须修改。启动完成后将 `reset_password` 改回 `false`，避免下次启动再次重置。重置会使原管理会话失效。

### 客户端命令行配置

客户端保留命令行参数，使用 `myfrpc --help` 或 `cargo run --locked --bin myfrpc -- --help` 查看完整参数。

| 参数 | 默认值 | 含义 |
| --- | --- | --- |
| `--admin-listen` | `127.0.0.1:7401` | 客户端管理页面监听地址 |
| `--data-dir` | `data/client` | 数据目录，相对当前工作目录解析 |
| `--admin-password` | `admin` | 首次启动管理密码，也可通过 `MYFRP_ADMIN_PASSWORD` 指定；默认密码首次登录必须修改 |
| `--reset-password` | 关闭 | 本次启动重置管理密码 |
| `--ca-cert` | 未设置 | 可选额外 CA 文件 |
| `--tls-cert` / `--tls-key` | 未设置 | 客户端管理页面 TLS 证书与私钥 |
| `--allow-insecure` | 关闭 | 允许连接明文 `ws` 服务端 |

客户端自定义管理密码至少 12 个字符；数据库初始化后不会因为参数或环境变量变化而覆盖密码。忘记密码时停止客户端，再加 `--reset-password` 启动；可配合 `--admin-password` 或 `MYFRP_ADMIN_PASSWORD` 设置新密码，未指定则重置为 `admin`，登录后必须修改。

### 运行日志

服务端和客户端左侧的“运行日志”记录程序启动、隧道连接、断线重连及运行错误，与“操作记录”中的管理审计分开查看。支持按日志级别和关键词筛选、手动刷新及导出当前筛选结果为文本文件。

日志页面按需拉取数据：登录后停留在隧道管理或操作记录时，不发起日志请求；进入运行日志后才加载。自动刷新仅在运行日志页处于可见状态时进行，离开页面、浏览器标签页转入后台或退出登录时停止，并取消未完成的日志请求。

日志存入各自的 SQLite 数据库，重启后保留，每个实例最多保存最近 10,000 条。日志和导出接口均需要管理员登录，默认密码尚未修改时不能访问。日志不记录管理密码、完整 JWT 或管理会话令牌。

运行事件先进入容量为 2048 条的有界队列，再以最多 64 条或等待 100 毫秒的批次写入，不逐字节记录传输数据。队列满时丢弃日志并统计数量，避免阻塞转发。默认记录 INFO 及以上级别；`RUST_LOG` 环境变量可调整记录级别，例如 `warn` 会省略常规连接信息。

每个实例应使用独立的数据目录。客户端数据库含连接凭证，服务端目录含签名私钥，因此数据目录应仅允许运行账号访问。Unix 下程序将目录权限设为 `0700`；Windows 下请将专用目录 ACL 限定到运行账号和必要的系统管理员。不要把数据目录提交到版本库或公开提供下载。

备份时先停止对应程序，再复制整个数据目录；不能在 SQLite WAL 模式运行中只复制 `state.sqlite`。恢复服务端时需要同时恢复数据库和 `signing.pem`，否则现有 JWT 无法验证。

Linux 后台运行示例见 `deploy/`。服务端 systemd 模板读取 `/etc/myfrp/server.json`；将模板中的数据目录设为 `/var/lib/myfrp-server`，证书使用 `/etc/myfrp/` 下的路径，并确保 `myfrp` 账号可读取配置和证书。Windows 可由任务计划程序在登录或系统启动时启动：服务端使用绝对配置文件路径；客户端设置工作目录或使用绝对 `--data-dir` 路径。

## 明确的运行规则

- 一条服务端隧道同时允许一个客户端会话；重复连接会被关闭，客户端显示错误并退避重试。
- 刷新凭证会立即撤销旧版本，并关闭该隧道现有连接；客户端需要手动替换新 JWT。
- 禁用保留隧道和凭证配置，但拒绝认证并释放公网端口。重新启用后，已因认证失败停止重试的客户端需要在界面重新保存或启用连接。
- 客户端的“停止”仅停止本机连接；服务端的“禁用”关闭公网入口。
- 客户端遇到网络错误自动退避重连，最长间隔约 31 秒。遇到 HTTP 401 或凭证过期则停止自动重试。
- 控制心跳每 10 秒一次，35 秒未响应断开；数据票据 15 秒有效，数据连接空闲 300 秒关闭。
- 编辑服务端隧道可能断开连接。修改监听地址时会先绑定新地址；新地址被占用时保留旧配置和监听。
- 状态页面每 3 秒刷新。在线状态代表控制会话在线；本地服务状态来自最近一次实际连接尝试，不是持续健康探测。
- 流量计数是当前运行周期的内存数据，重启或重建隧道运行实例后归零。
- JWT 可解码但不可篡改，客户端展示的目标地址在建立 TLS 及服务端认证成功前仅为未认证信息。

## 代码结构

```text
src/
  bin/server.rs       服务端入口
  bin/client.rs       客户端入口
  server.rs           隧道监听、JWT、票据、服务端管理 API
  server/config.rs    服务端 JSON 配置加载与路径解析
  client.rs           本地连接、重连、客户端管理 API
  common.rs           SQLite、管理认证、内嵌资源、共享协议
  bridge.rs           双向传输、半关闭、背压与空闲清理
ui/                   内嵌 HTML/CSS/JavaScript，无外部 CDN
tests/e2e.rs          真实进程、TCP 与 TLS 集成测试
tests/ui-smoke.cjs    浏览器操作测试
examples/stream_service.rs   WS / SSE 本地测试服务
examples/start-stream-demo.ps1   一键启动并配置独立穿透演示
deploy/               systemd 模板
```

服务端与客户端共用界面组件，但各自由自己的进程提供页面与 API。SQLite `entries` 表保存带类型的 JSON 配置，配置变更和审计记录在同一事务内提交；`settings` 保存管理密码哈希等设置，运行日志持久化到独立表，连接状态和流量放在内存中。

## 验证

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

集成测试会启动临时实例，测试多服务端连接、4 MiB 完整性与半关闭、JWT 校验、一次性票据重放、活动连接到期、TLS、自定义 CA、刷新撤销、禁用删除、端口冲突回滚、首次默认密码修改、会话撤销、日志鉴权/筛选/导出及重启恢复。

仅运行 WS / SSE 专项端到端测试：

```sh
cargo test --locked --test e2e websocket_and_sse -- --nocapture
```

这组测试使用与示例相同的本地服务，分别建立本机明文 WS 穿透通道和校验证书的 WSS 穿透通道。业务层同时运行多个 WS / SSE 连接，检查 WebSocket 升级、Unicode 文本、超过 1 MiB 的二进制数据、ping/pong、正常关闭，以及 SSE 类型、事件顺序、逐条到达、`Last-Event-ID` 序号恢复和不同连接间的数据隔离。测试使用临时数据目录和动态端口，不修改日常配置。

浏览器测试需要本机 Node.js、Playwright 包和 Microsoft Edge。先运行 `cargo build`，再运行 `node tests/ui-smoke.cjs`；如果 Playwright 安装在全局目录，可设置 `NODE_PATH` 为 `npm root -g` 的结果。截图输出到 `target/ui-preview/`。

当前版本聚焦 TCP 穿透。UDP、域名路由、多节点统一服务端后台、自动续期、带宽限速、桌面托盘、Windows 原生服务安装与长期流量报表尚未实现。
