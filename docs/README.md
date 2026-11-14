# Artifactory 文档

面向个人与内部可信团队的跨平台应用构建、签名与内部分发平台。

**只读 Git 项目 → 固定源码快照 → AI 执行计划 → 原生节点构建 → 签名与校验 → 私有制品 → 不可变 Release。**

[返回控制台](/) · [GitHub 仓库](https://github.com/zhubby/artifactory)

## 从这里开始

| 目标 | 阅读 |
|---|---|
| 接入项目并开始构建 | [零侵入 AI 构建](ai-builds.md) |
| 部署独立实例、注册构建节点 | [部署与运维](operations.md) |
| 使用 Kubernetes 或本机 kind | [Helm 部署](helm.md)、[Operator 运维](operator-operations.md) |
| 配置多副本或迁移数据库 | [控制平面高可用](control-ha.md)、[Toasty 迁移](toasty-migration.md) |
| 了解服务职责和目录布局 | [模块布局与命名](module-layout.md) |
| 升级节点、查询时序指标 | [Emissary OTA](emissary-ota.md)、[时序观测](telemetry.md) |
| 集成 HTTP API 或设备协议 | [HTTP API](api.md)、[Emissary 协议](protocol.md)、[Runtime / Source](runtime-protocol.md) |
| 确认可用范围与实际测试结果 | [实施与验收记录](acceptance.md) |

## 如何使用这份文档

主要手册按使用、部署、接口组织。目录中的验收记录描述各自日期、提交和环境下的实际结果；设计与计划归档保留当时的方案和待办，不能视为当前功能清单。当前源码、现行手册与历史计划不一致时，以源码和相应版本的实际验收为准。

文档随 Web 镜像构建和发布，部署后通过控制台同域名的 `/docs` 免登录阅读。阅读文档不依赖控制平面 API；部署方式和本地维护命令见[文档站点](operations.md#文档站点)。

目录可切换章节；工具栏提供搜索、主题切换和打印。搜索 `Emissary`、`Helm`、`S3` 等关键词可快速定位相关内容。品牌分区提供 Logo 图片、完整文件包和字体许可。
