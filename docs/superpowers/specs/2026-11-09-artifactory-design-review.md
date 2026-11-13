# Artifactory 设计与实现契约审阅

审阅基线：2026-11-09 用户确认的设计；实施审阅：2026-11-10。由本次主线程顺序审阅，**不是第三方独立安全审计**。

| 检查项 | 处理与证据位置 |
|---|---|
| 宿主 OS 与应用目标不混淆 | Recipe 使用 linux / darwin / windows；Android / iOS 为目标配置，见 configuration.md |
| 前端不复制授权 / 调度 | 所有写入调用 Rust `/api/v1`；真实 HTTP 权限测试与只读浏览器测试 |
| 首次注册与后续身份分离 | Enrollment 允许 TLS 无客户端证书；Control 强制登记证书摘要、有效期及吊销检查 |
| 未开始 / 已开始的重派边界 | assign → accept → 持久化 running → start，两阶段授权；PostgreSQL 与 Worker 单调截止期限 |
| 网络与数据库等待不能复活租约 | grant 锚定 Agent 请求发出时间；持有 attempt 锁后再次检查 PostgreSQL 当前时钟 |
| 配置和源码保持一致 | 提交、YAML、解析配置、参数、源仓库及凭据引用均在 build 固定 |
| YAML 不扩大签名权限 | 源解析检查项目授权；排队分配再检查当前项目及节点签名授权；对应数据库回归测试 |
| 文件不逃逸或被旧 PUT 替换 | 受管 attempt 目录、OpenRoot / 同描述符、安全路径；staging 校验后独立 final key |
| 日志故障不可隐藏 | 连接分离、序列及摘要去重、有界磁盘缓冲、超额失败、浏览器丢线提示与 no-transform SSE；S3 IO 不持有租约锁，真实暂停存储后独立续租的红 / 绿回归 |
| 发布不可变且不冒充构建结果 | 独立 publish_status / publish_attempts、项目版本唯一、发布事务与幂等键 |
| Token 不间接扩大发布权限 | 带发布版本的 job retry 也要求 release scope；数据库回归测试 |
| 协议与数据有明确基线 | 版本化 proto、v1 JSON schema、0001_platform / 0002_source_snapshot 迁移 |
| 平台运维不被交叉编译替代 | 三类服务安装说明、手动排空升级、30 天证书重新注册；兼容性矩阵列明实机缺口 |

本次审阅发现并修复：HTTPS askpass 以错误的命令字符串执行、Worker context 数据竞争、SSE 经 Next gzip 缓冲、手动打开 Dialog 的焦点归还、移动端日志连接状态隐藏、仓库配置变更污染排队任务、签名撤权未重新检查、可撤销节点的反向恢复、build-only Token 借 retry 开启自动发布，以及 staging PUT 覆盖已验证对象的风险。

保留边界：可信代码可操作服务账户可访问的宿主资源；不做不可信仓库隔离，不自动配置 SDK / Keychain / Windows ACL，不提供 OIDC、自动续证、多实例 HA、自动 GC、商店发布。现场 SDK、应用签名、安装和备份恢复仍需单独验收。

后续证据与准确结果以 `docs/acceptance.md` 为准，不将 M0/M1/M2 或设计审阅等同于全平台验收完成。
