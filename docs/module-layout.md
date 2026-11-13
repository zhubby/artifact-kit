# 模块布局与命名

| 模块 | 目录 | 包 / 命令 | 中文名称 |
|---|---|---|---|
| control-plane | `crates/control-plane` | Cargo `artifactory-control-plane`，Rust `artifactory_control_plane` | 控制平面 |
| agent-worker | `packages/agent-worker` | npm / CLI `artifactory-agent-worker` | Agent worker |

Rust 模块使用 `crates/`；Node.js 模块使用 `packages/`，各模块保留自己的 `package.json` 与锁文件。Next.js 控制台仍位于 `web/`。

控制平面的 Compose 服务、Helm Deployment / Service / PVC、镜像组件名统一为 `control-plane`；worker 对应名称统一为 `agent-worker`。Helm values 使用 `images.control-plane`、`resources.control-plane`、`persistence.control-plane` 和 `agentWorker.enabled` / `agentWorker.serverName`。

本次调整沿用数据库表、SQL 迁移文件内容、RPC、`/ai-workers` API、`ARTIFACTORY_AI_*` / `ARTIFACTORY_CONTROL_URL` 环境变量，以及 Operator 的 `controlUrl` 字段。已有注册身份和协议无需重新生成。

## 从旧布局升级

2026-11-13 之前的控制平面源码目录为 `crates/control`，命令为 `artifactory-control`；worker 源码目录为 `ai-worker`，命令为 `artifactory-ai-worker`。历史验收结果仍对应记录中的源码提交。

新安装使用 `.local/control-plane`、`.local/agent-worker`、`.deploy/control-plane` 和 `.deploy/agent-worker`。升级既有实例时，先按[运维手册](operations.md#备份恢复与保留)排空任务、停止写入并备份，再迁移原有控制平面数据和 worker 身份目录；沿用现有环境文件时，也需要更新其中的数据路径。保留原主密钥、CA、证书和身份配置。

Helm 的控制平面 PVC 从 `<release>-control` 改为 `<release>-control-plane`，worker PVC 从 `<release>-ai-worker` 改为 `<release>-agent-worker`。升级前将原 PVC 内容恢复到对应新 PVC，验证恢复数据和身份后再启动新 Deployment；原 PVC 继续按保留策略保留。同步更新自定义 values 中的组件键、镜像、服务 DNS 和代理配置，并为变更后的内部 DNS 续签服务器证书。
