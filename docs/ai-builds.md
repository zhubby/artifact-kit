# 零侵入 AI 构建

新构建只需仓库引用、构建目标和可选的平台约束。原仓库只读接入，平台不添加 `artifactory.yaml`、`.pi` 或 CI 文件，不创建分支、提交、标签或推送。构建说明、分析记录、执行计划及外置脚本保存在 artifactory。历史 YAML 任务保留原快照及重试方式。

## 对话式发起构建

首页、项目详情及构建列表的“发起构建”统一进入 `/builds/new`。从项目详情进入时通过 `?project=<id>` 带入项目；只有一个项目时也自动带入。引导按项目、源码、目标、平台、确认逐步收集信息，已回答内容和配置摘要始终可回看；修改项目后需要重新确认源码，其他目标与平台选择保留。

- 源码可搜索真实分支、标签，或显式切换为手动输入引用与 commit。分支、标签提交完整引用以区分同名条目。查询失败提供刷新与手动输入，不自动使用 `HEAD`，也不猜测默认分支。
- 目标支持常用模板和自由描述。平台默认由 AI 根据源码及授权节点能力选择，也可手动多选；无需在引导中选择模型、凭据或具体节点。
- 默认只构建。最终确认页可开启内部 Release 并填写版本，或展开高级参数 JSON；参数名称与字符串值遵循已有执行参数约束。
- 只有最终确认提交才调用创建构建接口。引导本身不调用模型，不拉取源码；提交后仍执行原有源码固定、AI 规划、澄清、执行及发布流程。列表中的 OID 是预览，不是提前固定的源码快照。
- 草稿只在页面内存中保留；刷新、取消或离开后不恢复。语言、主题及动效切换不清空草稿。提交失败保留答案，提交期间禁止重复操作，不自动重发请求。

`GET /api/v1/projects/{id}/refs` 使用项目读取权限与该项目已配置的仓库凭据，通过只读 `git ls-remote --symref` 返回：

```json
{"default_ref":"refs/heads/main","branches":[{"name":"main","ref":"refs/heads/main","oid":"<object-id>"}],"tags":[]}
```

默认分支无法识别或尚不存在时 `default_ref` 为 `null`。注释标签的 `^{}` 条目不重复展示，标签 `oid` 是标签本身的对象 ID。查询最多 30 秒、输出最多 4 MiB；超限明确失败，不返回截断列表。未知项目返回 404，无读取权限返回 403，上游或格式错误返回 502，超时返回 504，错误使用脱敏的 `{ "error": "..." }` 响应。该接口不创建构建或源码快照，不修改仓库引用。

引用查询复用项目凭据和主机信任，但不继承控制端用户的 askpass、SSH agent、默认身份或 `.netrc` 凭据。未配置仓库凭据时仅允许匿名访问；私有仓库查询失败时由用户刷新或明确选择手动输入，不尝试其他身份。

## 配置与运行

以下为默认 Poll Worker。可选 Kubernetes 路径由 [Rust Operator](operator-operations.md) 创建 Pi Job，Runtime 管理模型代理/预算/检查点，Source 处理源码与证据；Pi 只有 run grant，供应商密钥留在 Runtime。项目 API 可设置 `planner_backend: kubernetes`，默认项目使用 poll；在途 Build 的归属固定。两条路径均由控制平面校验计划、向 Rust Emissary 派发设备构建。详见 [运行协议](runtime-protocol.md)。

1. 在“访问与安全 → AI 规划”新增模型配置。支持 OpenAI Chat Completions、Responses、Anthropic Messages，填写真实接口地址、模型及密钥，选择平台默认配置。项目可覆盖默认配置，并保存平台侧构建说明和子模块凭据关联。
2. 启动独立的 `artifactory-ai-worker`。它使用 Node.js **24**，`@earendil-works/pi-agent-core` 和 `@earendil-works/pi-ai` 固定为 **1.5.2**。Rust Emissary 不接收模型密钥。
3. 在同一管理页面创建一次性 worker 注册令牌，通过标准输入完成注册。令牌有效 15 分钟。worker 使用独立 mTLS 身份，管理员可排空或不可逆吊销；证书过期前排空并重新注册。

