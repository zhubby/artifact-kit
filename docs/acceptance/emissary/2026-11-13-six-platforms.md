# Emissary 六平台验收

日期：2026-11-13，Asia/Shanghai。基线为
`28b686fee77f77b328b14a078515b3d2b44ad8e3`，本记录对应其后的工作区修改。
这次验收独立于同日的 Rust 迁移和真实供应商记录。

## 支持与发布约束

支持矩阵调整为 Darwin、Windows、Linux 各自的 arm64 / amd64。
CI 和发布工作流均包含六个平台，Windows arm64 使用 `windows-11-arm`。
控制面只导入六资产完整、签名正确的 manifest；设备验证器按自身平台选取资产。
发布工作流要求每个平台通过普通 Rust 测试及实际服务生命周期测试。

发布仍需已存在的 `emissary-v*` 标签与受保护环境中的 Ed25519 发布密钥。
本轮构建使用 `native-validation-20261113` 标识工作区验证产物，不创建正式标签或发布。
基线 commit 不表示包含本轮未提交修改；应同时保存工作区 diff 的 SHA-256。

## 验收环境与当前结果

| 平台 | 实际环境 | 普通测试 | 系统服务 |
| --- | --- | --- | --- |
| Darwin arm64 | Mac mini，Apple M4，macOS 27.2 | 30 通过 / 2 ignored | launchd 通过（3.04 秒） |
| Darwin amd64 | 同一 Mac mini，Rosetta，x86_64 Mach-O | 30 通过 / 2 ignored | launchd 通过（5.84 秒） |
| Linux arm64 | Mac 上 OrbStack 2.2.5，Ubuntu 24.04.5 / aarch64，systemd PID 1 | 30 通过 / 2 ignored | systemd 通过（2.30 秒） |
| Linux amd64 | Windows 上 VirtualBox 7.2.22，Ubuntu 24.04 / x86_64，systemd PID 1 | 30 通过 / 2 ignored | systemd 通过（6.05 秒） |
| Windows amd64 | Intel i5-10210U，Windows 10 Pro 22H2 / 19045 | 23 通过 / 2 ignored | SCM 通过（25.71 秒） |
| Windows arm64 | Mac 上 QEMU 11.1.3 / HVF，Windows 11 Pro ARM64 / 26300.9457 | 23 通过 / 2 ignored | SCM 通过（16.89 秒） |

Rosetta 结果表示翻译执行验证，不能作为 Intel Mac 硬件原生通过的证据。
两个 Linux 环境均执行各自架构的 Linux 二进制与 systemd 服务；未用容器结果替代。
Windows arm64 的程序在 Windows amd64 主机交叉编译，然后在实际 ARM64 Windows 客体
执行；逐个检查测试 EXE 的 PE Machine 为 `0xaa64`，同时检查客体 OS 架构为 ARM64。
这证明实际运行链路，不把交叉编译称为 ARM64 主机上的原生编译。

Mac 安装了 OrbStack、UTM 5.0.8、QEMU、Protobuf 和 Rust 1.95.0。
UTM 的命令行在 SSH 会话返回 `-1743`，因此 Windows ARM 虚拟机使用无界面的 QEMU/HVF。
已验证移除虚拟 TPM 后 HVF / UEFI / NVMe 组合能够初始化；添加 `tpm-tis-device`
时 QEMU 的 HVF 内存映射报 `HV_BAD_ARGUMENT`。Windows ARM 使用 4 vCPU、6 GiB 内存
和独立的 80 GiB NVMe 磁盘。通过 DISM 部署 Windows 11 Pro 镜像索引 3，离线安装
VirtIO 驱动后以 UEFI 启动；本轮没有虚拟 TPM / Secure Boot 验收，不改变宿主安全策略。
安装 ISO 的 SHA-256 为
`8992f2d2cfd2fa647e579e9eb31fc21f34da8333c29b6170fde17d2ed6623e9c`。

