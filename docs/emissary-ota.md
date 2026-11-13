# Emissary OTA 自动升级

控制面管理目标版本，节点通过已有 mTLS 出站连接接收升级命令，使用私有 S3 的短期签名地址下载。普通升级不需要 SSH 或管理员密码。已有节点默认不升级；先安装固定启动器，再由管理员指定节点或节点池目标。

## 发布与信任配置

`.github/workflows/emissary-release.yml` 在六种原生 runner 上测试、构建 Linux、macOS、Windows 各自的 amd64/arm64，然后生成 `emissary-manifest.json` 和 `emissary-manifest.sig`。每个平台都必须通过普通测试和原生系统服务验收，发布 job 才能运行。控制面导入必须包含全部六个资产。manifest 包含版本、Git commit、协议兼容范围和每个二进制的大小及 SHA-256。签名覆盖 manifest 的原始字节。协议主版本仍为 1。

在受保护的 GitHub `emissary-release` Environment 中设置 `EMISSARY_RELEASE_SIGNING_SEED`，值为独立 Ed25519 私钥的 32 字节 seed 的 base64。只允许发布 job 读取该 Secret。推送 `emissary-v*` Git 标签，或手动执行工作流并选择一个已存在的 Git 标签。工作流不覆盖已有 Release。

控制面 `${ARTIFACTORY_DATA_DIR}/emissary-releases.json` 由部署管理员配置，文件示例：

```json
{
  "repository": "owner/artifactory",
  "api_base": "https://api.github.com",
  "public_keys": ["BASE64_ED25519_PUBLIC_KEY"]
}
```

私有仓库可为控制面提供 `ARTIFACTORY_EMISSARY_GITHUB_TOKEN`。`api_base` 也可指向预先配置的 GitHub Enterprise API；生产环境要求 HTTPS。浏览器提交的字段只有版本标签，不能指定任意下载 URL。下载地址由固定仓库和 GitHub asset ID 构造，重定向限制为配置的 API 主机和 GitHub HTTPS 下载主机。

版本导入必须通过签名、六个平台、协议、大小、完整内容的 SHA-256 校验。全部校验成功后才建立版本目录记录。对象按 manifest 摘要存储；版本号内容不可覆盖，同内容重复导入返回原 ID。未配置验签公钥时拒绝导入。测试密钥必须独立于正式发布密钥。

## 一次性安装固定启动器

新安装使用 Rust Emissary、全新的 `emissary.json` 和注册身份。管理员为专用普通服务账户准备 SDK、PATH、签名环境和二进制后，以该账户初始化 OTA，再安装和启动原生服务：

