# Agent OTA 自动升级

控制面管理目标版本，节点通过已有 mTLS 出站连接接收升级命令，使用私有 S3 的短期签名地址下载。普通升级不需要 SSH 或管理员密码。已有节点默认不升级；先安装固定启动器，再由管理员指定节点或节点池目标。

## 发布与信任配置

`.github/workflows/agent-release.yml` 在五种原生 runner 上测试、构建 Linux amd64/arm64、macOS amd64/arm64、Windows amd64，然后生成 `agent-manifest.json` 和 `agent-manifest.sig`。manifest 包含版本、Git commit、协议兼容范围和每个二进制的大小及 SHA-256。签名覆盖 manifest 的原始字节。协议主版本仍为 1。

在受保护的 GitHub `agent-release` Environment 中设置 `AGENT_RELEASE_SIGNING_SEED`，值为独立 Ed25519 私钥的 32 字节 seed 的 base64。只允许发布 job 读取该 Secret。推送 `agent-v*` Git 标签，或手动执行工作流并选择一个已存在的 Git 标签。工作流不覆盖已有 Release。

控制面 `${ARTIFACTORY_DATA_DIR}/agent-releases.json` 由部署管理员配置，文件示例：

```json
{
  "repository": "owner/artifactory",
  "api_base": "https://api.github.com",
  "public_keys": ["BASE64_ED25519_PUBLIC_KEY"]
}
```

私有仓库可为控制面提供 `ARTIFACTORY_AGENT_GITHUB_TOKEN`。`api_base` 也可指向预先配置的 GitHub Enterprise API；生产环境要求 HTTPS。浏览器提交的字段只有版本标签，不能指定任意下载 URL。下载地址由固定仓库和 GitHub asset ID 构造，重定向限制为配置的 API 主机和 GitHub HTTPS 下载主机。

版本导入必须通过签名、五个平台、协议、大小、完整内容的 SHA-256 校验。全部校验成功后才建立版本目录记录。对象按 manifest 摘要存储；版本号内容不可覆盖，同内容重复导入返回原 ID。未配置验签公钥时拒绝导入。测试密钥必须独立于正式发布密钥。

## 一次性安装固定启动器

先停止接单并等待现有任务、日志和产物完成。备份原二进制、原生服务配置、`agent.json`、证书和私钥；保留服务账户、HOME、PATH、签名环境、SDK 及 macOS `DEVELOPER_DIR`。管理员将支持 OTA 的二进制安装在原服务路径，随后**以现有普通服务账户**执行：

```sh
artifactory-agent ota-init --config /path/to/agent.json --public-keys BASE64_ED25519_PUBLIC_KEY
artifactory-agent service --action start --config /path/to/agent.json
```

`ota-init` 拒绝覆盖已有 OTA 配置。它在配置旁创建独立的 `ota.json`，将初始运行程序复制到服务账户管理的版本目录。严格解析的 `agent.json` 保持原格式。

```json
{
  "directory": "/path/to/managed-root/ota",
  "public_keys": ["BASE64_ED25519_PUBLIC_KEY"]
}
```

`run` 和 `service-run` 检测 OTA 配置后启动固定 supervisor；旧配置仍直接运行 Agent。版本目录、升级记录和邮箱仅供服务账户访问。Windows 需给该账户授予目录的继承修改权限，保留证书和服务配置原有私有 DACL。固定服务 EXE 不参与 OTA，避免覆盖运行中的 Windows EXE。不要以 root / LocalSystem 运行构建。

## 运行与恢复

控制面使用节点事务锁协调升级与任务分配，`ota_draining` 独立于管理员 `draining`。每个节点最多一个未完成升级；节点版本覆盖优先于池目标。每池默认同时一个升级，失败或回滚会暂停后续升级。节点离线时目标保留，重连后继续。

阶段为：等待接收 → 下载校验 → 排空 → 启动确认 → 成功。下载在启动器后台进行，不阻塞 Agent 心跳。排空要求任务终态得到控制面确认、Worker 退出、所有日志得到确认；Worker 的产物登记先于任务终态。旧运行程序先通过已认证心跳报告即将激活，取得控制面确认后才允许启动器切换，因此候选程序无法连接时也保留可见的激活阶段。不会为了升级取消构建。

