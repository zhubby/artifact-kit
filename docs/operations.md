# 部署与运维

## 部署边界

Control 为 Linux 模块化服务，支持多副本。后端统一使用 TimescaleDB 2.32+ / PostgreSQL 17，关系表保存队列和业务状态，独立 Rust Telemetry 服务保存时序数据。多副本设备调度使用 PostgreSQL Outbox 与节点行锁，部署、容量与回退见 [Control 多副本](control-ha.md)；Kubernetes Pi 的 Runtime / Source / Operator 部署见 [Operator 运维](operator-operations.md)。宿主执行只接受内部可信项目；签名节点应与一般构建节点分池、最小授权、限制网络和服务账户权限。

生产需三个可解析的 DNS 名称：控制台、S3 HTTPS、Emissary gRPC。前两者通过 Caddy 80/443；构建 gRPC 使用钉住私有 CA 的 TLS 8443，独立遥测 mTLS 使用同一设备可达域名的 8444。仅向受信网络开放 8443 / 8444；不公开数据库、遥测内部查询端口、SeaweedFS 管理端口或其内部 filer/master。按组织要求提供受信 HTTPS、证书自动续期和反向代理限流。代理不得缓冲 SSE。

## Linux Compose

Kubernetes 和本机 kind 的 Chart、凭据初始化、TLS、持久卷与升级说明见 [Helm 部署指南](helm.md)。

在专用**非 root** 部署账户下准备：

```sh
ARTIFACTORY_DOMAIN=build.example.com \
ARTIFACTORY_S3_DOMAIN=artifacts.example.com \
ARTIFACTORY_GRPC_HOST=agents.example.com \
sh scripts/init-deploy.sh
docker compose --env-file .deploy/env config --quiet
docker compose --env-file .deploy/env up -d --build
```

`.deploy/env` 包含随机数据库密码、Git 凭据加密主密钥、S3 密钥和初始密码；`.deploy/pki` 包含节点证书签发私钥。均不入 Git，目录权限为 0700，文件为 0600；容器以该部署账户 UID/GID 读取。不要在已有部署上重新初始化或更换主密钥。

Compose 内 S3 的默认匿名访问已禁用。Emissary 必须能访问外部 S3 HTTPS 名称；控制平面也使用该 HTTPS 地址完成初始化和内容校验，因此部署宿主必须能回环访问这个域名。首次 ACME 颁证期间控制平面可能自动重启，TLS 就绪后恢复；不要用 `ARTIFACTORY_INSECURE_DEV` 绕过生产证书检查。使用组织内部 CA 时，将信任链安装到控制平面 / Emissary 的系统信任库。

控制平面 S3 客户端通过 Rustls 加载系统根证书。容器部署需把组织 CA 加入控制平面镜像或容器内的信任库，并确保运行账户可读；宿主安装的证书不会自动传播到容器。证书的签发链和 S3 endpoint 主机名都会校验。

S3 启动脚本把只读挂载、0600 权限的宿主认证配置复制为容器内 `seaweed` 用户专有的 0600 文件，再由镜像入口降权运行；宿主配置的权限和归属保持不变。首次初始化需要镜像原有的 root 启动阶段，长期运行的 SeaweedFS 进程为非 root。修改配置后需重建或重启 S3 容器，不能通过放宽秘密文件权限解决 UID 不同导致的读取失败。

### 用户自助注册

**自助注册默认开启，包括未设置新开关的升级部署。任何能够访问控制台的人均可注册为 `viewer`，查看并下载本实例全部项目的构建与产物。** 当前账号权限是实例级，不是项目成员隔离。新用户不能发起构建、创建项目或管理账号、凭据和节点；不会自动获得管理员权限。

