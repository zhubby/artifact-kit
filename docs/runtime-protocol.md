# Runtime 与 Source 协议 v1

链路为控制平面固定业务意图 → Rust Source 固定源码 → Rust Runtime 准入 → Rust Operator 创建 Kubernetes Job → Pi Agent 提案 → 控制平面接纳 → Rust Artifactory Emissary 构建。设备协议为 `artifactory.emissary.v1`，源码和设备协议不提供 Go 版本兼容层。Substrate 适配属于后续阶段。

## 状态归属

| 数据 | 唯一写入方 | 存储 |
| --- | --- | --- |
| 项目权限、Build、计划接纳、设备 Job/Attempt、制品、Release | control | 业务 PostgreSQL |
| 运行队列、有效 Pod、租约、累计模型预算、deadline、checkpoint、提案投递状态 | planner-runtime | 独立 PostgreSQL |
| 固定 manifest、Git/submodule/tag/LFS 对象、凭据版本、保留引用 | source-service | 独立 PostgreSQL、分片 PVC、S3 |
| AgentPool/SourcePool 配置 | 管理员 / Helm | Kubernetes API |
| AgentRun/SourceSnapshot 意图 | Operator dispatcher | Kubernetes API，只有已准入运行 |
| CRD status、临时 Job、Source StatefulSet/Service | Operator leader | Kubernetes API |

数据库、加密密钥分别配置；服务不能跨库查询。Source 的长下载锁使用 Source 连接池；不占用业务事务。Pi 不获得服务令牌、模型供应商密钥或 Kubernetes 通用 API 身份。

## 服务接口

`proto/artifactory/runtime/v1/runtime.proto` 定义 Tonic/Prost 0.14 的 `RuntimeGateway` RPC。首期消息使用版本化 JSON envelope，内部 DTO 在 `crates/runtime-protocol`，未知 DTO 字段拒绝。新增不兼容语义需要新协议版本。Operator 使用 TLS gRPC 的 GetRun/RegisterExecution；其他服务使用同一语义的 HTTPS JSON。

| Runtime 路径 | 调用方 / 身份 | 作用 |
| --- | --- | --- |
| `/v1/intent` | control / 服务令牌 | 不可变运行意图，或高版本 cancel tombstone |
| `/v1/queue` | Operator / 服务令牌 | pool/project 原子准入，重返所有有效 descriptor 以恢复未知创建结果 |
| `/v1/get`、`/v1/register` | Operator / 服务令牌 | 读取生命周期、登记真实 Pod UID |
| `/v1/acquire` | Pi / Kubernetes 投射令牌 | TokenReview audience `artifactory-runtime`，核验命名空间、SA、Pod 名及 UID |
| `/v1/heartbeat`、`/v1/report` | Pi / run grant | 约 10 秒心跳、事件 UUID/sequence 检查点和 usage |
| `/model/{run}/{generation}/{uid}/v1/...` | Pi / run grant | 唯一预算预留、固定模型与上游、透明 SSE 转发 |
| `/v1/tool`、`/v1/proposal` | Pi / run grant | 保存提案、查询同一提案结果；节点能力/失败日志按需请求业务服务 |
| `/v1/validate` | Source / 服务令牌 | 核验 run grant 并返回项目及固定 snapshot |
| `/metrics` | 运维 / 服务令牌 | Prometheus 文本；无运行 ID 标签 |

Source 的 `/v1/capture` 以 operation UUID 和输入摘要幂等冻结源码，`/v1/metadata` 只返回固定 manifest/digest；`/v1/inspect`、`/v1/evidence` 校验同一快照，`/v1/refs` 提供只读引用查询，`/v1/retain` 添加保留引用。以上端点使用服务令牌。Pi 的 `/v1/tool` 使用 run grant，直接读取 Source。设备通过 `/api/v1/source/{snapshot}/{entry}/git/...` 与 `/lfs/...` 下载，兼容现有 Basic/askpass 及 Bearer 临时凭证。

Source capture 返回的 `origin` 是设备下载基址；control 在 `source_snapshots.public_origin` 固定它，另存 `service_origin` 用于 Pi、证据核验和元数据 RPC。设备 Git/LFS descriptor 使用公开地址，LFS batch 中的下载 href 使用同一公开地址。地址不允许用户密码、query 或 fragment，生产要求 HTTPS。`0011_source_public_origin.sql` 是追加迁移；旧快照没有公开地址时沿用原 service origin，本地快照继续使用 control public URL。修改下载网关后必须保留历史地址或显式迁移对应元数据，不能改变源码 digest/manifest。

