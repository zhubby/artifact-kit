# 部署与运维

## 部署边界

控制面为 Linux 单实例模块化单体。PostgreSQL 是队列和状态的唯一事实来源，不部署多个调度实例，不使用数据库拷贝建立活动 / 活动 HA。宿主执行只接受内部可信项目；签名节点应与一般构建节点分池、最小授权、限制网络和服务账户权限。

生产需三个可解析的 DNS 名称：控制台、S3 HTTPS、Agent gRPC。前两者通过 Caddy 80/443；gRPC 使用钉住私有 CA 的 TLS 8443。仅向受信网络开放 8443；不公开 PostgreSQL、SeaweedFS 管理端口或其内部 filer/master。按组织要求提供受信 HTTPS、证书自动续期和反向代理限流。代理不得缓冲 SSE。

## Linux Compose

在专用**非 root** 部署账户下准备：

```sh
AK_DOMAIN=build.example.com \
AK_S3_DOMAIN=artifacts.example.com \
AK_GRPC_HOST=agents.example.com \
sh scripts/init-deploy.sh
docker compose --env-file .deploy/env config --quiet
docker compose --env-file .deploy/env up -d --build
```

`.deploy/env` 包含随机数据库密码、Git 凭据加密主密钥、S3 密钥和初始密码；`.deploy/pki` 包含节点证书签发私钥。均不入 Git，目录权限为 0700，文件为 0600；容器以该部署账户 UID/GID 读取。不要在已有部署上重新初始化或更换主密钥。

Compose 内 S3 的默认匿名访问已禁用。Agent 必须能访问外部 S3 HTTPS 名称；控制面也使用该 HTTPS 地址完成初始化和内容校验，因此部署宿主必须能回环访问这个域名。首次 ACME 颁证期间控制面可能自动重启，TLS 就绪后恢复；不要用 `AK_INSECURE_DEV` 绕过生产证书检查。使用组织内部 CA 时，将信任链安装到中心 / Agent 的系统信任库。

控制面 S3 客户端通过 Rustls 加载系统根证书。容器部署需把组织 CA 加入控制面镜像或容器内的信任库，并确保运行账户可读；宿主安装的证书不会自动传播到容器。证书的签发链和 S3 endpoint 主机名都会校验。

S3 启动脚本把只读挂载、0600 权限的宿主认证配置复制为容器内 `seaweed` 用户专有的 0600 文件，再由镜像入口降权运行；宿主配置的权限和归属保持不变。首次初始化需要镜像原有的 root 启动阶段，长期运行的 SeaweedFS 进程为非 root。修改配置后需重建或重启 S3 容器，不能通过放宽秘密文件权限解决 UID 不同导致的读取失败。

控制面数据目录同时承载 Git 临时目录与制品校验临时文件；至少留出最大并发校验文件的磁盘空间。单文件上限 20 GiB 不意味着小内存机器应使用相同大小的 tmpfs。S3 与 PostgreSQL 使用独立持久卷，默认不自动删除历史日志或制品。

使用外部 PostgreSQL / S3 时可直接部署镜像并设置与 `Settings::from_env` 对应的环境变量；S3 公共签名地址必须与客户端实际访问的 endpoint 一致。外部对象桶必须私有，不启用公开读取策略。

本仓库 Compose 清单和镜像不代表已经部署到用户生产域名；公有证书、DNS、磁盘容量、备份恢复与实际网络仍需现场验收。

### 隔离 Compose / TLS 验收

在有 Docker / Compose、Go、Git、Python 3.12+ 和 OpenSSL 的 Unix 非 root 账户下运行：

```sh
python3 -u scripts/compose_acceptance.py
```

脚本从当前 Git HEAD 创建私有源码快照，构建实际控制面与控制台镜像；以独立项目名、卷、网络和随机 loopback 端口运行生产清单的全部五个服务。测试覆盖使用 Caddy 内部 CA 的真实 HTTPS、Secure Cookie / CSRF、运行中 SSE、mTLS Agent、HTTPS Git 原生 C 构建、私有 S3 上传下载与中心重启。CA 只注入测试客户端与独立控制面进程，不修改宿主信任库。默认不加载已有 `.local/env` 或 `.deploy/env`，运行后仅清理归属标签匹配本次 run 的容器、网络与卷。

