# Kubernetes Pi Operator 运维

Operator、Runtime、Source 与 Artifactory Emissary 均由 Rust 编写；Pi Agent 复用 Node 24 / Pi SDK 1.5.2。设备部署 Emissary，主动连接业务控制平面。Kubernetes 中的 Pi 只读分析与规划，构建/签名在设备 Emissary 上执行。

设备端 Rust 替换与当前链路证据见 [Emissary 验收](acceptance/emissary/2026-11-13.md)；原 Go 验收属于历史记录。

## 版本与安装

首期锁定 Rust 1.95.0、kube-rs 5.0.0、k8s-openapi 0.29.0 `v1_37`、Tonic/Prost 0.14。Cargo.lock 为依赖依据；CRD group `agents.artifactory.dev/v1alpha1`。实际集群及平台证据见 [验收记录](acceptance/operator/2026-11-12.md)，其他 Kubernetes minor 需要单独验证。

1. 备份业务数据库、原主密钥和 PKI。为 Runtime/Source 分别配置数据库/角色、32 字节 base64 加密 key；配置 S3、Service TLS、服务间随机 token。数据库与 S3 需独立容量及备份策略，chart 不创建生产数据库。
2. 审查并独立应用 `operator/config/crd/resources.yaml`。Helm CRD 目录用于首次安装，Helm 不自动升级或删除已有 CRD。CRD 兼容性变化需要单独审查。
3. 在新命名空间创建 `service-tls` Secret，含 `tls.crt/tls.key/ca.crt`。SAN 应覆盖 Runtime HTTP/gRPC 地址及每个 Source shard FQDN。Pi 和 Operator 只挂载 CA；服务器挂载证书及私钥。不要授予 Pi Secret 读取权限。
4. 创建 Runtime Secret：`DATABASE_URL`、`ARTIFACTORY_SERVICE_KEY`、`ARTIFACTORY_SERVICE_TOKEN`。创建 Source Secret：上述三个变量，以及 `ARTIFACTORY_S3_ENDPOINT/BUCKET/ACCESS_KEY/SECRET_KEY`；私有 Git HTTPS CA 可用 `GIT_SSL_CAINFO=/run/service-tls/ca.crt`，独立 Git CA 需自定义挂载。S3 bucket 首先由业务部署初始化，Source 不使用业务主密钥。
5. 构建/发布 `operator/Dockerfile` 共享四个 Rust binary 的镜像，以及 `packages/agent-worker/Dockerfile`。生产 values 用固定镜像 tag/digest 对应的发布记录；选择与节点一致的 arm64/amd64 架构。

```sh
kubectl apply -f operator/config/crd/resources.yaml
helm upgrade --install native charts/artifactory-operator -n agent-runtime --create-namespace -f private-values.yaml
kubectl -n agent-runtime get agentpools,sourcepools,deploy,statefulsets,pvc
```

values 至少配置 `image.repository/tag`、`runtime.secretName/url/grpcUrl`、`controlUrl`、`tls.secretName`、`agentPool.image`、`sourcePool.secretName/storageClass/storageSize/shards`。`controlUrl` 必须是可访问内部 API 的 HTTPS 网关；control-plane 自身默认 HTTP 8080，需要 TLS 终结。每个命名空间部署一套服务/Pool，Operator 两副本共享一个 Lease。

主 chart 配置 `planning.runtimeUrl`、`planning.sourceUrls`（逗号分隔每个 shard 的 HTTPS FQDN）、`planning.agentPool`、`planning.serviceSecretName`、`planning.serviceCaSecretName`。服务 token 必须一致；业务 DB、Runtime DB、Source DB 的密钥必须分别保管。开启项目 `planner_backend: kubernetes` 后，新 Build 使用该路径；默认仍为 poll。

集群外设备需要可访问的下载网关。设置 `sourcePool.publicUrlTemplate: 'https://{pod}.source.example.com'`，可用变量为 `{pod}`、`{pool}`、`{namespace}`；留空只适用于能访问集群内部 DNS 的设备。Operator 把每个 Pod 的公开地址传给 Source，control-plane 保存该地址用于设备 Git/LFS，Pi 和内部 RPC 仍使用 `planning.sourceUrls`。下载 HTTPS 证书需被设备信任，私有 CA 配入设备 `git_ca_file`。设备还需能访问 control-plane gRPC 和制品对象存储的 advertised endpoint。