不希望开放注册时，在本地控制平面环境中设置 `ARTIFACTORY_REGISTRATION_ENABLED=false` 并重启控制平面；Compose 在 `.deploy/env` 中设置相同值并重新创建 control-plane 服务；Helm 使用 `--set auth.registrationEnabled=false` 后升级。开关仅接受 `true` / `false`，默认 `true`。初始化脚本尊重已导出的开关值，但不会覆盖已有环境文件。关闭后注册 API 返回 `403`，登录页不展示注册入口，已有用户和会话保持可用；重新开启不改变已创建账号的角色。

注册成功自动登录，使用与现有登录一致的 24 小时 HttpOnly / SameSite=Strict 会话，生产包含 Secure。用户名区分大小写，密码长度按 UTF-8 字节校验；不提供邮箱验证、密码找回、邀请码或审批。每实例每 5 分钟最多受理 10 次有效格式的注册尝试，额度存储在数据库审计中，不随进程重启清零；超限返回 `429` 与 `Retry-After`。公开部署仍需配置反向代理连接 / IP 级防护；应用不会信任客户端自行提供的转发 IP 来放宽限流。

### 数据与外部存储

控制平面数据目录同时承载 Git 临时目录与制品校验临时文件；至少留出最大并发校验文件的磁盘空间。单文件上限 20 GiB 不意味着小内存机器应使用相同大小的 tmpfs。S3 与 PostgreSQL 使用独立持久卷，默认不自动删除历史日志或制品。

使用外部 TimescaleDB / S3 时可直接部署镜像并设置与 `Settings::from_env` 对应的环境变量。每个数据库由管理员预先安装 TimescaleDB 2.32+ 扩展；遥测使用独立数据库与应用角色。S3 公共签名地址必须与客户端实际访问的 endpoint 一致。外部对象桶必须私有，不启用公开读取策略。新安装的 Compose / Helm 自动初始化遥测数据库；遥测 token、证书 SAN、数据保留和独立连接池见 [时序观测](telemetry.md)。

本仓库 Compose 清单和镜像不代表已经部署到用户生产域名；公有证书、DNS、磁盘容量、备份恢复与实际网络仍需现场验收。

### 隔离 Compose / TLS 验收

在有 Docker / Compose、Git、Python 3.12+ 和 OpenSSL 的 Unix 非 root 账户下运行：

```sh
python3 -u scripts/compose_acceptance.py
```

脚本从当前 Git HEAD 创建私有源码快照（未提交候选需先复制到隔离临时 Git 仓库并提交），构建实际控制平面与控制台镜像；以独立项目名、卷、网络和随机 loopback 端口运行生产清单的全部七个服务。测试覆盖使用 Caddy 内部 CA 的真实 HTTPS、Secure Cookie / CSRF、运行中 SSE、mTLS Emissary、HTTPS Git 原生 C 构建、私有 S3 上传下载与控制平面重启。CA 只注入测试客户端与独立控制平面进程，不修改宿主信任库。默认不加载已有 `.local/env` 或 `.deploy/env`，运行后仅清理归属标签匹配本次 run 的容器、网络与卷。

日志故障测试把验收 Emissary 缓冲设为 1 MiB，仅暂停本次归属标签匹配的 S3，再让真实 Worker 输出超过缓冲上限。脚本核对子进程终止、磁盘缓冲边界及独立心跳，恢复 S3 后检查明确失败原因、无制品 / Release、日志按序确认和清理，并使用同一 Emissary 再构建原生 C 程序。缓存块的大小 / SHA-256 同时与真实 PostgreSQL 索引及 HTTPS SSE 内容比较。测试容器使用 Docker init 回收孤儿子进程；失败清理也会先恢复本次暂停的 S3。执行的 controller 副本及 controller / probe 摘要随本地证据保留。