日志故障测试把验收 Agent 缓冲设为 1 MiB，仅暂停本次归属标签匹配的 S3，再让真实 Worker 输出超过缓冲上限。脚本核对子进程终止、磁盘缓冲边界及独立心跳，恢复 S3 后检查明确失败原因、无制品 / Release、日志按序确认和清理，并使用同一 Agent 再构建原生 C 程序。缓存块的大小 / SHA-256 同时与真实 PostgreSQL 索引及 HTTPS SSE 内容比较。测试容器使用 Docker init 回收孤儿子进程；失败清理也会先恢复本次暂停的 S3。执行的 controller 副本及 controller / probe 摘要随本地证据保留。

`--image-prefix` 可指定拉取镜像所用的 registry 镜像源；输入变更、各镜像的实际平台与 ID 随证据记录，不修改默认部署镜像或全局 Docker 配置。可选 `--control-image` / `--web-image` 仅复用 `org.opencontainers.image.revision` 与当前 HEAD、`artifact-kit.dockerfile.sha256` 与对应 Dockerfile 均匹配的镜像。`--probe-image` 可复用包含 python3、Git、cc、OpenSSL 的 Linux 测试工具镜像；Agent 二进制仍从本次源码快照重新编译，验收脚本也由本次 run 单独挂载。

日志和证据保存在 `.local/compose-tls/<run>/`。其中源码快照含本次部署的主密钥、数据库 / S3 密码和测试 PKI，必须保持私有；CI 仅上传显式列出的诊断日志和证据文件，不上传快照、生成的 Compose 配置或 Agent 私钥。专用 probe 容器只承载该验收的原生 Linux 进程，不属于产品的容器执行器，也不证明实体 Agent 服务、五类真实应用、发行签名或公网 DNS / ACME 已验收。

## Agent 安装与注册

先运行 `sh scripts/cross-build-agent.sh`，结果位于 `agent/bin`。所有目标为 CGO-disabled 二进制；这只是可编译性，不是宿主运行验证。使用已有 OS 原生工具链，不安装 SDK。

注册时钉住管理员通过可信渠道分发的 `ca.pem`；不要从未知网络下载后直接信任。通过 `AK_ENROLLMENT_TOKEN` 传短期令牌，注册后立即清除环境变量。配置文件和 `node.key` 只能由专用服务账户读取。

### Linux / systemd

以专用 `artifact-agent` 非 root 账户完成注册，给予其平台管理的工作目录与项目所需 SDK 访问，不给予 sudo 或通用管理员权限。

```sh
artifact-agent register --endpoint agents.example.com:8443 --ca /etc/artifact-kit/ca.pem --config /var/lib/artifact-kit/agent.json --root /var/lib/artifact-kit/data
artifact-agent doctor --config /var/lib/artifact-kit/agent.json
sudo artifact-agent service --action install --user artifact-agent --config /var/lib/artifact-kit/agent.json
sudo artifact-agent service --action start --config /var/lib/artifact-kit/agent.json
```

安装操作需要管理员创建 systemd 服务，但服务本身必须使用非 root 账户。确保该账户的 PATH 与 doctor 结果相同，SDK 许可已接受。

### macOS / launchd

使用专用构建账户，先在相同账户下完成 Xcode 许可、SDK、Keychain 与签名身份设置。以该账户注册，再由管理员安装带 `--user` 的 LaunchDaemon；不能假定后台 daemon 自动获得交互用户已解锁的钥匙串、登录会话或 UI 自动化授权。

```sh
artifact-agent doctor --config /Users/build/.config/artifact-kit/agent.json
sudo artifact-agent service --action install --user build --config /Users/build/.config/artifact-kit/agent.json
sudo artifact-agent service --action start --config /Users/build/.config/artifact-kit/agent.json
```

完整 macOS 应用通常还需要按正确嵌套顺序签名、notarization / stapling。原生命令由仓库维护；本机 ad-hoc 验证不是 Developer ID 或 App Store 验收。