边缘网关应按 shard 定向，仅公开 `/api/v1/source/` 下载路径，保留 Authorization。可为每个 ordinal 创建 Service，selector 同时包含 `artifactory.dev/source-pool: sources` 与 `statefulset.kubernetes.io/pod-name: sources-0`，8090 指向对应 Pod；Ingress/Gateway 的 `sources-0.source.example.com` 绑定这个 Service，后端 HTTPS 并核验证书。多 shard 不能将这些域名随机负载均衡到整个 headless Service。网关和 DNS 由部署环境管理，chart 不假设具体 Ingress controller。变更模板后保留旧下载域名以支持在途和历史快照；禁止通过业务控制进程转发源码字节。

## 存储与扩容

SourcePool 由 Operator 创建 headless Service 和 StatefulSet；每个 replica 使用独立 `ReadWriteOnce` PVC。已有项目在 source_project_routes 固定 origin，增加 shard 不自动重映射旧项目。新增 shard 时同步 TLS SAN 和控制平面 sourceUrls；缩容或更换 PVC 参数需要显式迁移，CRD 会拒绝直接缩容/变更 PVC 模板。

删除 Operator release 或 SourcePool 不级联删除 Source StatefulSet/Service/PVC，PVC retention 为 Retain。Job 清理按 UID 显式级联删除 Pod；终态 AgentRun 在清理完成且创建超过 24 小时后回收，尚有 `CleanupPending` 或 Pod UID 的资源继续保留。dispatcher 标记的 SourceSnapshot 元数据在没有 AgentRun 引用且超过 24 小时后清理，用户声明的 SourceSnapshot 保留。元数据删除不删除持久快照、DB、S3 或 PVC。首期 Source 无自动破坏性 GC，要监控磁盘/对象容量并按备份策略保留。

Runtime 支持多个副本，数据库行锁/advisory lock 保护准入、预算与结果；Operator 通过 Kubernetes Lease CAS 选主，失去新鲜 Lease 即停止变更。业务 control-plane 可按 [Control 多副本方案](control-ha.md) 部署，设备命令使用业务 PostgreSQL Outbox；不能将 Runtime 的排队容量当作设备连接容量。

## 观测与诊断

Runtime / Source 的 `/metrics` 使用服务 Bearer token，采集服务请求数/错误数/在途数/响应字节/请求累计耗时。Runtime 额外输出各状态运行数、调用预留、pending proposal、outbox backlog 与最长排队时间；Source 输出持久快照及保留引用。数据面字节包含 Git/LFS，运行 ID 不作为标签。请求耗时为累计量，P95 应从调用侧/网关 histogram 获取。CPU/RSS、网络、PG 连接/锁等待和 Kubernetes API 写入速率使用 cAdvisor/Prometheus、PostgreSQL 与 apiserver 指标采集，不能从这些计数器推断。

AgentPool status 显示准入/排队数，AgentRun 显示 Pod UID、等待原因/非零退出码。先检查 `kubectl describe pod` 的镜像、CSI、调度与事件，再看 Operator/Runtime/Source 日志。`provisioning` 不是有效执行，等待镜像不启动 90 秒租约或规划预算。无法访问 Source/业务权威时，pending outbox 保持待确认；不应通过新建 run“解卡”。

各类故障应分别记录授权撤销和物理停止时间。AgentRun phase 表示 Runtime 的逻辑状态；取消后，Runtime 拒绝模型/工具请求，Operator 按 UID 删除 Job。Job 删除确认后仍检查其 Pod，保留 `message: CleanupPending: ...` 和 Pod UID，每 5 秒重试；Kubernetes API 读取失败不会清空身份，Job/Pod 都消失后才清除这些字段。节点失联时，API 资源消失仍不能单独证明远端进程已停止，应结合节点恢复或隔离措施确认。运行日志不得记录 provider key、run grant、源码临时 token 或完整服务 Secret。

模型响应头前失败时，Runtime 记录调用预留耗时、剩余期限、上游耗时、请求字节数、超时/连接分类及可用的系统 I/O 错误码。请求剩余 600 秒但提前超时不一定是规划期限：Linux HTTP 客户端还有 TCP 确认超时，需要结合服务端连接状态、网络策略与数据面丢包证据定位。未知上游结果仍占用原预留，不应手工减少 calls 或重置 deadline。

## 灰度与回退

