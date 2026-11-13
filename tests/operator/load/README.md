# Runtime 队列与卸压验证

`admission.py` 使用独立 PostgreSQL schema 写入 10,000 个运行并并发调用实际 Runtime 准入 API，断言 pool=8、每项目=2、其余 9,992 个仍未开始 deadline/消耗调用。只需设置一次性测试库 `ARTIFACTORY_TEST_DATABASE_URL`；证据保存在 `.local/operator-load/`，不输出连接凭据。

```sh
python3 tests/operator/load/admission.py
```

Kubernetes 端到端脚本验证固定源码与真实 Pi/Go 构建，记录 Source 下载字节、业务库源码字节为零、业务表中规划 calls/周期事件为零，以及持有模型请求期间 30 个业务 API 样本的 P95。该观察是小负载验证，不能当作生产容量承诺。

`compare.py` 在同一已保存的隔离安装中运行真实 Poll Worker 与 Kubernetes Pi。默认固定随机种子生成 8 MiB 数据，保持相同 commit、4 并发、60 秒模型等待；记录各服务 kubelet CPU/RSS/网络、三库连接与事务计数、业务 P95 和源码下载字节。资源 delta 只在同一存活 Pod UID 内计算；本地试验阈值为 overview P95 < 1 秒、无错误，不代表生产 SLO。`.local/operator-load/20261112-184744-compare/evidence.json` 是首次完整通过的对照。

```sh
python3 tests/operator/load/compare.py --context <saved-context> --namespace <saved-namespace>
python3 tests/operator/load/active.py --context <saved-context> --namespace <saved-namespace> --active 100 --queued 10
```

`active.py` 验证实际 Pi Pod（每个运行真实 Node/Pi 循环并在模型请求等待），检查活动配额、排队预算、Runtime/Operator 重启后的累计预算与固定源码，再并发取消并观察授权撤销和 Pod 清理。恢复检查等待旧 Runtime/Operator Pod UID 全部消失，确认每个原 run 在新 Runtime 新增模型调用预留，并再保持 15 秒稳定；仅 Deployment rollout 完成不算恢复。记录业务 API、各服务及 etcd/apiserver 的 CPU/RSS/网络、PG 连接/等待、AgentRun 数量及 Kubernetes API 变更计数。启动前根据 Pod 上限和请求内存检查容量；默认 25m/64Mi 是本地 fixture 请求值，生产资源要按真实模型和上下文大小重测。100/500/1,000 档位需要各自的真实资源，不能用数据库排队数代替活动 Pod。

10,000 排队的完整准入验证使用 `admission.py`；活动脚本可另设 `--queued` 验证未准入运行不产生同量 Pod/CRD。超过本机资源时应使用更大的独立测试集群，保留实际档位、采样窗口、故障与取消证据。