LaunchDaemon 的标准输出和错误日志保存在私有配置文件所在目录，文件名为 `artifact-kit-agent.out.log` / `artifact-kit-agent.err.log`；服务账户必须能写入该目录。launchd 降权后无法在默认 `/var/log` 中为普通账户新建文件，此时 Agent 尚未执行就会以 `EX_CONFIG` 退出。

### Windows / Service Control Manager

使用具有“作为服务登录”权限的专用账户，不使用 LocalSystem。以管理员 PowerShell 安装，`AK_SERVICE_PASSWORD` 仅供安装过程读取，不写到命令参数。工作目录、配置和私钥使用 NTFS ACL，仅授予该账户、SYSTEM 与管理员必需权限；Go 的 Unix mode 位不能替代 Windows ACL 验证。

```powershell
artifact-agent.exe doctor --config C:\ArtifactKit\agent.json
artifact-agent.exe service --action install --user '.\artifact-build' --config C:\ArtifactKit\agent.json
Remove-Item Env:\AK_SERVICE_PASSWORD
artifact-agent.exe service --action start --config C:\ArtifactKit\agent.json
```

给服务账户配置 Visual Studio / SDK、PowerShell、Git 与签名证书的真实访问权限。Windows Job Object 使用挂起创建 → 分配 Job → 恢复，防止子进程在归属 Job 前逃逸；必须在 Windows 实机上验收取消、断线和服务停止。

三类系统均支持 `service --action stop|restart|uninstall`。本轮未向用户宿主安装任何系统服务。

Agent 启动后若发生不可恢复的本地错误（例如配置从未注册），服务进程会记录错误并以非零状态退出，让系统服务管理器看到失败；网络中断仍由 Agent 重连。系统是否自动重启失败服务取决于实际服务管理器及其恢复策略。

Unix Worker 将 SIGTERM、SIGINT 和因 Agent 关闭输出管道产生的 SIGPIPE 转为执行取消，先终止任务进程树并删除该 attempt 的受管工作目录。Worker 的信号处理只在独立 Worker 命令中启用。Windows 继续通过控制管道关闭取消任务，由 Job Object 清理后代。

系统服务不要求交互式用户的 HOME / APPDATA：服务使用安装时保存的绝对 `--config` 路径。没有用户配置目录时，交互式注册命令也必须显式提供 `--config` 和 `--root`。systemd 单元对可执行文件与配置参数逐项引用，允许安装路径含空格；不把执行参数的转义误用为启动条件路径。

### 临时 CI 原生服务验收

`native-agent` 作业覆盖 GitHub 托管临时 VM 的 Linux amd64 / arm64、macOS amd64 / arm64 与 Windows amd64 五种组合，并显式启用 `TestNativeServiceLifecycle`。测试在系统变更前核对矩阵传入的 `AK_NATIVE_EXPECTED_PLATFORM`、实际 Go 运行平台和 runner 系统 / 架构，记录镜像版本。随后创建专用非管理员账户和临时私有配置，通过实际 systemd / launchd / SCM 安装 Agent，核对自动启动配置和真实进程账户，并连接仅监听 loopback、要求客户端证书的 Go 测试控制端；重启必须产生新的认证连接及 PID，停止后连接必须关闭，卸载后服务记录必须消失。二进制及配置路径均含空格，测试结束卸载服务、删除其创建的账户和目录。

测试控制端还分配固定本地 Git 提交的受控任务。任务启动真实后代进程，测试通过 OS 进程信息核对专用账户和 Agent → Worker → 步骤 / 后代的实际归属；在服务重启及停止后要求所有旧任务进程消失、工作目录被删除，再让新 Agent / Worker 完成后续任务。受控程序及其后代有两分钟运行上限；失败清理只停止本次观察端，让这些任务自行退出。Windows 使用进程句柄观察退出，避免只按 PID 推断结果。

普通 `go test` 会明确跳过此项；只设置 `AK_NATIVE_SERVICE_TESTS=1` 也不允许在开发机运行，必须同时处于 GitHub 托管临时 VM。Windows 临时账户只单独授予 `SeServiceLogonRight`，不改写既有登录权限策略；私有目录 ACL 限该账户、当前测试进程身份、SYSTEM 与管理员。