Windows 上安装了 Visual Studio 2022 C++ Build Tools、Git 2.56.1、PowerShell 7.7.0、
Protobuf 36.2 和 Rust 1.95.0。VirtualBox 使用 NAT，不创建桥接或宿主网络适配器。
Ubuntu amd64 的 SSH 通过 Windows 环回端口转发，只接受本次生成的 SSH 公钥。

## 实际场景

普通测试运行 `cargo test --locked --offline -p artifactory-emissary --all-targets`，覆盖
固定 CA / TLS 1.3 / mTLS、租约和取消、进程树、Git/LFS、签名、下载中断、
OTA 成功升级、坏候选回滚及 generation fencing。
其中实时 Control / PostgreSQL / S3 入口和系统服务入口默认 ignored，不能计入普通测试通过数。

原生服务测试使用独立临时账号 `artifactory-ci`，实际安装 systemd / launchd / SCM 服务，
通过 mTLS 接收任务，启动长任务的子进程树，重启和停止服务后检查进程终止、
连接关闭、后续任务执行及卸载。路径刻意包含空格、`%n`、`%i` 和字面量 `$HOME`。
测试拒绝覆盖已有服务或账号，退出时删除自己的账号、服务和临时目录。

专用主机运行时显式设置：

```sh
ARTIFACTORY_NATIVE_SERVICE_TESTS=1 \
ARTIFACTORY_NATIVE_SERVICE_SCOPE=dedicated-host \
ARTIFACTORY_NATIVE_EXPECTED_PLATFORM=linux/amd64 \
cargo test --locked --offline -p artifactory-emissary \
  --test native_service -- --ignored --nocapture
```

账号与服务管理需要管理员权限；Unix 测试调用 `sudo -n`，macOS 本轮直接以 root
运行预先编译的测试程序。GitHub 路径保留 hosted runner、OS 和架构校验。
专用主机路径仍校验实际二进制的平台。

## 发现和修复

- **launchd 重启竞争：** 首次 Hello 成功，重启后没有新连接。`bootout` 返回时旧 job
  仍在退出，随后的 `kickstart` 可能作用于旧实例。现在停止操作有界等待 job 移除后
  才允许启动。原服务测试先失败，修复后 Darwin arm64 全生命周期通过。
- **服务测试阻塞消息确认：** 同步停止命令阻塞单线程测试 runtime，测试对端无法确认
  最终事件和日志。改用两个 runtime worker，实际服务退出时对端继续工作。
  Linux arm64、Linux amd64 和 Darwin arm64 均已通过。
- **Linux OTA 夹具过大：** Linux 的 DWARF 在 ELF 内，调试二进制达 141 MiB，
  被现有 128 MiB 限制正确拒绝。测试先去掉调试段，再以真实可执行文件完成
  签名、替换和回滚。产品大小限制保持 128 MiB，两个 Linux 场景复测通过。
- **诊断：** 服务 Worker 提前完成时直接报告其结果，避免只显示等待子进程超时。
- **Rosetta 工具链：** macOS 的系统 Git 经 `xcrun` 加载命令行工具；本机的
  `libxcrun.dylib` 只含 arm64 / arm64e，缺少 x86_64，导致翻译执行的服务无法获取源码。
  服务验收配置明确固定已安装的 Homebrew Git 路径后通过。Emissary 为 x86_64，
  其 Git 工具为 arm64；这不构成 Intel Mac / Intel Xcode 构建工具链的验收。
- **telemetry spool 锁释放：** 并发 fork 的子进程在 exec 前可能暂时保留锁文件的
  打开描述符，仅关闭父进程的文件不足以立即释放 `flock`。回归测试以重复描述符
  复现失败后，增加 spool 的显式解锁析构；同时验证持有者在存活期间仍拒绝第二个写入者。
