import { runTask, safeError, type Task, type Rpc } from "./runtime.js";

let sequence = 0;
const pending = new Map<number, { resolve: (value: Record<string, unknown>) => void; reject: (error: Error) => void }>();
const controller = new AbortController();
let started = false;
const rpc: Rpc = (method, payload) => new Promise((resolve, reject) => {
  const id = ++sequence;
  pending.set(id, { resolve, reject });
  process.send?.({ type: "rpc", id, method, payload });
});
process.on("disconnect", () => { controller.abort(); process.exit(1); });
process.on("SIGTERM", () => controller.abort());
process.on("message", (value: unknown) => {
  const message = value as { type?: string; id?: number; error?: string; result?: Record<string, unknown>; task?: Task };
  if (message.type === "cancel") { controller.abort(); return; }
  if (message.type === "reply" && message.id !== undefined) {
    const request = pending.get(message.id); pending.delete(message.id);
    if (message.error) request?.reject(new Error(message.error)); else request?.resolve(message.result || {});
  }
  if (message.type === "start" && message.task && !started) {
    started = true;
    const task = message.task;
    void runTask(task, rpc, controller.signal).catch(async (error) => {
      // Shutdown, lost connectivity and server cancellation leave the leased
      // run to the server. A restart must not turn recoverable work into failure.
      if (controller.signal.aborted) return;
      try { await rpc("report", { kind: "failed", payload_json: JSON.stringify({ error: safeError(error, task.model.api_key) }) }); } catch { /* A canceled/reassigned task cannot report. */ }
    }).finally(() => {
      process.send?.({ type: "done" }, () => { process.disconnect(); });
    });
  }
});
