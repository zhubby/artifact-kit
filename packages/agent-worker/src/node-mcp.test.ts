import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { once } from "node:events";
import { connectNodeMcp } from "./node-mcp.js";
import { httpPost, runtimeRpc } from "./transport/runtime.js";

test("real MCP initialization, discovery and calls delegate only to the bound task RPC", async () => {
  const calls: unknown[] = [];
  const session = await connectNodeMcp(async (method, payload) => { calls.push([method, payload]); return { nodes: [{ node_id: "authorized" }] }; });
  try {
    assert.equal(session.client.getServerVersion()?.name, "artifactory-nodes");
    assert.deepEqual(session.tools.map(t => t.name), ["summarize_nodes", "query_nodes", "get_node"]);
    const reply = await session.client.callTool({ name: "query_nodes", arguments: { os: "linux", limit: 50 } });
    assert.equal(reply.isError, false);
    assert.deepEqual(calls, [["tool", { name: "query_nodes", arguments_json: '{"os":"linux","limit":50}' }]]);
    const denied = await session.client.callTool({ name: "sql", arguments: {} });
    assert.equal(denied.isError, true);
    assert.equal(calls.length, 1);
  } finally { await session.close(); }
});

test("same MCP session works over HTTP Runtime lease transport and rejects fenced leases", async () => {
  const lease = { run_id: "run", generation: 2, uid: "worker", token: "private" };
  const abort = new AbortController();
  let queries = 0;
  const server = createServer(async (request, response) => {
    let input = "";
    for await (const chunk of request) input += chunk;
    const body = JSON.parse(input);
    assert.equal(request.url, "/v1/tool");
    if (JSON.stringify(body.lease) !== JSON.stringify(lease)) {
      response.writeHead(401); response.end(); return;
    }
    queries++;
    assert.equal(body.name, "get_node");
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({ nodes: [] }));
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  assert.ok(address && typeof address === "object");
  const post = httpPost(`http://127.0.0.1:${address.port}`, abort.signal);
  const rpc = runtimeRpc(lease, "", post, abort.signal);
  const session = await connectNodeMcp(rpc);
  try {
    const result = await session.client.callTool({ name: "get_node", arguments: { node_id: "00000000-0000-0000-0000-000000000001" } });
    assert.equal(result.isError, false);
    const fenced = await connectNodeMcp(runtimeRpc({ ...lease, generation: 1 }, "", post, abort.signal));
    try {
      const rejected = await fenced.client.callTool({ name: "get_node", arguments: { node_id: "00000000-0000-0000-0000-000000000001" } });
      assert.equal(rejected.isError, true);
      assert.equal(queries, 1);
    } finally { await fenced.close(); }
  } finally { await session.close(); server.close(); server.closeAllConnections(); }
});

test("MCP enforces argument and response byte budgets", async () => {
  let calls = 0;
  const session = await connectNodeMcp(async () => { calls++; return { nodes: ["x".repeat(128 * 1024)] }; });
  try {
    const input = await session.client.callTool({ name: "query_nodes", arguments: { metadata: { "custom.large": "x".repeat(16 * 1024) } } });
    assert.equal(input.isError, true);
    assert.equal(calls, 0);
    const output = await session.client.callTool({ name: "query_nodes", arguments: {} });
    assert.equal(output.isError, true);
    assert.equal(calls, 1);
    assert.ok(Buffer.byteLength(JSON.stringify(output)) < 128 * 1024);
  } finally { await session.close(); }
});