本地安装示例（不要覆盖已有私有配置）：

```sh
cd ai-worker
npm ci
npm run build
cd ..
export ARTIFACTORY_NODE=/absolute/path/to/node24
export ARTIFACTORY_AI_ENDPOINT=localhost:8443
export ARTIFACTORY_AI_CA_FILE="$PWD/.local/pki/ca.pem"
sh scripts/ai-worker.sh init
sh scripts/ai-worker.sh enroll
# 将一次性令牌输入标准输入，以 EOF 结束
sh scripts/ai-worker.sh run
```

worker 配置包含 `endpoint`、`ca_file`、`certificate_file`、`key_file`、`name` 和注册后的 `worker_id`；可选 `server_name` 用于连接内部地址时校验服务器证书 SAN。相对文件路径以配置文件目录为基准。配置和私钥应为 0600，父目录为 0700。

Compose 包含 worker 服务，首次启动等待注册：`docker compose --env-file .deploy/env exec -T ai-worker node dist/main.js enroll`，通过标准输入传令牌。Helm 使用独立身份 PVC；可执行 `kubectl exec -i deployment/artifactory-ai-worker -- node dist/main.js enroll`。CA 挂载仅含公钥，worker 不获得控制中心 CA 私钥。注册后正在等待的进程会读取身份并开始轮询。

模型密钥由现有主密钥加密落库，普通查询只返回掩码。平台通过 mTLS 将选定模型配置交给受信 worker，每个规划任务启动独立子进程，密钥经 IPC 进入子进程，不出现在参数、源码目录或会话记录中。模型端点由管理员管理；worker 必须能访问模型端点。

DashScope 可选择 OpenAI Chat Completions，地址为 `https://dashscope.aliyuncs.com/compatible-mode/v1`，模型标识为 `qwen3.8-max`。Qwen3 系列通过 `enable_thinking` 传递推理开关，并使用该接口支持的 `system` 消息角色。`thinking_level: off` 会明确关闭思考模式，避免仅生成思考内容而耗尽输出预算；真实调用仍受所选模型的账户额度约束。

## 源码与缓存

下述控制中心缓存规则适用于旧 Poll 路径。Kubernetes 路径的 Git/LFS、引用查询和源码工具转到独立 Rust Source；每个分片使用自己的 PVC，固定 bundle/LFS/manifest 在 Ready 前写入 S3。设备 descriptor 直接指向 Source，不经过业务控制服务传输源码。原始缓存指标 API 不包含 Source 独立库，应采集 Source `/metrics` 和数据库下载计数。新路径首期保守保留对象，不执行旧 LRU 的破坏性 GC。

控制中心按项目、仓库地址、凭据版本隔离持久 bare Git 缓存。相同请求合并拉取，后续增量更新；每次构建固定 commit、相关标签、递归子模块和 LFS OID。相对及嵌套子模块由平台解析，跨凭据仓库可在项目设置中绑定凭据。缺少凭据或对象时解析失败，不能继续使用不完整源码。

Emissary 通过平台 HTTPS 的只读 Git Smart HTTP 和 LFS 获取对象。临时凭证绑定有效 attempt，取消、租约到期、节点吊销后不能开始新的下载；无 Git 写接口。生产网关将 `/api/*` 直接转给控制服务，源码不经过 Next 请求体缓冲。开发时也应让 Emissary 使用实际可达的控制服务 URL。

Emissary 的 Git/LFS 缓存优先命中已有对象，每次 attempt 创建独立工作目录。工作目录与持久缓存没有可写硬链接或共享 LFS 目录。子模块及 LFS 下载地址通过任务专属 Git 配置覆盖，`.gitmodules` 和 `.lfsconfig` 保持原样。每步检查 tracked 内容、文件模式、固定提交及子模块状态，变化即以 `source_modified` 终止。允许普通未跟踪生成文件。