Windows 的 MSVC 构建还需要与 Emissary 架构匹配的 Microsoft Visual C++ v14 运行库。
从 Microsoft 获取 [amd64](https://aka.ms/vs/17/release/vc_redist.x64.exe) 或
[arm64](https://aka.ms/vs/17/release/vc_redist.arm64.exe) 安装包，校验 Microsoft
Authenticode 签名后由管理员安装。缺少 `VCRUNTIME140.dll` 时，程序可能以
`0xc0000135` 退出且没有命令行输出。运行库属于一次性主机准备，未包含在 OTA 资产中。

准备完成后执行：

```sh
artifactory-emissary ota-init --config /path/to/emissary.json --public-keys BASE64_ED25519_PUBLIC_KEY
artifactory-emissary service --action start --config /path/to/emissary.json
```

`ota-init` 拒绝覆盖已有 OTA 配置。它在配置旁创建独立的 `ota.json`，将初始运行程序复制到服务账户管理的版本目录。设备配置严格解析 `emissary.json`，拒绝未知字段和重复字段。

```json
{
  "directory": "/path/to/managed-root/ota",
  "public_keys": ["BASE64_ED25519_PUBLIC_KEY"]
}
```

`run` 和 `service-run` 检测 OTA 配置后启动固定 supervisor；没有 OTA 配置时直接运行 Emissary。版本目录、升级记录和邮箱仅供服务账户访问。Windows 需给该账户授予目录的继承修改权限，保留证书和服务配置原有私有 DACL。固定服务 EXE 不参与 OTA，避免覆盖运行中的 Windows EXE。不要以 root / LocalSystem 运行构建。

## 运行与恢复

控制面使用节点事务锁协调升级与任务分配，`ota_draining` 独立于管理员 `draining`。每个节点最多一个未完成升级；节点版本覆盖优先于池目标。每池默认同时一个升级，失败或回滚会暂停后续升级。节点离线时目标保留，重连后继续。

阶段为：等待接收 → 下载校验 → 排空 → 启动确认 → 成功。下载在启动器后台进行，不阻塞 Emissary 心跳。排空要求任务终态得到控制面确认、Worker 退出、所有日志得到确认；Worker 的产物登记先于任务终态。旧运行程序先通过已认证心跳报告即将激活，取得控制面确认后才允许启动器切换，因此候选程序无法连接时也保留可见的激活阶段。不会为了升级取消构建。

候选运行程序必须在 120 秒内以原证书重连，报告目标版本，并取得同一连接连续三次心跳确认，才能原子提交当前版本。重连会重置确认计数。启动失败、确认前退出或超时会恢复原版本；节点成功报告完成前，控制面持续阻止新任务。升级命令带持久化单调序号，重复和迟到命令不重启已完成操作。

`state.json` 原子持久化当前、候选、上一版本及升级结果。启动器重启发现未完成激活会恢复原版本；下载或排空期间被中断会报告失败并保留原版本，不自动循环重试。Windows 使用 Job Object 清理候选进程树；原生服务仍负责自动启动及启动器故障重启。

原生服务停止或重启时，启动器关闭私有控制管道通知运行程序退出，运行程序等待 Worker 清理构建进程树和工作目录，并保留当前 mTLS 连接最多 5 秒等待任务终态和日志确认；启动器等待 8 秒后仍未退出才强制结束。Linux systemd 使用 `KillMode=mixed`，先通知主服务，超时再清理整个服务控制组。macOS 安装器设置 launchd `ExitTimeOut=15`，避免系统默认的较短期限提前结束启动器、留下不响应退出的候选。运行程序仅响应进程停止信号，网络连接断开产生的 `SIGPIPE` 不会触发退出。此退出路径独立于升级排空流程。

取消排队升级立即恢复调度；已送达节点的取消要等待启动器确认。已经进入激活的操作继续完成或回滚，界面显示最终结果。取消不会修改原来的管理员排空状态。失败、回滚和取消的同一目标不会自动循环重试；管理员可以显式重试。回退历史版本通过选择已导入版本执行同一安全流程。

`spool/` 中存在未确认日志时保持排空，不能通过删除日志绕过。可检查节点磁盘、证书期限、连接状态与产物存储后恢复传输。固定启动器自身、SDK 和系统工具链不属于第一版 OTA；启动器更换需要管理员安装。证书也不会自动续签。

## 管理接口

均位于 `/api/v1`。查询要求会话 viewer，写入要求管理员会话、`x-artifactory-csrf: 1` 和与 `ARTIFACTORY_PUBLIC_URL` 一致的 Origin。项目 Token 无权访问管理接口。目标、策略、导入、重试、取消和终态均有审计。

| 方法与路径 | 请求 / 行为 |
| --- | --- |
| `GET /emissary-releases` | 已导入版本目录 |
| `POST /emissary-releases` | `{"tag":"emissary-v0.2.0"}` |
| `PATCH /nodes/{id}/emissary-update` | `{"release_id":"uuid"}`；null 清除节点覆盖 |
| `PATCH /pools/{id}/emissary-update` | `{"release_id":"uuid或null","concurrency":1,"paused":false}`（release_id 为 UUID 或 JSON null） |
| `GET /emissary-updates` | 最近 500 条执行记录 |
| `POST /emissary-updates/{id}/retry` | 重试终态失败 / 回滚 / 取消记录，生成新执行 ID 并恢复池策略 |
| `POST /emissary-updates/{id}/cancel` | 取消尚未进入激活的执行 |

节点页显示版本、启动器、目标、进度、失败原因和回滚结果。设置页的 Emissary 自动升级标签提供导入、池并发 / 暂停策略与历史操作。未初始化 OTA 的 Emissary 显示“需要安装 OTA 支持”。

## 验证边界

`cargo test -p artifactory-emissary --all-targets` 覆盖签名、下载中断、generation、排空和真实子进程经 mTLS 确认的升级 / 回滚。PostgreSQL 测试覆盖节点锁竞争、池限流 / 暂停、权限、过期会话、吊销和证书期限。六平台 CI 覆盖 Darwin、Windows、Linux 各自的 arm64 / amd64，使用 Rust 原生构建；具体通过结果以 CI run 为准。

测试 loopback peer 不替代真实控制平面、对象存储或原生服务安装验收；历史 Go Mac mini 记录不作为 Rust Emissary 的运行证据。各 OS 服务、Xcode / SDK 和正式发行签名需分别验收。
