# Artifactory 命名与迁移记录

首次更名：2026-11-11；命名核对：2026-11-13，Asia/Shanghai。

产品名称由 Artifact Kit 统一改为 **Artifactory**，界面和文档中的产品展示名首字母大写；命令、配置和资源标识使用 `artifactory`。项目仓库为 [zhubby/artifactory](https://github.com/zhubby/artifactory)。当前模块布局见[模块布局与命名](module-layout.md)。

## 当前名称与公共接口

| 范围 | 当前标识 |
|---|---|
| Rust 控制平面 | `artifactory-control-plane` / `artifactory_control_plane`，目录 `crates/control-plane` |
| Rust 设备运行程序 | `artifactory-emissary` |
| Node.js 规划 Worker | `artifactory-agent-worker`，目录 `packages/agent-worker` |
| 控制台 | `artifactory-console` |
| 历史配方文件 | `artifactory.yaml`；新建 AI 构建无需仓库配置文件 |
| 配方 schema | `schemas/artifactory.v1.schema.json` |
| 环境变量及步骤参数前缀 | `ARTIFACTORY_`，包括 `ARTIFACTORY_PARAM_` |
| 会话 Cookie / CSRF 请求头 | `artifactory_session` / `x-artifactory-csrf` |
| 浏览器偏好 | `artifactory-locale` / `artifactory-theme` / `artifactory-effects` |
| Emissary Protobuf 命名空间 | `artifactory.emissary.v1` |
| Helm Chart | `charts/artifactory` |
| 发布镜像 | `ghcr.io/zhubby/artifactory-{control-plane,web,agent-worker}` |

设备端使用 Rust Emissary，共享协议位于 `proto/artifactory/emissary/v1`。当前协议、系统服务和 OTA 操作以 [Emissary 运维](operations.md#emissary-安装与注册)及 [OTA 文档](emissary-ota.md)为准。

默认 Compose 项目、Helm release / namespace、数据库名称与用户名、S3 bucket / 身份以及产品运行目录统一使用 `artifactory`。Emissary 可执行文件和发布资源使用 `artifactory-emissary` 前缀。CI 专用账户为 `artifactory-ci`，适用于 Windows 的用户名长度限制。

## 已有部署

更名不提供旧名称别名或自动迁移。设备端与控制平面需按协议兼容性同步切换；使用历史配方的构建仓库需采用当前文件名，部署环境需设置当前前缀变量，浏览器需重新登录和选择界面偏好。

现有 `.local` / `.deploy` 配置、密钥、证书、数据库、对象及已安装系统服务不会由更名操作重写或删除。默认初始化脚本也不会覆盖已有环境文件或持久凭据。已有部署应先保留备份，再在单独环境按现行运维指南配置新版本。

镜像发布使用当前产品名和仓库 source 标签。Emissary 发布签名使用受保护 Environment 中的 `EMISSARY_RELEASE_SIGNING_SEED` Secret，工作流传给程序的变量为 `ARTIFACTORY_EMISSARY_RELEASE_SIGNING_SEED`；通用 GitHub / SDK 环境变量按各自接口使用。

## 历史证据与 2026-11-11 验证

[原实施与验收记录](acceptance.md)、[原 Agent OTA 验收记录](agent-ota-acceptance-2026-11-11.md)及带日期的设计文档统一使用当前产品名称，保留原提交、运行编号、摘要、结果及验收边界。历史记录对应各自当时的代码和运行，不能作为后续命名或模块调整已通过验收的证据。

以下为 2026-11-11 更名时执行的验证，测试使用隔离数据库、对象存储和临时工作目录；其中 Go Agent 结果属于当时的实现：

| 验证范围 | 当时结果 |
|---|---|
| Python 脚本测试 | 10 项通过 |
| Rust | 格式、Clippy、默认测试与构建通过；真实 PostgreSQL 数据库测试 12 项通过 |
| Go | Vet、Race 测试通过；Linux amd64 / arm64、macOS amd64 / arm64、Windows amd64 五种 Agent 构建成功；当时仓库模块路径的版本注入有效 |
| 前端 | 类型检查、生产构建通过；57 项 UI 测试和 10 项真实后台浏览器测试通过，覆盖中英文、桌面、平板及移动端；已检查名称与页面标题 |
| 原生 Rust / Go 联测 | 35 项验收检查通过，覆盖注册、新配置解析、构建、macOS ad-hoc 签名、日志、制品上传下载、取消、进程退出与服务重连 |
| 独立协议客户端 | 完整实时联测通过，覆盖 mTLS、单次注册、续租、事件隔离、重放、并发上传与实际 S3 阻塞 |
| 当时公共接口 | 6 项实测通过：更名后的会话 Cookie / CSRF 请求头可用，旧标识被拒绝，退出清理 Cookie，仅含旧配置文件的仓库构建失败 |
| OTA | 五种实际二进制的资源名、下载路径、大小和哈希一致；Go 生成的签名通过 OpenSSL 与 Rust 验证；已签名旧资源名和被篡改清单被拒绝 |
| 部署 | Helm 默认与 kind 配置严格检查和渲染通过，生产 HTTP 配置被拒绝；Compose 配置解析通过，隔离 TLS 验收 25 项环境检查及 30 项流量检查通过 |
| 静态一致性 | 当时 Protobuf 重生成结果一致，字段与协议主版本不变；镜像命名脚本在 Linux Bash 中验证通过；现行标识清理以当前仓库检查结果为准 |

该轮 Compose/TLS 使用包含更名修改的独立临时 Git 仓库，快照为 `d555133ac4e7ee7e2e4b4a89875b96db7cf1ec4c`，运行编号为 `d006be00aee9`。该轮非文档源码与验收快照一致；后续变更需以各自提交和验证记录为准。测试通过真实 Caddy HTTPS、Agent mTLS、私有 S3、SSE、运行中构建重启和日志缓冲耗尽恢复，并核对 PostgreSQL 中日志序号、大小与哈希；该轮临时容器、卷、网络均已移除。

镜像构建遇到外部依赖下载超时后，改用与锁文件核对过的本地 Cargo / npm 依赖缓存离线构建，并通过 `docker.m.daocloud.io` 获取基础镜像。该轮产品源码未变；原生产 Dockerfile 和实际验证 Dockerfile 的哈希分别留存。这证明缓存辅助构建和实际容器运行通过，不代表该轮无缓存公网构建成功。控制平面与前端为 Linux arm64，通用探针及 PostgreSQL / S3 使用本机 Docker 支持的 amd64 镜像；探针执行的是该轮快照交叉编译的 Agent。

该轮验证摘要与证据保存在 `.local/artifactory-rename-20261111/`，包括 `summary.json`、`compose-evidence.json`、原生 / 协议 / 浏览器日志及缓存构建配方。该轮原有 198 个私有配置、密钥和证书文件的哈希保持不变，原 PostgreSQL / S3 容器的标识、启动时间与重启次数保持不变；当时 4 份历史记录逐字节保留。当前命名清理更新文档文字，保留历史提交、运行编号和摘要。

系统服务原生生命周期验收仅在显式启用的 GitHub 托管临时 VM 中执行；该轮未运行该跨平台 CI，也未发布镜像或修改生产部署。
