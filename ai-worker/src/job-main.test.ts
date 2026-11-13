import test from "node:test";
import assert from "node:assert/strict";
import { runtimeRpc, reportTerminalFailure } from "./transport/runtime.js";
import { checkpoint, restoreTaskCheckpoint, type Task } from "./runtime.js";

test("a lost submission acknowledgement queries the same proposal without new inference", async () => {
  const requests: string[] = [];
  let proposal = "";
  const post = async (path: string, body: Record<string, unknown>) => {
    requests.push(path);
    if (path === "/v1/tool") { proposal = String(body.request_id); throw new TypeError("connection lost after commit"); }
    assert.equal(body.proposal_id, proposal);
    return { accepted: true, plan_revision_id: "original" };
  };
  const rpc = runtimeRpc({ run_id: "run", generation: 1, uid: "pod", token: "grant" }, "https://source.invalid", post, new AbortController().signal);
  const result = await rpc("tool", { name: "submit_plan", arguments_json: '{"plan":{}}' });
  assert.equal(result.plan_revision_id, "original");
  assert.deepEqual(requests, ["/v1/tool", "/v1/proposal"]);
});

test("a rejected durable proposal restores its tool result after a Pod restart", () => {
  const context = { checkpoint: { messages: checkpoint([]), pending_tool: { id: "original-tool", name: "submit_plan" } }, recovered_proposal: { result: { error: "unsupported platform" } } };
  const messages = restoreTaskCheckpoint(context);
  assert.equal(messages?.length, 1);
  assert.equal(messages?.[0]?.role, "toolResult");
  assert.deepEqual(restoreTaskCheckpoint({ ...context, checkpoint: { ...context.checkpoint, messages: checkpoint(messages!) } }), messages);
});

test("a disconnected Job leaves recovery to Runtime but explicit budget failure is terminal", async () => {
  const task: Task = { context: {}, model: { api: "openai-completions", base_url: "https://runtime.invalid", model: "test", api_key: "private-grant", parameters: {} }, max_calls: 20, timeout_seconds: 600 };
  const lease = { run_id: "run", generation: 1, uid: "pod", token: "private-grant" };
  const events: Record<string, unknown>[] = [];
  const post = async (_path: string, body: Record<string, unknown>) => { events.push(body); return { ok: true }; };
  const signal = new AbortController().signal;
  for (const error of [new TypeError("fetch failed"), new Error("runtime request rejected (503)"), new Error("request timed out"), new Error("socket connection lost")]) {
    await reportTerminalFailure(error, task, lease, post, signal);
  }
  assert.equal(events.length, 0, "transport failure must not permanently fail the logical run");
  await reportTerminalFailure(new Error("model call limit reached"), task, lease, post, signal);
  assert.equal(events.length, 1);
  assert.equal(events[0]?.kind, "failed");
  assert.deepEqual(events[0]?.payload, { error: "model call limit reached" });
  await reportTerminalFailure(new Error("planning deadline reached"), task, lease, post, signal);
  assert.deepEqual(events[1]?.payload, { error: "planning deadline reached" });
  await reportTerminalFailure(new Error("401 private-grant"), task, lease, post, signal);
  assert.deepEqual(events[2]?.payload, { error: "model authentication rejected; check the platform model profile" });
});
