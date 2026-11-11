import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { once } from "node:events";
import { runTask, checkpoint, restoreCheckpoint, safeError, toolDefinitions, type Task } from "./runtime.js";

test("worker exposes only read-only analysis and structured completion tools", () => {
  assert.deepEqual(toolDefinitions.map((tool) => tool.name), ["list_tree", "read_file", "search_source", "git_metadata", "node_capabilities", "read_logs", "submit_plan", "request_input"]);
  assert.equal(safeError(new Error("401 secret-value"), "secret-value"), "model authentication rejected; check the platform model profile");
  assert.equal(safeError(new Error('429: organization org-c401 request reached max RPM'), "secret-value"), "model provider quota or rate limit reached");
  assert.equal(safeError(new Error("request with key secret-value failed"), "secret-value").includes("secret-value"), false);
  assert.equal(restoreCheckpoint({ messages: checkpoint([]) })?.length, 0);
});

test("Pi uses task key only for authentication and terminates after clarification", async () => {
  let requests = 0;
  const server = createServer(async (request, response) => {
    let body = "";
    for await (const chunk of request) body += chunk;
    assert.equal(body.includes("task-secret"), false);
    assert.equal(request.headers["x-api-key"], "task-secret");
    requests++;
    response.writeHead(200, { "content-type": "text/event-stream" });
    const event = (name: string, data: unknown) => response.write(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);
    event("message_start", { type: "message_start", message: { id: "test", type: "message", role: "assistant", model: "test-model", content: [], usage: { input_tokens: 10, output_tokens: 0 } } });
    event("content_block_start", { type: "content_block_start", index: 0, content_block: { type: "tool_use", id: "question-1", name: "request_input", input: {} } });
    event("content_block_delta", { type: "content_block_delta", index: 0, delta: { type: "input_json_delta", partial_json: JSON.stringify({ question: "Which product?" }) } });
    event("content_block_stop", { type: "content_block_stop", index: 0 });
    event("message_delta", { type: "message_delta", delta: { stop_reason: "tool_use" }, usage: { output_tokens: 15 } });
    event("message_stop", { type: "message_stop" });
    response.end();
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address(); assert.ok(address && typeof address === "object");
  const task: Task = { context: { goal: "Build the selected product", usage: {totalTokens: 100} }, model: { api: "anthropic-messages", base_url: `http://127.0.0.1:${address.port}`, model: "test-model", api_key: "task-secret", parameters: { max_tokens: 1000 } }, timeout_seconds: 10, max_calls: 2 };
  let calls = 0; let questions = 0; let totalTokens = 0;
  try {
    await runTask(task, async (method, payload) => {
      assert.equal(JSON.stringify(payload).includes("task-secret"), false);
      if (method === "report" && payload.kind === "model_call") calls++;
      if (method === "report" && payload.kind === "usage") totalTokens = JSON.parse(String(payload.payload_json)).totalTokens;
      if (method === "tool") { assert.equal(payload.name, "request_input"); questions++; return { accepted: true, needs_input: true }; }
      return { ok: true };
    }, new AbortController().signal);
    assert.equal(requests, 1); assert.equal(calls, 1); assert.equal(questions, 1);
    assert.ok(totalTokens > 100, "resumed usage must include the previous worker's tokens");
  } finally { server.close(); server.closeAllConnections(); }
});

test("OpenAI-compatible Qwen requests honor the configured thinking level", async (t) => {
  for (const fixture of [
    { model: "qwen3.8-max", level: "off", thinking: false },
    { model: "qwen3.8-max", level: "low", thinking: true },
    { model: "gpt-4.1", level: "off", thinking: undefined },
  ]) await t.test(`${fixture.model} ${fixture.level}`, async () => {
    let sent: Record<string, unknown> | undefined;
    const server = createServer(async (request, response) => {
      let body = "";
      for await (const chunk of request) body += chunk;
      sent = JSON.parse(body);
      response.writeHead(200, { "content-type": "text/event-stream" });
      const chunk = (delta: unknown, finish_reason: string | null = null) => response.write(`data: ${JSON.stringify({ id: "qwen-test", object: "chat.completion.chunk", created: 1, model: fixture.model, choices: [{ index: 0, delta, finish_reason }] })}\n\n`);
      chunk({ role: "assistant", tool_calls: [{ index: 0, id: "question-1", type: "function", function: { name: "request_input", arguments: JSON.stringify({ question: "Which product?" }) } }] });
      chunk({}, "tool_calls");
      response.end("data: [DONE]\n\n");
    });
    server.listen(0, "127.0.0.1"); await once(server, "listening");
    const address = server.address(); assert.ok(address && typeof address === "object");
    const task: Task = { context: { goal: "Build the selected product" }, model: { api: "openai-completions", base_url: `http://127.0.0.1:${address.port}/v1`, model: fixture.model, api_key: "task-secret", parameters: { thinking_level: fixture.level } }, timeout_seconds: 10, max_calls: 1 };
    try {
      await runTask(task, async (method, payload) => {
        if (method === "tool") { assert.equal(payload.name, "request_input"); return { accepted: true, needs_input: true }; }
        return { ok: true };
      }, new AbortController().signal);
      assert.ok(sent);
      assert.equal(sent.enable_thinking, fixture.thinking);
      if (fixture.model.startsWith("qwen")) assert.equal((sent.messages as { role: string }[])[0]?.role, "system");
      assert.equal(JSON.stringify(sent).includes("task-secret"), false);
    } finally { server.close(); server.closeAllConnections(); }
  });
});

test("planning deadline interrupts an awaited rate-limit delay", async () => {
  let requests = 0;
  const server = createServer(async (request, response) => {
    for await (const _ of request) { /* consume request */ }
    requests++;
    response.writeHead(200, {"content-type": "text/event-stream"});
    const event = (name: string, data: unknown) => response.write(`event: ${name}\ndata: ${JSON.stringify(data)}\n\n`);
    event("message_start", {type:"message_start", message:{id:"loop", type:"message",role:"assistant",model:"test-model",content:[],usage:{input_tokens:1,output_tokens:0}}});
    event("content_block_start", {type:"content_block_start",index:0,content_block:{type:"text",text:""}});
    event("content_block_delta", {type:"content_block_delta",index:0,delta:{type:"text_delta",text:"I will inspect the repository."}});
    event("content_block_stop", {type:"content_block_stop",index:0});
    event("message_delta", {type:"message_delta",delta:{stop_reason:"end_turn"},usage:{output_tokens:5}});
    event("message_stop", {type:"message_stop"});response.end();
  });
  server.listen(0,"127.0.0.1");await once(server,"listening");
  const address=server.address();assert.ok(address && typeof address === "object");
  const task:Task={context:{goal:"Inspect and build"},model:{api:"anthropic-messages",base_url:`http://127.0.0.1:${address.port}`,model:"test-model",api_key:"test-secret",parameters:{requests_per_minute:1}},timeout_seconds:1,max_calls:20};
  const started=Date.now();
  try {
    await assert.rejects(runTask(task,async()=>({ok:true}),new AbortController().signal),/deadline/);
    assert.ok(Date.now()-started<3000,"deadline must not wait for the 60-second call throttle");
    assert.equal(requests,1);
  } finally {server.close();server.closeAllConnections();}
});
