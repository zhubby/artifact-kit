# Artifact Kit

面向个人与内部可信团队的跨平台应用构建、签名与内部分发平台。

**Git 项目 → 固定提交与配置 → 原生节点构建 → 签名 / 校验 → 私有制品 → 不可变 Release。**

## 实现范围

- Rust/Axum/Tonic 单实例控制中心；PostgreSQL 持久化队列、租约与审计，无 Redis。
- Go Agent 主动出站接入；Linux/macOS/Windows 原生执行器；独立 Worker、进程树取消、有界日志缓冲。
- Next.js/React/shadcn 风格 Radix 组件/Tailwind 控制台，项目、构建矩阵、SSE 日志、节点、制品、Release、身份与授权均使用真实 API。
- HTTPS/SSH Git 凭据加密托管；提交、仓库地址、凭据引用、配置、参数按构建快照固定。
- 节点证书认证、一次性注册、角色与项目范围 Token、S3 内容校验、签名引用的项目 / 节点双向授权。
- 原生执行仅适用于可信代码，不是安全沙箱。不兼容 GitHub Actions YAML、通信协议或插件生态。

**状态：已提供可执行首版与自动化验收；不能将本机联测或交叉编译视为三类宿主、五种应用目标全部验收。实际证据与缺口见 [验收记录](docs/acceptance.md)。**

## 本地开发

需要 Rust、Go 1.24+、Node 22、npm、protoc、OpenSSL、Git、Python 3、Docker。具体已验证版本见验收记录。

```sh
sh scripts/init-local.sh
sh scripts/local-services.sh
set -a; . .local/env; set +a
export GIT_SSL_CAINFO="$AK_PKI_DIR/ca.pem"
cargo run --bin artifact-control
```

另开终端启动控制台：

```sh
cd web
npm ci
npm run dev
```

浏览器访问 `http://127.0.0.1:3000`；初始账号及随机密码在私有文件 `.local/env`。数据库和 S3 仅绑定本机；本地开发明确启用 HTTP，不能直接用于生产。

Agent 注册：在控制台创建 15 分钟有效的一次性令牌，以环境变量传递，不写入命令行或仓库。

```sh
cd agent
CGO_ENABLED=0 go build -o bin/artifact-agent ./cmd/artifact-agent
export AK_ENROLLMENT_TOKEN='从控制台获取'
bin/artifact-agent register --endpoint localhost:8443 --ca ../.local/pki/ca.pem --config ../.local/agent/agent.json --root "$(pwd)/../.local/agent/data" --allow-http-uploads
unset AK_ENROLLMENT_TOKEN
bin/artifact-agent doctor --config ../.local/agent/agent.json
bin/artifact-agent run --config ../.local/agent/agent.json
```

生产部署、三类系统服务与签名节点设置见 [运维手册](docs/operations.md)。不要在已有生产配置旁运行本地验收脚本。

## 验证

```sh
python3 -m unittest discover -s scripts -p 'test_*.py' -v
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
AK_TEST_DATABASE_URL=postgres://artifact:local-test-only@127.0.0.1:15432/artifact cargo test --test database -- --ignored
(cd agent && go vet ./... && go test -race ./...)
sh scripts/cross-build-agent.sh
(cd web && npm run typecheck && npm run build)
```

真实端到端验收会创建带 Basic Auth 的 HTTPS Git 服务、真实原生 C 构建、真实 Agent、S3 上传 / 下载、取消和故障场景。macOS 额外执行 ad-hoc codesign，**不是开发者分发签名**。