`--image-prefix` 可指定拉取镜像所用的 registry 镜像源；输入变更、各镜像的实际平台与 ID 随证据记录，不修改默认部署镜像或全局 Docker 配置。可选 `--control-plane-image` / `--web-image` 仅复用 `org.opencontainers.image.revision` 与当前 HEAD、`artifactory.dockerfile.sha256` 与对应 Dockerfile 均匹配的镜像。`--probe-image` 可复用包含 python3、Git、cc、OpenSSL 的 Linux 测试工具镜像；Emissary 二进制仍从本次源码快照重新编译，验收脚本也由本次 run 单独挂载。

日志和证据保存在 `.local/compose-tls/<run>/`。其中源码快照含本次部署的主密钥、数据库 / S3 密码和测试 PKI，必须保持私有；CI 仅上传显式列出的诊断日志和证据文件，不上传快照、生成的 Compose 配置或 Emissary 私钥。专用 probe 容器只承载该验收的原生 Linux 进程，不属于产品的容器执行器，也不证明实体 Emissary 服务、五类真实应用、发行签名或公网 DNS / ACME 已验收。

## Emissary 安装与注册

运行 `sh scripts/cross-build-emissary.sh`，默认构建本机 Rust 目标，输出位于 `.local/emissary-bin`。其他目标由 `EMISSARY_TARGETS` 显式指定，需预装对应 linker / SDK；正式发布由五种原生 runner 分别编译。设备程序不依赖 Go。

注册时钉住管理员通过可信渠道分发的 `ca.pem`；不要从未知网络下载后直接信任。通过 `ARTIFACTORY_ENROLLMENT_TOKEN` 传短期令牌，注册后立即清除环境变量。配置文件和 `node.key` 只能由专用服务账户读取。

### Linux / systemd

以专用 `artifactory-emissary` 非 root 账户完成注册，给予其平台管理的工作目录与项目所需 SDK 访问，不给予 sudo 或通用管理员权限。

```sh
artifactory-emissary register --endpoint agents.example.com:8443 --ca /etc/artifactory/ca.pem --config /var/lib/artifactory/emissary.json --root /var/lib/artifactory/data
artifactory-emissary doctor --config /var/lib/artifactory/emissary.json
sudo artifactory-emissary service --action install --user artifactory-emissary --config /var/lib/artifactory/emissary.json
sudo artifactory-emissary service --action start --config /var/lib/artifactory/emissary.json
```

安装操作需要管理员创建 systemd 服务，但服务本身必须使用非 root 账户。确保该账户的 PATH 与 doctor 结果相同，SDK 许可已接受。

### macOS / launchd

使用专用构建账户，先在相同账户下完成 Xcode 许可、SDK、Keychain 与签名身份设置。以该账户注册，再由管理员安装带 `--user` 的 LaunchDaemon；不能假定后台 daemon 自动获得交互用户已解锁的钥匙串、登录会话或 UI 自动化授权。

```sh
artifactory-emissary doctor --config /Users/build/.config/artifactory/emissary.json
sudo artifactory-emissary service --action install --user build --config /Users/build/.config/artifactory/emissary.json
sudo artifactory-emissary service --action start --config /Users/build/.config/artifactory/emissary.json
```

完整 macOS 应用通常还需要按正确嵌套顺序签名、notarization / stapling。原生命令由仓库维护；本机 ad-hoc 验证不是 Developer ID 或 App Store 验收。

LaunchDaemon 的标准输出和错误日志保存在私有配置文件所在目录，文件名为 `emissary.log`；服务账户必须能写入该目录。launchd 降权后无法在默认 `/var/log` 中为普通账户新建文件，此时 Emissary 尚未执行就会以 `EX_CONFIG` 退出。

### Windows / Service Control Manager

使用具有“作为服务登录”权限的专用账户，不使用 LocalSystem。以管理员 PowerShell 安装，`ARTIFACTORY_SERVICE_PASSWORD` 仅供安装过程读取，不写到命令参数。工作目录、配置和私钥使用 NTFS ACL，仅授予该账户、SYSTEM 与管理员必需权限；Unix mode 位不能替代 Windows ACL 验证。