control 的 `/internal/runtime/event` 幂等接纳阶段结果，`/internal/runtime/tool` 提供设备能力/失败日志，`/internal/runtime/source-authorize` 仅短事务核验设备 attempt 权限。内部端点同样要求服务令牌，禁止从公网直接暴露。

服务默认 HTTPS，证书由 `ARTIFACTORY_SERVICE_TLS_CERT/KEY` 指定，客户端可用 `ARTIFACTORY_SERVICE_CA` 添加私有 CA。只有显式 `ARTIFACTORY_INSECURE_DEV=true` 可使用 HTTP。服务客户端禁重定向，JSON 请求/响应默认上限 2 MiB；源码 Git/LFS 数据流不受 JSON 限制。错误响应不返回内部凭据或上游认证信息。

## 幂等、fencing 与预算

- Build 创建时固定 `planner_backend`；planning_run 在同一数据库事务的 trigger 中固定 authority 并写 runtime_intents outbox。投递前先持久化加密 payload，再重投相同内容。取消为 version 2；先收到取消后收到创建不能复活，重复取消幂等。
- Execution 身份为 `(run UUID, generation, Pod UID, namespace, Pod name)`。Kubernetes 决定 Pod 放在哪个节点；Runtime 决定哪个身份持有有效授权。确定性 Job 名与 Owner UID 处理创建结果未知；即使短暂出现两个 Pod，也只能登记一个有效 UID。
- 默认预算 20 次调用、600 秒逻辑期限、90 秒租约。首次有效 acquire 才启动 deadline。排队不消耗预算；镜像 provisioning 最长 180 秒/次，有界 generation 重试。重试延续原 deadline/calls/checkpoint，不迁移到新的源码或模型配置。
- 模型请求必须有 `x-artifactory-request-id` UUID。Runtime 在发送前原子保存次数预留与 profile RPM 时隙。重放同 ID 返回 409，不再次发送；响应未知仍消耗次数。Pi SDK 禁应用层重试，HTTP 客户端显式禁用协议级自动重试，强制 SSE。支持 OpenAI Completions、Responses、Anthropic Messages；供应商密钥只在 Runtime 解密使用。
- 流式响应期间周期复查授权，取消/租约丢失终止转发。物理 Pod 停止与授权撤销是不同时间，不能把取消 API 成功解释为进程已停止。CNI 必须落实 Pi NetworkPolicy，阻止绕过代理直连供应商。
- report 使用事件 UUID 与递增 sequence；同 ID 不同内容拒绝，乱序事件不覆盖新状态。Pod 重启后沿用已持久化 checkpoint。

## 提案与接纳

Pi 在提交工具前保存含 pending_tool 的检查点。Runtime 原子登记 proposal、原 generation/deadline、固定 snapshot digest 和 outbox；pending 阻止新推理及物理重试。接纳确认丢失只查询或投递原 proposal，不重新调用模型。

control 在持业务行锁之前完成远程证据校验，再以 Build → run 的锁顺序检查取消、期限、修订、平台和签名约束，并使用原计划验证/创建 Job 逻辑。接纳结果与 receipt 同一事务提交；重投返回原 revision，即使之后取消或 deadline 到期。非法计划返回工具错误，Pi 可在原预算内修正。真正超期且未接纳的提案不能新建任务。

源码持久包 Ready 前写入 Git bundle、LFS 对象及 manifest；源码操作保存首次解析的 commit/tag。PVC 丢失从 S3 恢复，再核验 Git 对象和 LFS SHA-256。SourceSnapshot CRD 删除不删除持久对象；首期没有破坏性 GC，保留引用不可用时保守保留。

## 兼容与迁移

默认 `poll`，Poll Worker 与 Kubernetes Pi 两种规划后端均支持 Rust Emissary。新 `kubernetes` run 不由 Poll/reaper 领取；高频心跳、预留和 checkpoint 不写业务 planning hot tables。相同模型 profile 在有活动 run 时禁止跨 authority 交接，防止 Poll Report 与 Runtime 代理形成双重预留。先排空该 profile，或使用独立 profile 做灰度。不能直接修改在途 Build/run 的 backend、authority 或计数实现回退。

运维及验收方式见 [Operator 运维](operator-operations.md)。
