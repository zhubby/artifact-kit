# 实施与验收记录

记录日期：2026-11-10，Asia/Shanghai。此记录区分源代码实现、实际自动化结果、运行环境和外部验收；**不是三类实体宿主、五种应用目标的最终验收证明**。

## 结论与里程碑

| 阶段 | 本次交付 | 状态 |
|---|---|---|
| M0 | 已确认设计、设计自审、版本化 Protobuf、v1 JSON schema、两次数据库迁移、状态机文档 | 已交付 |
| M1 | 内置登录、三类角色、项目范围 Token、一次性注册、节点证书认证、心跳、能力、排空、诊断与服务管理 CLI | 本机协议及三类临时原生 VM 服务生命周期通过；实体机重启与业务负载待验 |
| M2 | Git 引用解析及不可变快照、事务调度、独立 Worker、进程树清理、SSE 日志、私有制品校验与下载 | macOS arm64 原生闭环及 Linux 容器五服务联测通过 |
| M3 | 项目 / 节点 / 本机签名引用授权、sign / verify 阶段、不可变内部 Release、幂等发布重试、审计 | 本机 ad-hoc 签名与发布通过；发行签名待验 |
| M4 | Compose / TLS 五服务实跑、镜像、服务安装说明、运维手册、故障自动化、浏览器测试、二进制交叉构建、兼容性矩阵 | 部分交付，外部验收尚未完成 |

控制台页面调用真实 Rust API，覆盖项目、构建表单与历史、目标矩阵 / 步骤 / 日志、节点、制品、Release、用户 / Token / Git 凭据 / 审计。构建成功与发布失败分别显示，不用页面展示或模拟响应代替执行结果。

## 已验证环境

| 项目 | 实际版本或范围 |
|---|---|
| 开发 / 执行宿主 | macOS 27.0.1，build 26A434，arm64 |
| Rust | rustc 1.95.0；Axum / Tonic 依赖以 Cargo.lock 为准 |
| Go | go1.27.3 darwin/arm64；CGO_ENABLED=0 构建 Agent |
| 前端运行时 | Node v22.22.0，npm 10.9.4 |
| 前端依赖 | Next 16.4.5、React 19.3.1、Tailwind 4.3.5、Playwright 1.65.0；以 package-lock.json 为准 |
| Protobuf 编译器 | protoc 36.2 |
| 原生编译器 | Apple clang 21.0.0，CommandLineTools |
| PostgreSQL | 17.11，Linux x86_64 容器，真实数据库，不是内存替代 |
| S3 | SeaweedFS 4.53，Linux amd64 容器，启用身份认证、匿名访问关闭 |
| 浏览器 | Chromium 桌面 1440 × 1000；iPhone 13 尺寸的 Chromium 移动模拟 |

本机只有 CommandLineTools，`xcodebuild` 路径存在但不能正常执行，**没有完整 Xcode**。修正后的 Agent 不再把此类占位程序报告为可用工具链。版本 / 帮助探测仍不证明 SDK、许可证、签名身份和设备满足项目要求。

## 自动化结果