```powershell
artifactory-emissary.exe doctor --config C:\artifactory\emissary.json
artifactory-emissary.exe service --action install --user '.\artifactory-build' --config C:\artifactory\emissary.json
Remove-Item Env:\ARTIFACTORY_SERVICE_PASSWORD
artifactory-emissary.exe service --action start --config C:\artifactory\emissary.json
```

给服务账户配置 Visual Studio / SDK、PowerShell、Git 与签名证书的真实访问权限。Windows Job Object 使用挂起创建 → 分配 Job → 恢复，防止子进程在归属 Job 前逃逸；必须在 Windows 实机上验收取消、断线和服务停止。

三类系统均支持 `service --action stop|restart|uninstall`。本轮未向用户宿主安装任何系统服务。

Emissary 启动后若发生不可恢复的本地错误（例如配置从未注册），服务进程会记录错误并以非零状态退出，让系统服务管理器看到失败；网络中断仍由 Emissary 重连。系统是否自动重启失败服务取决于实际服务管理器及其恢复策略。

Worker 在 SIGTERM / SIGINT、租约到期和父控制管道 EOF 时先终止任务进程树并删除该 attempt 的受管工作目录。Windows 的后代由 Job Object 清理。

系统服务不要求交互式用户的 HOME / APPDATA：服务使用安装时保存的绝对 `--config` 路径。没有用户配置目录时，交互式注册命令也必须显式提供 `--config` 和 `--root`。systemd 单元对可执行文件与配置参数逐项引用，允许安装路径含空格；不把执行参数的转义误用为启动条件路径。

### 临时 CI 原生服务验收

`native-emissary` 作业在 Linux amd64 / arm64、macOS amd64 / arm64 与 Windows amd64 原生 runner 上运行 Rust 测试及 Clippy。`process_tree` 直接启动系统进程验证进程组 / Job Object 清理；`runtime` 使用真实 TLS 1.3 和 mTLS loopback 服务验证认证连接、OTA 替换 / 回滚，以及 Unix 前台和 supervisor 停止时的在途 Worker、日志确认和目录清理。

`native_service_lifecycle` 另由显式启用的 GitHub 托管临时 VM 执行，核对实际平台，创建专用账户，以带空格的路径安装、启动、重启、停止并卸载 systemd / launchd / SCM 服务，验证认证连接、在途进程树和后续任务。普通测试跳过此项，不在开发机安装系统服务。systemd / launchd / SCM 的专用账户安装、OS 重启、各平台 SDK 与发行签名必须在相应的临时 VM 或实体机单独验收；不能用过去 Go 实现的服务记录代替 Rust 版本验收。实际执行结果见 [Emissary 移植说明](../crates/emissary/PORTING.md) 与带日期的验收记录。

## 本机签名配置

在 Emissary 的私有 JSON 中增加引用到环境变量的映射，例如：

```json
{
  "signing_profiles": {
    "apple-release": {"ARTIFACTORY_APPLE_IDENTITY": "Developer ID Application: YOUR TEAM"},
    "android-release": {"ARTIFACTORY_ANDROID_KEYSTORE": "/private/signing/app.jks", "ARTIFACTORY_ANDROID_STORE_PASSWORD": "LOCAL_SECRET"}
  }
}
```

只是对已有完整配置的增量示例，不是可独立运行的配置。控制平面只保存引用名和授权；不要上传应用签名私钥。项目、节点设置及本机配置都需授权同一引用。

doctor 报告路径、版本命令是否成功、实际 OS/CPU、可发现工具及本机配置的签名引用；不保证 SDK 完整、证书有效、iOS 设备可安装或商店可发布。可使用本机 `tools` 配置协助发现非标准路径；实际脚本仍需明确可执行路径 / PATH。

## 排空、升级、续证

常规 Emissary 升级使用 [Emissary OTA](emissary-ota.md)。下面的手动流程用于首次安装启动器或管理员恢复。

