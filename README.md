# Artifactory

自助注册默认开启：访客注册后自动登录为全实例只读用户，可查看和下载全部项目的构建与产物。仅允许管理员分配账号时，设置 `ARTIFACTORY_REGISTRATION_ENABLED=false`；Helm 使用 `auth.registrationEnabled=false`。详情见 [用户自助注册](docs/operations.md#用户自助注册)。

面向个人与内部可信团队的跨平台应用构建、签名与内部分发平台。

项目于 2026-11-11 统一更名为 **Artifactory**，仓库为 [zhubby/artifactory](https://github.com/zhubby/artifactory)。命令、配置与环境变量使用 `artifactory` 标识；设备端现为 Rust `artifactory-emissary`。命名与迁移说明见 [更名记录](docs/artifactory-rename-2026-11-11.md)，后续复审修复及实际部署见[本机 / Mac mini 验收](docs/local-macmini-acceptance-2026-11-11.md)；历史设计和验收记录保留各自提交、运行结果与验收边界。

**只读 Git 项目 → 固定源码快照 → Pi AI 执行计划 → 原生节点构建 → 签名 / 校验 → 私有制品 → 不可变 Release。**

新构建不需要仓库配置文件。用户描述目标，AI 只读分析源码并生成命令与外置脚本，所有说明和计划保存在平台；最多自动修订两次。配置模型、注册 worker、源码分发与迁移说明见 [零侵入 AI 构建](docs/ai-builds.md)，真实模型及本地 / Mac mini 测试见 [AI 构建验收记录](docs/ai-builds-acceptance-2026-11-11.md)。

## 实现范围

- Rust/Axum/Tonic 控制平面，支持 [多副本部署](docs/control-ha.md)；PostgreSQL 持久化命令、队列、租约与审计，无新增消息中间件。
- Rust Artifactory Emissary 主动出站接入；Linux/macOS/Windows 原生执行器；独立 Worker、进程树取消、有界日志缓冲。Pi Agent 专注仓库分析和规划。
- Next.js/React/shadcn 风格 Radix 组件/Tailwind 控制台，项目、构建矩阵、SSE 日志、节点、制品、Release、身份与授权均使用真实 API。
- HTTPS/SSH Git 凭据加密托管；提交、仓库地址、凭据引用、配置、参数按构建快照固定。
- 节点证书认证、一次性注册、角色与项目范围 Token、S3 内容校验、签名引用的项目 / 节点双向授权。
- 原生执行仅适用于可信代码，不是安全沙箱。不兼容 GitHub Actions YAML、通信协议或插件生态。

**状态：已提供可执行首版与自动化验收；不能将本机联测或交叉编译视为三类宿主、五种应用目标全部验收。实际证据与缺口见 [验收记录](docs/acceptance.md)。**

## 本地开发

需要 Rust 1.95、Node 24、npm、protoc、OpenSSL、Git、Git LFS、Python 3、Docker。只有独立验收的上游 HTTP fixture 使用 Go；设备运行程序完全由 Rust 实现。

```sh
sh scripts/init-local.sh
sh scripts/local-services.sh
set -a; . .local/env; set +a
export GIT_SSL_CAINFO="$ARTIFACTORY_PKI_DIR/ca.pem"
cargo run --bin artifactory-control-plane
```

后端依赖统一为 TimescaleDB 2.32 / PostgreSQL 17；新建本地环境还需在独立终端运行 `sh scripts/run-telemetry.sh`。Emissary 的主机指标、托管构建进程与能力清单通过独立 Rust 遥测服务记录，配置、保留策略和查询 API 见 [时序观测](docs/telemetry.md)。

另开终端启动控制台：

```sh
cd web
npm ci
npm run dev
```

浏览器访问 `http://127.0.0.1:3000`；初始账号及随机密码在私有文件 `.local/env`。数据库和 S3 仅绑定本机；本地开发明确启用 HTTP，不能直接用于生产。

Emissary 注册：在控制台创建 15 分钟有效的一次性令牌，以环境变量传递，不写入命令行或仓库。

```sh
cargo build --locked --bin artifactory-emissary
export ARTIFACTORY_ENROLLMENT_TOKEN='从控制台获取'
target/debug/artifactory-emissary register --endpoint localhost:8443 --ca .local/pki/ca.pem --config .local/emissary/emissary.json --root "$(pwd)/.local/emissary/data" --allow-http-uploads
unset ARTIFACTORY_ENROLLMENT_TOKEN
target/debug/artifactory-emissary doctor --config .local/emissary/emissary.json
target/debug/artifactory-emissary run --config .local/emissary/emissary.json
```

生产部署、三类系统服务与签名节点设置见 [运维手册](docs/operations.md)。不要在已有生产配置旁运行本地验收脚本。

Kubernetes / 本机 kind 使用 [Helm 部署指南](docs/helm.md)。GitHub Release 发布时自动构建并推送 control-plane / web / agent-worker 的 AMD64、ARM64 镜像到 GHCR。新构建前需按 [AI 构建指南](docs/ai-builds.md) 配置模型并注册 worker。

可选 Kubernetes Pi 使用独立的 Rust Operator、Runtime 与 Source 服务，分出源码与推理热路径；控制平面接纳计划后，将设备构建下发给 Rust Emissary。项目默认保留 Poll Worker，可按项目选择 `planner_backend: kubernetes`。部署、暂停准入及回退见 [Operator 运维](docs/operator-operations.md)，协议见 [Runtime/Source v1](docs/runtime-protocol.md)。

## 验证

```sh
python3 -m unittest discover -s scripts -p 'test_*.py' -v
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
ARTIFACTORY_TEST_DATABASE_URL=postgres://artifactory:local-test-only@127.0.0.1:15432/artifactory cargo test --test database -- --ignored
cargo test --locked -p artifactory-emissary --all-targets
sh scripts/cross-build-emissary.sh
(cd web && npm run typecheck && npm run build)
(cd packages/agent-worker && npm ci && npm run check && npm test)
```

端到端验收创建独立配方记录、带 Basic Auth 的 HTTPS Git 服务，覆盖原生 C 构建、Rust Emissary、S3 上传 / 下载、取消和故障场景。macOS 额外执行 ad-hoc codesign，**不是开发者分发签名**。

Rust 测试运行真实前台 Emissary / Worker，检查父管道断开、租约到期和 SIGTERM 后的进程树、日志确认与工作目录清理，以及经 mTLS 确认的 OTA 替换与回滚。各操作系统的原生服务安装验收单独记录；详见 [移植说明](crates/emissary/PORTING.md) 与 [当前 Rust 验收](docs/acceptance/emissary/2026-11-13.md)。

```sh
cargo build --bin artifactory-control-plane
cargo build --locked --bin artifactory-emissary
set -a; . .local/env; set +a
python3 -u scripts/acceptance.py --keep-alive
```

此命令自行启动控制平面，须先停止手动运行的控制平面；保留前台供其他终端运行协议 / 浏览器验收。Ctrl-C 清理本次控制平面、Emissary 与 Git 服务，不删除数据库或证据。浏览器测试前启动 `npm run dev` 或已构建的控制台。

```sh
set -a; . .local/env; set +a
export ARTIFACTORY_LIVE_TESTS=1
export ARTIFACTORY_LIVE_S3_CONTAINER=artifactory-s3-test
export ARTIFACTORY_LIVE_EMISSARY_CONFIG="$(python3 -c 'import json; print(json.load(open(".local/acceptance/latest.json"))["emissary_config"])')"
export ARTIFACTORY_LIVE_PROJECT_ID="$(python3 -c 'import json; print(json.load(open(".local/acceptance/latest.json"))["project_id"])')"
cargo test --locked -p artifactory-emissary --test live_protocol -- --ignored --nocapture
(cd web && npm test)
```

`ARTIFACTORY_LIVE_S3_CONTAINER` 会短暂暂停指定的 S3 容器，以验证存储阻塞不影响心跳续租；只允许填写本次独立测试环境的容器，不要指向生产或共享存储。不做该故障检查时不要设置此变量。

协议联测还需要 `psql`，且 `.local/env` 中的 `DATABASE_URL` 必须指向该验收控制平面的数据库。测试会锁住本次创建的任务记录，实际提交节点吊销、数据库证书期限失效及 mTLS 会话替换，验证等待中的续租、开始授权与完成事件被拒绝，同时保留正常授权的成功对照；另行验证失败任务重试后的旧完成事件重放及后续任务开始。

制品联测同样持有真实 attempt 行锁，验证准备上传和完成登记在等待期间遇到吊销、证书期限失效、构建取消及租约实际到期后，均不得提交新的上传声明或制品记录。

取消联测分别持有 attempt 与节点行锁，验证真实 API 在等待期间提交取消后，accept 不再获得 start，finished 按 canceled 登记；未取消的相同请求仍可正常完成。

在上述验收控制平面和 Emissary 仍运行时，可继续验证 SSH Git。需要本机 OpenSSH server / `git-shell`；脚本创建仅监听 loopback 的临时 sshd、密钥和带空格路径的 Git 仓库，并验证正确密钥构建、错误主机指纹和未授权密钥拒绝：

```sh
python3 -u scripts/ssh_acceptance.py
```

随后 Ctrl-C 停止验收控制平面，并停止控制台，即可运行冷备份隔离恢复演练。需要 rclone；最近一次成功的 SSH 测试配置和仓库必须仍在原路径。脚本复制数据库、对象、主密钥和 PKI，在全新、带归属标签的 Docker 卷中恢复，验证旧 Release 下载和使用恢复的 SSH 凭据重新构建 / 发布：

```sh
python3 -u scripts/recovery_acceptance.py
```

两个命令都需要加载 `.local/env`。Linux 临时 CI runner 使用 `--sudo-sshd` 并准备 `/run/sshd`，仅隔离 sshd 通过无密码 sudo 启动。本地恢复脚本仅接受固定的 loopback 开发环境，清理本次新建的容器 / 卷，保留私有备份与证据。备份包含明文秘密，不能上传为 CI artifact；此演练不代替生产加密、异地备份和灾难恢复验收。

完整 Compose / TLS 验收使用独立 Git 快照、数据库 / 对象卷、网络、测试 CA 和随机 loopback 端口，不需要加载 `.local/env`：

```sh
python3 -u scripts/compose_acceptance.py
```

需要 Unix 非 root Docker 账户及 Git、Python 3.12+、OpenSSL。覆盖生产六服务、真实 HTTPS / mTLS、原生 Linux C 构建、运行中 SSE、私有制品、控制平面重启，以及 S3 暂停时的日志缓冲耗尽与恢复；保留现有开发服务，不修改宿主信任库。输入、证据和正式环境验收边界见 [运维手册](docs/operations.md#隔离-compose--tls-验收)。

## 仓库导航

模块命名及旧目录、部署数据的迁移说明见[模块布局与命名](docs/module-layout.md)。

| 目录 | 内容 |
|---|---|
| `crates/control-plane` | 控制平面、SQL 迁移、权限 / 状态 / 数据库测试 |
| `operator` / `charts/artifactory-operator` | Rust Operator、四个 CRD、独立 Helm release |
| `crates/planner-runtime` / `crates/runtime-protocol` | Pi 队列、租约/预算、模型代理和幂等交接 |
| `crates/source-core` / `crates/source-service` | 固定 Git/LFS 快照、源码工具、直接下载与 S3 恢复 |
| `crates/emissary` / `crates/emissary-protocol` | Rust 设备运行程序、独立 Worker、原生服务、OTA 与共享协议 |
| `packages/agent-worker` | Node 24 Pi SDK 规划进程、只读工具和模型接口 |
| `packages` | Node.js 模块目录，各模块独立管理依赖与构建 |
| `proto/artifactory/emissary/v1` | Emissary Protobuf 公共契约 |
| `schemas` / `examples` | 内部执行配方和历史配置 schema 与目标参考 |
| `web` | 控制台、Playwright 真实后台测试 |
| `deploy` / `compose.yaml` | Linux 单实例部署 |
| `docs` | 配置、API、协议、运维与验收边界 |

容器执行器、OCI 镜像、商店发布、Webhook、OIDC、多实例 HA 不属于本版；未用空实现伪装支持。
