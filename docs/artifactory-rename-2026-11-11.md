# artifactory 更名记录

日期：2026-11-11，Asia/Shanghai。

项目内部名称由 Artifact Kit 统一改为 `artifactory`。GitHub 仓库仍为 `https://github.com/zhubby/artifact-kit`，本地检出目录不改名。版本号、业务 API 路由、数据库表结构与制品领域术语保持现状。

## 新名称与公共接口

| 原名称 | 新名称 |
|---|---|
| `artifact-control` / Rust `artifact_control` | `artifactory-control-plane` / `artifactory_control_plane` |
| `artifact-agent` / `artifact-kit-agent` 服务 | `artifactory-agent` |
| `artifact-kit-console` | `artifactory-console` |
| 仓库根目录 `artifact-kit.yaml` | `artifactory.yaml` |
| `schemas/artifact-kit.v1.schema.json` | `schemas/artifactory.v1.schema.json` |
| `AK_` 环境变量及步骤参数前缀 | `ARTIFACTORY_`，包括 `ARTIFACTORY_PARAM_` |
| `ak_session` / `x-artifact-csrf` | `artifactory_session` / `x-artifactory-csrf` |
| `ak-locale` / `ak-theme` / `ak-effects` | `artifactory-locale` / `artifactory-theme` / `artifactory-effects` |
| `artifactkit.agent.v1` | `artifactory.agent.v1` |
| `charts/artifact-kit` | `charts/artifactory` |
| `ghcr.io/zhubby/artifact-kit-{control-plane,web}` | `ghcr.io/zhubby/artifactory-{control-plane,web}` |

Go 模块改为实际仓库路径 `github.com/zhubby/artifact-kit/agent`。Go 导入、Protobuf `go_package`、代码生成参数和发布时的版本注入使用同一路径。Protobuf 消息及字段编号未改变，协议主版本仍为 1；gRPC 方法使用新命名空间。

默认 Compose 项目、Helm release / namespace、数据库名称与用户名、S3 bucket / 身份以及产品运行目录统一使用 `artifactory`。Agent 可执行文件和发布资源使用 `artifactory-agent` 前缀。CI 专用账户为 `artifactory-ci`，适用于 Windows 的用户名长度限制。

## 已有部署

本次不提供旧名称别名或自动迁移。旧 Agent 与新控制平面需同步切换；构建仓库需提交新文件名的配置，部署环境需设置新前缀变量，浏览器需重新登录和选择界面偏好。

现有 `.local` / `.deploy` 配置、密钥、证书、数据库、对象及已安装系统服务不会由更名操作重写或删除。默认初始化脚本也不会覆盖已有环境文件或持久凭据。已有部署应先保留备份，再在单独环境按现行运维指南配置新版本。

镜像发布显式使用新产品名，同时保留真实 GitHub source 标签；仓库未改名不会使镜像回到旧名称。签名 Secret `AGENT_RELEASE_SIGNING_SEED` 和通用 GitHub / SDK 环境变量维持原名，工作流传给程序的变量使用 `ARTIFACTORY_`。

## 历史证据与本次验证

[原实施与验收记录](acceptance.md)、[原 Agent OTA 验收记录](agent-ota-acceptance-2026-11-11.md) 及带日期的设计文档保留原文。它们对应更名前的提交与运行，不能作为本次新名称已通过验收的证据。

本次已执行以下验证，测试使用隔离数据库、对象存储和临时工作目录：

| 验证范围 | 结果 |
|---|---|
| Python 脚本测试 | 10 项通过 |
| Rust | 格式、Clippy、默认测试与构建通过；真实 PostgreSQL 数据库测试 12 项通过 |
| Go | Vet、Race 测试通过；Linux amd64 / arm64、macOS amd64 / arm64、Windows amd64 五种 Agent 构建成功；实际仓库模块路径的版本注入有效 |
| 前端 | 类型检查、生产构建通过；57 项 UI 测试和 10 项真实后台浏览器测试通过，覆盖中英文、桌面、平板及移动端；已检查名称与页面标题 |
| 原生 Rust / Go 联测 | 35 项验收检查通过，覆盖注册、新配置解析、构建、macOS ad-hoc 签名、日志、制品上传下载、取消、进程退出与服务重连 |
| 独立协议客户端 | 完整实时联测通过，覆盖 mTLS、单次注册、续租、事件隔离、重放、并发上传与实际 S3 阻塞 |
| 新公共接口 | 6 项实测通过：新会话 Cookie / CSRF 请求头可用，旧标识被拒绝，退出清理新 Cookie，仅含旧配置文件的仓库构建失败 |
| OTA | 五种实际二进制的资源名、下载路径、大小和哈希一致；Go 生成的签名通过 OpenSSL 与 Rust 验证；已签名旧资源名和被篡改清单被拒绝 |
| 部署 | Helm 默认与 kind 配置严格检查和渲染通过，生产 HTTP 配置被拒绝；Compose 配置解析通过，隔离 TLS 验收 25 项环境检查及 30 项流量检查通过 |
| 静态一致性 | Protobuf 重生成结果一致，字段与协议主版本不变；镜像命名脚本在 Linux Bash 中验证通过；现行源码未残留旧产品标识，真实仓库地址除外 |

Compose/TLS 使用包含本次修改的独立临时 Git 仓库，快照为 `d555133ac4e7ee7e2e4b4a89875b96db7cf1ec4c`，运行编号为 `d006be00aee9`。当前非文档源码与该快照一致。测试通过真实 Caddy HTTPS、Agent mTLS、私有 S3、SSE、运行中构建重启和日志缓冲耗尽恢复，并核对 PostgreSQL 中日志序号、大小与哈希；本次临时容器、卷、网络均已移除。

镜像构建遇到外部依赖下载超时后，改用与锁文件核对过的本地 Cargo / npm 依赖缓存离线构建，并通过 `docker.m.daocloud.io` 获取基础镜像。产品源码未变；原生产 Dockerfile 和实际验证 Dockerfile 的哈希分别留存。这证明缓存辅助构建和实际容器运行通过，不代表本次无缓存公网构建成功。控制平面与前端为 Linux arm64，通用探针及 PostgreSQL / S3 使用本机 Docker 支持的 amd64 镜像；探针执行的是本次快照交叉编译的 Agent。

本地验证摘要与证据保存在 `.local/artifactory-rename-20261111/`，包括 `summary.json`、`compose-evidence.json`、原生 / 协议 / 浏览器日志及缓存构建配方。原有 198 个私有配置、密钥和证书文件的哈希保持不变，原 PostgreSQL / S3 容器的标识、启动时间与重启次数保持不变；4 份历史记录逐字节保留。

系统服务原生生命周期验收继续仅在显式启用的 GitHub 托管临时 VM 中执行；本次未运行该跨平台 CI，也未发布镜像或修改生产部署。