- **Windows 测试路径：** `canonicalize` 引入的 `\\?\` 设备路径会被 Git for Windows
  拒绝，导致 SCM 夹具的本地仓库获取失败。夹具在 Windows 使用普通绝对路径，
  Unix 保留 canonicalize；launchd 文本断言也按当前平台的路径分隔符生成。

- **工具标准输入隔离：** 源码工具、能力探测和构建步骤显式使用空 stdin，避免读取
  Worker 的租约和取消控制管道。回归测试保持父输入管道打开，实际执行读取输入的工具；
  原实现超时，修复后收到 EOF。Unix 和 Windows 使用各自实际 shell 验证。
- **Windows PID 标记读取：** Win32 文件 API 接受混合路径分隔符，CMD 的 `type`
  对夹具生成的 `source/child.pid` 路径返回失败。测试改用直接文件读取，SCM 的重启、
  停止、后续任务和卸载全流程通过。
- **Windows ARM 运行库：** 全新 Windows 客体缺少 `VCRUNTIME140.dll`，测试程序
  以 `0xc0000135` 退出。验证 Microsoft Authenticode 签名并安装 ARM64 Visual C++ v14
  运行库后，完整测试和 SCM 通过。该运行库是当前 MSVC 构建的安装前置条件，已补入
  [安装说明](../../emissary-ota.md)。
- **Rosetta 复测记录：** 前序退出场景出现过 `runtime disconnected before event`。
  临时诊断代码已移除；最终源码的完整测试、连续五次同一退出场景和 launchd 均通过。
  早期偶发失败的根因未确认，本记录不把它描述为已确认修复。

## 发布产物验证

六个 release 可执行文件均已构建，并在对应目标环境实际执行 `version`。
产物标识为 `native-validation-20261113`；Windows ARM 的 release 在 ARM64 客体执行。
Rust 工作区 Clippy（全部目标和 feature）、Windows amd64 / arm64 的 Clippy、
rustfmt、Control 的六平台签名测试及工作区 rustdoc 均通过。

使用独立临时 Ed25519 密钥调用真实 `emissary-release` 生成完整 manifest。
验证每个文件的格式、架构、大小、SHA-256 和清单签名；再通过 Control 的真实
`ota::verify` 接纳清单，以及 Emissary 的真实 `ota::verify` / `verify_bytes`
逐个平台选择并校验文件。篡改清单或任一文件均被拒绝。

各构建主机上的 31 个运行时代码和依赖输入文件均与当前工作区一致，输入映射摘要为
`b1a96c831af2f8011f9fe9983036474538badc294e3ec1afb3216a6582530e0c`。
六资产、测试签名清单和输入映射位于
`.local/native-platforms/20261113/artifacts/`；验证报告为同级的 `artifact-verification.json`。
交付归档为 `.local/native-platforms/20261113/native-validation-20261113.tar.gz`，
仅收录上述六资产、公开测试公钥、清单、验收记录、验证摘要和源码差异。
归档内的 `PROVENANCE.json` 记录基线、当前 tracked diff、运行时输入和验收文档摘要，
`SHA256SUMS` 覆盖其余全部文件；SSH 密钥、测试签名私钥和 VM 安装配置不在归档内。
这是测试签名和发布准备，不等同于 macOS Developer ID / notarization 或 Windows
Authenticode 正式签名；未创建 Git 标签或正式 Release。

## 证据与边界

私有日志和安装文件位于 `.local/native-platforms/20261113/`，远端为各用户的
`native-platforms/`。这些目录包含临时凭据、SSH 密钥和 VM 安装配置，不能整体公开。
本记录不保存账号密码、签名下载 URL 或私钥。

本轮没有替换 Mac 已有的旧设备服务。原生验收创建的服务和临时账户已经清理。
请求创建的两个 Ubuntu 虚拟机和 Windows ARM 虚拟机保留供后续使用。
GitHub Actions 尚未运行这份未提交修改，表内结果来自本轮专用主机和客体执行。
