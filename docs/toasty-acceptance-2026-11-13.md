# Toasty 迁移验收记录

范围：访问层收敛提交 `409b1a4`，Toasty 迁移提交 `88c7ad5`，流式读取及连接取消提交 `b359028`，构建状态并发修复提交 `57bde58`。最终服务验收使用 `57bde58df9b6c2c084dbef5d7d061dc586d1fb35`。开发在独立 worktree/分支进行，既有共享环境没有切换。使用独立 PostgreSQL/TimescaleDB、S3 bucket、Agent 身份与配置。

## 已验证

| 检查 | 结果与证据 |
|---|---|
| SQLx 清零及访问边界 | Cargo 依赖图（所有目标）和 Cargo.lock 均无 SQLx；入口 SQL/驱动引用检查通过；19 项 Python 检查通过 |
| Rust | `b359028` 完整 workspace 显式数据库测试通过（136 项，零失败、零 ignored）；`57bde58` 再次通过 Control 的 31 项 ignored 数据库回归；Clippy 警告视为错误、格式和共享库文档通过 |
| baseline 与旧最终 schema 对照 | 四个数据库的列、索引、约束、函数、触发器、序列及 TimescaleDB 对象完全相同，忽略迁移元数据表 |
| 迁移与故障恢复 | 并发迁移、重复启动、只读校验、旧 schema/未知版本拒绝、失败回滚、取消/savepoint 回滚、连接中断及连接池恢复均通过 |
| 遥测分页 | 固定快照期间修改/插入数据不改变后续页；每批至多 128 行；6000 行与 2 MiB 边界通过 |
| 遥测内存 | 流式优化后，候选结果 6001 行/393281536 字节时峰值 8872586 字节；12001 行/786497536 字节时峰值 8873432 字节；两次均返回 31 行后截断 |
| 原生端到端 | `b3b94f933226`：35 项通过，包括真实原生编译、macOS ad-hoc 签名、日志、下载、发布、权限、取消、断网租约失效及崩溃恢复 |
| Emissary 协议 | 真实 mTLS、行锁、等锁后取消/授权复查、旧会话/过期证书/租约隔离、并发产物、重放、S3 阻塞时独立心跳均通过 |
| 浏览器 | 最终提交的 12 项桌面/移动端业务测试通过，使用默认动画设置，耗时 3.4 分钟 |
| Compose | `8d10b9871251`：最终提交的 25 项部署检查与 30 项流量检查通过，包含真实 HTTPS/mTLS/S3/SSE、重启及日志缓冲故障；本次容器、卷和网络已清理 |
| Operator | 专属 kind 命名空间 `artifact-operator-toasty-stream-20261113`：69 项检查通过，覆盖四库隔离、真实 Pi 预算与纠错、Source/PVC 恢复、领导者替换、Pod 重启、取消与准入暂停；独立 Telemetry 及 macOS arm64 主机原生构建验收通过 |
| 新版本备份恢复 | `20261113-173048-recovery`：四库、TimescaleDB 列存/聚合、22 个 S3 对象、密钥和路由恢复通过；恢复后重新完成真实构建、授权和恢复流程，原数据 Pod 保留 |

最终 Operator 的构建标签为 `toasty-20261113-03`，源码标签为上述最终服务提交，镜像 ID 为 `sha256:d408b672923964ff3fb783fee54bfd15d9040396adf1414792588b4c568f3238`，registry manifest 摘要为 `sha256:bfe9789ddab421169522199c61ced80ddd3641b41a48774576c645ed8fb48903`。主机原生构建证据为 `20261113-172803-native`；临时 Agent 身份已吊销、临时路由已恢复。

baseline 对照计数（完整结果保存在私有 `.local/schema-parity.json`）：

