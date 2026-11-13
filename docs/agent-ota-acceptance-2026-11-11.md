# Agent OTA 验收记录（2026-11-11）

实现分支为 `codex/agent-ota`。本记录验证的运行程序源码提交是 `a35051e4a852e5d2787537054750b6a9b42f9159`；Mac mini 的固定启动器保留 `ba2b0b582bc225cf7f3621012b6e9ae35081c264`。运行程序和启动器均使用协议主版本 1。安装、信任配置、接口和恢复方法见 [Agent OTA 操作说明](emissary-ota.md)。

安装器修复 `59d3e82` 为 macOS 服务设置 15 秒退出期限；现有 Mac mini 已备份并补齐相同配置，保留运行程序、启动器、账户、环境和身份。

## 已交付能力

- 五平台原生发布工作流生成带版本、提交、协议范围、大小和 SHA-256 的 Ed25519 签名 manifest。控制面从预先配置的 GitHub 仓库按标签导入，验证全部资产后保存至私有 S3；相同版本不能覆盖。
- 固定启动器以原普通服务账户管理版本目录及持久化升级记录，使用独立 `ota.json`，保留严格解析的 `agent.json`、节点身份、证书、签名配置和工具链。
- 下载不阻塞心跳；切换等待构建结束、Worker 清理、产物登记、任务终态和日志确认。候选版本须在 120 秒内完成 mTLS 重连及连续三次真实心跳确认，失败则恢复原版本。
- 节点事务锁协调调度与升级，节点覆盖优先于池目标，OTA 排空独立于管理员排空。每池默认一个升级，失败或回滚暂停该池；取消、重试和历史版本回退保留审计。
- `/api/v1` 管理接口及中英文 Nodes / Settings 界面提供版本导入、目标、策略、进度与恢复操作。写操作要求管理员会话和 CSRF，项目 API Token 不取得升级权限；旧 Agent 继续构建并显示安装提示。

## 本地部署与实体机结果

控制面和 Web 在本地运行，复用本地测试 PostgreSQL 与私有对象存储。节点通过 Tailscale 主动连接控制面的 mTLS 服务。kind 中 `control`、`postgres`、`s3`、`web` 四个 Deployment 保持 0 副本，三个 PVC 均保持 Bound。

`codehubmac-mini` 使用 macOS arm64、Xcode 和原 `artifactbuild` 服务账户。一次性安装启动器前备份原二进制、服务配置和身份；后续升级均从控制面下发。恢复后逐文件 SHA 校验确认身份和证书与备份一致。

| 场景 | 实际结果 |
| --- | --- |
| A → B 自动升级，期间有真实 Xcode 构建 | 原构建以同一个 attempt 完成；日志及三个产物确认后才切换，排队构建随后继续 |
| 排空时取消、随后重试 | 不取消在途构建，不修改管理员排空状态，取消确认后恢复调度 |
| 错签名、损坏、截断、平台或协议不兼容 | 全部拒绝导入，原版本目录和节点继续可用；同内容重复导入返回原 ID |
| 签名但启动后崩溃的候选 | 自动恢复上一版本并暂停节点池；最终版本 E 上再次复验通过 |
| 候选启动但不重连 | 实际等待 120 秒健康期限后自动回滚并暂停池 |
| 激活期间重启原生启动器 | 从持久化记录恢复原版本，保留身份并报告回滚，未循环升级 |
| 不响应退出的候选遇到原生服务停止 | 复现 launchd 默认约 5 秒提前结束启动器的问题；配置 `ExitTimeOut=15` 后，约 8.3 秒完成强制清理再重启，无遗留候选 |
| 最终运行程序 E | 报告提交 `a35051e`；固定启动器仍为 A，普通升级不替换启动器 |
| E 执行 Xcode 构建期间重启 Rust 控制面 | Agent 自动重连，原始 attempt 成功完成并发布，没有增加重试 attempt |
| E 回滚后再次构建、发布、下载运行 | 三个产物大小和 SHA-256 均正确；应用通过 `codesign --verify --deep --strict` 和 `--self-test`，实际打开原生窗口 |

最终复验的可追溯标识：

- E 的成功升级执行：`b39dcedb-9855-45e0-8675-6b41514fe109`。
- 控制面重启期间的构建：`ffa0b068-4f74-49e4-a0db-52cc9310cd5c`。
- 崩溃候选回滚执行：`5ae1b376-278a-4194-80a0-6a69babe680c`。
- 不响应退出的候选清理及中断恢复：`b33cb693-125b-4302-9baa-29582ff8e0ce`。
- E 版本崩溃回滚后的构建：`b665002a-c540-4248-953a-21a3b68daabf`。
- 服务退出期限修复后的最终构建：`282b840e-292e-40c1-8498-ad5a3787bb6b`。
- 最终应用自测报告 `ok: 1`、构建账户 `artifactbuild`、架构 `arm64`，构建 ID 与控制面一致。演示应用源码提交为 `d0f75d011623265bfcf504212889bfd39277ddff`。