1. 控制台设置 draining，不分配新任务，等待 active_jobs 为 0。
2. 停止 Emissary 服务，备份私有配置、证书与签名环境；按目标 OS/CPU 替换二进制。
3. 使用同一账户运行 version / doctor，再启动，验证能力、标签、slots 和一次实际构建。
4. 证书为 30 天有效，本版需提前排空、吊销旧节点、新路径重新注册，再重设节点授权；没有自动续证和证书热替换。
5. 协议主版本不兼容时先排空，不允许新旧版本盲目混跑。

## 故障处理

- 控制平面重启：数据库租约保留，90 秒内重连可以继续同一个 attempt；不要手动把 running 改为 queued。
- 网络分区：本地租约截止杀进程树；已开始任务记 lost，不自动重跑。确认副作用后显式 retry。
- Emissary 崩溃：Worker 控制管道关闭，终止子进程；服务恢复后旧任务不能复活。检查本机 `lost/`、`spool/` 和审计。
- 日志超额 / 存储不可用：任务明确失败或保留未确认日志，不删除 spool 来伪造成功。需要人工按 attempt 对照数据库保存日志证据。
- 发布失败：构建状态与制品保留，查看 publish_error / publish_attempts；可重试网络类问题，版本冲突不能覆盖，应以新版本创建构建。
- 取消：未开始任务取消，在途任务收到取消或失去续租后终止；日志可在剩余有效租约内补传。

## 备份、恢复与保留

必须协调备份各 TimescaleDB 数据库、S3 对象、`.deploy/env` 中主密钥及 PKI；只有数据库备份无法解密凭据，也无法恢复制品。备份应离线加密，并独立限制访问。

1. 排空节点、停止新触发、等待在途任务结束，停止 control-plane / web / agent-worker 及独立 Runtime / Source / Telemetry 写入。
2. `pg_dump` 分别保存实际部署的业务、Runtime、Source、遥测数据库，记录 schema 与 TimescaleDB 扩展版本；同时保存 S3 内容与对象校验清单。分别导出的数据库不自动构成跨库事务快照，必须先停写。
3. 保存主密钥、S3 密钥、签发 CA、节点授权与镜像版本；不要在普通工单里复制这些秘密。
4. 在相同 TimescaleDB 版本的隔离环境恢复：各库预装扩展，执行 `timescaledb_pre_restore()`，串行 `pg_restore` 后执行 `timescaledb_post_restore()`。使用相同 master key，检查凭据解密、Release 下载 SHA-256、遥测列存 / 聚合 / 后台策略及最新摘要，再开放流量。
5. 升级前保留备份，执行迁移后回滚必须连同 schema 和对象状态一起评估，不能仅回退二进制。

构建日志与制品没有自动 GC / retention API；遥测按 [保留策略](telemetry.md#数据与保留策略) 自动清理。禁止直接批量删除平台数据目录或 Release 对象。暂存对象和孤立失败日志的清理必须停写、基于数据库引用清单、先备份再删除；已发布制品不能按文件时间猜测是否无用。生产备份恢复演练仍是独立验收项。

开发环境可用 `scripts/recovery_acceptance.py` 重放隔离冷恢复，执行顺序见 README。它使用 process-only rclone remote 和空配置文件，不修改已有全局 remotes；备份位于私有 `.local/acceptance/<run_id>/backup`。脚本不会自动加密或异地传输该目录，生产环境需另行提供密钥托管、加密备份与恢复流程。

## Agent worker 与源码缓存

模型配置、worker 注册、任务预算、源码下载、缓存指标与升级顺序见 [AI 构建运维](ai-builds.md)。首次部署必须配置真实模型并完成 worker 注册；七服务 Compose 的 worker 默认等待注册，不持有模型密钥环境变量。备份必须包含 worker 身份和平台源码快照元数据。
