# 执行配方 v1 与历史配置

所有新构建使用 [零侵入 AI 构建](ai-builds.md)：平台保存目标和说明，Pi 生成 `ExecutionPlan v1`，其 `recipe` 复用下述校验规则。无需在原仓库创建配置文件。以下 YAML 仅用于理解内部配方和迁移前历史记录，不是新构建入口。

历史构建保留仓库根目录 `artifactory.yaml`（最大 256 KiB）的原始 YAML、解析快照、参数和所选目标。已解析历史任务重试使用原计划和提交，不重新解析移动分支。

编辑器 schema：`schemas/artifactory.v1.schema.json`。真正的跨字段限制以 `Recipe::parse` 为准：阶段必须按 build → sign → verify 排序；签名必须同时配置授权引用、sign 阶段和 verify 阶段；步骤超时不能大于目标超时。未知字段和 schema 版本被拒绝。

```yaml
version: 1
parameters:
  CHANNEL: preview
release:
  enabled: true
targets:
  linux-amd64:
    os: linux
    arch: amd64
    executor: native
    labels: {zone: internal}
    capabilities: [cargo]
    timeout_seconds: 7200
    steps:
      - name: build
        run: cargo build --release --locked && mkdir -p dist && cp target/release/myapp dist/myapp
      - name: verify
        phase: verify
        run: ./dist/myapp --version
    artifacts: [dist/myapp]
```

## 字段与语义

- `os` 是 **Emissary 宿主**，只接受 `linux`、`darwin`、`windows`；Android/iOS 是应用目标名，不是新的 Emissary OS。
- `arch` 为宿主 `amd64` / `arm64`。需要特定产物架构时由节点标签、能力和项目命令明确约束，不能从宿主架构推断制品兼容性。
- `executor` 本版仅为 `native`。容器执行环境与未来 OCI 制品类型分别扩展。
- `capabilities` 是工具版本 / 帮助命令可执行性探测的约束，失败的占位程序不进入能力集合；仍不证明 SDK 完整、授权有效或真实构建成功。默认额外要求 Git 和每一步所用 shell 可用。
- Unix 默认 `bash`，Windows 默认 `powershell`；允许显式 `sh`、`bash`、`pwsh`、`powershell`。工具链需要预装，Emissary 不自动安装。
- `labels` 必须精确匹配管理员设置的节点标签。项目可用节点池由中心项目授权确定，执行计划不能扩大授权。
- `parameters` 为非敏感字符串默认值，名称匹配 `[A-Z][A-Z0-9_]{0,63}`，值最大 4096 字节，传入 `ARTIFACTORY_PARAM_<NAME>`。API 不能传入未声明参数。
- 内置环境包括 `ARTIFACTORY_COMMIT_SHA`、`ARTIFACTORY_BUILD_ID`、`ARTIFACTORY_TARGET`。不得把密码、Token、签名私钥放进参数或打印到日志。
- 目标默认 7200 秒，最大 86400 秒；步骤默认 3600 秒。缩短目标超时时必须同步指定更短的步骤超时。
- 步骤顺序执行；错误停止该目标。不同目标独立调度，不支持任意 DAG、步骤条件或 Actions `uses:`。

## 制品

`artifacts` 是相对于本次 checkout 的 doublestar glob；每一条都必须至少匹配一个文件。拒绝绝对路径、`.` / `..` 路径段、反斜杠、冒号、符号链接及非普通文件；工作区按 attempt 分开。目录需要先打包；不收集 `.git`。

单个文件大小范围 1 byte 到 20 GiB。Emissary 用同一打开描述符计算摘要并上传；服务端再次检查字节数与 SHA-256，把验证过的字节写到只读下载的独立 final key。旧 staging PUT 地址不能覆盖已发布内容。临时校验文件使用控制面数据目录，需要足够磁盘空间。

目前不实现内容去重、构建缓存或自动对象保留清理；运维不能随意清除已被 Release 引用的对象。

## 签名与发布

`signing_profiles` 只包含引用名。项目授权、节点授权、本机 Emissary 的 `signing_profiles` 三者都需要匹配；私钥不上传中心。本机配置中对应的环境变量仅在 sign / verify 步骤注入，但**同账号可信宿主执行不构成秘密沙箱**。

本版每个所选目标都视为必需目标。所有目标、步骤、签名 / 校验和制品登记成功后，填写版本的构建才自动发布。`release.enabled` 在历史配方中是仓库许可。新 AI 构建由 API 的显式发布请求及调用者 release 权限控制，模型不能开启发布。

Release 一经生成不更新、删除或覆盖；同项目版本唯一。构建结果与发布结果分离，版本冲突表现为构建成功、发布失败。发布重试复用制品；重试带自动发布版本的失败目标同样需要 release 权限。

`examples/` 是需要按真实项目、SDK、证书与分发方式适配的模板，不代表五种目标已经通过设备或商店验收。

## Control 多副本

连接数、制品校验并发、调度批量、排空、独立 Source 和统一 OTA 配置见 [Control 多副本配置](control-ha.md#存储和连接预算)。这些预算按每进程计算；Helm 的 `values-ha.yaml` 包含三副本示例。