从 E 版本构建 `b665002a` 的控制面持久化事件流读回 128 个日志块，包含 Xcode `BUILD SUCCEEDED`。三项下载为应用 ZIP、ZIP 校验文件和 `self-test.json`，均经私有 S3 签名地址获取。应用采用测试用 ad hoc 签名。

下载的 `.app` 还通过 LaunchServices 实际启动，在本机显示 `Artifact Kit — macOS Build Test` 原生窗口。窗口截图中的构建 ID、提交、架构、Mac mini 主机和服务账户均与该构建一致；该图形界面运行检查独立于命令行自测。

上述窗口截图对应构建 `b665002a`。服务退出期限修复后，构建 `282b840e` 再次完成 Xcode、发布、三个私有下载的大小 / SHA-256 校验、签名校验及应用自测，另读回 135 个持久化日志块；修复后的服务运行完整链路保持可用。

验收结束时节点在线，当前与目标均为 `v0.1.0-ota-e`，池已恢复，无活动升级或排空。节点界面保留最近一次坏候选的回滚结果供检查。

## 自动验证与浏览器

运行程序提交对应 CI：[platform-contracts / 42958769246](https://github.com/zhubby/artifact-kit/actions/runs/42958769246)，全部通过。安装器修复提交 `59d3e82053474d1deb3bd61a9b3704d3abe302ff` 对应 [platform-contracts / 42962434781](https://github.com/zhubby/artifact-kit/actions/runs/42962434781)，七项 Job 全部通过，包含五个平台的原生服务测试、Compose TLS 和 Linux 全链路。

| 验证 | 结果 |
| --- | --- |
| Linux amd64 原生普通账户服务升级与回滚 | 通过，`ubuntu-latest` |
| Linux arm64 原生普通账户服务升级与回滚 | 通过，`ubuntu-24.04-arm` |
| macOS arm64 原生普通账户服务升级与回滚 | 通过，`macos-latest` |
| macOS amd64 原生普通账户服务升级与回滚 | 通过，`macos-15-intel` |
| Windows amd64 原生普通账户服务升级与回滚 | 通过，`windows-latest`，包含服务账户、EXE 文件锁及进程树清理路径 |
| Compose TLS 全链路 | 通过，23 项验收和 30 项协议探测 |
| Linux Rust / Go / PostgreSQL / 私有 S3 / 浏览器全链路 | 通过，57 项 UI、10 项真实浏览器、真实协议联测、10 项 SSH 和 16 项恢复验收 |

本地 Rust 全量测试（包含 12 个隔离 PostgreSQL schema 测试）、Clippy 和格式检查、Go vet 及全量 race 测试均通过。测试覆盖节点锁和池并发、失败暂停、调度竞争、会话隔离、吊销和证书期限、管理员和项目 Token 权限、下载中断、重复命令、启动和确认失败、持久化恢复，以及真实进程 mTLS 升级与回滚。

正常服务停止的回归测试确认 Worker 进程树、工作目录和日志终态得到清理及确认；Linux 测试单独验证网络 `SIGPIPE` 不会使运行程序退出。

macOS 新增回归先确认缺少退出期限会失败，再验证安装器生成的 plist 可被 `plutil` 解析且保留原账户、带特殊字符的路径及环境。两种架构的 macOS 原生服务测试均进一步启动不读取退出管道的候选，停止服务后检查进程已消失，再启动并确认恢复原版本；最终 CI 的两项原生 macOS Job 均通过此回归。

本地 production 模式 Playwright 全套 **57/57** 通过，其中 UI fixture 验证管理员与只读权限展示，真实 PostgreSQL / HTTP 测试验证后端权限。实际浏览器另外操作了签名版本导入、目标指定、升级排空、取消、回滚和恢复，检查了激活提示、实际重试版本及页面宽度。

私有原始证据保存在工作树 `.local/ota-macmini/`：`evidence.json`、`final-evidence.json`、`latest-evidence.json`、`latest-log-evidence.json`、`latest-gui-evidence.json`、`latest-app-window.png`、`restart-cleanup-fixed-evidence.json`、`service-deadline-evidence.json`、`after-service-fix-evidence.json`、`after-service-fix-logs.json`、`final-process-evidence.json`、`final-state.json`、`kind-final-evidence.json`、构建详情、产物、CI 日志和浏览器截图。配置、测试私钥、节点身份及登录会话不提交到 Git。

## 发布与恢复边界

本轮使用**独立测试 Ed25519 密钥及 GitHub Release API fixture**。签名、按标签导入、五资产验证、私有 S3、Tailscale / mTLS 和原生启动器均实际执行；尚未发布正式 GitHub Release。正式发布需配置受保护的 `agent-release` Environment Secret `AGENT_RELEASE_SIGNING_SEED`，并部署对应验签公钥。

中断恢复实际执行的是原生服务进程重启，**未进行实体机断电或系统重启**。Windows、Linux 和 Intel Mac 的原生服务证据来自对应 GitHub runner；交叉编译结果不单独作为原生验收依据。

固定启动器、SDK、系统工具链和节点证书续签不在第一版自动升级范围内。已有节点默认不自动升级，管理员指定目标版本后才生效；始终等待在途构建，不提供强制取消构建以升级的操作。