这项测试覆盖原生服务管理及受控 Worker 的在途停止链路。Go 测试控制端只确认事件和日志，不代替真实 Rust / PostgreSQL / S3 联测；固定 Git 中的有限测试程序不代表五类真实应用、SDK 或发行签名环境。自动启动配置也不证明 VM / 实体机重启后的恢复。

## 本机签名配置

在 Agent 的私有 JSON 中增加引用到环境变量的映射，例如：

```json
{
  "signing_profiles": {
    "apple-release": {"AK_APPLE_IDENTITY": "Developer ID Application: YOUR TEAM"},
    "android-release": {"AK_ANDROID_KEYSTORE": "/private/signing/app.jks", "AK_ANDROID_STORE_PASSWORD": "LOCAL_SECRET"}
  }
}
```

只是对已有完整配置的增量示例，不是可独立运行的配置。中心只保存引用名和授权；不要上传应用签名私钥。项目、节点设置及本机配置都需授权同一引用。

doctor 报告路径、版本命令是否成功、实际 OS/CPU、可发现工具及本机配置的签名引用；不保证 SDK 完整、证书有效、iOS 设备可安装或商店可发布。可使用本机 `tools` 配置协助发现非标准路径；实际脚本仍需明确可执行路径 / PATH。

## 排空、升级、续证

1. 控制台设置 draining，不分配新任务，等待 active_jobs 为 0。
2. 停止 Agent 服务，备份私有配置、证书与签名环境；按目标 OS/CPU 替换二进制。
3. 使用同一账户运行 version / doctor，再启动，验证能力、标签、slots 和一次实际构建。
4. 证书为 30 天有效，本版需提前排空、吊销旧节点、新路径重新注册，再重设节点授权；没有自动续证和证书热替换。
5. 协议主版本不兼容时先排空，不允许新旧版本盲目混跑。

## 故障处理

- 中心重启：数据库租约保留，90 秒内重连可以继续同一个 attempt；不要手动把 running 改为 queued。
- 网络分区：本地租约截止杀进程树；已开始任务记 lost，不自动重跑。确认副作用后显式 retry。
- Agent 崩溃：Worker 控制管道关闭，终止子进程；服务恢复后旧任务不能复活。检查本机 `lost/`、`spool/` 和审计。
- 日志超额 / 存储不可用：任务明确失败或保留未确认日志，不删除 spool 来伪造成功。需要人工按 attempt 对照数据库保存日志证据。
- 发布失败：构建状态与制品保留，查看 publish_error / publish_attempts；可重试网络类问题，版本冲突不能覆盖，应以新版本创建构建。
- 取消：未开始任务取消，在途任务收到取消或失去续租后终止；日志可在剩余有效租约内补传。

## 备份、恢复与保留

必须协调备份 PostgreSQL、S3 对象、`.deploy/env` 中主密钥及 PKI；只有数据库备份无法解密凭据，也无法恢复制品。备份应离线加密，并独立限制访问。

1. 排空节点、停止新触发、等待在途任务结束，停止 control / web。
2. `pg_dump` 保存事务一致的数据库，记录 schema 迁移版本；同时保存 S3 内容与对象校验清单。
3. 保存主密钥、S3 密钥、签发 CA、节点授权与镜像版本；不要在普通工单里复制这些秘密。
4. 在隔离环境恢复数据库及对象，使用相同 master key，检查凭据解密、Release 清单与下载 SHA-256，再开放流量。
5. 升级前保留备份，执行迁移后回滚必须连同 schema 和对象状态一起评估，不能仅回退二进制。

本版没有自动 GC / retention API；禁止直接批量删除平台数据目录或 Release 对象。暂存对象和孤立失败日志的清理必须停写、基于数据库引用清单、先备份再删除；已发布制品不能按文件时间猜测是否无用。生产备份恢复演练仍是独立验收项。

开发环境可用 `scripts/recovery_acceptance.py` 重放隔离冷恢复，执行顺序见 README。它使用 process-only rclone remote 和空配置文件，不修改已有全局 remotes；备份位于私有 `.local/acceptance/<run_id>/backup`。脚本不会自动加密或异地传输该目录，生产环境需另行提供密钥托管、加密备份与恢复流程。
