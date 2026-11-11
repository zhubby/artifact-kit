# HTTP API v1

统一前缀 `/api/v1`。控制台通过同源 Next 代理调用中心，不在 Next 重新实现授权或调度。JSON 错误形态为 `{"error":"..."}`；401 未登录、403 禁止、404 不存在、409 冲突、400 参数错误；无效 JSON / schema 由 Axum 返回 4xx。

## 身份与权限

- `POST /auth/login`：`{username,password}`，设置 HttpOnly / SameSite=Strict cookie，生产添加 Secure。
- `GET /auth/me`、`POST /auth/logout`。
- Cookie 写请求需要 `x-artifact-csrf: 1`，有 Origin 时必须精确匹配 `AK_PUBLIC_URL`。Bearer Token 不使用 cookie 身份。
- 管理员：账户、项目授权、凭据、节点池、注册与节点管理。操作者：构建、取消、重试和内部发布。只读：查询与下载。
- API Token 明确绑定项目 ID 列表与 `read/build/release` scopes，同时不能超过所属账户角色；不能调用管理 API 或再创建 Token。
- session 有效期 24 小时；Token 有效期 1 到 365 天。登录失败按账号持久化限流，反向代理仍应部署连接 / IP 级保护。

## 资源

| 方法 / 路径 | 输入 / 说明 |
|---|---|
| `GET /overview` | 概览统计，仅会话 |
| `GET, POST /users` | 创建 `{username,password,role}`，密码 12 到 256 字符 |
| `GET, POST /tokens` | 创建 `{name,project_ids,scopes,days}`，仅本次返回明文 |
| `DELETE /tokens/{id}` | 吊销当前用户的 Token |
| `GET, POST /pools` | 创建 `{name}` |
| `POST /enrollment-tokens` | `{pool_id}`，返回一次性令牌 |
| `GET /nodes` | 实际能力、诊断、slots、online、active_jobs |
| `PATCH /nodes/{id}` | `{draining,revoked,slots,labels,signing_profiles,pool_id}`；吊销不可逆 |
| `GET, POST /credentials` | 创建 `{name,kind,username,secret,known_hosts}`；kind 为 https / ssh |
| `GET /audit` | 最近 200 条审计事件，仅管理员 |
| `GET, POST /projects` | 创建 `{name,repository_url,credential_id,pool_ids,signing_profiles}` |
| `GET, PATCH /projects/{id}` | PATCH 使用完整项目设置，不是任意 JSON Patch |
| `POST /projects/{id}/builds` | `{ref,parameters?,targets?,release_version?}` |
| `GET /builds` | 可选 `?project_id=`，最多 100 条 |
| `GET /builds/{id}` | 配置快照、目标、所有 attempt、步骤、制品和发布 attempt |
| `POST /builds/{id}/cancel` | 空对象，停止队列和在途任务 |
| `POST /jobs/{id}/retry` | 空对象，仅失败 / lost / canceled 且尚未发布的 job |
| `POST /builds/{id}/publish` | `{version?}`；既有版本不可更改；重试复用制品 |
| `GET /builds/{id}/events` | SSE `snapshot` / `log`；`Last-Event-ID` 或 `?after=` 恢复日志 |
| `GET /artifacts` | 可按项目过滤，不返回对象 key |
| `GET /artifacts/{id}/download` | 项目 read 权限后临时重定向到私有对象 GET |
| `GET /releases` | 可按项目过滤 |
| `GET /releases/{id}` | 不可变版本与制品目录 |

创建资源返回 `{id}`；Token 类创建还返回明文值；修改操作通常返回 `{ok:true}`。所有查询使用真实数据库，不返回演示对象。

## 触发示例

```sh
curl --fail-with-body -X POST 'https://build.example.com/api/v1/projects/PROJECT_UUID/builds' \
  -H "Authorization: Bearer $AK_API_TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{"ref":"main","parameters":{"CHANNEL":"preview"},"targets":["linux-amd64"],"release_version":"v1.0.0-preview.1"}'
```

发布版本的请求与会重新启用自动发布的 retry 均需要 release scope。更改项目仓库不会改变已经创建的构建源地址。凭据轮换应创建新的引用并修改项目；历史构建不静默切换凭据。

当前列表为内部小团队规模的有界查询，并未提供全量导出、复杂分页或外部商店发布 API；Webhook / OIDC / 多实例并非隐藏开启的功能。

## Agent 版本与升级

版本目录、节点 / 节点池目标、执行记录、重试及取消见 [Agent OTA 管理接口](agent-ota.md#管理接口)。这些管理写操作只允许管理员会话，项目 API Token 不获得升级权限。
