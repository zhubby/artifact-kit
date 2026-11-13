# artifactory 复审修复与本机 / Mac mini 验收

日期：2026-11-11，Asia/Shanghai。最终检查时间：16:23。

复审确认的 S3 凭据文件截断问题已修复，新版已部署到本机控制平面、前端和 `codehubmac-mini` 的专用 Agent。服务保持运行，最终没有活动构建，Agent 在线且未排空。

## 修复与候选代码

旧实现先以写模式打开 `.local/s3.json`，随后读取环境变量。保留的环境文件缺少新设置时，程序会在报错前清空原凭据。

`scripts/local-services.sh` 现在先检查两个 S3 设置是否存在且非空，完成 JSON 序列化后，在同目录创建权限为 0600 的临时文件，并通过原子替换更新凭据。配置不完整时，原凭据和环境文件保持不变。回归测试运行真实初始化 / 启动脚本，使用隔离目录和禁止实际 Docker 操作的替身；缺失、空值和完整配置均有覆盖。Python 脚本测试共 13 项通过。

部署代码来自包含全部更名修改和本次修复的独立临时 Git 仓库，候选 SHA 为 `df13cc8a1a96e2a9903e2e98feb5d52baebe476c`。194 个候选文件与部署时工作区逐一核对 SHA-256 一致。该 SHA 是部署快照，不是 GitHub 上的新提交。后续 README 和本文更新不改变已部署程序。

| 部件 | 部署内容 |
|---|---|
| Rust | `artifactory-control-plane 0.1.0`，macOS arm64 release |
| 前端 | `artifactory-console 0.1.0`，Next.js 生产 standalone |
| Agent | `artifactory-agent 0.1.0`，macOS arm64，protocol 1，注入上述候选 SHA |
| 控制平面 SHA-256 | `53e3f3319d2dfdf6a7cb0207874665637b460a0de829928e06694a95d9853ada` |
| 最终 Agent SHA-256 | `c2b29f3ef95ac65289ccb7c893dbbadc7e4057eb5340d16dd6568b140dcfebcf` |

本机 Rust release 构建遇到工具链生成的 SQLx 编译插件无法被 macOS 加载的问题。使用以下构建覆盖后成功；产品 release 的其余设置保留：

```sh
cargo --config 'profile.release.build-override.strip="none"' build --release --locked --bin artifactory-control-plane
```

前端使用实际控制平面地址重新构建，API rewrite 指向 `127.0.0.1:28080`，避免复用测试构建中的默认端口。

## 部署和数据保留

本机运行 Rust HTTP / gRPC 与 Next.js，继续使用现有 Docker PostgreSQL / S3。控制台经 Tailscale HTTPS 9443 提供访问，Agent 经 Tailscale TCP 19443 使用 mTLS 主动连接。本机监听仍限制在 loopback。

Mac mini 的固定启动器为 `/usr/local/libexec/artifactory/artifactory-agent`，配置为 `/Users/Shared/artifactory/agent.json`，launchd label 为 `artifactory-agent`。沿用已有普通专用账户 `artifactbuild`、UID 600 和完整 Xcode 环境。配置、CA、节点证书和节点私钥保持 0600，节点 ID 仍为 `cee1a1a0-fdb1-4dee-8f4f-915b3be0651c`，新旧身份文件的哈希一致。

切换前完成数据库逻辑备份、S3 对象备份及远端原运行目录 / 配置 / 二进制 / plist 备份。原私有环境文件未覆盖；数据库和桶通过显式设置继续使用现有资源。旧系统服务已停止并禁用开机加载，旧文件保留；新服务启用。清除了指向更名前测试包的 OTA 目标。

最终核对结果：原有 8 个 Release 详情完全一致，原有 1,338 个 S3 文件对象的大小和哈希未变，原 Docker 容器身份与启动时间保留，原 Tailscale 路由保持一致。原 kind 的四个 Deployment 仍为 0 副本，未重新激活。

## 实际验收

