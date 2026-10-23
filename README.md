# OCI Pull

[![Build portable packages](https://github.com/SirlyDreamer/OCI-Pull/actions/workflows/build.yml/badge.svg)](https://github.com/SirlyDreamer/OCI-Pull/actions/workflows/build.yml)
[下载 Portable 版本](https://github.com/SirlyDreamer/OCI-Pull/releases/latest)

用 Rust 和 GPUI 编写的独立容器镜像下载器。输入镜像引用，直接从 Registry 下载并导出离线镜像归档，不启动 Docker，也不调用 skopeo、crane、tar 或 zstd 等外部程序。

## 使用

只发布解压即用的 Portable 包：

| 平台 | 发布文件 | 图形界面入口 |
| --- | --- | --- |
| Windows x64 | `oci-pull-v版本-windows-x64-portable.zip` | `oci-pull.exe` |
| macOS 15+ Apple Silicon | `oci-pull-v版本-macos-arm64-portable.tar.gz` | `OCI Pull.app` |
| macOS 15+ Intel | `oci-pull-v版本-macos-x64-portable.tar.gz` | `OCI Pull.app` |
| Linux x64（Ubuntu 24.04+） | `oci-pull-v版本-linux-x64-portable.tar.gz` | `./oci-pull` |

每个包还包含可选的 `oci-pull-cli`（Windows 为 `.exe`）、说明和许可证，图形界面可独立使用。Release 附带 `SHA256SUMS.txt`。不提供 MSI、DMG、deb 或 rpm 安装程序。

Portable 指无需安装即可运行；配置与任务数据仍使用下文所述的用户目录，不写入程序所在目录。macOS 包采用本地签名，未做 Apple 公证；首次启动可能需要在系统“隐私与安全性”设置中允许。Linux 需要图形会话、支持 Vulkan 的驱动，以及 Fontconfig、Wayland/X11、xkbcommon 和 OpenSSL 3 系统库。

主界面是任务列表，无侧栏。顶部提供搜索、状态筛选、“设置”、文件夹按钮和“新建任务”。文件夹按钮位于“新建任务”左侧，打开当前默认下载目录（最近创建任务的保存目录）；目录不存在时自动创建。首次启动默认保存到下载目录下的 `oci-pull` 文件夹。

界面参考 [poco-ai/react-hono-template](https://github.com/poco-ai/react-hono-template) 的中性主题，使用细边框、紧凑表格和 Lucide 图标；保留 Rust + GPUI 原生实现。浅色／深色跟随系统变化，输入框、下拉菜单、弹窗及交互状态统一适配。Windows 字体使用 Segoe UI，中文由系统字体回退显示。标题栏由 GPUI 绘制，显示 OCI Pull 名称和圆角矩形 Container Logo，支持拖动、双击最大化／还原、最小化、最大化／还原和关闭；随应用切换深浅配色，不显示系统标题栏。

Logo 使用深灰圆角矩形和白色 Lucide Container 图标，外侧透明。标题栏显示同款 Logo；Windows 程序资源嵌入 16–256 像素的九档 ICO；macOS 应用包含 16–1024 像素 ICNS。矢量 SVG、512 像素 PNG、ICO、ICNS 与生成脚本在 `assets/brand` 和 `tools/generate-logo.cjs`；普通构建和运行均不依赖 Node.js 或外部图像文件。

点击“新建任务”打开紧凑的两步弹窗：

1. **镜像来源**：填写 `alpine:latest`、`ghcr.io/owner/image:tag` 或 `@sha256:…` 引用，通过下拉框选择目标平台；点击“下一步”解析一次镜像。
2. **保存与确认**：显示解析后的镜像、平台、大小、层数和 digest；通过下拉框选择 Docker／OCI 格式和压缩格式，自定义文件名，填写或浏览保存路径，点击“创建任务”。更改压缩格式时自动调整文件扩展名，已有同名文件不会被覆盖。

第二步和下载任务复用解析结果；返回第一步但未改动镜像／平台，也不会重新解析。更改镜像或平台才重新解析。下载按已确认 digest 获取，仍执行摘要和完整性校验。

新建向导不包含下载或网络设置。主界面滑杆图标统一打开“设置”，输入下载并发数（默认 16，范围 1–128），通过下拉框选择分块大小（默认 64 MiB），也可填写代理、仓库协议和指定仓库的凭据；凭据仅用于匹配的仓库。下载配置保存到偏好文件，代理与凭据仅在当前会话中使用，不把密码或代理 URL 写入磁盘。代理与认证更改用于后续新建或继续的连接。

任务列表显示大小、进度、速度和状态。每行固定提供 Lucide **继续、暂停、删除** 三个图标按钮，按状态启用；失败任务也可点击继续重试。点击任务行打开 **任务详情** 弹窗，展示镜像、平台、归档格式、完整保存路径、摘要、当前阶段和错误信息，并实时更新总体进度、速度及各层进度。层列表按清单顺序显示摘要、已下载／总大小、百分比和等待／下载／暂停／缓存命中／已下载状态，同一摘要只显示一次。完成后可在弹窗中打开文件位置。关闭弹窗不影响下载，列表中的操作按钮不会打开详情。

层明细随任务记录保存。继续任务时重新检查缓存和断点，按实际文件长度恢复进度，不以界面上次记录的字节数作为续传依据。

删除运行中的任务会先取消并等待工作停止，再回收该任务缓存并移除记录，保留已经导出的归档。最多同时运行两个镜像任务，其余排队；各镜像任务内部使用统一分块队列，默认最多 16 路下载。

重启后未完成的任务显示为暂停；点击继续直接恢复参数、文件名和原 digest，不打开新建向导。若上次使用了代理或私有凭据且本次尚未设置，会在主界面提示补充，然后可再次继续。

界面采用 GPUI 的 GPU 渲染，Windows 使用 Direct3D，macOS 使用 Metal，Linux 使用 Vulkan。Windows 发布版静态链接 C 运行库，下载核心无需管理员权限。

## 下载调度与续传

“设置”中的下载配置对所有镜像任务统一应用，保存后立即通知正在运行的调度器，无需暂停或重建任务：

| 配置 | 默认值 | 输入／选项 | 运行时行为 |
| --- | --- | --- | --- |
| 下载并发数 | 16 | 可输入 1–128 的整数 | 每个镜像任务的分块下载上限，覆盖该镜像全部层；增加时立即唤醒排队工作，降低时让已获得名额的块完成，期间不补充超出新上限的下载 |
| 分块大小 | 64 MiB | 8、16、32、64、128、256 MiB | 下一块获得下载名额时使用新大小；正在传输及已有断点的块保持原字节边界 |

所有层共用该任务的下载名额，在有待下载分块的层之间轮流派发，不再给每个层固定连接配额。合并、哈希校验和归档不占用下载名额。多个镜像任务分别遵守这个上限。

调度方式参考 [aria2 的分块和连接配置](https://aria2.github.io/manual/en/html/aria2c.html)，在 Rust 进程中实现，不依赖 aria2 程序。`DownloadManager` 管理全局实时配置；任务调度器分配名额；分块布局模块记录字节范围；HTTP 传输模块只负责单块请求和校验。UI 与这些模块通过共享管理器及进度事件通信。

暂停、连接中断或程序重启后，完整分块不重复下载，部分分块通过 HTTP Range 从其实际文件长度继续。逐次校验响应的 `Content-Range` 起点、终点、总大小和长度，再写入断点；全部分块合并后校验整个层的 SHA-256。已完整保存的层复用前也会重新校验。普通下载留下的连续 `.part` 文件按原连续方式续传。

仓库忽略 Range、返回不合法范围或 416 时回退到普通下载，200 响应从文件头写入，不追加到旧断点。断点遭破坏时清理无效数据再重试，最多三次；不支持 Range 的仓库无法保证单层续传。任务暂停会等待正在写入的数据落盘；同一层缓存由文件锁防止多个写入者同时修改。归档阶段中断后会重新打包，镜像层断点继续保留。

## 两个独立的格式选项

| 选项 | 输出内容 | 用途 |
| --- | --- | --- |
| Docker 兼容 | `manifest.json`、镜像配置、按原顺序排列的未压缩层 tar | 供 Docker image load 导入 |
| OCI | `oci-layout`、`index.json`、`blobs/sha256/…` | 供支持 OCI Image Layout 的工具导入 |
| tar / tar.gz / tar.bz2 / tar.xz / tar.zst | 上述镜像归档外部的压缩方式 | 未压缩 / gzip / bzip2 / xz / zstd；Docker 和 OCI 格式均可选 |

Docker 模式保留镜像配置、层和标签，不是把容器文件系统扁平化的 `docker export`。带 digest 且没有标签的源不会被擅自添加 `latest` 标签。

在目标 Docker 环境导入 Docker 兼容格式：

```sh
docker image load --input alpine-offline.tar.xz
```

已覆盖 [Docker 官方文档](https://docs.docker.com/reference/cli/docker/image/load/)列出的全部归档压缩类型：gzip、bzip2、xz、zstd，以及未压缩 tar。旧目标环境不支持某格式时，可选择 tar.gz 或 tar。OCI 格式的实际导入命令由目标运行时决定。

## 已实现

- Registry v2 / OCI 镜像解析、标准仓库认证、公开或用户名＋密码／令牌访问。
- 多架构索引中选取一个平台，支持 `os/arch/variant`，按 digest 获取具体清单。
- 任务级分块调度、并发和块大小实时配置、单层／分块内续传、最多三次重试、SHA-256 和大小校验。
- 已下载的配置与镜像层按内容摘要缓存，暂停／失败重试时复用并重新校验；导出成功后回收该任务缓存。
- Docker 导出逐层解压并校验 `diff_id`；支持 gzip、zstd 和未压缩的源镜像层。
- OCI 导出保留层数据及清单附加字段；Docker media type 转换到对应 OCI media type。
- 内置 tar / gzip / bzip2 / xz / zstd 编解码，不需要辅助可执行程序。
- 可取消任务、显示层进度；临时归档完成后才发布最终文件，拒绝覆盖同名文件。
- 记住镜像、平台、目录和格式；不将仓库密码或代理地址保存到偏好文件。

## 配置、缓存与回收

当前配置保存在用户临时目录的 `oci-pull/preferences.json`，Windows 通常为 `%TEMP%\oci-pull\preferences.json`，记录下载参数、缓存目录、历史缓存目录和上次保存选项。代理与凭据仍仅保存在会话内。

任务记录独立保存在 `%LOCALAPPDATA%\oci-pull\tasks\task-<字母编码>\task.json`。默认缓存根目录为 `%LOCALAPPDATA%\oci-pull\cache`，可在“设置 → 缓存目录”中输入绝对路径或点击文件夹图标选择。配置与任务缓存独立管理，不读取早期原型的目录或环境变量。

```text
<缓存根目录>/
  .oci-pull-lock                     # 根目录协调锁，0 字节
  locks/                            # 使用中的条目锁；释放后删除
  tasks/task-<字母编码>/
    .oci-pull-cache                  # 缓存归属标记
    manifests/<sha256>.json          # 镜像清单与配置
    blobs/
      <sha256>                      # 校验通过的完整 blob
      <sha256>.part                 # 普通下载断点
      parts-<摘要的字母编码>/
        layout.json                 # 分块字节范围
        <start>-<end>.part           # 分块断点
      <sha256>.lock                 # blob 写入锁
      <sha256>.no-range             # Range 回退标记
  previews/preview-<字母编码>/                    # 预览临时数据
  cli/<镜像与平台的字母编码>/          # CLI 下载与失败断点
```

应用自行生成的目录名只使用小写英文字母 `a-z` 和连字符 `-`。任务 ID、预览标识和 CLI／分块摘要采用可逆、无碰撞的字母编码，目录名不含数字、空格、下划线或点号。规则适用于应用管理的目录层级；用户选择的父路径、文件名及 Docker／OCI 归档内部的标准路径保持各自规则。

回收规则：

- 导出成功：立即释放该任务的镜像层、断点和元数据缓存，保留任务历史和导出归档。
- 暂停或失败：保留缓存，继续时恢复。删除任务：先停止工作，再清理缓存与任务记录。
- 预览窗口及解析工作结束：释放预览缓存。启动时回收已完成任务、无对应记录的缓存、异常退出遗留的预览，以及超过 7 天未使用的 CLI 断点。
- 设置左下角“清除缓存”：清理已使用目录中可回收的缓存，包括暂停／失败任务的断点；保留任务记录和导出文件，跳过正在使用的条目，并显示回收大小。清理后继续未完成任务，会重新下载。

更换目录且有旧任务缓存时，会弹窗提供 **迁移、清理、取消**。迁移保留分块及断点；清理会释放旧数据。运行中的任务先安全停止，操作结束后自动恢复，原本暂停的任务保持暂停。迁移先复制并对每个文件进行 SHA-256 校验，再原子保存新路径，最后回收旧副本；目标无法写入、被占用或已有冲突数据时保留可恢复的副本并报告错误。多个任务逐个提交，部分失败时已经完成的迁移保留，默认目录仅在全部成功后切换。旧目录清理失败的残留可在下次启动或手动清理时重试。

缓存清理只处理带 OCI Pull 归属标记的条目，使用操作系统锁保护正在下载、打包、预览或由另一进程占用的缓存，不跟随缓存内的链接或重解析点。缓存目录不能与配置、任务记录目录重叠，不能包含镜像保存目录，也不能与旧缓存目录相互嵌套。

CLI 的 `--cache` 指定缓存根目录；省略时读取同一偏好文件中的缓存目录，未配置则使用默认目录。CLI 解析使用临时预览缓存，成功导出自动清理，失败或取消保留断点。

## 首版边界

分块合并时会同时保留分块和正在合并的完整层，校验成功后清除分块，因此该阶段可能需要约两份层数据的磁盘空间。Docker 导出还需要一个正在解压的层临时文件和最终归档的磁盘空间。OCI 模式直接打包下载后的层。大镜像不会全部读入内存。

每个任务处理一个镜像和一个平台，可以连续创建多个任务。暂停保留完整层和未完成的层／分块断点。暂不支持批量粘贴导入、多平台合并导出、云厂商专用凭据助手、Docker schema 1、非 SHA-256 摘要、签名及 SBOM 附件同步。仓库自身的限流、权限和网络限制仍然适用。

预览锁定具体 manifest digest；排队、暂停和重启后继续都使用这个 digest，即使源标签改变也不会切换镜像。重启后会按 digest 重新获取并校验元数据。

已做真实 Docker Hub 下载及归档结构验证。本机没有可用的 Docker 命令，因此没有执行实际 `docker load` 或运行容器测试。真实私有仓库登录尚未验证。

## 命令行

```powershell
.\oci-pull-cli.exe inspect alpine:latest
.\oci-pull-cli.exe pull alpine:latest --platform linux/amd64 --format docker --compression zstd --output D:\Images
.\oci-pull-cli.exe pull alpine:latest --platform linux/arm64 --format oci --compression gzip --output D:\Images
.\oci-pull-cli.exe pull alpine:latest --format docker --compression xz --filename alpine-offline --output D:\Images
.\oci-pull-cli.exe pull alpine:latest --format docker --compression bzip2 --filename alpine-offline.tar.bz2 --output D:\Images
.\oci-pull-cli.exe pull nginx:latest --proxy http://127.0.0.1:7890 --concurrency 16 --chunk-size-mib 64 --zstd-level 3
```

CLI 的 `--concurrency` 接受 1–128（默认 16），`--chunk-size-mib` 接受 1–256（MiB，默认 64）；图形界面的分块选项固定为 8、16、32、64、128、256 MiB；已有断点保持原范围，新分块使用指定大小。

`--compression` 可选 `none`、`gzip`、`bzip2`（别名 `bz2`）、`xz`、`zstd`。`--filename` 可填写基本文件名或带归档扩展名的文件名，扩展名按压缩选择统一调整。私有仓库密码通过 `OCI_PULL_PASSWORD` 环境变量提供，配合 `--username`；也可用 `--password-env` 指定其他环境变量名。`--http` 显式启用明文仓库连接，默认 HTTPS。取消命令行任务使用 Ctrl+C。

## 从源码构建

Rust 版本由 `rust-toolchain.toml` 固定，依赖由 `Cargo.lock` 固定。Windows 需要 Visual Studio C++ Build Tools 和 Windows SDK；macOS 需要 Xcode Command Line Tools；Ubuntu 24.04 先运行 `bash tools/install-linux-deps.sh`。

```sh
cargo run --locked --bin oci-pull
cargo fmt --all -- --check
cargo clippy --locked --release --all-targets -- -D warnings
cargo test --locked --release --all-targets -- --test-threads=1
cargo build --locked --release --bins
```

仅构建下载核心和 CLI：

```sh
cargo build --locked --release --no-default-features --bin oci-pull-cli
```

生成 Portable 包（将 target 替换为对应平台；Python 3.11+）：

```sh
cargo build --locked --release --bins --target x86_64-pc-windows-msvc
python tools/collect-licenses.py --target x86_64-pc-windows-msvc --output target/THIRD-PARTY-LICENSES.txt
python tools/package.py --target x86_64-pc-windows-msvc --licenses target/THIRD-PARTY-LICENSES.txt
```

macOS 打包需要在 macOS 上运行，以签名并校验 `.app`。打包程序逐个核对归档内文件的 SHA-256，Linux/macOS 同时检查可执行权限，输出到 `dist/`。

## GitHub Actions 与发布

`.github/workflows/build.yml` 在 `main` 推送、PR 和手动触发时，分别在 Windows、macOS ARM64、macOS x64、Ubuntu x64 原生 runner 上运行格式检查、Clippy、测试和构建，上传 Portable 包为 Actions artifacts。

更新 `Cargo.toml` 与 `Cargo.lock` 的版本后，推送对应 `v*` 标签（例如 `v0.1.0`）。只有四个平台均通过，才创建 Release、上传四个包及 `SHA256SUMS.txt` 并公开发布。标签与 Cargo 版本不一致会拒绝打包。GitHub Actions 使用固定提交的官方 actions；Release 使用仓库自带的 `GITHUB_TOKEN`，不需要额外配置个人令牌。

## 代码结构

- `src/ui.rs`、`src/ui/`：任务列表、两步向导、详情、设置和自绘标题栏。
- `src/registry.rs`：Registry 解析、下载流程编排。
- `src/download/`：独立的下载管理器、调度器、持久化分块布局和 HTTP 传输。
- `src/cache.rs`、`src/paths.rs`：缓存锁、归属标记、迁移、回收和目录规范。
- `src/archive.rs`：Docker / OCI 归档与压缩；`src/cli.rs` 复用同一下载核心。
- `tests/pipeline.rs`：本地 Registry fixture，经真实 HTTP 客户端覆盖下载、续传、Range 回退、归档和缓存行为。
- `vendor/oci-client`：0.18.0 的本地补丁，说明见 `PATCHES.md`，保留上游 Apache-2.0 许可证。

UI 使用 [GPUI Kit](https://github.com/longbridge/gpui-kit)，图标使用 [Lucide](https://lucide.dev/)。应用采用 MIT 许可证；发布包按目标平台收集依赖声明及许可文本。Logo 图标的许可随 `assets/brand/LICENSE-LUCIDE` 提供。
