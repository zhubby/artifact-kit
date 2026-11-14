# 统一版本与发布

项目使用单一稳定语义版本，权威来源为仓库根目录的 `VERSION`。0.2.0 是首个统一发布版本；Git 标签使用 `v0.2.0`，Helm Chart 与应用版本使用 `0.2.0`。

## 管理范围

- Rust workspace 版本及其全部一方 crate 的 `Cargo.lock` 记录；crate 必须使用 `version.workspace = true`。
- Web、`packages/` 下的一方 Node.js 包、对应 lockfile 顶层与根包版本。
- `charts/` 下各 Chart 的 `version`、`appVersion`。
- Release 镜像的 `0.2.0` / `v0.2.0` 标签和 OCI 版本、源码提交标识。

第三方依赖版本、数据库迁移编号、HTTP / Protobuf API 版本不随应用版本一起替换。Operator Chart 保持其独立的 Kubernetes 最低版本约束。

## 更新与检查

```sh
python3 scripts/version.py --set 0.3.0
python3 scripts/version.py --check
python3 scripts/version.py --check --tag v0.3.0
python3 -m unittest scripts.test_version -v
cargo metadata --locked --no-deps --format-version 1 > /dev/null
```

`--set` 同步版本清单和 lockfile 中的一方版本，不升级依赖；Node JSON 文件在发生版本变化时使用两空格格式。当前流程只接受不带前导零的稳定 `major.minor.patch`，不隐式推送提交、创建标签或发布。

## 发布流程

1. 更新统一版本，完成测试并提交源码；核对工作区没有夹带私有配置。
2. 推送源码，创建指向该提交的注释标签 `v<version>`，推送标签；禁止移动已发布标签。
3. 创建同名 GitHub Release。`release-images` 先检查标签与所有版本一致，再构建五类 Linux amd64 / arm64 镜像。
4. 镜像发布任务验证 manifest 同时具有两种架构。部署使用稳定版本或已核对的 digest，不以 `latest` 作为发布证据。
5. 同一流水线上传两个 Helm 包、`VERSION`、`SOURCE_COMMIT` 和 `SHA256SUMS`；下载后独立核对校验和与提交。
6. 源码 CI、镜像发布、真实集群部署、设备运行分别记录结果。任何一层失败都不能冒称下一层通过。

原生 Emissary 二进制仍使用 `emissary-release` 的六平台构建与签名流程。该流程需要独立的签名 secret 和发布环境；普通 `v<version>` 镜像发布不代表已提供签名 OTA manifest，也不代表 Apple / Windows 正式发行签名。

## 隔离的 Kubernetes 试部署

`deploy/kubernetes-trial.yaml` 只创建 `artifactory` 命名空间及其内部的资源配额、部署身份和网络策略。仅在命名空间尚不存在、确认无他人资源时执行初始化；不要把文件中的 Namespace 用于接管已有命名空间。

```sh
kubectl --kubeconfig "$PRIVATE_KUBECONFIG" apply -f deploy/kubernetes-trial.yaml
kubectl --kubeconfig "$PRIVATE_KUBECONFIG" \
  --as system:serviceaccount:artifactory:artifactory-deployer \
  auth can-i patch deployments -n default
```

最后一条必须返回 `no`；同样检查该身份不能创建 CRD 或修改节点。初始化后，Secret 和 Helm 安装均使用该受限身份，而不是管理员身份。

```sh
python3 scripts/init-helm.py --namespace artifactory --release artifactory \
  --grpc-host 127.0.0.1 --output .local/artifactory-trial-secrets
kubectl --kubeconfig "$PRIVATE_KUBECONFIG" -n artifactory \
  --as system:serviceaccount:artifactory:artifactory-deployer apply \
  -f .local/artifactory-trial-secrets/credentials.json \
  -f .local/artifactory-trial-secrets/pki.json
helm upgrade --install artifactory charts/artifactory \
  --kubeconfig "$PRIVATE_KUBECONFIG" --namespace artifactory \
  --kube-as-user system:serviceaccount:artifactory:artifactory-deployer \
  -f charts/artifactory/values-trial.yaml --atomic --wait --timeout 15m
```

该 profile 为三副本控制平面、独立单副本 Source、PostgreSQL 和 S3；不是数据库 / 对象存储的全栈 HA。所有服务为 ClusterIP，不创建 Ingress、NodePort、CRD、ClusterRole 或 StorageClass。HTTP 仅用于明确批准的内网试验；正式上线需另行配置可信 HTTPS、备份和外部 HA 数据服务。

配额限定 4 CPU / 6 GiB requests、12 CPU / 12 GiB limits、40 GiB PVC 申请量；所有试部署 Pod 使用 `preemptionPolicy: Never`，不能抢占其他工作负载。不要通过提高其他工作负载优先级、驱逐他人 Pod 或修改存储组件来满足调度。网络策略只允许本命名空间、DNS 与公网 HTTP(S)；接入私有 Git / 模型前应另行批准精确出口。网络策略是否实际执行还需核对现有 CNI，不得为此替换集群网络。

```sh
kubectl --kubeconfig "$PRIVATE_KUBECONFIG" -n artifactory \
  port-forward --address 127.0.0.1 service/artifactory-web 38100:80
kubectl --kubeconfig "$PRIVATE_KUBECONFIG" -n artifactory \
  port-forward --address 127.0.0.1 service/artifactory-s3 38333:8333
```

控制台为 `http://127.0.0.1:38100`，文档为同域 `/docs`。验收包含每个控制副本的 `/readyz`、实际登录、CSRF、项目读取和文档；未经 Agent 注册及构建 / 下载验证，不得称为设备或完整构建链路验收。

失败时只使用本命名空间内的 Helm 历史与回滚；不得删除其他命名空间或集群组件。PVC 与外部 Secret 保留，禁止以删除整个命名空间作为例行回滚。保留初始非目标工作负载规格摘要并在部署后复核，区分他人的并发变更与本次操作。