Unix 的 Go 测试还会运行真实前台 Agent / Worker，检查正常退出、Agent 强制退出及 Worker 收到 SIGTERM 后的任务进程树与工作目录清理。系统服务验收仅在显式启用的 GitHub 托管临时 VM 上执行，包含专用账户下在途任务的服务停止 / 重启及后续任务；详见 [原生服务验收](docs/operations.md#临时-ci-原生服务验收)。

```sh
cargo build --bin artifact-control
(cd agent && CGO_ENABLED=0 go build -o bin/artifact-agent ./cmd/artifact-agent)
set -a; . .local/env; set +a
python3 -u scripts/acceptance.py --keep-alive
```

此命令自行启动中心，须先停止手动运行的中心；保留前台供其他终端运行协议 / 浏览器验收。Ctrl-C 清理本次中心、Agent 与 Git 服务，不删除数据库或证据。浏览器测试前启动 `npm run dev` 或已构建的控制台。

```sh
set -a; . .local/env; set +a
export AK_LIVE_TESTS=1
export AK_LIVE_S3_CONTAINER=artifact-kit-s3-test
export AK_LIVE_AGENT_CONFIG="$(python3 -c 'import json; print(json.load(open(".local/acceptance/latest.json"))["agent_config"])')"
export AK_LIVE_PROJECT_ID="$(python3 -c 'import json; print(json.load(open(".local/acceptance/latest.json"))["project_id"])')"
(cd agent && go test -v ./internal/client -run TestLiveRustGoProtocol)
(cd web && npm test)
```

`AK_LIVE_S3_CONTAINER` 会短暂暂停指定的 S3 容器，以验证存储阻塞不影响心跳续租；只允许填写本次独立测试环境的容器，不要指向生产或共享存储。不做该故障检查时不要设置此变量。

在上述验收中心和 Agent 仍运行时，可继续验证 SSH Git。需要本机 OpenSSH server / `git-shell`；脚本创建仅监听 loopback 的临时 sshd、密钥和带空格路径的 Git 仓库，并验证正确密钥构建、错误主机指纹和未授权密钥拒绝：

```sh
python3 -u scripts/ssh_acceptance.py
```

随后 Ctrl-C 停止验收中心，并停止控制台，即可运行冷备份隔离恢复演练。需要 rclone；最近一次成功的 SSH 测试配置和仓库必须仍在原路径。脚本复制数据库、对象、主密钥和 PKI，在全新、带归属标签的 Docker 卷中恢复，验证旧 Release 下载和使用恢复的 SSH 凭据重新构建 / 发布：

```sh
python3 -u scripts/recovery_acceptance.py
```

两个命令都需要加载 `.local/env`。Linux 临时 CI runner 使用 `--sudo-sshd` 并准备 `/run/sshd`，仅隔离 sshd 通过无密码 sudo 启动。本地恢复脚本仅接受固定的 loopback 开发环境，清理本次新建的容器 / 卷，保留私有备份与证据。备份包含明文秘密，不能上传为 CI artifact；此演练不代替生产加密、异地备份和灾难恢复验收。

完整 Compose / TLS 验收使用独立 Git 快照、数据库 / 对象卷、网络、测试 CA 和随机 loopback 端口，不需要加载 `.local/env`：

```sh
python3 -u scripts/compose_acceptance.py
```

需要 Unix 非 root Docker 账户及 Go、Git、Python 3.12+、OpenSSL。覆盖生产五服务、真实 HTTPS / mTLS、原生 Linux C 构建、运行中 SSE、私有制品、中心重启，以及 S3 暂停时的日志缓冲耗尽与恢复；保留现有开发服务，不修改宿主信任库。输入、证据和正式环境验收边界见 [运维手册](docs/operations.md#隔离-compose--tls-验收)。

## 仓库导航

| 目录 | 内容 |
|---|---|
| `crates/control` | 控制面、SQL 迁移、权限 / 状态 / 数据库测试 |
| `agent` | Go Agent、原生 Worker、生成的 Go 协议、进程树测试 |
| `proto/artifactkit/agent/v1` | 版本化 Protobuf 公共契约 |
| `schemas` / `examples` | 配置 schema 与五类目标的适配模板 |
| `web` | 控制台、Playwright 真实后台测试 |
| `deploy` / `compose.yaml` | Linux 单实例部署 |
| `docs` | 配置、API、协议、运维与验收边界 |

容器执行器、OCI 镜像、商店发布、Webhook、OIDC、多实例 HA 不属于本版；未用空实现伪装支持。
