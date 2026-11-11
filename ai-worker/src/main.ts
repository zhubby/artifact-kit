#!/usr/bin/env node
import * as grpc from "@grpc/grpc-js";
import * as loader from "@grpc/proto-loader";
import { fork, spawn, type Serializable } from "node:child_process";
import { readFile, writeFile, mkdir, rename, chmod, rm } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

interface Configuration { endpoint: string; ca_file: string; certificate_file: string; key_file: string; server_name?: string; worker_id?: string; name?: string }
interface LeasedTask { run_id: string; lease_token: string; context_json: string; model_config_json: string; timeout_seconds: number; max_calls: number }
const here = dirname(fileURLToPath(import.meta.url));
const schema = loader.loadSync(resolve(here, "../../proto/artifactory/agent/v1/agent.proto"), { keepCase: true, defaults: true, longs: Number });
const definitions = grpc.loadPackageDefinition(schema) as unknown as { artifactory: { agent: { v1: Record<string, grpc.ServiceClientConstructor> } } };
const services = definitions.artifactory.agent.v1;

function call(client: grpc.Client, method: string, payload: unknown): Promise<Record<string, any>> {
  return new Promise((accept, reject) => {
    const fn = (client as unknown as Record<string, Function>)[method];
    if (!fn) return reject(new Error("invalid platform method"));
    fn.call(client, payload, { deadline: Date.now() + 30000 }, (error: grpc.ServiceError | null, result: Record<string, any>) => error ? reject(error) : accept(result));
  });
}
async function credentials(config: Configuration, enroll = false) {
  return grpc.credentials.createSsl(await readFile(config.ca_file), enroll ? undefined : await readFile(config.key_file), enroll ? undefined : await readFile(config.certificate_file));
}
async function command(binary: string, args: string[]) {
  await new Promise<void>((accept, reject) => {
    const child = spawn(binary, args, { stdio: "ignore" });
    child.on("error", reject); child.on("exit", (code) => code === 0 ? accept() : reject(new Error(`${binary} failed`)));
  });
}
async function enroll(config: Configuration, path: string) {
  let token = "";
  for await (const chunk of process.stdin) token += String(chunk);
  token = token.trim();
  if (!token) throw new Error("provide the one-time enrollment token on standard input");
  await mkdir(dirname(config.key_file), { recursive: true, mode: 0o700 });
  await mkdir(dirname(config.certificate_file), { recursive: true, mode: 0o700 });
  const temporaryKey = `${config.key_file}.enroll-${process.pid}`;
  const csr = `${temporaryKey}.csr`;
  await writeFile(temporaryKey, "", {mode: 0o600, flag: "wx"});
  await command("openssl", ["req", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-subj", "/CN=artifactory-ai-worker", "-keyout", temporaryKey, "-out", csr]);
  await chmod(temporaryKey, 0o600);
  const Client = services.PlannerEnrollment!;
  const client = new Client(config.endpoint, await credentials(config, true), config.server_name ? {"grpc.ssl_target_name_override": config.server_name} : {});
  try {
    const response = await call(client, "Enroll", { token, name: config.name || "artifactory-ai-worker", csr_pem: await readFile(csr, "utf8") });
    const temporaryCertificate = `${config.certificate_file}.enroll-${process.pid}`;
    await writeFile(temporaryCertificate, response.certificate_pem, { mode: 0o600, flag: "wx" });
    await rename(temporaryKey, config.key_file); await rename(temporaryCertificate, config.certificate_file);
    config.worker_id = response.agent_id;
    const temporary = `${path}.tmp-${process.pid}`;
    await writeFile(temporary, JSON.stringify(config, null, 2), { mode: 0o600 }); await rename(temporary, path);
    console.log(`AI worker enrolled: ${config.worker_id}`);
  } finally { client.close(); await rm(csr, {force: true}); await rm(temporaryKey, {force: true}); }
}

async function execute(client: grpc.Client, task: LeasedTask, stopping: AbortSignal) {
  const safeEnvironment: NodeJS.ProcessEnv = {};
  for (const key of ["PATH", "HOME", "LANG", "LC_ALL", "TZ", "TMPDIR", "TEMP", "TMP", "SystemRoot", "NODE_EXTRA_CA_CERTS", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"]) if (process.env[key]) safeEnvironment[key] = process.env[key];
  const child = fork(resolve(here, "task.js"), [], { env: safeEnvironment, stdio: ["ignore", "ignore", "ignore", "ipc"] });
  const send = (message: Serializable) => { if (child.connected) child.send(message, () => {}); };
  let ended = false;
  let lastHeartbeat = Date.now();
  const cancel = () => { if (!ended) { send({ type: "cancel" }); const kill = setTimeout(() => child.kill("SIGKILL"), 2500); kill.unref(); } };
  stopping.addEventListener("abort", cancel, { once: true });
  const heartbeat = setInterval(() => {
    void call(client, "Heartbeat", { run_id: task.run_id, lease_token: task.lease_token }).then((result) => {
      lastHeartbeat = Date.now(); if (result.canceled) cancel();
    }).catch(() => { if (Date.now() - lastHeartbeat > 60000) cancel(); });
  }, 10000);
  const deadline = setTimeout(cancel, (task.timeout_seconds + 5) * 1000);
  child.on("message", (value: unknown) => {
    const message = value as { type?: string; id?: number; method?: string; payload?: Record<string, unknown> };
    if (message.type !== "rpc" || message.id === undefined || !["tool", "report"].includes(message.method || "")) return;
    const method = message.method === "tool" ? "Tool" : "Report";
    void call(client, method, { ...message.payload, run_id: task.run_id, lease_token: task.lease_token }).then((result) => {
      send({ type: "reply", id: message.id, result: JSON.parse(result.result_json || "{}") });
    }).catch((error: grpc.ServiceError) => {
      send({ type: "reply", id: message.id, error: error.code === grpc.status.RESOURCE_EXHAUSTED ? "model call limit reached" : "platform request rejected or planning lease expired" });
      if ([grpc.status.UNAUTHENTICATED, grpc.status.FAILED_PRECONDITION].includes(error.code)) cancel();
    });
  });
  send({ type: "start", task: { context: JSON.parse(task.context_json), model: JSON.parse(task.model_config_json), timeout_seconds: task.timeout_seconds, max_calls: task.max_calls } });
  if (stopping.aborted) cancel();
  try { await new Promise<void>((accept, reject) => { child.once("exit", () => accept()); child.once("error", reject); }); }
  finally { ended = true; clearInterval(heartbeat); clearTimeout(deadline); stopping.removeEventListener("abort", cancel); }
}

async function main() {
  if (Number(process.versions.node.split(".")[0]) !== 24) throw new Error("artifactory-ai-worker requires Node.js 24");
  const argument = process.argv.indexOf("--config");
  const path = resolve(argument < 0 ? process.env.ARTIFACTORY_AI_CONFIG || "ai-worker.json" : process.argv[argument + 1] || "ai-worker.json");
  const controller = new AbortController();
  process.once("SIGINT", () => controller.abort()); process.once("SIGTERM", () => controller.abort());
  let config: Configuration;
  try { config = JSON.parse(await readFile(path, "utf8")) as Configuration; }
  catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT" || !process.env.ARTIFACTORY_AI_ENDPOINT || !process.env.ARTIFACTORY_AI_CA_FILE) throw error;
    config = { endpoint: process.env.ARTIFACTORY_AI_ENDPOINT, ca_file: process.env.ARTIFACTORY_AI_CA_FILE,
      certificate_file: "worker.pem", key_file: "worker.key", name: process.env.ARTIFACTORY_AI_NAME || "artifactory-ai-worker", server_name: process.env.ARTIFACTORY_AI_SERVER_NAME };
    await mkdir(dirname(path), {recursive: true, mode: 0o700});
    try { await writeFile(path, JSON.stringify(config, null, 2), {mode: 0o600, flag: "wx"}); }
    catch (error) { if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error; }
    config = JSON.parse(await readFile(path, "utf8")) as Configuration;
  }
  for (const key of ["ca_file", "certificate_file", "key_file"] as const) config[key] = resolve(dirname(path), config[key]);
  if (process.argv.includes("enroll")) {
    if (config.worker_id) throw new Error("worker already enrolled; revoke it before creating a new identity");
    return enroll(config, path);
  }
  if (!config.worker_id) console.log("AI worker waiting for enrollment through the platform");
  while (!config.worker_id && !controller.signal.aborted) {
    try { await delay(2000, undefined, { signal: controller.signal }); } catch { return; }
    config = JSON.parse(await readFile(path, "utf8")) as Configuration;
  }
  if (controller.signal.aborted) return;
  for (const key of ["ca_file", "certificate_file", "key_file"] as const) config[key] = resolve(dirname(path), config[key]);
  const Client = services.Planning!;
  const client = new Client(config.endpoint, await credentials(config), { "grpc.max_receive_message_length": 2 * 1024 * 1024, "grpc.max_send_message_length": 768 * 1024, ...(config.server_name ? {"grpc.ssl_target_name_override": config.server_name} : {}) });
  try {
    while (!controller.signal.aborted) {
      try {
        const task = await call(client, "Poll", {}) as LeasedTask;
        if (task.run_id) await execute(client, task, controller.signal);
      } catch { console.error("AI worker connection unavailable; retrying"); }
      try { await delay(2000, undefined, { signal: controller.signal }); } catch { break; }
    }
  } finally { client.close(); }
}
void main().catch((error) => { console.error(error instanceof Error ? error.message : "AI worker failed"); process.exitCode = 1; });
