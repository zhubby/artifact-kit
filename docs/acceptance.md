# 实施与验收记录

记录日期：2026-11-10，Asia/Shanghai。此记录区分源代码实现、实际自动化结果、运行环境和外部验收；**不是三类实体宿主、五种应用目标的最终验收证明**。

## 结论与里程碑

| 阶段 | 本次交付 | 状态 |
|---|---|---|
| M0 | 已确认设计、设计自审、版本化 Protobuf、v1 JSON schema、两次数据库迁移、状态机文档 | 已交付 |
| M1 | 内置登录、三类角色、项目范围 Token、一次性注册、节点证书认证、心跳、能力、排空、诊断与服务管理 CLI | 软件实现与本机协议联测通过；三类系统服务实机待验 |
| M2 | Git 引用解析及不可变快照、事务调度、独立 Worker、进程树清理、SSE 日志、私有制品校验与下载 | macOS arm64 真实原生闭环通过 |
| M3 | 项目 / 节点 / 本机签名引用授权、sign / verify 阶段、不可变内部 Release、幂等发布重试、审计 | 本机 ad-hoc 签名与发布通过；发行签名待验 |
| M4 | Compose、镜像、服务安装说明、运维手册、故障自动化、浏览器测试、二进制交叉构建、兼容性矩阵 | 部分交付，外部验收尚未完成 |

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
| Rust 测试 | 17 项通过：9 项配置 / 加密 / 权限 / 匹配测试，8 项显式启用的真实 PostgreSQL 测试 |
| Go 检查 | `go vet ./...`、`go test -race ./... -count=1` 通过；不启用实时环境时协议联测会显式跳过 |
| 验收启动回归 | 3 项真实子进程测试通过：提前退出立即报告状态与日志、就绪超时终止进程、健康检查成功后保留运行进程 |
| 跨语言协议 | 独立 Go 客户端与真实 Rust TLS / gRPC 服务的注册、身份、事件、日志及制品联测通过；实际暂停 S3 验证日志 / 制品存储阻塞不阻断续租 |
| 原生端到端 | `scripts/acceptance.py --keep-alive` 35 项检查通过，详细边界见下节 |
| 前端 | TypeScript 检查、生产构建和 Docker 镜像构建通过；容器运行的控制台通过 10 项真实后台浏览器测试 |
| Agent 编译 | Linux amd64 / arm64、Darwin amd64 / arm64、Windows amd64 五个目标二进制生成成功 |
| Windows 原生回归 | GitHub `windows-latest` 上 `go vet ./...` 与 `go test -race ./...` 通过，实际执行 Job Object 子进程树终止回归；未验收系统服务与真实 Windows 应用 |
| 配置与部署文件 | 五个示例通过真实 Rust Recipe 解析；JSON schema 语法、shell 脚本语法、Compose 配置校验通过 |
| 远端 CI | [首次运行 42735894958](https://github.com/zhubby/artifact-kit/actions/runs/42735894958) 的 Linux / macOS / Windows 原生 Agent 检查通过；Linux 完整端到端控制面启动超时，正在定位，**整体 CI 尚未通过** |

普通 `cargo test --all-targets` 会跳过需要数据库的 8 项测试；本次另外设置 `AK_TEST_DATABASE_URL` 并使用 `--ignored` 执行。测试为每次运行创建隔离 schema，不能以默认命令中的 ignored 项冒充数据库验收。

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

### 真实端到端记录

最终修复后的完整运行：`e85df4be20b7`，35 项通过；测试 Git 提交为 `4a2528945915febdfc0879815d1fa3c3899673a5`。此前通过的运行还包括 `7f8d8b60fce5`、`2089db7ceec6`。生成的完整运行清单保存在忽略版本控制的 `.local/acceptance/<run_id>/evidence.json`，最近一次结果另存 `.local/acceptance/latest.json`。证据中的 `commit` 是测试 Git 仓库的提交，不是当前平台仓库提交。

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
| macOS arm64 Agent | Go 原生与交叉二进制均已生成 | 本机前台 Agent / Worker、实际进程树和故障联测 | launchd 安装、重启、自启及正式升级流程实机验收 |
| macOS amd64 Agent | CGO=0 二进制已生成 | 未运行 | Intel Mac 与对应工具链 |
| Linux amd64 Agent | CGO=0 ELF 已生成 | Linux 容器中注册 / 心跳 / 续连通过；实体宿主未运行 | Linux 服务账户、systemd 安装 / 重启 / SDK 与原生应用构建 |
| Linux arm64 Agent | CGO=0 ELF 已生成 | 未运行 | arm64 Linux 主机与工具链 |
| Windows amd64 Agent | PE 已生成；Job Object 测试已交叉编译 | GitHub Windows runner 原生 Go race 与 Job Object 子进程终止回归通过 | Windows 实体宿主、SCM 服务安装 / 重启、真实构建与故障验收 |
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
- 尚未在正式 Linux 主机运行完整生产 Compose / Caddy / 公网 TLS、远程 Agent 上传和备份恢复演练；配置解析成功不能代替部署验收。
- 三类服务安装 / 卸载 / 手动排空升级命令已经实现并记录，但本次没有在宿主安装系统服务或修改开机启动。
- Linux 容器运行和 Agent 交叉编译都不能代替用户要求的三类实体机验收。

最后一轮容器证据：`.local/container-smoke/314d392904/evidence.json`，7 项通过，Linux 7.2.12-orbstack x86_64，非 root UID 501。该轮控制面镜像为 `sha256:9857943b2dc629d0a2aabee1862747508a9ab1bc37d34b8a1cc1f96117e82c83`；最终前端镜像为 `sha256:f52c3f640b4a39628f4fa3b33f4dbeab32f9d2340d300af8bcc8caf40233c4ce`。本机镜像标签分别是 `artifact-kit-control:local-acceptance` 和 `artifact-kit-web:local-acceptance`，未发布到镜像仓库。

镜像下载存在本地网络限制，本次仅通过构建输入替换使用镜像源并传入本地网络代理，没有改写提交的默认基础镜像或全局 Docker 配置。生产应自行确认镜像来源、固定 digest、受信证书和服务账户权限。

## 复现与剩余验收

快速启动及测试命令见根 README；测试需先启动 PostgreSQL / S3 并加载私有 `.local/env`。端到端脚本自行管理中心、Agent 和 Git 服务，不要在同一端口再启动另一个中心。协议和浏览器测试应在输出 `ACCEPTANCE READY` 后执行。

交付前仍需提供环境并完成：

1. Linux、macOS、Windows 三类实体机的系统服务安装 / 运行 / 重启 / 卸载、槽位与进程树故障验收。
2. 五种实际应用项目、相应 SDK / 工具链和 CPU 架构的完整构建记录，不能用本仓库模板解析替代；SSH Git 需额外进行真实 SSH 服务、主机指纹和密钥联测，本次真实 Git 闭环使用 HTTPS。
3. 各目标发行签名、原生校验及分发 / 安装；iOS 必须明确证书与 profile 对应的分发方式。
4. 生产域名、TLS / 证书信任、跨机器 Git / gRPC / 私有对象存储连通性、上传容量与磁盘容量测试。
5. 加密密钥 / PKI / 数据库 / 对象的一致备份、隔离恢复演练、持续负载 / 故障演练、日志容量和人工保留策略。

本版不支持不可信代码沙箱、容器执行器、OCI 发布、官方商店、Webhook、OIDC、多实例 HA、自动证书续期或自动垃圾回收。节点证书到期处理、手动升级和恢复约束见运维手册。

设计自审不是独立安全审计。
