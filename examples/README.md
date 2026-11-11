# 目标适配模板

这些 YAML 是内部执行配方和历史任务的目标参考，**不要将其加入被构建仓库**。新构建在平台描述目标，由 Pi 生成执行计划；平台侧构建说明可参考这里的工具链、阶段和制品规则。示例不代表 artifactory 已完成对应设备或商店验收。

| 模板 | 所需宿主 / 工具链 | 项目适配 |
|---|---|---|
| linux-rust | Linux amd64 / Rust | `myapp` 可执行文件和版本检查 |
| macos-xcode | macOS arm64 / Xcode / codesign | 提供 `ci/sign-macos.sh`，按嵌套顺序签名；根据渠道增加 notarization / stapling |
| windows-rust | Windows amd64 / Rust / Windows SDK | 证书库身份、时间戳服务器及服务账户私钥权限 |
| android-gradle | Linux amd64 / JDK / Android SDK | Gradle wrapper、unsigned APK 路径、zipalign / apksigner / keystore |
| ios-xcode | macOS arm64 / Xcode / Keychain | 提供 build / export / verify 脚本，匹配 Team、entitlements、provisioning profile 和导出选项 |

iOS 的 verify 脚本应解包 IPA、对真实 `.app` 及嵌套代码执行原生签名校验、检查 profile / entitlements / bundle ID 与所选分发方式，而不是仅检查文件存在。Android 应在签名前完成要求的对齐，验证签名方案及目标 API 的兼容性。复杂项目的原生脚本由仓库维护，平台不猜测签名命令。

本机签名引用示例：`apple-release` → `ARTIFACTORY_APPLE_IDENTITY`，`windows-release` → 证书指纹与时间戳地址，`android-release` → 本机 keystore / 密码，`ios-internal` → 本机导出 plist 路径。所有引用都需中心项目授权、中心节点绑定和本机配置。

下载入口是内部制品目录，不绕过操作系统信任、应用证书、iOS 设备 / profile 限制或商店审核。不得把模板 schema 校验、SDK 探测、交叉编译标记为真实应用验收。