控制中心 `ARTIFACTORY_SOURCE_CACHE_BYTES` 和 Emissary 配置 `source_cache_bytes` 默认各为 20 GiB。平台按仓库 LRU 回收，保护非终态构建和活动下载，保留快照元数据。历史重试只恢复原 commit/tag/OID；上游删除对象且缓存已回收时明确失败。npm、Cargo、Maven 等依赖的离线镜像不在本期范围。

部分 Git 服务拒绝直接下载未公开的旧对象 ID。恢复缓存时，平台会尝试从当前可访问分支和标签的历史中找回对象，再逐一核对原 commit、标签对象及 LFS OID。引用移动不会替换快照；旧标签对象已无法通过上游协议取得时，恢复明确失败。需要长期保留可重试能力的仓库应保留相应上游对象，或为平台缓存配置足够保留空间。

管理员 `GET /api/v1/source-metrics` 提供缓存大小、平台缓存命中、Emissary 命中/未命中及实际下发 Git/LFS 字节。`fetched_bytes` 是拉取后 Git 缓存的磁盘增长量，**不是上游线路流量**；准确上游传输量需由 Git/LFS 服务或网络测量。引用检查可以产生元数据请求，已有对象无需重复传输。

## 规划、执行与修订

```text
resolving → planning → queued → running → succeeded
               ↓                    ↓
          needs_input          replanning → queued
                                    ↓
                                  failed
```

Pi 工具仅能列目录、读文件、搜索、读取 Git 元数据、节点能力和失败日志、提交计划或请求补充。不提供编辑或通用 Shell，不加载仓库 Pi 配置、插件或钩子。试运行统一在 Rust Emissary 执行。每轮默认 10 分钟、20 次模型调用，管理员可调整；可通过 `requests_per_minute` 限制调用速率。服务器持久化调用预算、截止时间、租约和上下文，worker 重启不能重置预算。

`ExecutionPlan v1` 包含摘要、实际文件依据、外置脚本和经过现有 Recipe 验证的目标/步骤/制品规则。平台固定源码快照、模型配置版本及计划版本；attempt 引用自己的计划，历史版本不可覆盖。脚本位于 `ARTIFACTORY_SCRIPT_DIR`，命令工作目录为 checkout，`ARTIFACTORY_WORK_DIR` 为其父 attempt 目录。

有效计划自动执行。执行失败最多自动修订两次，共三轮；只修订失败目标，成功目标的执行与制品保留。修订不能改变 commit、目标集合、平台或扩大签名/节点池权限。源码凭据、模型认证及权限错误直接失败。所有阶段都可取消，提交计划与取消通过服务端事务排序，过期结果不能排队。

人工重试复用原提交及该目标原计划；改变目标需新建构建。发布必须由有 release 权限的人显式请求，只收集每个成功目标最终有效 attempt 的制品。失败/旧轮次产物不能混入发布。

## 验收

`scripts/ai_acceptance.py --full` 使用真实平台模型，覆盖无平台文件的 Git/标签/嵌套子模块/LFS、外置脚本、签名、制品、澄清、修订和预算。密钥只从调用进程环境读取，不写入脚本或命令参数。证据位于私有 `.local/ai-acceptance/`，其中配置含秘密，不能整目录公开。

现有协议、SSH、恢复及 Compose 回归通过测试 fixture 直接创建迁移前构建，验证历史兼容；它们不替代真实模型验收，也没有为产品新增旧构建入口。Compose/TLS 验收从 Git HEAD 归档，验证未提交候选时必须先将候选复制到隔离临时 Git 仓库并提交，再从该仓库执行脚本。

部署前备份数据库、对象、主密钥、PKI、Emissary 和 worker 身份，排空活动任务，再升级 control、worker、Emissary。新增迁移保留原构建、日志、配方和发布记录。原生服务生命周期由 Rust 临时 CI 验收单独执行；交叉编译不代表原生宿主或物理设备验收。
