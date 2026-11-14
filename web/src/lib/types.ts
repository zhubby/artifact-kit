export interface User { id: string; username: string; role: "admin" | "operator" | "viewer" }
export interface AuthConfig { registration_enabled: boolean }
export interface Project { model_profile_id?: string | null; build_context?: string; source_credentials?: {repository_url: string; credential_id: string}[]; id: string; name: string; repository_url: string; credential_id: string | null; pool_ids: string[]; signing_profiles: string[]; build_count: number; last_build: { id: string; status: string; created_at: string } | null }
export interface StepRun { step_index: number; name: string; phase: string; status: string; exit_code: number | null; started_at: string; finished_at: string | null }
export interface Attempt { id: string; node_id: string; node_name: string; status: string; lease_expires_at: string; error: string | null; started_at: string | null; finished_at: string | null; steps: StepRun[] }
export interface Job { placement?: { reason: string | null; selected_node: string | null; selected_reason: string | null; placement: {candidates: {node_id: string; reason: string}[]} } | null; id: string; target: string; status: string; target_config: { os: string; arch: string; steps: { name: string; phase: string }[] }; attempts: Attempt[] }
export interface Artifact { id: string; path: string; size: number; sha256: string; target: string; created_at: string; project_name?: string; build_id?: string; attempt_status?: string }
export interface Build { goal?: string | null; source_snapshot_id?: string | null; current_plan_id?: string | null; revision_count?: number; clarification?: {question: string} | null; id: string; project_id: string; project_name: string; requested_ref: string; commit_sha: string | null; status: string; publish_status: string; publish_error: string | null; error: string | null; release_version: string | null; created_at: string; finished_at: string | null; jobs?: Job[]; targets?: { id: string; target: string; status: string }[]; artifacts?: Artifact[]; config_yaml?: string; config_snapshot?: Record<string, unknown>; publish_attempts?: { id: string; status: string; error: string | null }[] }
export interface NodeTool {
  available?: boolean | null;
  path?: string | null;
  version?: string | null;
  [key: string]: unknown;
}
export interface Node {
  id: string;
  name: string;
  os: string;
  arch: string;
  pool_id: string;
  pool_name: string;
  labels: Record<string, string>;
  capabilities: string[];
  signing_profiles: string[];
  slots: number;
  active_jobs: number;
  draining: boolean;
  revoked: boolean;
  online: boolean;
  last_seen: string | null;
  created_at?: string | null;
  certificate_expires_at: string;
  emissary_target_release?: string | null;
  emissary_desired_release?: string | null;
  emissary_current_version?: string | null;
  emissary_launcher_version?: string | null;
  emissary_ota_status?: Record<string, unknown> | null;
  emissary_pool_paused?: boolean;
  ota_draining?: boolean;
  diagnostics: {
    hostname?: string;
    emissary_version?: string;
    emissary_commit?: string;
    protocol_major?: number;
    configured_signing_profiles?: string[];
    buffered_log_bytes?: number;
    metrics?: {
      logical_cpus?: number;
      emissary_rss_bytes?: number;
      managed_free_bytes?: number;
      [key: string]: unknown;
    };
    tools?: Record<string, NodeTool>;
    [key: string]: unknown;
  };
}
export interface NodeMetadataSnapshot {
  node_id?: string;
  startup_id: string | null;
  digest?: string | null;
  received_at: string | null;
  metadata: Record<string, unknown>;
}
export interface NodeAccelerator {
  id: string;
  vendor?: string | null;
  model?: string | null;
  driver?: string | null;
  memory_total_bytes?: number | null;
}
export interface NodeInventory {
  os?: string;
  arch?: string;
  emissary_version?: string;
  os_version?: string | null;
  kernel_version?: string | null;
  boot_time?: number | null;
  cpu_model?: string | null;
  logical_cpus?: number | null;
  physical_cores?: number | null;
  physical_memory_bytes?: number | null;
  effective_cpu_limit?: number | null;
  effective_memory_limit_bytes?: number | null;
  container?: string | null;
  virtualization?: string | null;
  accelerators?: NodeAccelerator[];
  tools?: Record<string, NodeTool>;
  capabilities?: string[];
  signing_profile_refs?: string[];
  collectors?: Record<string, string>;
  [key: string]: unknown;
}
export interface NodeHostSample {
  cpu_percent?: number | null;
  memory_total_bytes?: number | null;
  memory_used_bytes?: number | null;
  available_memory_bytes?: number | null;
  build_disk_available_bytes?: number | null;
  swap_total_bytes?: number | null;
  swap_used_bytes?: number | null;
  load_one?: number | null;
  active_attempts?: number | null;
  disks?: { device: string; mount: string; total_bytes?: number | null; available_bytes?: number | null; read_bytes?: number | null; written_bytes?: number | null }[];
  networks?: { interface: string; received_bytes?: number | null; transmitted_bytes?: number | null; receive_errors?: number | null; transmit_errors?: number | null }[];
  accelerators?: { id: string; utilization_percent?: number | null; memory_used_bytes?: number | null }[];
}
export interface NodeTelemetry {
  host: {
    node_id?: string;
    startup_id?: string | null;
    session_id?: string | null;
    sampled_at?: string | null;
    received_at?: string | null;
    fresh?: boolean;
    dropped_batches?: number | null;
    sample: NodeHostSample;
    [key: string]: unknown;
  } | null;
  inventory: {
    sampled_at?: string | null;
    digest?: string | null;
    inventory: NodeInventory;
  } | null;
}
export interface Release { id: string; project_id: string; project_name: string; version: string; build_id: string; commit_sha: string; created_at: string; artifact_count: number; artifacts?: Artifact[] }
export interface Pool { id: string; name: string; emissary_target_release?: string | null; emissary_update_concurrency?: number; emissary_updates_paused?: boolean }
export interface Credential { id: string; name: string; kind: string; created_at: string }
export interface Token { id: string; name: string; scopes: string[]; project_ids: string[]; expires_at: string; revoked: boolean }
export interface Audit { id: number; actor: string; action: string; resource_id: string; created_at: string }
export interface Overview { nodes_registered?: number; projects: number; nodes_online: number; running: number; queued: number; releases: number; builds_today: number }

export interface EmissaryRelease { id: string; version: string; created_at: string }
export interface EmissaryUpdate { id: string; node_id: string; node_name: string; version: string; previous_version: string | null; status: string; error: string | null; cancel_requested: boolean; finished_at: string | null; updated_at: string }

export interface ModelProfile { id: string; name: string; api: string; base_url: string; model: string; has_key: boolean; enabled: boolean; is_default: boolean; max_calls: number; timeout_seconds: number; revision: number; parameters: { context_window?: number; max_tokens?: number; thinking_level?: string; temperature?: number; requests_per_minute?: number } }
export interface AIWorker { id: string; name: string; online: boolean; draining: boolean; revoked: boolean; active_run: string | null; last_seen: string | null; certificate_expires_at: string }
export interface PlanRevision { id: string; revision: number; sha256: string; created_at: string; contents: { summary: string; evidence: {path: string; reason: string}[]; scripts: {name: string; content: string}[]; recipe: {targets: Record<string, {os: string; arch: string; steps: {name: string; phase: string; run: string}[]}>} } }
export interface PlanningHistory { plans: PlanRevision[]; runs: {id: string; state: string; purpose: string; calls: number; error: string | null; usage: {totalTokens?: number; cost?: number}}[]; events: {id: number; kind: string; payload: Record<string, unknown>; created_at: string}[] }
