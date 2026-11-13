# Artifactory Emissary Rust port

The device runtime is rewritten in Rust. Pi remains the Node.js planning runtime.
There is no Go executable, old protocol alias, configuration converter or migration layer.
The transport namespace is `artifactory.emissary.v1`, the executable and native service
are `artifactory-emissary`, and the device configuration is `emissary.json`.

## Behavioral baseline and module map

The pre-port Go suite passed on macOS arm64 on 2026-11-12. Live/native tests in
that suite were opt-in; this baseline does not claim they ran.

| Original Go API | Rust API | Contract |
| --- | --- | --- |
| config.Config, Load, Save | config::Config::{load,save} | strict configuration and private atomic records |
| client.Dial, Enroll | client::{dial,enroll,control} | private P-256 identity, pinned TLS 1.3 and mTLS |
| executor.SafeRelative, Collect, OpenArtifact | artifacts::{safe_relative,collect,open} | confined regular files and one descriptor for hashing/upload |
| executor.ProcessTree, StartProcess, Kill, Close | process::{spawn,wait}, owned ChildWrapper | process-group / Job Object lifetime and cancellation |
| executor.Lease, NewLease, Renew, Expired | worker::Lease::{new,renew,expired} | local monotonic deadline anchored to the original request |
| worker.Input, Event, Run, logWriter.Write | worker::{Input,Event,run,Emitter}, process::read_log | isolated worker and bounded event pipe |
| worker.Askpass | main askpass entry, source credential environment | private Git credentials excluded from build steps |
| worker source preparation | source::{checkout,lock,build_environment} | immutable checkout, cache locks, LFS copies and local signing |
| agent.Runner, Run, Doctor | runner::run, doctor::{doctor,metrics} | acknowledged events, bounded disk spool and working capabilities |
| ota.Config, Image, State, Request, Health, HealthWindow | ota::{OtaConfig,Image,State,Request,Health,HealthWindow} | durable generation fence, drain, health and rollback |
| ota.State.Recover, HealthWindow.Ack | ota::State::recover, ota::HealthWindow::ack | interrupted activation and consecutive session confirmations |
| ota.Atomic, Read, Load, Snapshot, Submit, Confirm | config::{atomic_json,read_json}, ota::{load,snapshot,submit,confirm} | bounded, atomic private records |
| ota.MaxBinarySize, Asset, Manifest, Verify, VerifyBytes | ota::{MAX_BINARY_SIZE,Asset,Manifest,verify,verify_bytes} | signed manifest and exact size/hash verification |
| ota.Initialize, Supervise | ota::{initialize,supervise} | signed executable replacement and rollback |
| cmd/artifactory-agent | main, service::{manage,launchd,systemd} | CLI, foreground, systemd / launchd / SCM lifecycle |
| cmd/agent-release | bin/emissary-release | five-platform Ed25519 release manifest |

## Test mapping

Integration test targets are files in `tests/`. Related live tests share a fixture
and retain separate scenario functions; native service acceptance remains opt-in.

