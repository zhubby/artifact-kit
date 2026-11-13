# Helm / Kubernetes 部署

从旧组件名称升级时，先按[模块布局与命名](module-layout.md#从旧布局升级)迁移 PVC、身份及自定义 values，再启动新组件。

Chart 位于 `charts/artifactory`，包含可配置副本数的 control-plane、Next.js / Caddy 网关、TimescaleDB 2.32 / PostgreSQL 17、私有 SeaweedFS S3、独立 Pi Agent worker 和 Rust Telemetry 服务，以及四个独立 PVC。默认 control-plane、数据库、S3 使用 Recreate 升级。Control 多副本使用独立临时目录，由独立 Source 持有原数据卷；迁移步骤和连接预算见 [Control 多副本](control-ha.md)。

`auth.registrationEnabled` 默认 `true`，访客可自助注册为全实例只读用户，查看和下载全部项目的构建与产物。仅允许管理员分配账号的部署应在安装或升级时设置 `--set auth.registrationEnabled=false`；该开关映射到 control-plane 的 `ARTIFACTORY_REGISTRATION_ENABLED`，不会停用已有账号或会话。

## Release 镜像

GitHub `release-images` 工作流仅在 **Release published** 时执行，正式版本和预发布版本都构建。它检出该 Release 的 tag，在原生 AMD64、ARM64 runner 分别构建，再发布包含 `linux/amd64` / `linux/arm64` 的 GHCR manifest：

- `ghcr.io/zhubby/artifactory-control-plane:<release-tag>`
- `ghcr.io/zhubby/artifactory-web:<release-tag>`
- `ghcr.io/zhubby/artifactory-agent-worker:<release-tag>`

标签包含原始 Release tag、完整 `sha-<commit>`；语义版本额外提供不带 `v` 的版本标签。只有正式 Release 更新 `latest`。工作流使用 `GITHUB_TOKEN` 的 `packages:write`，不需要另存 PAT；它不会创建 Release。发布前先让对应提交的 `platform-contracts` 检查通过。首次 GHCR 包如果为私有，需要设置包可见性，或通过 `imagePullSecrets` 提供有读取权限的凭据。

## 首次准备凭据与 PKI

需要 Helm 3、kubectl、Python 3、OpenSSL、支持 ReadWriteOnce 的默认 StorageClass。先确认 kube context 与目标命名空间。以下命令中的域名、镜像版本、Ingress 类和 TLS Secret 均需替换为实际配置。

```sh
python3 scripts/init-helm.py \
  --namespace artifactory --release artifactory \
  --grpc-host agents.example.com \
  --output .deploy/helm/production

kubectl --context YOUR_CONTEXT create namespace artifactory
kubectl --context YOUR_CONTEXT apply \
  -f .deploy/helm/production/credentials.json \
  -f .deploy/helm/production/pki.json
```

`--output` 必须是新目录；脚本拒绝覆盖。生成目录与私钥只对当前用户开放。`credentials.json` 是 Kubernetes Secret，保存数据库口令、加密主密钥、S3 身份及管理员初始口令；`pki.json` 包含签发 CA、服务器证书及私钥。`access.json` 提供初始登录信息，`pki/ca.pem` 通过可信渠道交给 Agent。均保留在 Git 忽略的 `.deploy/` 中，应离线加密备份。

Chart 引用已有 Secret，**不生成或轮换任何密钥**。默认名称为 `<release>-credentials` / `<release>-pki`，也可设置 `secrets.credentials` / `secrets.pki`。使用 `fullnameOverride` 时，初始化脚本的 `--resource-prefix` 必须与之对应。凭据中的数据库 URL 使用同一资源前缀的 PostgreSQL Service。

默认数据库镜像为 `timescale/timescaledb:2.32.0-pg17`。新卷自动初始化独立遥测库与非管理员应用角色；`credentials.json` 另包含 `telemetry-database-password`、`telemetry-database-url` 和 `telemetry-token`。`telemetry.endpoint` 应填写 Emissary 可访问、证书 SAN 覆盖的 HTTPS 8444 地址；8093 只供 Control 内部查询。已有 PostgreSQL 卷不自动转换，部署配置与 TimescaleDB 恢复步骤见 [时序观测](telemetry.md)。

CA 有效期十年，gRPC 服务器证书一年；到期前使用原 CA 为正确 SAN 续签，更新 PKI Secret 并安排 control-plane 重启。不要重新初始化整个目录来续签，亦不要替换已有部署的 master key。

## 生产配置

准备 `production-values.yaml`，例如：

```yaml
images:
  control-plane:
    tag: v0.1.0
  web:
    tag: v0.1.0
console:
  publicUrl: https://build.example.com
s3:
  publicEndpoint: https://artifacts.example.com
ingress:
  enabled: true
  className: nginx
  consoleHost: build.example.com
  s3Host: artifacts.example.com
  consoleTlsSecret: artifactory-console-tls
  s3TlsSecret: artifactory-s3-tls
  annotations:
    nginx.ingress.kubernetes.io/proxy-buffering: "off"
    nginx.ingress.kubernetes.io/proxy-read-timeout: "3600"
    nginx.ingress.kubernetes.io/proxy-body-size: "0"
```

```sh
helm lint charts/artifactory --strict -f production-values.yaml
helm upgrade --install artifactory charts/artifactory \
  --kube-context YOUR_CONTEXT --namespace artifactory \
  -f production-values.yaml --wait --timeout 10m
```

示例版本必须已发布；Chart 不会自动创建域名、Ingress Controller 或 TLS Secret。生产默认要求控制台和 S3 都使用 HTTPS。control-plane 默认也通过公开 S3 HTTPS 地址进行初始化和校验，因此 Pod 必须能够回环访问该域名及信任证书。`s3.endpoint` 可覆盖控制平面使用的地址，但签名给 Agent / 浏览器的 `s3.publicEndpoint` 必须与实际可达的外部地址一致。组织内部 CA 需加入 control-plane 镜像内的系统信任库，不能只在宿主安装。

Web Service 的入口是 Caddy；它先将 `/api/*` 和 `/healthz` 直接转发给 control-plane，再将页面转发给 Next.js，SSE 不缓冲。这避免 Next 镜像构建时固定的 `control-plane` DNS 与 Helm 的资源前缀冲突。Ingress 也必须关闭响应缓冲并允许实际制品大小。

gRPC 使用独立的 `<release>-grpc:8443` Service，保留服务器 TLS 与节点 mTLS。通过受信网络的 TCP LoadBalancer、NodePort 或 TLS passthrough 暴露，确保 Agent 访问的主机名/IP 在生成证书 SAN 中。不要使用普通 HTTP Ingress 终止这条私有 CA 认证连接，也不要公开数据库或 SeaweedFS 内部管理端口。

## 本机 kind

本机 profile 显式启用 HTTP 开发模式；gRPC 仍校验私有 CA。默认端口为控制台 31080、S3 31333、gRPC 31443。先确认端口未被其他 Service 使用。

```sh
python3 scripts/init-helm.py \
  --namespace artifactory --release artifactory \
  --grpc-host 127.0.0.1 --grpc-host localhost \
  --output .deploy/helm/kind
kubectl --context YOUR_KIND_CONTEXT create namespace artifactory
kubectl --context YOUR_KIND_CONTEXT apply \
  -f .deploy/helm/kind/credentials.json -f .deploy/helm/kind/pki.json
```

未发布 Release 时，可以从本次源码构建并加载本地镜像：

```sh
docker build -t artifactory-control-plane:kind .
docker build -t artifactory-web:kind web
kind load docker-image --name YOUR_CLUSTER artifactory-control-plane:kind artifactory-web:kind

helm upgrade --install artifactory charts/artifactory \
  --kube-context YOUR_KIND_CONTEXT --namespace artifactory \
  -f charts/artifactory/values-kind.yaml \
  --set images.control-plane.repository=artifactory-control-plane --set images.control-plane.tag=kind \
  --set images.web.repository=artifactory-web --set images.web.tag=kind \
  --wait --timeout 10m
```

如果 kind 已配置本地 registry，也可把镜像推入该 registry，并在 Helm values 设置对应 repository。不要重建已有集群来加载镜像。

macOS 常规 kind 网络不能直接访问节点 NodePort。默认 values 使用 loopback，分别在三个终端运行：

```sh
kubectl --context YOUR_KIND_CONTEXT -n artifactory port-forward service/artifactory-web 31080:80
kubectl --context YOUR_KIND_CONTEXT -n artifactory port-forward service/artifactory-s3 31333:8333
kubectl --context YOUR_KIND_CONTEXT -n artifactory port-forward service/artifactory-grpc 31443:8443
```

浏览器访问 `http://127.0.0.1:31080`。Agent 注册使用 `127.0.0.1:31443`、本次 `pki/ca.pem`，并仅在这个开发环境添加 `--allow-http-uploads`。若 OrbStack 等环境允许直接访问节点 IP，可将 `console.publicUrl` / `s3.publicEndpoint` 改为实际节点 IP 和 NodePort；初始化时也须将该 IP 加入 `--grpc-host`。浏览器 Origin、配置 URL 及签名 S3 地址必须一致，不要随意切换 localhost / IP。

## 检查与升级

```sh
kubectl --context YOUR_CONTEXT -n artifactory get pods,pvc,services
curl -fsS https://build.example.com/healthz
helm --kube-context YOUR_CONTEXT -n artifactory history artifactory
```

健康检查成功后，仍需验证真实登录、CSRF、Agent mTLS 注册/心跳、SSE 和一次制品构建/下载。HTTP kind 联测不代表公网 TLS、真实业务应用、实体机启动恢复、发行签名或生产备份恢复已验收。

升级使用同一命名空间、release 名、Secret 和 PVC，显式指定新镜像 tag。保持 control-plane 副本数为一；升级前排空 Agent，协调备份数据库、对象存储、control-plane 数据、主密钥与 CA。Secret 更新后容器环境变量不会自动刷新，需要安排相关 Deployment 重启；数据库用户密码也不会因更改初始化环境变量而自动更新。

三个 PVC 带有 `helm.sh/resource-policy: keep`，`helm uninstall` 会保留它们；外部 Secret 同样保留。**删除命名空间仍会删除这些资源**。不要将卸载、删除命名空间或重建 kind 集群用作升级流程。

## Agent worker

默认启用 `agentWorker.enabled`，使用独立身份 PVC、只读 CA 和单副本 Recreate。`images.agent-worker` 需与 control-plane/web 使用同一候选版本；配置 `agentWorker.serverName` 时必须匹配服务器证书 SAN。初始 Pod 等待注册，按 [AI 构建指南](ai-builds.md) 从标准输入注册，并在管理界面配置真实模型。`sourceCache.maxBytes` 控制平台缓存配额，kind 默认 512 MiB。已有 PVC/Secret 不得重新初始化覆盖。
