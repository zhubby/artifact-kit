# 节点元数据、MCP 与 AI 选机

Emissary 在进程启动时探测一次静态信息，控制平面持久化快照；动态资源继续进入独立 Telemetry 数据库。内置构建 Agent 通过 MCP 查询授权设备、排列候选，控制平面在分配时复查并执行。传输复用已有 mTLS gRPC 或 Runtime 通道，无新增公开端口或消息中间件。

## 启动快照

每个运行进程生成独立 `startup_id`，一次性采集系统、硬件、工具版本和加速器。首个认证 hello 携带快照；断线重连复用同一快照，不重新探测。控制平面持久化后发送 `metadata_ack`，ACK 丢失时节点在心跳周期重试。相同启动只能提交同一内容，新启动完整替换，已删除字段随之消失。持久化的启动历史拒绝退役进程重新夺取连接；旧 session 也不能提交元数据或修改当前能力。

`nodes.metadata` 为 JSONB 键值对象，配套保存 `startup_id`、`metadata_digest` 和服务端 `metadata_received_at`。摘要覆盖 metadata 和 capabilities，调度使用摘要检查 AI 决策依据是否变化。

| 命名空间 | 内容 |
| --- | --- |
| `system.*` | OS、架构、内核、系统启动时间、容器与虚拟化信息 |
| `hardware.*` | CPU、内存、有效资源上限、加速器 |
| `tools.*` | 工具是否可用及版本 |
| `emissary.*` | Emissary 版本及采集器支持情况 |
| `custom.*` | 节点配置的自定义值 |

节点 JSON 配置可增加 `"metadata": {"site": "lab", "rack": "A3"}`，上报为 `custom.site`、`custom.rack`；即使配置键使用 `system.os`，它也只会生成 `custom.system.os`。管理员 labels、签名授权和 slots 独立管理，不接受元数据覆盖。

整个编码后的启动快照最多 64 KiB。自动探测不收集环境变量值、凭证或全主机进程清单。Inventory 历史复用同一静态快照，在本进程中缓冲一次并可靠投递；不再每 15 分钟重探测，也不再周期性发送全量静态信息。普通心跳保留任务、在线、OTA 和少量运行指标。

## 资源与 MCP 查询

主机资源约每 30 秒采样，包含 `available_memory_bytes`、构建根目录的 `build_disk_available_bytes` 和 CPU 利用率。Linux 可用内存同时考虑 cgroup v1/v2 及祖先限制；缺失或无法读取的指标保持未知。

Telemetry 的兼容字段 `Batch.session_id` 对新版 Emissary 表示 `startup_id`。独立库的 `startup_resources` 按节点和启动保存最新样本，只有更新的采样时间才能替换同一启动的记录。调度与查询使用当前业务节点的启动 ID 批量读取；历史重放不能替换其他启动的资源。采样和服务端接收时间都必须在过去 90 秒以内，允许最多 30 秒的时钟超前。必需指标未知或资源过期时不满足分配条件。

Worker 使用官方 `@modelcontextprotocol/sdk` 的 client/server、`initialize`、`tools/list`、`tools/call` 会话；内存 transport 不监听网络。工具调用复用当前规划任务绑定的认证通道，Agent 无数据库连接、SQL 或通用 Shell 能力。

| 工具 | 输入与结果 |
| --- | --- |
| `summarize_nodes` | 按能力分组，汇总在线数、任务槽、活动任务和可用槽；构建上下文仅放此类摘要 |
| `query_nodes` | 按 `os`、`arch`、metadata 键值包含关系、`tools` 和 `capabilities` 筛选；返回节点 ID、metadata 摘要、最新资源和新鲜度 |
| `get_node` | 以 `node_id` 按需读取完整 metadata 和最新状态 |

查询取当前项目节点池／签名权限与构建创建时冻结范围的交集。没有授权的节点不会返回详情。分页默认 50 台、最多 100 台，以 UUID 游标继续；整个响应最多 128 KiB，超出预算时返回 `truncated` 和 `next_cursor`。资源包含 `sampled_at`、`received_at`，外层包含 `queried_at` 和 `resources_fresh`；Telemetry 不可用时资源为未知。

控制台可用只读 `GET /api/v1/nodes/{id}/metadata` 展开设备详情，需要具有 viewer 及以上角色的用户会话。列表不默认加载全量 metadata；项目 API token 不能读取全局节点信息。

## 排序、分配与有限重选

新版 AI 的 `ExecutionPlan` 按 target 保存 `placements`，例如：

```json
{
  "placements": {
    "linux": {
      "candidates": [
        {
          "node_id": "00000000-0000-0000-0000-000000000001",
          "metadata_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "reason": "原生 Linux 工具链满足构建要求且可用资源充足"
        }
      ],
      "resources": {
        "min_available_memory_bytes": 4294967296,
        "min_build_disk_available_bytes": 21474836480,
        "max_cpu_percent": 80
      }
    }
  }
}
```

每个目标需要 1–8 个不同的有序候选。分配前批量读取资源，然后在短事务中锁定构建、任务、项目权限和节点，复查节点池、平台、工具能力、签名范围、metadata 摘要、在线、排空、OTA、资源时效和剩余 slots。首选不合格则尝试下一个；没有合格候选时排队，不使用列表外设备。锁后重新检查任务槽，防止多副本超额分配。资源快照不构成内存／磁盘独占预留。

全部候选持续不可用 30 秒后可触发重选。每个构建最多 2 轮，启动间隔至少 60 秒；每轮最多 5 次模型调用、60 秒，模型配置更严的限制仍生效。轮次在调用模型前持久化，重启或多个副本不能重置预算。

重选只接受未分配目标的候选列表，原资源门槛、脚本、目标平台和签名范围保持不变。提交时校验任务版本，跳过已运行的目标，拒绝取消、过期租约或过期轮次的结果。预算用尽后继续排队，在构建详情展示原因。实际构建失败后的脚本修订仍沿用原有修订机制，与选机重选预算分开。

## 启用与升级

1. 升级 Control，追加业务迁移 `2026111402 / 0002_node_placements.sql`；旧节点和在途任务继续工作。
2. 升级 Telemetry，追加独立库迁移 `2026111403 / 0001_startup_resources.sql`。业务授权角色继续只读，无需新增写权限。
3. 升级 Runtime、Worker，随后升级 Emissary，确认新启动 metadata 和当前资源均已上报。
4. 最后设置 `ARTIFACTORY_AI_PLACEMENT=true`，Helm 对应 `controlPlane.aiPlacement: true`。

开关默认为关闭。新 AI 构建创建时固定 `planning_context.placement_version`；已有计划、旧版本 AI 构建和普通 recipe 继续使用原调度方式。更换 Worker 或开关不会改变在途构建的模式。关闭开关只影响之后创建的 AI 构建；已启用的构建仍需要支持选机的组件处理。

本次交付源码与隔离环境验收，不自动升级现有集群或本机常驻节点。数据库回归、MCP 探针、浏览器和三副本故障压测见 [验收记录](acceptance/nodes/2026-11-14.md)。