1. 使用新命名空间及独立 profile 验证 fixture 链路、故障、源码恢复和现有设备协议；再使用真实供应商和目标 OS 验证。
2. 项目 API 的 `planner_backend` 切到 kubernetes，仅影响新 Build。Build/run 插入时固定 backend/authority；不要更新在途记录。共享 profile 切换前排空所有 queued/leased run，避免预算 authority 冲突。
3. 停止新准入可设 AgentPool `maxActive: 0`，已运行的继续收尾；需要停止任务时通过业务取消 API，等待撤销及 Pod 物理停止。将项目改回 poll，只改变未来 Build。
4. 回退前核对 run UUID/generation、snapshot digest、deadline、calls、checkpoint、proposal/receipt、原 revision/jobs。保留新 migration、DB/S3 数据与服务，直到在途新 authority 运行清空；不回滚 schema，不重置预算，不复制在途 run 到 Poll。
5. 恢复分别备份的 Runtime/Source DB、加密 key、S3 对象及项目路由。旧 Release/source 元数据仍需保留；PVC 可重建但缺失持久对象必须明确失败。

Substrate 接入属于后续阶段。设备运行程序已经统一为 Rust Emissary；回退规划后端不会切换设备实现。

## 隔离验收与容量边界

```sh
python3 tests/operator/acceptance/prepare.py --platform linux/arm64
python3 -u tests/operator/acceptance/run.py --context kind-substrate-orbstack
```

prepare 构建三类镜像并推入本地 registry；可 `--kind <name>` 加载到临时 kind。run 创建全新 `artifact-operator-*` 命名空间和随机 CA/密钥、三个独立数据库、S3、真实 Pi 推理循环与 Rust Emissary 构建 Pod。模型为确定性 SSE fixture，包含一次非法计划与修正。保留命名空间供检查；`.local/operator-acceptance/<namespace>` 含秘密，不能整目录上传或公开。清理时仅删除本次命名空间、其专属 TokenReview ClusterRole/Binding；共享 CRD 不删除。现有 release 不受此脚本操作。

隔离集群应使用支持 NetworkPolicy 的 CNI；仓库提供 `tests/operator/acceptance/kind-cilium.yaml`，CI 固定 Cilium 1.20.4。先安装 CNI 并等待 Node Ready，再运行 harness。`network.py --context <context> --namespace <saved-namespace> --controller 'Cilium 1.20.4'` 从三个全新 Pod 的第一次 TCP 请求检查 Runtime/Source 可达、control-plane/provider/PG/S3 被阻断，同时使用不受限 Pod 做正向对照。仅后台安装 policy controller 并等待规则收敛不足以证明启动期间隔离。

`native.py` 在宿主启动独立 Rust Emissary 进程，用临时 localhost 下载网关验证集群外 Git/LFS 和制品上传，结束后撤销身份、读回撤销状态并恢复路由，不安装系统服务。`recovery.py` 停止准入和服务，备份三库、S3、服务密钥和路由，再恢复到每次唯一的全新数据 Pod；等待 PostgreSQL 认证 SQL 和 S3 端口就绪后复制数据，校验哈希后提升 Service，并从无缓存/上游不可达状态恢复源码，最后重跑业务链路。旧数据 Pod 保留供回退。该脚本只支持明确的专用验收 context 与本机保存 PKI 对应的 namespace。私有备份目录不能整体上传；生产仍需自己的备份存储、密钥托管与 RPO/RTO 演练。

`cleanup.py --context <dedicated-kind-context> --namespace <saved-namespace> --node-container <kind-control-plane>` 验证节点无法清理时的取消状态。它核对 Docker kind 归属、单节点、命名空间 PKI 和无其他业务负载，暂停一个真实 Pi 并停止 kubelet，通过独立 Docker/API 通路取消，检查撤权、进程仍存在时保留 `CleanupPending`，最后恢复进程与 kubelet 并等待 Pod 清理。此测试模拟 kubelet 不可达，不等同于完整网络分区；只用于本次 harness 创建的专用 kind。

传入 `--real-model-env /private/path/env.json` 可在 fixture 后使用显式选择的本机开发加密 profile 验证真实供应商；凭据仅在内存中读取/解密，不写诊断或命令参数。`--resume --namespace <this-harness-namespace>` 仅恢复本机保存 PKI 与集群 CA 一致的测试环境。harness 最后会卸载并重装自己的独立 release，验证 Source StatefulSet/Service/PVC 保留；不应对已有业务 release 使用该测试脚本。

本轮按用户确认以流程验证收口，500/1,000 活动实例作为后续容量验证。PostgreSQL 测试有 10,000 queued/8 admitted/每项目 2 的并发准入场景；这证明队列有界，不证明 1,000 活动 Pod 或生产 API P95。100/500/1,000 活动档位应按真实集群容量逐级压测，记录数据集、模型/源码负载、时间窗、API P95、PG 等待、CPU/RSS/网络、Pod/CRD 数量及故障恢复；没有测得的数据不填“通过”。
