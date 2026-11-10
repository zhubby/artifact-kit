# Agent 协议与状态机

公共契约位于 `proto/artifactkit/agent/v1/agent.proto`。目前协议主版本为 1；不兼容的主版本在注册和 hello 时拒绝。Go 生成代码提交在 `agent/gen`，修改协议后运行 `scripts/generate-proto.sh`；Rust 在 Cargo 构建时生成服务端。

## 连接与身份

1. 管理员为指定节点池创建 15 分钟、一次性注册令牌。
2. Agent 本地产生 ECDSA P-256 私钥与 CSR，通过钉住 CA 的 TLS 调用 `Enrollment.Enroll`。
3. 中心验证 CSR，签发 30 天节点证书；仅保存证书摘要与到期时间。节点私钥不离开本机。
4. 所有 `Control` RPC 校验客户端证书链和数据库登记的证书摘要，并检查吊销 / 到期状态。TLS 层允许无证书连接只为注册；这不意味着 Control 可匿名调用。
5. `Connect` 为双向流。hello 绑定节点 ID、协议版本、能力及诊断。新连接会替代旧 session；旧 session 不能继续改写任务。
6. 每 10 秒心跳。35 秒未收到消息关闭连接；是否重派由 90 秒持久化任务租约决定，而不是简单的 online 标志。

节点证书到期前需要排空、吊销并以新配置重新注册；本版没有自动续证或自动升级。吊销不可反向恢复，防止旧私钥重新有效。

## 两阶段开始与租约

```text
job: queued → leased → running → succeeded / failed / canceled / lost
                │          │
                └─租约过期─┘
           未开始: queued   已开始: lost

center: persist leased → assign
agent:  assign 去重 → accept
center: persist running → start grant
worker: 收到 start grant 后才能执行
```

任务身份为 `attempt_id + lease_token`。命令有独立 `command_id`，续租 / start grant 的 `request_id` 回显 Agent 请求编号。Agent 记录请求发出时的单调时间，以该锚点计算本地截止期限，网络延迟不能延长服务端租约。

执行开始前过期，旧 attempt 记为 lost、job 可重新排队；执行开始后过期，job 为 lost，必须显式重试生成新 attempt。中心重启保留 PostgreSQL 内的在途租约，不立即重派。心跳只续有效且未取消任务的租约，不能复活旧 attempt。

任务事件等待 attempt / 节点行锁后，在同一事务连接上重新检查当前租约与构建取消标记。等待期间已提交的取消阻止新的 start 授权；finished 也按当前取消状态登记为 canceled。

Agent 在租约内断线保持 Worker，重连后补报；无续租则本地终止进程树。父 Agent 退出 / 崩溃关闭 Worker 控制管道，Worker 终止正在运行的命令。Unix 使用进程组，Windows 在恢复挂起的初始进程前分配 Job Object，关闭 Job 时杀死所有后代。

`started` 的保守定义是中心已提交 running 并准备发送 start；即使随后响应丢失，也不假定副作用未发生，因此不自动重跑。

## 事件与日志

- `accept` 可重放；同 attempt 不重复创建 Worker。运行事件从 1 单调递增，重复事件只返回已提交游标，不重复写状态；跳号、终态后的新事件和过期租约均拒绝。
- 日志使用独立 gRPC 连接及 `UploadLogs` 流，不与心跳串行传输。每批最多 64 块，每块不超过 64 KiB；按 attempt 维护连续序号。相同序号不同摘要拒绝。
- S3 日志写入发生在租约行锁之外，对象 key 绑定内容摘要；短事务在网络完成后重新检查有效租约、序号与摘要，再登记日志索引。存储等待不会阻塞同一 attempt 的心跳续租，冲突上传不会覆盖已接受内容；过期或冲突对象不进入有效日志索引，可能留下私有孤立对象，由人工保留策略处理。
- Agent 默认磁盘日志预算 256 MiB；满额或写失败会中止并明确报告失败，不静默丢弃。旧日志留存在本机，计入下次启动的总预算。
- finished 通常等到日志得到确认才上报。最终日志阻塞 120 秒会改为显式失败，未确认日志保留本机。取消后的剩余日志在原有效租约内仍可上传，制品则不再接受。
- 浏览器 SSE 以数据库日志 ID 作为恢复游标；55 秒轮换连接并重新鉴权，`Last-Event-ID` 支持续传。返回 `no-transform`，Next 关闭内部压缩，反向代理须禁用 SSE 缓冲。

## 制品传输

`PrepareArtifact` 声明相对路径、大小和 SHA-256，获得 15 分钟 HTTPS staging PUT URL。`CompleteArtifact` 获取对象，验证完整内容，再写入从不签发 PUT URL 的 final key；成功后登记制品。上传声明和制品登记各自在事务提交前获得 attempt / 节点共享锁，并重新检查当前租约、节点证书期限、吊销和构建取消状态；锁等待期间失效则回滚本次记录。完成接口在 S3 校验结束后才取得这些锁，保留独立心跳续租。重复完成返回相同制品，仍需当前授权有效。下载经 HTTP 项目权限校验，签发 60 秒 GET URL，不能匿名访问桶。

对象传输不使用双向控制流。未来容器执行器可以复用 attempt / lease / log / artifact 契约，但不改变制品为文件还是 OCI 镜像的独立建模。