| 检查 | 结果与证据 |
|---|---|
| Rust 格式与静态检查 | `cargo fmt --all --check`、`cargo clippy --all-targets -- -D warnings` 通过 |
| Rust 测试 | 9 项配置 / 加密 / 权限 / 匹配测试、8 项显式启用的真实 PostgreSQL 测试、1 项真实 HTTPS 信任回归通过；TLS 测试另有一个子进程辅助入口 |
| S3 TLS 信任 | 实际 `Storage::initialize` 在独立子进程加载指定的 CA 信任文件，受信 CA 连接成功、无关 CA 和错误主机名均被拒绝；独立 Python HTTPS 客户端验证同一证书链 |
| Go 检查 | `go vet ./...`、`go test -race ./... -count=1` 通过；不启用实时环境时协议联测会显式跳过 |
| 服务启动失败回归 | 真实 `service-run` 子进程在未注册配置下报错后必须以非零状态退出；修复前进程存活并在 5 秒后触发测试超时，修复后通过 |
| 原生服务生命周期 | [42756742531](https://github.com/zhubby/artifact-kit/actions/runs/42756742531) 在 `23ab4ac` 上的 Linux amd64、macOS arm64、Windows amd64 三个 native-agent 作业全部通过：真实服务管理器、专用账户、mTLS、重启 / 停止 / 卸载及私有配置权限；完整 Linux 流水线另行核验，不由服务结果推断 |
| 验收启动回归 | 3 项真实子进程测试通过：提前退出立即报告状态与日志、就绪超时终止进程、健康检查成功后保留运行进程 |
| 构建失败诊断回归 | 意外终态保留构建及 attempt 错误，并验证诊断不包含租约令牌；与启动及备份保护共 7 项 Python 回归 |
| 备份保护回归 | 3 项通过：复制前拒绝目录内及根目录符号链接，备份缺失主密钥 / 数据库配置时不得继承当前进程秘密 |
| 跨语言协议 | 独立 Go 客户端与真实 Rust TLS / gRPC 服务的注册、身份、事件、日志及制品联测通过；实际暂停 S3 验证日志 / 制品存储阻塞不阻断续租 |
| 原生端到端 | `scripts/acceptance.py --keep-alive` 35 项检查通过，详细边界见下节 |
| SSH Git | 10 项检查通过：真实 OpenSSH、Rust 源解析、Go Worker 拉取 / 编译 / 下载 / 发布，错误指纹及未授权密钥拒绝 |
| 隔离冷恢复 | `8efa99ec0523` 16 项通过：21 张表及序列、784 个对象（3,037,785 字节）、私有配置 / PKI、旧 Release 下载执行、恢复后的 SSH 新构建 / 发布 |
| 前端 | TypeScript 检查、生产构建和 Docker 镜像构建通过；容器运行的控制台通过 10 项真实后台浏览器测试 |
| Agent 编译 | Linux amd64 / arm64、Darwin amd64 / arm64、Windows amd64 五个目标二进制生成成功 |
| Windows 原生回归 | GitHub `windows-latest` 上 vet / race、Job Object 与 SCM 专用账户的在途进程树重启 / 停止及后续任务通过；真实 Windows 应用、实体机重启与发行签名仍待验 |
| 配置与部署文件 | 五个示例通过真实 Rust Recipe 解析；JSON schema 语法、shell 脚本语法、Compose 配置校验通过 |
| 生产 tmpfs 回归 | 实际 Compose 解析与 Docker daemon 先复现未加引号的挂载被拆成两项并拒绝启动；修复后解析为一个挂载，真实非 root / read-only 容器核对 1777 并写入 `/tmp`；Python 回归现共 8 项 |
| 完整 Compose / TLS | 产品源码 `1dae112` 的本机运行 `1e38068d0dea`：外层 23 项、实际流量 21 项通过，全部五服务、受信 HTTPS / mTLS、运行中 SSE、私有 S3、在途构建中重启及凭据持久化均已实跑 |
| 日志缓冲真实故障 | 产品源码 `325dea7` 的本机运行 `4c9038b3043c`：外层 27 项、流量 30 项通过，真实 S3 暂停、1 MiB 缓冲耗尽、子进程终止、心跳继续、日志恢复及下一构建成功 |
| 远端 CI | Linux / macOS / Windows 原生 Agent 检查通过；[诊断运行 42746144635](https://github.com/zhubby/artifact-kit/actions/runs/42746144635) 确认 Linux S3 配置权限错误，已本地复现并修复；完整流水线现包含 SSH 与冷恢复，最新结果见 [main Actions](https://github.com/zhubby/artifact-kit/actions/workflows/ci.yml?query=branch%3Amain) |

普通 `cargo test --all-targets` 会跳过需要数据库的 8 项测试；本次另外设置 `AK_TEST_DATABASE_URL` 并使用 `--include-ignored` 执行。测试为每次运行创建隔离 schema，不能以默认命令中的 ignored 项冒充数据库验收。

TLS 回归需要 `python3` 和支持 `req -addext` 的 OpenSSL。修复前，同一证书链在独立 Python 客户端通过，但实际 Storage 客户端使用 WebPKI 根证书、忽略指定的 CA 信任文件并报 `UnknownIssuer`。切换为系统根证书后通过受信、错误 CA 和错误主机名三种请求。该 loopback HTTPS 服务只验证真实产品客户端的 TLS 行为，不代替完整 S3、正式域名或生产部署验收。

### 数据库与本机回归

- 并发调度领取不重复、不超过节点槽位；中心数据库为真实 PostgreSQL。
- 未开始租约过期重新入队；已开始租约过期进入 lost，不自动重跑。
- 不完整制品不能生成 Release；发布在事务内原子可见，版本不可覆盖，幂等重试复用既有发布。
- Cookie 写请求的 CSRF 检查、Token 项目 / scope / 角色约束、带自动发布版本的 retry 权限检查。
- 编辑项目不会改变排队构建的仓库地址 / 凭据引用；排队签名任务在分配前重新检查授权。
- 节点吊销不可逆；即使请求尝试恢复节点，审计仍记录实际生效的 revoked=true。
- Unix Worker 的父管道关闭、单调租约到期实际终止子进程树；有界日志缓冲溢出明确失败且不跳过序号。
- 重复 assign 不重建 Worker；延迟到达的续租授权以请求发送时刻为锚点，不能延长本地有效期。
- 诊断使用 Go 的 `version` 子命令；探测失败的 Xcode 占位程序及自定义工具不进入调度能力集合。

### 三类原生服务生命周期

运行 `42756742531` 在 `23ab4ac3feb5381b5ba5a1936b83928f31cbe696` 上，三类原生作业及 `TestNativeServiceLifecycle` 都通过：Ubuntu 26.04 amd64 的 systemd、macOS 26 arm64 的 launchd、Windows amd64 的 SCM。测试先拒绝既有同名账户 / 服务，再创建专用非管理员账户，注册真实 Agent 二进制为自动启动服务，以 loopback mTLS Go 测试控制端观察认证连接；核对服务进程确实属于该账户，重启后 PID 和认证连接均更换，停止后旧连接关闭，卸载后服务记录消失，测试创建的服务、账户和目录清理完成。Windows 使用实际 NTFS DACL 与单独的服务登录权限；Unix 核对 0700 私有目录和 0600 配置 / 私钥。可执行文件及配置路径含空格、百分号、美元符及模板花括号。

真实红 / 绿记录包括：Agent 报错后外层服务仍存活、没有 HOME / APPDATA 时提前拒绝显式配置、服务库将 systemd 执行参数转义误用于路径条件、launchd 普通账户无法创建默认 `/var/log` 输出文件。对应修复后均有子进程或原生服务结果。systemd 还分别处理可执行文件和参数的美元符规则；launchd 日志移到专用账户的私有配置目录。早期失败运行保留为失败；部分旧流水线被后续提交替代后取消，未计作成功。

此项没有在用户的 Mac 上安装系统服务，没有执行 VM / 实体机重启，没有给服务分配真实应用或发行签名任务。Go 测试控制端只验证服务管理链路，不代替 Rust 协议联测。自动启动配置存在不等于重启后恢复已验证，空闲服务启停不等于在途构建及子进程终止已验收。

### 在途 Worker 的退出与清理

新增 `TestForegroundAgentStopsActiveProcessTree` 在本机 macOS arm64 上实际运行编译后的 Agent / Worker、固定本地 Git 提交和有两分钟上限的后代程序。正常 SIGINT 退出、Agent 强制退出及单独向 Worker 发送 SIGTERM 三个场景均核对真实进程祖先与账户、旧 Worker / 步骤 / 后代消失，以及该 attempt 工作目录删除；三项修复前失败、修复后通过。

独立管道复现保留于 `.local/native-service-workload/`：只关闭控制管道时原 Worker 以状态 1 退出且目录删除；同时关闭输出管道时被 SIGPIPE 终止（状态 -13），SIGTERM 则为 -15，两者均留下目录。Worker SIGTERM 的回归还实际观察到存活的后代。修复将独立 Unix Worker 的这些信号转为执行上下文取消，保证进程树终止和目录清理先完成；相同三个复现修复后均以状态 1 退出、无遗留目录。全部 Go vet / race 与五目标交叉构建通过。

`TestNativeServiceLifecycle` 同时增加受控在途任务的真实服务重启 / 停止、OS 进程账户与归属、工作目录清理和后续任务成功检查。[42773543511](https://github.com/zhubby/artifact-kit/actions/runs/42773543511) 在 `2ae03c150a88db965d15c8889625e74ce1db8796` 上的 Linux amd64、macOS arm64、Windows amd64 三个原生作业全部通过：每类 VM 均实际清理两次在途 Worker / 步骤 / 后代及其工作目录，并成功执行两次后续任务，最终服务、账户及目录清理通过。该精确 SHA 的完整五作业 CI 也全部成功；原三类空闲服务结果仍按上节历史运行记录。固定 Git 测试程序不代表真实应用、实体机重启、SDK 或发行签名。

后续 [42786554877](https://github.com/zhubby/artifact-kit/actions/runs/42786554877) 的 macOS arm64 服务作业暴露验收观察器的消息归属错误：前一成功任务的完成事件在确认到达 Agent 前重复传递，等待下一任务时被误读。两项前台 Agent 回归明确插入已完成任务的重复通知，修复前分别误判当前进程树结束和下一任务失败；按 attempt ID 筛选后两项通过，仍核对当前任务实际运行、进程树终止和目录清理。等待完成结果使用固定总超时，重复消息不延长等待；该失败运行保留为失败，最终结果以最新提交的完整 CI 为准。

### 五种宿主系统与架构的原生 CI

原生矩阵增加 `ubuntu-24.04-arm`（Linux arm64）与 `macos-15-intel`（macOS amd64），补齐五个 Agent 二进制目标的原生运行覆盖。标签来自 [GitHub 官方 runner 列表](https://github.com/actions/runner-images#available-images)。五种组合均执行 Go vet / race，以及显式启用的专用账户服务安装、在途任务重启 / 停止、进程树与工作目录清理、后续任务和卸载验收。

服务测试在任何系统变更前核对 CI 预期平台、测试进程实际 `runtime.GOOS/GOARCH` 与 `RUNNER_OS/RUNNER_ARCH`，记录镜像版本；真实 Agent 的 mTLS hello 另行核对相同 OS / 架构。预期平台缺失或不匹配立即失败，原有 GitHub 托管临时 VM 限制继续生效。[42777333982](https://github.com/zhubby/artifact-kit/actions/runs/42777333982) 在 `fe2f0932e6cdb5c98de15513665f6139bea66914` 上的五类原生作业全部通过，每类均实际核对两次在途进程树 / 工作目录清理及两次后续任务成功；完整七作业 CI 也全部成功。原生 CI 不代替实体机重启、真实应用工具链或发行签名验收。

### 完整 Compose / TLS 隔离联测

`scripts/compose_acceptance.py` 从 Git HEAD 快照构建产品镜像，并运行生产清单的 PostgreSQL、SeaweedFS、Rust 控制面、Next.js、Caddy 五个实际服务。运行 `1e38068d0dea` 的产品源码为 `1dae112f6a5319fc0521f092ae30da5fe5240bc8`，外层 23 项及专用 probe 的 21 项检查通过：

- Caddy HTTPS 使用独立内部 CA，Python / Rust / Go 客户端实际校验证书；登录返回 Secure / HttpOnly / SameSite=Strict Cookie，拒绝匿名访问、缺少 CSRF 头和外站 Origin。
- Go Agent 以新登记证书完成真实 mTLS，并从带 Basic Auth 的 HTTPS Git 固定相同提交，使用 Linux cc 编译、执行校验。
- 构建仍在运行时，真实 `log` SSE 事件经过 Next.js 与 Caddy 到达客户端；制品经 HTTPS 上传、服务端校验、私有签名下载，独立核对大小 / SHA-256 并运行下载的二进制；匿名下载拒绝。
- 中心在构建进行中重启；Agent 使用原证书重连，同一个 attempt 继续成功，没有重复派发。原会话、Release 和下载保持可用，随后新建构建再次成功，确认持久化的 Git 凭据可解密。
- 控制面非 root、read-only、capabilities 全移除；三个宿主端口全部绑定随机 loopback 地址。仅删除归属标签匹配本次 run 的资源，确认既有开发 PostgreSQL / S3 的容器身份、启动时间和重启次数保持。

证据及日志在 `.local/compose-tls/1e38068d0dea/`。本机使用镜像源和已有 Docker 网络设置，控制面 / Web / PostgreSQL / S3 / probe 为 Linux amd64，Caddy 为 Linux arm64/v8；各服务显式采用实际镜像平台，未覆盖原缓存镜像或全局默认平台。控制面和 Web 均验证源码 revision 与 Dockerfile 标签；probe 的测试工具镜像可复用，本次 Go Agent 从相同源码快照重新编译。诊断白名单扫描未发现生成的部署密码、主密钥、S3 secret 或私钥内容。

早期失败保留原状：`7501a32f5242` 是缓存 Caddy 平台与进程默认平台冲突，`ab074741b6d8` 发现生产 tmpfs YAML 拆项，`d011d2795e6a` 是重启测试在 HTTP 就绪前查询节点。对应修正后使用新建资源重新运行。测试 CA 仅存在于独立栈和客户端；完整 Compose 成功不证明公网 DNS / ACME、正式部署主机、实体 Agent 服务重启、五类实际应用或发行签名已经验收。

### 日志缓冲耗尽与恢复

原设计要求日志缓冲有界、耗尽后明确失败。原有 Go 单测核对了 spool 方法的容量与序号；新增整栈故障测试实际运行 Go Agent / Worker、Rust 中心、PostgreSQL 和私有 S3。运行 `4c9038b3043c` 的产品源码为 `325dea72ac0d6ab8964c6197059802b89be92759`，27 项外围检查与 30 项流量检查全部通过，包含原有 HTTPS / mTLS / SSE / 重启链路。

- 先启动会持续写存活标记的真实后代进程，仅暂停归属本次运行的 S3，随后让 Worker 输出超过 1 MiB 的日志。验收 Agent 使用 1 MiB 缓冲，产品默认配置为 256 MiB。
- S3 暂停期间，实际磁盘 spool 未越界，Worker 工作目录清理、后代 PID 消失；仍观察到新心跳，构建保持在等待日志确认的运行态。
- 保存未确认块的连续序号、大小与 SHA-256，再恢复 S3。构建以 `log buffer exhausted or unavailable` 明确失败，既没有制品，也没有 Release 记录；确认后的 spool 文件被删除。
- HTTPS SSE 实际返回的缓存块与原始字节 / 顺序一致，真实 PostgreSQL 的完整序号连续、大小与摘要相同。随后同一个 Agent 再次完成 HTTPS Git 原生 C 构建，证明容量释放后可以继续工作。

证据在 `.local/compose-tls/4c9038b3043c/`；执行的 controller 副本及 controller / probe 摘要用于核对测试代码。外围 27 项包含复用镜像的两个 revision 检查和已有两个开发容器的保留检查，不能与无此环境的 CI 检查点数直接比较。该测试仍使用隔离 Linux 容器与测试应用；生产持续负载、实体服务、真实应用和发行签名门槛继续开放。

另对运行 `44d3abff77c5` 在 S3 已暂停时强制终止测试容器。控制脚本按预期以状态 1 失败，Docker daemon 的 `unpause` 事件证明失败清理先恢复了本次 S3，随后容器 / 卷 / 网络全部移除，原开发服务身份 / 启动时间 / 重启次数保持。`interruption-regression.json` 和原始 Docker 事件保留在该运行目录；此运行保留为预期失败记录。

远端 `42766169439` 的 Compose 运行 `5288d46f5b1d` 在原有 21 项流量检查通过后，遇到故障 fixture 的就绪检查竞态：Worker 已开始执行，而构建总状态由调度器下一轮按秒汇总。原检查立即读取总状态，可能仍为 queued；可控复现使用“子进程标记已就绪、API 先 queued 后 running”的时序，修复前失败，改为复用 running 等待及意外终态检查后通过。随后本机运行 `39256b4483b3` 的 25 项外围与 30 项流量检查再次通过；25 项包含两个现有开发容器保留检查，该轮重新构建产品镜像。原远端失败及清理证据保留，新增修正的远端结果以对应提交 Actions 为准。

就绪修正提交 `39118c7` 的远端 [42767804429](https://github.com/zhubby/artifact-kit/actions/runs/42767804429) 已 completed / success，五个作业全部通过。Compose 运行 `6b0136f0b6c4` 的 23 项外围、30 项真实流量检查通过；实际缓冲 1,027,819 字节，小于 1 MiB，24 个缓存块恢复后与 SSE / PostgreSQL 一致，后续构建成功且清理无错误，源码与执行脚本摘要已核对。

### 跨语言协议联测

`agent/internal/client/protocol_integration_test.go` 在 `AK_LIVE_TESTS=1` 时连接实际 Rust 中心，使用真实登记证书和对象存储，覆盖：

- 无节点证书的 Control RPC 被拒绝；不兼容协议主版本被拒绝；首次注册成功。
- 重复接受任务和事件得到可重放确认；日志有序续传，重复内容接受，冲突内容 / 序号空洞被拒绝。
- 冲突日志重放后，浏览器 SSE 读取的已接受内容保持不变，不能被并发 / 被拒绝上传覆盖。
- 错误大小 / SHA-256 上传不能登记；正确完成请求可幂等重放。
- 已验证内容写入独立 final key；重用旧 staging PUT 地址不能修改已登记对象。
- 取消后的迟到成功不能更新任务；已吊销节点即使提交 revoked=false 也不能恢复 RPC 权限。
- 显式指定独立测试 S3 容器时，分别暂停实际 S3，让日志上传、制品校验请求处于等待中；同一 attempt 的独立控制流在 3 秒内仍获得续租授权，存储恢复后请求成功完成。

最后一项有明确红 / 绿证据：修复前，暂停 S3 后日志上传持有 attempt 行锁，独立心跳在 3 秒内无法续租；修复后两种存储暂停探针均通过。日志先写摘要绑定的独立对象，再用短事务检查租约、序号和摘要并登记索引；不在等待网络时持有租约锁，迟到对象不会登记为有效日志。

### 控制消息等待任务锁时的节点授权

独立 Go / mTLS 客户端新增 `TestLiveRustGoProtocolLeaseFencing` 与 `TestLiveRustGoProtocolEventFencing`。测试使用真实 Rust 中心和 PostgreSQL，持有本次 attempt 行锁，以 `pg_blocking_pids` 确认控制请求确实在等待，再提交节点吊销、数据库证书期限失效或新认证会话替换，最后释放行锁并核对持久状态和 RPC 结果。

修复前分别实跑复现三类失效授权仍可延长租约、把 leased 任务改为 running，以及写入完成状态 / 事件记录。修复后，在拿到任务锁后持有节点共享锁，并在同一事务用当前数据库时钟、节点授权和 active_session 重新检查；节点吊销 / 会话替换与该次写入串行化，续租 UPDATE 也直接检查当前授权和期限。三类控制消息各含一个正常授权对照和三个失效场景，本机全部 12 项通过；完成事件使用受控 failed 结果，并核对 attempt / job 状态、租约、事件序号及记录数。

`TestLiveRustGoProtocolReplayedCompletion` 另行验证显式重试后的旧完成事件：实际失败并重试 job，在持有该节点行锁时重放旧事件，确认请求确实等待；释放后只确认旧序号，不改写 queued job 或旧 attempt，随后由真实调度器分配不同 attempt 并取得开始授权。该项及上述 12 项本机联测全部通过。调度器的节点及候选 job 查询使用 `SKIP LOCKED`，可跳过被事件持有的 job。

协议探针没有真实 Worker 响应取消。完成授权断言后，清理通过 API 取消本次构建，仅将本次仍活跃的 attempt 租约设为过期，再等待真实调度器将该构建及 job 终结。`42786554877` 的协议、浏览器与 SSH 通过后，冷恢复预检发现旧清理仍留下活跃记录；新增清理终态断言也已独立复现此问题。后续修正保持冷恢复的无活跃任务要求，并保留原失败记录。

原始红 / 绿日志、源码及二进制摘要保存在 `.local/lease-node-fencing/`。首次新测试的 psql 连接参数错误另行保留，未算作产品竞态复现；修复前原有 35 项本机端到端验收通过。这里调整的是临时节点的数据库授权期限，没有将 TLS 证书实际老化，也没有替代真实 Worker / 实体宿主故障验收。CI 同时保留 `.local/protocol-live.log`，本次提交的远端结果按精确 SHA 的 [Actions](https://github.com/zhubby/artifact-kit/actions/workflows/ci.yml?query=branch%3Amain) 核验。

### 控制事件等待锁期间的取消

`TestLiveRustGoProtocolCancellationFencing` 使用真实 Rust / Go mTLS、PostgreSQL 和取消 API，分别让 accept / finished 等待本次 attempt 或节点的行锁，并用 `pg_blocking_pids` 确认已阻塞。四个未取消对照正常 start / 完成；另四项在释放锁前提交构建取消，检查 accept 不获得 start、finished 把原本的 failed 结果登记为 canceled，同时核对 attempt / job 状态、事件序号及事件条数。

原实现的四个取消场景全部失败：两个 accept 仍转为 running，两个 finished 仍登记 failed。修复将取消标记从初始加锁查询移除，在所有锁等待结束后复用原有期限检查，从同一事务连接读取当前租约及取消状态。八项红 / 绿回归与全部 32 个协议回归场景、原始协议和两类真实 S3 阻塞续租检查在本机通过，无 skip，执行源码 / 二进制摘要匹配；证据在 `.local/lease-node-fencing/`。完整远端结果仍按当前提交的 CI 核验。

### 制品接口等待任务锁时的授权

`TestLiveRustGoProtocolArtifactFencing` 使用真实 mTLS、PostgreSQL 和 S3，分别阻塞 `PrepareArtifact` 与 `CompleteArtifact` 对本次 attempt 的数据库访问，并用 `pg_blocking_pids` 证明请求确实等待。每个接口包含正常授权、节点吊销、数据库证书期限失效、构建取消和租约在等待期间实际到期五种场景；完成接口先把真实内容上传至私有 staging 对象。

原实现的两个正常对照通过，八个失效场景均错误提交了上传声明或制品记录。修正后将准备上传纳入同一事务，提交前取得 attempt / 节点共享锁并以当前时钟重新检查；完成接口在 S3 校验结束后才取得这些锁，并对首次登记及重复完成都重新验证。上述十项本机红 / 绿检查通过。回滚覆盖数据库声明 / 索引；已写入 S3 的私有对象沿用既有人工保留策略。

`TestLiveRustGoProtocolArtifactConcurrency` 另行并发准备同一路径，再混合重复准备、完成及同一 attempt 的心跳，检查上传身份及内容一致、只登记一份上传 / 制品、心跳续租成功；本机检查通过。原有两类真实 S3 暂停续租、控制流授权和重放检查也通过。证据保留在 `.local/lease-node-fencing/`，最终远端结果按当前提交的完整 CI 核验。

### 真实端到端记录

最近一次完整本机运行：`c4032f9f5461`，35 项通过；测试 Git 提交为 `1154016965ec486d1f9a1b45d92fccfd64133905`。此前通过的运行还包括 `e85df4be20b7`、`7f8d8b60fce5`、`2089db7ceec6`。生成的完整运行清单保存在忽略版本控制的 `.local/acceptance/<run_id>/evidence.json`，最近一次结果另存 `.local/acceptance/latest.json`。证据中的 `commit` 是测试 Git 仓库的提交，不是当前平台仓库提交。

中途运行 `b2220d8fa962` 在 Agent 崩溃后等待 lost 超时，没有记录为成功。其完整原始日志仍保留；排查期间稳定复现并修复了上述存储等待持锁问题，但不足以证明历史超时的唯一原因。后续完整复跑通过，生产环境仍需进行持续负载 / 故障演练，不能删除失败现场后宣称从未失败。

验收脚本实际创建带 Basic Auth 的 HTTPS Git 仓库，平台加密托管其凭据；使用路径中含空格的真实 Go Agent 执行以下链路：

1. 一次性令牌登记，证书接入、心跳及能力上报。
2. 中心固定提交、配置和参数；Agent checkout 同一提交。
3. 在 macOS arm64 上用 clang 编译最小 C 程序；依次执行 ad-hoc `codesign`、`codesign --verify` 与原生运行。
4. 实际上传私有 S3、服务端大小与摘要校验、鉴权下载；下载后再次运行与验证签名。
5. 自动生成内部 Release、幂等发布重试；同项目同版本冲突保留原 Release。
6. 缺少必需制品、符号链接逃逸、步骤非零退出、部分目标失败均不能形成整体成功或自动 Release。
7. 显式 retry 创建不同 attempt；编辑项目后，已排队任务仍使用原始源码快照。
8. 中心进程被杀后在 90 秒内恢复，继续同一 started attempt，不产生重复构建。
9. 中心不可用超过执行租约，本机单调截止期限终止实际子进程树；中心恢复后记 lost，不自动重跑。
10. 取消以及 Agent SIGKILL 后，Worker / 子进程实际终止；不依赖进程枚举或模拟 ACK 宣称清理成功。

**35 项是脚本检查点数，其中包含预期失败 / lost / canceled 的结果检查；不是 35 个操作系统或签名目标。**

### SSH 与隔离冷恢复

`scripts/ssh_acceptance.py` 最近一次运行 `1607261e4bc0`，10 项通过。临时 OpenSSH 服务仅监听 loopback，使用生成的主机公钥建立可信 `known_hosts`；密码、TTY 和转发均关闭，远端命令限制为 Git。真实 Rust 控制面加密保存私钥并解析提交，Go Worker 用同一固定提交编译 C，下载后执行并发布 Release；错误主机指纹与未授权密钥在分配 Worker 前失败。该测试不是对外部 Git 服务或生产网络的验收。

冷恢复运行 `8efa99ec0523`，16 项通过。停止本机控制面后，备份 PostgreSQL、S3、环境主密钥和 PKI，在独立标签、全新 Docker 卷及随机 loopback 端口上恢复；21 张 public 表的行摘要和序列状态一致，784 个对象、3,037,785 字节通过实际下载逐字节校验。恢复后既有 Release 下载摘要正确且可执行，新登记的真实 Agent 能用恢复的加密 SSH 凭据构建并发布新 Release，原 Release 保持不变。演练前后源数据库摘要一致，临时容器和卷全部清理，私有备份与证据保留。

演练曾复现真实路径缺陷：控制面 / Worker 的数据目录含空格时，OpenSSH 会再次解析 `UserKnownHostsFile`，外层 shell 引号被移除后主机信任文件路径被拆开。修复同时保留 OpenSSH 与 shell 所需的引号；最终冷恢复使用含空格和单引号的数据目录通过。失败运行 `4241b532629f` 以及之后已完成构建但验收脚本误读发布响应的 `a40240c2d147` 均保留为失败记录。

Linux CI 的 S3 启动问题也有真实红 / 绿验证：0600 配置由宿主 UID 4242 持有时，原入口降为 UID 1000 后读取失败；新入口保留宿主原权限 / 归属，在容器内生成 UID 1000 的 0600 私有副本，S3 就绪、匿名请求被拒绝，主进程以 UID 1000 运行。隔离 Compose / TLS 实跑结果见上节；正式主机和公网部署仍待验收。

后续远端运行 `42750492414` 已通过三类原生 Agent、Rust 静态与数据库测试、Go 检查、交叉编译及前端构建，但首个 HTTPS fixture 构建失败。独立 Linux Git / GnuTLS 探针复现了 `TLS connection was non-properly terminated`：测试 HTTP/1.0 服务依靠 TLS EOF 结束响应，未声明长度。为 401 和 Git CGI 响应设置实际 `Content-Length` 后，同一 Linux 客户端使用带空格路径的真实 Agent askpass 成功认证并取得固定提交；完整远端结果仍以对应提交的 Actions 为准。

冷恢复只验证本地私有明文备份及隔离还原；生产备份加密、异地保存和灾难恢复目标仍需另行验收。CI 只保留明确列出的日志 / evidence，不上传备份目录、环境文件、私钥、PKI 或 Agent 配置。

### 浏览器与视觉验证

10 项测试来自桌面和移动模拟各 5 项；使用真实后台、真实构建 / 日志 / 制品 / Release，不替换 API：

- 六个主要操作页面和无文档横向溢出；下载真实原生制品。
- 真实 SSE 日志回放、断线提示与重连；部分失败矩阵和发布失败不显示整体成功。
- 只读身份不能修改节点或触发构建。
- 项目和构建表单提交后真正编译、上传并下载制品。
- 对话框初始焦点、Tab、Escape、关闭后的焦点归还，移动导航可用。

最新截图和 trace 位于 `web/test-results/`，HTML 报告位于 `web/playwright-report/`，均不提交仓库。桌面及移动构建页已实际查看。移动测试是 Chromium 尺寸模拟，**不等同于 Safari、真实 iPhone 或 iOS 安装测试**。

## 宿主与应用目标兼容性矩阵

| 对象 | 编译 / 配置 | 实际运行 | 未完成门槛 |
|---|---|---|---|
| macOS arm64 Agent | Go 原生与交叉二进制均已生成 | 本机前台 Agent / Worker 与故障联测；临时 VM 的 launchd 在途任务重启 / 停止、清理及后续任务通过 | 实体机重启自启、正式升级与真实工具链 / 签名环境验收 |
| macOS amd64 Agent | CGO=0 二进制已生成 | `macos-15-intel` 的原生 Go / launchd 在途任务重启 / 停止、清理及后续任务通过 | Intel 实体 Mac、重启、真实工具链及应用验收 |
| Linux amd64 Agent | CGO=0 ELF 已生成 | 临时 VM 的 systemd 在途任务重启 / 停止、清理及后续任务通过；CI 最小原生闭环通过 | 实体机重启自启、SDK 与真实项目应用验收 |
| Linux arm64 Agent | CGO=0 ELF 已生成 | `ubuntu-24.04-arm` 的原生 Go / systemd 在途任务重启 / 停止、清理及后续任务通过 | arm64 实体 Linux、重启、真实工具链及应用验收 |
| Windows amd64 Agent | PE 已生成；Job Object 测试已交叉编译 | 临时 VM 的 Go race、Job Object 及 SCM 在途任务重启 / 停止、清理及后续任务通过 | Windows 实体宿主、重启自启、真实工具链 / 应用与故障验收 |
| Linux 应用 | Rust 配置模板已解析 | 尚未构建真实项目应用 | Linux 宿主、项目依赖、原生运行或安装验收 |
| macOS 应用 | Xcode 模板已解析；最小 C 可执行文件实际编译 | arm64 C CLI + ad-hoc 签名 / 校验通过 | 完整 Xcode `.app`、发行证书、notarization / 安装验收 |
| Windows 应用 | Rust 配置模板已解析 | 尚未构建真实项目应用 | Windows 工具链、Authenticode 证书与安装验收 |
| Android 应用 | Gradle / apksigner 配置模板已解析 | 未产出 APK / AAB | Android SDK、真实 Gradle 项目、keystore、原生验证与设备 |
| iOS 应用 | Xcode archive / export 配置模板已解析 | 未产出 IPA | 完整 Xcode、项目、证书 / provisioning profile、分发方式与设备 |

签名私钥没有上传中心。本次 `adhoc-test` 仅验证签名引用授权和原生阶段执行，不是 Developer ID、App Store、Ad Hoc provisioning 或企业分发的验收。内部下载可用不代表任意设备可安装。

## 部署验证与运行边界

- PostgreSQL 与认证 S3 实际在本地 Linux 容器运行；开发端口仅绑定 `127.0.0.1`。
- 前端 standalone Docker 镜像已构建；使用真实中心完成 10 项浏览器测试，非仅检查 HTTP 200。
- Rust Linux 控制面镜像已成功构建；隔离 Linux amd64 容器中完成 7 项检查：非 root / 只读根文件系统启动、迁移及私有 S3、内置登录、真实 Linux Go Agent 注册、mTLS 心跳、排空、中心容器重启后的项目 / 节点状态及证书重连。过程使用独立数据库和对象 bucket，没有启动第二个中心操作主验收数据库。
- 尚未在正式 Linux 主机运行完整生产 Compose / Caddy / 公网 TLS、远程 Agent 上传和生产备份恢复演练；配置解析成功不能代替部署验收。
- 三类服务安装 / 卸载 / 手动排空升级命令已经实现并记录，但本次没有在宿主安装系统服务或修改开机启动。
- Linux 容器运行和 Agent 交叉编译都不能代替用户要求的三类实体机验收。

最后一轮容器证据：`.local/container-smoke/314d392904/evidence.json`，7 项通过，Linux 7.2.12-orbstack x86_64，非 root UID 501。该轮控制面镜像为 `sha256:9857943b2dc629d0a2aabee1862747508a9ab1bc37d34b8a1cc1f96117e82c83`；最终前端镜像为 `sha256:f52c3f640b4a39628f4fa3b33f4dbeab32f9d2340d300af8bcc8caf40233c4ce`。本机镜像标签分别是 `artifact-kit-control:local-acceptance` 和 `artifact-kit-web:local-acceptance`，未发布到镜像仓库。

镜像下载存在本地网络限制，本次仅通过构建输入替换使用镜像源并传入本地网络代理，没有改写提交的默认基础镜像或全局 Docker 配置。生产应自行确认镜像来源、固定 digest、受信证书和服务账户权限。

## 复现与剩余验收

快速启动及测试命令见根 README；测试需先启动 PostgreSQL / S3 并加载私有 `.local/env`。端到端脚本自行管理中心、Agent 和 Git 服务，不要在同一端口再启动另一个中心。协议和浏览器测试应在输出 `ACCEPTANCE READY` 后执行。

交付前仍需提供环境并完成：

1. Linux、macOS、Windows 三类实体机的系统服务安装 / 运行 / 重启 / 卸载、槽位与进程树故障验收。
2. 五种实际应用项目、相应 SDK / 工具链和 CPU 架构的完整构建记录，不能用本仓库模板解析替代；本机 HTTPS / SSH 最小 C 闭环已通过，外部 Git 网络仍需现场验收。
3. 各目标发行签名、原生校验及分发 / 安装；iOS 必须明确证书与 profile 对应的分发方式。
4. 生产域名、TLS / 证书信任、跨机器 Git / gRPC / 私有对象存储连通性、上传容量与磁盘容量测试。
5. 将已验证的本地密钥 / PKI / 数据库 / 对象一致恢复扩展到生产加密与异地备份，完成持续负载 / 故障演练、日志容量和人工保留策略。

本版不支持不可信代码沙箱、容器执行器、OCI 发布、官方商店、Webhook、OIDC、多实例 HA、自动证书续期或自动垃圾回收。节点证书到期处理、手动升级和恢复约束见运维手册。

设计自审不是独立安全审计。