| 场景 | 结果 |
|---|---|
| 新公共接口 | `artifactory_session` 为 Secure / HttpOnly；新 CSRF 请求头有效；实际旧 Cookie `ak_session` 和旧 CSRF 请求头均被拒绝 |
| 新配置与构建 | 认证 HTTPS Git 仓库仅使用 `artifactory.yaml` 和新步骤环境变量；Mac mini 完成真实 Xcode Release 构建、ad-hoc 签名和严格签名验证 |
| 日志与制品 | 观察到任务仍运行时的真实 HTTPS SSE 快照和远端日志；三个制品上传、签名下载、字节数和 SHA-256 匹配，无签名 S3 读取被拒绝 |
| 失败与取消 | 故意退出码 23 按预期失败；运行中取消成功；两者均无制品和发布；最终远端工作目录、日志缓冲和 Worker 进程均已清理 |
| 服务重启 | 本机控制平面和 Mac mini launchd 均更换 PID；Agent 自动重连，后续真实构建、发布及三份下载成功 |
| 原生应用 | 下载后的应用在本机独立执行自测；同一下载包在 Mac mini 登录会话中通过严格签名检查、进入 AppKit GUI 事件循环并正常自动退出 |
| 实际浏览器 | 10 个已部署页面场景通过，覆盖中英文、1440px 桌面与 390px 移动端；展开的移动导航显示 `artifactory`；主题存储和语言 Cookie 使用新前缀，0 个运行时错误 |
| 五平台资源 | Linux amd64 / arm64、macOS amd64 / arm64、Windows amd64 均从候选代码构建，资源名均为 `artifactory-agent-…`，版本注入参数使用实际 Go 模块路径 |
| 真实 OTA | Go 签名、Rust 导入验证、HTTPS 私有 S3 下载、Go 再次校验和 mTLS 健康确认贯通；测试升级及恢复到 `0.1.0` 均成功，固定启动器、节点身份和最终二进制哈希保留 |
| OTA 后构建 | 真实 Xcode 构建、发布及三个制品下载再次成功，元数据保留 Mac mini 主机与专用账户身份 |

| 构建场景 | Build ID | 结果 |
|---|---|---|
| 首次新版完整构建 | `1c5011f7-4a56-4ec8-aa9c-877c8fad5799` | succeeded |
| 退出码 23 | `443d667d-e2fa-4175-a050-ae8c5d84200c` | failed，符合预期 |
| 运行中取消 | `d895ce70-001f-414e-91c8-6d2d192abd76` | canceled，符合预期 |
| 两端重启后 | `fbc73cfb-7c96-4be4-8676-7145ed4ed4f9` | succeeded |
| OTA 恢复后 | `6d2e81f4-97df-4f9c-8616-ceb1edd4e52d` | succeeded |

最终数据库共有 16 个构建（12 成功、2 失败、2 取消）、36 个制品、11 个 Release、1 个节点、0 个活动构建。失败和取消包含之前的历史记录及本次预期场景。

## OTA 测试包纠正记录

首个测试包的清单标签为 `v0.1.0`，实际二进制版本字符串为 `0.1.0`。签名和下载通过后，健康确认因版本字符串不一致而被拒绝，120 秒后自动回退，节点恢复在线。该次属于测试包制作错误，不能计作成功升级。

重新构建并核对清单与二进制版本完全一致后，`v0.1.0-rename-test` 的实际升级成功（更新 ID `dd4504a8-dc2d-4154-98ae-98735b69849c`），随后通过独立签名包恢复到 `0.1.0` 成功（更新 ID `41da01a3-2c52-492b-8735-462ab1879089`）。最终版本仍为 `0.1.0`，临时 OTA 目标已清除，池暂停和并发设置已恢复。

## 证据与边界

完整私有部署目录为 `.local/artifactory-deploy/20261111-154424/`，`latest` 指向此目录。其中包含候选源码、源文件和二进制哈希、部署配置、数据库 / S3 备份、浏览器截图、下载应用及验收 JSON。凭据和备份未写入本文。

本次实际执行宿主为 macOS arm64；其他四个目标完成交叉编译。OTA 的 GitHub 元数据接口使用隔离 HTTPS 夹具，下载、签名验证、实际远端进程切换和健康确认均为真实链路。未向 GitHub 发布 Release 或镜像，未重启两台 Mac，未执行 Windows / Linux 宿主的系统服务生命周期，也未执行 Developer ID / notarization。本次修复之外的更名测试证据见[初次更名记录](artifactory-rename-2026-11-11.md)，原历史记录保留原文。
