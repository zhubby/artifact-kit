# HTTP API v1

统一前缀 `/api/v1`。生产网关将 `/api/*` 直接转给控制中心，源码流不经过 Next 请求体缓冲；开发控制台可使用同源代理。JSON 错误形态为 `{"error":"..."}`；401 未登录、403 禁止、404 不存在、409 冲突、400 参数错误；无效 JSON / schema 由 Axum 返回 4xx。

## 身份与权限

- `POST /auth/login`：`{username,password}`，设置 HttpOnly / SameSite=Strict cookie，生产添加 Secure。
- `GET /auth/config`：无需登录，返回 `{registration_enabled}`；响应禁止缓存。
- `POST /auth/register`：无需登录，仅接受 `{username,password}`，用户名去除首尾空白后为 1–100 UTF-8 字节，区分大小写且不能包含 NUL；密码为 12–256 UTF-8 字节。未知字段（包括 `role`、确认密码）被拒绝。新账号固定为 `viewer`，可读取本实例全部项目、构建和产物，不具备构建或管理权限。
- 注册成功返回 `201 {id,username,role}`，创建与登录相同的 24 小时会话。用户、会话与 `user_registered` 审计原子提交；密码仅以 Argon2 哈希保存。无效输入返回 `400`，注册关闭或 CSRF 不符合要求返回 `403`，用户名冲突返回 `409`。
- 注册在参数校验后、密码哈希前消耗限流额度：每实例滚动 5 分钟最多受理 10 次有效格式的尝试，重复用户名和后续失败也消耗额度；额度持久化在 `registration_attempt` 审计中，通过数据库事务锁防止并发越限。超限返回 `429` 和 `Retry-After`（秒）；重启不会重置额度，其他接口和登录限流不受影响。
- `GET /auth/me`、`POST /auth/logout`。
- Cookie 写请求需要 `x-artifactory-csrf: 1`，有 Origin 时必须精确匹配 `ARTIFACTORY_PUBLIC_URL`。Bearer Token 不使用 cookie 身份。
- 管理员：账户、项目授权、凭据、节点池、Emissary 注册与节点管理。操作者：构建、取消、重试和内部发布。只读：查询与下载。
- API Token 明确绑定项目 ID 列表与 `read/build/release` scopes，同时不能超过所属账户角色；不能调用管理 API 或再创建 Token。
- session 有效期 24 小时；Token 有效期 1 到 365 天。登录失败按账号持久化限流，反向代理仍应部署连接 / IP 级保护。

## 资源