| Original test | Rust test target/name |
| --- | --- |
| TestBoundedSpoolDoesNotSkipSequenceOnOverflow | runner::tests::bounded_spool_does_not_skip_sequence_on_overflow |
| TestDuplicateAssignmentAndDelayedRenewalDoNotExtendLease | runner::tests::duplicate_assignment_and_delayed_renewal_do_not_extend_lease |
| TestOTADrainWaitsForTerminalAckWorkersAndLogs | runner::tests::ota_drain_waits_for_terminal_ack_workers_and_logs |
| TestOTAHealthRequiresCurrentOutstandingHeartbeat | runner::tests::ota_health_requires_current_outstanding_heartbeat |
| TestDoctorOnlyAdvertisesWorkingToolchains | doctor::tests::only_advertises_working_toolchains (Unix) |
| TestCollectRejectsEscapesAndSymlinks | contracts::collect_rejects_escapes_and_symlinks |
| TestLeaseUsesLocalMonotonicDeadline | contracts::lease_uses_local_monotonic_deadline |
| TestJobObjectTerminatesDescendants | process_tree::process_tree_terminates_descendants (Windows) |
| TestPlatformCheckoutReusesObjectsAndIsolatesAttempts | source::checkout_reuses_objects_and_isolates_attempts |
| TestBuildEnvironmentAndLFSFilesAreIndependent | source::build_environment_and_lfs_files_are_independent |
| TestWorkerKillsProcessTreeOnParentDeathAndLeaseExpiry | process_tree::worker_kills_process_tree_on_parent_death_and_lease_expiry (Unix, also SIGTERM) |
| TestSignedPackageVerification | contracts::signed_package_verification |
| TestInterruptedActivationRollsBackOnce | contracts::interrupted_activation_rolls_back_once |
| TestHealthRequiresConsecutiveSessionConfirmations | contracts::health_requires_consecutive_session_confirmations |
| TestInterruptedDownloadNeverReplacesCurrentBinary | ota_download::interrupted_download_never_replaces_current_binary |
| TestServiceRunExitsWhenAgentCannotStart | process_tree::service_run_exits_when_emissary_cannot_start |
| TestLaunchdAllowsSupervisorCleanup | service::tests::launchd_allows_supervisor_cleanup |
| TestForegroundAgentStopsActiveProcessTree | runtime::foreground_and_ota_shutdown_stop_authenticated_worker_tree; business native acceptance also kills the parent |
| TestServiceStopsAuthenticatedAgent | native_service::native_service_lifecycle |
| TestNativeServiceLifecycle | native_service::native_service_lifecycle (disposable GitHub-hosted machine) |
| TestOTASupervisorMTLSUpgradeAndRollback | runtime::ota_supervisor_mtls_upgrade_and_rollback |
| TestOTASupervisorStopsActiveWorker | runtime::foreground_and_ota_shutdown_stop_authenticated_worker_tree |
| TestLiveRustGoProtocol | live_protocol::live_emissary_protocol → basic, storage_stalls |
| TestLiveRustGoProtocolCancellationFencing | live_protocol → cancellation (8 lock scenarios) |
| TestLiveRustGoProtocolLeaseFencing | live_protocol → authorization (heartbeat) |
| TestLiveRustGoProtocolEventFencing | live_protocol → authorization (accept/finished; 12 authorization scenarios total) |
| TestLiveRustGoProtocolReplayedCompletion | live_protocol → replay_completion |
| TestLiveRustGoProtocolArtifactFencing | live_protocol → artifact_fence (10 lock scenarios) |
| TestLiveRustGoProtocolArtifactConcurrency | live_protocol → concurrent_artifacts (8 preparations, then 17 mixed requests including heartbeat) |

Additional Rust tests cover strict configuration, TLS 1.3 / CA pinning, escaped
service paths, cancellation overtaking an OTA apply request, and a slow peer's
acknowledgements allowing all step and terminal events to complete.

## Deliberate differences

- Device names and transport identifiers change together; existing registrations
  are not read through an alias. Development test databases start from fresh schema.
- Metrics report resident memory (`emissary_rss_bytes`) instead of Go heap usage.
- Private worker/spool records encode byte arrays as JSON arrays; no converter is provided.
- Tokio monotonic time and owned cancellation replace goroutines and contexts.
- Event delivery sends new events once per connection and replays unacknowledged
  events every five seconds or after reconnecting. Replaying the full pending
  queue after every acknowledgement caused a real business-flow timeout; a
  delayed-acknowledgement test failed before this fix and passes afterward.
- Unix process groups and suspended Windows Job Objects preserve descendant cleanup.
- Build toolchains may still include Go; Go is not an Emissary implementation dependency.

## Preserved behavior

- Duplicate assignments never restart a worker or extend its lease. A delayed
  renewal uses the request send time, and cannot revive an expired attempt.
- Ordinary steps receive a clean toolchain environment; signing variables are
  injected only for sign/verify steps with authorized local profile references.
- OTA activation waits for workers, terminal acknowledgements and buffered logs.
  Three consecutive current-session health confirmations accept a replacement.
- Started attempts lost to lease expiry require an explicit retry. Source snapshots,
  final artifacts and accepted plans retain their immutable identities.

## Validation

See [the dated Rust Emissary acceptance record](../../docs/acceptance/emissary/2026-11-13.md)
for executed commands, real business/Kubernetes fixtures and platform boundaries.
Ordinary macOS arm64 Rust tests include real foreground workers, process cleanup,
Git/LFS, signed OTA replacement/rollback and strict TLS. Live protocol acceptance
uses real Control, PostgreSQL and private S3, including actual storage pauses.

Native system services on Linux/macOS/Windows require the five-platform CI matrix;
they were not installed on the developer host. Historical Go service runs do not
prove the new Rust implementation passed those native environments. OTA uses test
keys and executable bytes, not a formally published or OS-signed release.