候选运行程序必须在 120 秒内以原证书重连，报告目标版本，并取得同一连接连续三次心跳确认，才能原子提交当前版本。重连会重置确认计数。启动失败、确认前退出或超时会恢复原版本；节点成功报告完成前，控制面持续阻止新任务。升级命令带持久化单调序号，重复和迟到命令不重启已完成操作。

`state.json` 原子持久化当前、候选、上一版本及升级结果。启动器重启发现未完成激活会恢复原版本；下载或排空期间被中断会报告失败并保留原版本，不自动循环重试。Windows 使用 Job Object 清理候选进程树；原生服务仍负责自动启动及启动器故障重启。

原生服务停止或重启时，启动器关闭私有控制管道通知运行程序退出，运行程序等待 Worker 清理构建进程树和工作目录，并保留当前 mTLS 连接最多 5 秒等待任务终态和日志确认；启动器等待 8 秒后仍未退出才强制结束。Linux systemd 使用 `KillMode=mixed`，先通知主服务，超时再清理整个服务控制组。macOS 安装器设置 launchd `ExitTimeOut=15`，避免系统默认的较短期限提前结束启动器、留下不响应退出的候选；管理员为已有服务安装 OTA 时也必须给现有 plist 补上该字段，并保留原账户、环境和路径。运行程序仅响应进程停止信号，网络连接断开产生的 `SIGPIPE` 不会触发退出。此退出路径独立于升级排空流程。

取消排队升级立即恢复调度；已送达节点的取消要等待启动器确认。已经进入激活的操作继续完成或回滚，界面显示最终结果。取消不会修改原来的管理员排空状态。失败、回滚和取消的同一目标不会自动循环重试；管理员可以显式重试。回退历史版本通过选择已导入版本执行同一安全流程。

`spool/` 中存在未确认日志时保持排空，不能通过删除日志绕过。可检查节点磁盘、证书期限、连接状态与产物存储后恢复传输。固定启动器自身、SDK 和系统工具链不属于第一版 OTA；启动器更换需要管理员安装。证书也不会自动续签。

## 管理接口

均位于 `/api/v1`。查询要求会话 viewer，写入要求管理员会话、`x-artifactory-csrf: 1` 和与 `ARTIFACTORY_PUBLIC_URL` 一致的 Origin。项目 Token 无权访问管理接口。目标、策略、导入、重试、取消和终态均有审计。

| 方法与路径 | 请求 / 行为 |
| --- | --- |
| `GET /agent-releases` | 已导入版本目录 |
| `POST /agent-releases` | `{"tag":"agent-v0.2.0"}` |
| `PATCH /nodes/{id}/agent-update` | `{"release_id":"uuid"}`；null 清除节点覆盖 |
| `PATCH /pools/{id}/agent-update` | `{"release_id":"uuid或null","concurrency":1,"paused":false}`（release_id 为 UUID 或 JSON null） |
| `GET /agent-updates` | 最近 500 条执行记录 |
| `POST /agent-updates/{id}/retry` | 重试终态失败 / 回滚 / 取消记录，生成新执行 ID 并恢复池策略 |
| `POST /agent-updates/{id}/cancel` | 取消尚未进入激活的执行 |

节点页显示版本、启动器、目标、进度、失败原因和回滚结果。设置页的 Agent 自动升级标签提供导入、池并发 / 暂停策略与历史操作。旧 Agent 继续正常构建，显示“需要安装 OTA 支持”。

## 验证边界

`go test -race ./...` 覆盖包校验、任务排空、命令重放及真实子进程 mTLS 升级 / 回滚。Postgres 测试覆盖锁竞争、池限流 / 暂停、权限、过期会话、吊销和证书期限。CI 原生服务生命周期测试在五种 runner 上以普通服务账户执行升级与回滚，包含 Windows 服务 / Job Object 路径。交叉编译不代表其他平台的原生服务验收，实际执行结果应以对应 CI run 为准。

实体 Mac mini 的 Rust 控制面 / Go Agent 联测另验证 Tailscale、Xcode、签名、产物上传、发布及下载运行；测试发布源使用独立签名的 GitHub Release API fixture，正式 GitHub 发布仍需要仓库配置发布 Secret。
