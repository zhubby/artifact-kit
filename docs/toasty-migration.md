# Toasty 数据访问与整体切换

本版本只支持新建数据库，不升级 SQLx 数据库，不导入历史业务数据。部署时所有后端一起升级；旧数据库、对象存储、配置和 Agent 身份保留在旧环境中。

## 代码边界

HTTP、gRPC、调度和后台任务通过业务服务调用 store。连接、模型、行解码、SQL 和事务仅出现在以下位置：

| 数据库 | 实现 | 初始化版本 |
|---|---|---|
| Business / Control | `crates/control-plane/src/store/` | 2026111301 |
| Runtime | `crates/planner-runtime/src/store.rs`、`store/` | 2026111302 |
| Source | `crates/source-core/src/store/`；服务初始化位于 source-service/store | 2026111303 |
| Telemetry | `crates/telemetry-service/src/store/` | 2026111304 |
| Business 只读授权 | telemetry-service/store/authorization | 校验 Business 版本，不迁移 |

`crates/database` 仅管理连接策略、扩展检查、嵌入式迁移、参数和结果转换以及 SQLSTATE 错误。普通 CRUD 使用私有 Toasty 模型；锁、服务器时间、条件写入、聚合、响应投影和 TimescaleDB 操作使用 Toasty 参数化 SQL。模型不直接序列化为 API 响应。

源码抓取和下载协调锁仍覆盖原来的 Git 操作/响应流生命周期。上传保持在数据库事务外，提交上传结果时在短事务中复查节点、会话、租约、序号和摘要。同一原子操作的模型调用及 SQL 共用同一个 Toasty 事务；嵌套操作使用 savepoint。

数据库时间使用 jiff，在 store 边界转换为 chrono UTC，精度固定到 PostgreSQL 微秒。JSON null 与 SQL NULL、空 UUID 数组与 NULL 数组分别编码。约束错误通过驱动的 SQLSTATE 分类，保留 400/404/409 和现有 gRPC 状态码，不匹配错误文本。

Toasty 固定为 0.11.0。连接丢失、异步回滚、原生结果流和 SQLx TLS 参数别名兼容的本地修订见 `vendor/toasty/PATCH.md` 与 `vendor/toasty-driver-postgresql/PATCH.md`，原始发布包的校验和与逐文件来源见各目录中的 `upstream.json`。已有 URL 中的 `ssl-mode`、`ssl-root-cert` / `ssl-ca`、`ssl-cert` 和 `ssl-key` 继续按对应的标准参数校验。

## 数据库初始化

所有数据库预先安装 TimescaleDB 2.32+。扩展安装由管理员完成，应用角色不需要读取管理员级服务器设置。

- 主机/Compose 的 poll 模式：Business 和 Telemetry；Source 缓存表在 Business 中，不创建独立 Runtime/Source 数据库。
- Operator 模式：Business、Runtime、Source、Telemetry 四套独立数据库，沿用现有角色和网络隔离。
- Telemetry 写连接迁移 Telemetry；查询连接和业务授权连接保持只读并校验已知版本。

Control HA 在 Business 基线 `2026111301` 上追加 `2026111401 / 0001_control_replication.sql`，原有 Toasty 库可增量升级；迁移回填在途命令并保留租约。此增量升级不改变 SQLx 旧环境需要独立新库的切换边界。Telemetry 业务只读授权兼容基线或基线加 HA 两种已知历史。

每套 baseline 使用 Toasty `embed_migrations!`，迁移历史保存在 `__toasty_migrations`。迁移期间持有数据库级 advisory 事务锁。空库初始化、重复启动和并发启动均可执行；未识别的表、函数、类型或迁移版本会使启动失败。不会清空数据库或接受旧 `_sqlx_migrations`。