| 方法 / 路径 | 输入 / 说明 |
|---|---|
| `GET /overview` | 概览统计，仅会话 |
| `GET, POST /users` | 创建 `{username,password,role}`，密码 12 到 256 UTF-8 字节 |
| `GET, POST /tokens` | 创建 `{name,project_ids,scopes,days}`，仅本次返回明文 |
| `DELETE /tokens/{id}` | 吊销当前用户的 Token |
| `GET, POST /pools` | 创建 `{name}` |
| `POST /enrollment-tokens` | `{pool_id}`，返回一次性令牌 |
| `GET /nodes` | 实际能力、诊断、slots、online、active_jobs |
| `GET /nodes/{id}/telemetry` | viewer 会话；最新主机采样、fresh 与版本化清单 |
| `GET /nodes/{id}/inventory` | viewer 会话；最近 100 次清单观察，附 truncated |
| `GET /nodes/{id}/telemetry/history` | viewer 会话；主机时序，显式 from / to / resolution |
| `GET /attempts/{id}/telemetry` | 项目 read；构建资源时序，显式 from / to / resolution |
| `GET /attempts/{id}/processes` | 项目 read；托管进程明细，显式 from / to / resolution=raw |
| `PATCH /nodes/{id}` | `{draining,revoked,slots,labels,signing_profiles,pool_id}`；吊销不可逆 |
| `GET, POST /credentials` | 创建 `{name,kind,username,secret,known_hosts}`；kind 为 https / ssh |
| `GET /audit` | 最近 200 条审计事件，仅管理员 |
| `GET, POST /projects` | 创建 `{name,repository_url,credential_id,pool_ids,signing_profiles,model_profile_id?,build_context?,source_credentials?}` |
| `GET, PATCH /projects/{id}` | PATCH 使用完整项目设置，不是任意 JSON Patch |
| `POST /projects/{id}/builds` | `{ref,goal,target_requirements?,parameters?,targets?,release_version?}`；所有新构建走 AI |
| `GET /builds` | 可选 `?project_id=`，最多 100 条 |
| `GET /builds/{id}` | 源码快照、当前计划 ID、修订次数、澄清、目标、所有 attempt、步骤、制品和发布 attempt |
| `GET /builds/{id}/plans` | 不可变计划历史、摘要、文件依据、规划运行和调用统计；不返回模型密钥或 checkpoint |
| `POST /builds/{id}/clarifications` | `{answer}`；只在 needs_input 继续规划，仍固定原源码与目标 |
| `GET, POST /model-profiles` | 管理员；接口类型/地址/模型/密钥/parameters/enabled/is_default/max_calls/timeout_seconds |
| `PATCH, DELETE /model-profiles/{id}` | 更新增加配置版本；密钥空值保留原值，查询仅返回掩码 |
| `GET /model-options` | 操作者会话可读模型 ID、名称和 enabled，用于项目选择 |
| `GET /ai-workers` | 管理员；在线、排空、吊销、证书期限和活动规划任务 |
| `POST /ai-enrollment-tokens` | 管理员；15 分钟一次性 worker 注册令牌 |
| `PATCH /ai-workers/{id}` | `{draining,revoked}`；吊销不可逆 |
| `GET /source-metrics` | 管理员；平台/Emissary 缓存命中、磁盘占用及下载字节，fetched_bytes 是磁盘增长 |
| `POST /builds/{id}/cancel` | 空对象，停止队列和在途任务 |
| `POST /jobs/{id}/retry` | 空对象，仅失败 / lost / canceled 且尚未发布的 job |
| `POST /builds/{id}/publish` | `{version?}`；既有版本不可更改；重试复用制品 |
| `GET /builds/{id}/events` | SSE `snapshot` / `log`；`Last-Event-ID` 或 `?after=` 恢复日志 |
| `GET /artifacts` | 可按项目过滤，不返回对象 key |
| `GET /artifacts/{id}/download` | 项目 read 权限后临时重定向到私有对象 GET |
| `GET /releases` | 可按项目过滤 |
| `GET /releases/{id}` | 不可变版本与制品目录 |

创建资源返回 `{id}`；Token 类创建还返回明文值；修改操作通常返回 `{ok:true}`。所有查询使用真实数据库，不返回演示对象。

遥测历史使用 ISO 8601 `from/to` 左闭右开区间，`resolution=raw|minute|hour`，默认 minute；单次范围分别限 6 小时、2 天、31 天，并受数据保留期约束。历史及清单查询返回 `{"samples":[],"truncated":false}`，结果超过行数或字节预算时标记截断。采集范围、未知值、权限和保留策略见 [时序观测](telemetry.md)。

## 触发示例

```sh
curl --fail-with-body -X POST 'https://build.example.com/api/v1/projects/PROJECT_UUID/builds' \
  -H "Authorization: Bearer $ARTIFACTORY_API_TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{"ref":"main","goal":"Build the CLI, test it and collect the executable","target_requirements":[{"os":"linux","arch":"amd64"}],"parameters":{"CHANNEL":"preview"},"release_version":"v1.0.0-preview.1"}'
```

发布版本的请求与会重新启用自动发布的 retry 均需要 release scope。更改项目仓库不会改变已经创建的构建源地址。凭据轮换应创建新的引用并修改项目；历史构建不静默切换凭据。

未指定 goal 时使用项目平台侧 build_context；两者均为空时拒绝。项目模型为空时继承平台默认值，创建构建固定该配置版本。`target_requirements` 是 `{os,arch}` 数组；`targets` 若提供则固定目标名集合。人工 job 重试使用原提交与计划，规划中不可重试。状态为 resolving / planning / needs_input / replanning / queued / running / succeeded / failed / canceled / lost。执行失败最多两次自动修订，权限、认证和源码凭据错误不自动修订，详见 [AI 构建](ai-builds.md)。

当前列表为内部小团队规模的有界查询，并未提供全量导出、复杂分页或外部商店发布 API；Webhook / OIDC / 多实例并非隐藏开启的功能。

## Emissary 版本与升级

版本目录、节点 / 节点池目标、执行记录、重试及取消见 [Emissary OTA 管理接口](emissary-ota.md#管理接口)。这些管理写操作只允许管理员会话，项目 API Token 不获得升级权限。