| 数据库 | 列 | 索引 | 约束 | 函数 | 触发器 | 序列 | hypertable | 视图 | 策略/任务 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Business | 297 | 69 | 132 | 3 | 3 | 5 | 0 | 0 | 0 |
| Runtime | 45 | 8 | 10 | 0 | 0 | 0 | 0 | 0 | 0 |
| Source | 38 | 9 | 15 | 0 | 0 | 0 | 0 | 0 | 0 |
| Telemetry | 103 | 15 | 8 | 0 | 0 | 0 | 3 | 4 | 14 |

Toasty 原版连接丢失后的 panic 已在隔离测试中复现；本地修订保留上游发布包来源、校验和及许可证。流式结果需在读取期间持有连接，提前结束时发送取消请求并关闭物理连接；仅关闭 socket 不足以中断 PostgreSQL 的锁等待。连接复用、提前结束、中途 SQL 错误及锁等待取消的 4 项新增回归通过。详见 [Toasty 修订](https://github.com/zhubby/artifactory/blob/main/vendor/toasty/PATCH.md) 与 [PostgreSQL 驱动修订](https://github.com/zhubby/artifactory/blob/main/vendor/toasty-driver-postgresql/PATCH.md)。

流式版本复验还暴露了既有构建汇总竞争：汇总在获得构建行锁前读取任务状态，等锁后可用旧的 `failed` 覆盖刚提交的重试/规划状态。确定性测试在修复前失败；汇总 SQL 增加 `xmin` 行版本条件后通过，并保留单语句更新和原锁顺序。最终原生 35 项复验通过，包括显式重试创建独立执行。失败的原生运行 `09c822e68608` 保留诊断，不计入通过结果。

最终原生、协议和默认动画浏览器日志分别保存在 `.local/toasty-native-verified.log`、`.local/toasty-protocol-verified.log` 和 `.local/toasty-browser-verified-default.log`。Compose 日志为 `.local/toasty-compose-verified.log`；Operator 四阶段日志为 `.local/toasty-operator-verified-{acceptance,telemetry,native,recovery}.log`，不包含在发布包中。原生及 Web 验收监听已停止，最终 kind 命名空间和备份保留供复核。完整 workspace 组合命令单独跳过真实协议及 `native_service_lifecycle`；真实协议随后已通过，后者仅允许在可丢弃的 GitHub 托管 VM 创建服务账户，本机没有执行它。

## 性能对照

在同一主机、同一 PostgreSQL/TimescaleDB 实例上，交替运行 `409b1a4` 保存的 SQLx 测试二进制与 Toasty 流式实现，共五轮。每个动作预热 32 次、测量 256 次，计时范围及夹具保持一致；比较五轮 p95 的中位数。测量前停止本次浏览器和原生验收进程，Rust/Docker 构建及 Compose 验收均已结束。共享主机仍有其他负载，因此这些结果代表该隔离测试环境，不代表生产容量。

| 动作 | SQLx p95 中位数 | Toasty p95 中位数 | 变化 | 10% 门槛 |
|---|---:|---:|---:|---|
| 调度 | 81.983 ms | 86.200 ms | +5.14% | 通过 |
| 心跳 | 105.359 ms | 93.490 ms | -11.27% | 通过 |
| 事件提交 | 111.272 ms | 95.968 ms | -13.75% | 通过 |
| 遥测读取 | 35.142 ms | 36.403 ms | +3.59% | 通过 |
| 遥测写入 | 115.901 ms | 66.542 ms | -42.59% | 通过 |

[全部轮次与基线二进制校验和](acceptance/toasty/2026-11-13-performance.json)随代码保存，记录的流式实现为 `b359028`；后续汇总行版本修复不涉及这两个性能测试的计时调用路径。内存测试同时覆盖 393 MB 与 786 MB 的完整候选数据，客户端分配峰值均约 8.9 MB。

## 切换准备

统一停写、排空、身份重建、spool 归档和整体回退操作见 [迁移操作说明](toasty-migration.md)。生产目标尚未指定；本次没有执行实际生产停写、数据库重建或服务切换。