Control 初始化后、Telemetry 启动前，按 [遥测授权说明](telemetry.md#传输故障与权限) 为独立业务授权角色执行 `deploy/telemetry-business-readonly.sql`。该角色只读取 `nodes`、`attempts` 和 `__toasty_migrations`，不执行迁移；数据库版本不符时仍拒绝启动。

迁移前检查各服务实际注入的 DATABASE_URL。新旧数据库名必须不同，新系统的对象存储使用新 bucket。不要复用旧 PVC 或 Compose volume。使用独立目录保存新旧配置，权限为目录 0700、文件 0600，不把含密码的 URL 放入终端历史或日志。

Control 支持只执行迁移：

```sh
artifactory-control-plane migrate
```

使用新环境的私有配置运行该命令。Runtime、Source 和 Telemetry 通过各自服务启动完成初始化，只启动该部署模式启用的服务。随后在隔离入口下启动 Control，利用新 bootstrap 配置初始化管理员。首次验收前重新配置项目、凭据、模型配置、节点池和规划授权。

## 验证

```sh
python3 scripts/check_data_access.py --dependencies
python3 -m unittest discover -s scripts -p 'test_*.py'
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets -- --include-ignored \
  --skip live_emissary_protocol --skip native_service_lifecycle --skip device_fleet_load
```

数据库测试显式配置隔离的 `ARTIFACTORY_TEST_DATABASE_URL` 和测试 S3。真实协议、浏览器、Compose、Operator 及恢复测试的命令沿用 `docs/acceptance.md`、`docs/operator-operations.md` 和 CI。不要把本地数据库测试当作实际服务切换验收。

遥测历史查询使用单条 SELECT 的固定 MVCC 快照，通过 Toasty 原生结果流每批解码最多 128 条，保留原有排序、6000 条及 2 MiB 样本上限。读取期间独占只读连接；完整消费后归还，截断、异常和取消时丢弃连接并关闭物理查询，避免后台继续读取完整结果。测试覆盖分页之间的并发修改、两个截断边界、取消后的连接释放及完整候选结果翻倍后的客户端内存峰值。

性能使用相同主机、数据库、样本数和计时边界交替运行迁移前后测试。保留每轮原始 p95；调度、心跳续租、事件提交、遥测写入/读取任何一项退化超过 10% 都不得开放入口。恢复演练必须包含数据库版本记录、业务数据、序列、TimescaleDB 策略/列存、对象和密钥，并在恢复后的服务上重新执行流程。

## 切换窗口

1. 在独立数据库、bucket、配置和工作目录准备新版本。提前完成所有隔离验收和新版本备份恢复；保留镜像摘要、源码提交、配置指纹和验收记录。
2. 在网关/入口关闭新构建、规划、发布、OTA 和管理写入；保留在途 Agent/规划器的心跳、日志、上传与终态提交。暂停外部自动化入口。各入口返回明确维护状态。
3. 核对 Business 中没有非终态 builds、leased/running attempts、queued/leased planning_runs、执行中的 OTA；Operator 中没有在途运行/提案/outbox，Source 没有抓取或下载，Telemetry spool 已排空。检查服务请求/上传和后台任务，不只检查一张表。未排空就推迟，不能把强制取消当作排空成功。
4. 停止旧版本的全部写服务及 Agent。保留旧数据库完整备份、旧 bucket、配置/主密钥、PKI、Agent 配置和工作目录。验证备份可以读取，记录旧服务恢复命令。旧环境存储禁止清理。
5. 一次替换所有后端到新配置；不要让旧进程连接新库。按新节点池签发一次性 enrollment token，为每个 Agent 使用新的配置文件和工作目录重新注册。先完整归档旧 Agent 身份和遥测 spool，再使用新 spool；不要将旧身份产生的批次重放到新库。
6. 保持入口关闭，依次验收管理员登录、权限/错误状态、创建构建、规划、Agent 执行、日志、产物下载、原子发布、遥测和后台恢复。确认所有启用数据库记录对应 baseline，连接角色和只读设置正确。验收负责人记录 Go/No-Go 后才开放入口。

## 整体回退

开放入口前失败：停止新服务和新 Agent，保存失败诊断；将全部后端、路由、数据库、对象存储和密钥恢复到旧配置，再恢复旧 Agent 身份/工作目录。确认旧版登录、执行和遥测通过后重新开放。不能用旧二进制连接 Toasty baseline，也不能用新二进制连接 SQLx schema。

开放入口后失败：再次停写并排空/记录在途操作；备份新库、新对象、新配置、新 Agent 身份与 spool，明确这段窗口产生的数据。整体恢复旧环境后，新环境继续封存，后续恢复这些数据需要单独评估。本版本不自动合并新旧业务记录。

以上操作面向选定部署环境执行。本地验收不会停止、重建或切换现有共享/生产环境。
