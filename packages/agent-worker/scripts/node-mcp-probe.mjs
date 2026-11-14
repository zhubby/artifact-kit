// Isolated acceptance probe. Credentials arrive over stdin and are never printed.
import * as grpc from "@grpc/grpc-js";
import * as loader from "@grpc/proto-loader";
import { fileURLToPath } from "node:url";
import { connectNodeMcp } from "../src/node-mcp.ts";

let input = "";
for await (const chunk of process.stdin) input += chunk;
const task = JSON.parse(input);
let client;
let rpc;
let runtimeRpc, post;
const signal = AbortSignal.timeout(20000);
if (task.transport === "runtime") {
  ({ runtimeRpc } = await import("../dist/transport/runtime.js"));
  const { httpPost } = await import("../dist/transport/runtime.js");
  post = httpPost(task.endpoint, signal);
  rpc = runtimeRpc(task.lease, "", post, signal);
} else {
  const schema = loader.loadSync(fileURLToPath(new URL("../../../proto/artifactory/emissary/v1/emissary.proto", import.meta.url)), { keepCase: true, defaults: true });
  const { Planning } = grpc.loadPackageDefinition(schema).artifactory.emissary.v1;
  client = new Planning(task.endpoint.replace(/^https?:\/\//, ""), grpc.credentials.createSsl(Buffer.from(task.ca), Buffer.from(task.key), Buffer.from(task.certificate)), { "grpc.ssl_target_name_override": "localhost" });
  rpc = (method, payload) => new Promise((resolve, reject) => {
    if (method !== "tool") return reject(new Error("only node queries are permitted"));
    client.Tool({ ...payload, run_id: task.run_id, lease_token: task.lease_token }, { deadline: Date.now() + 10000 }, (error, reply) => {
      if (error) {
        process.stderr.write(`node query RPC failed (${error.code}): ${error.details}\n`);
        reject(error);
      } else resolve(JSON.parse(reply.result_json));
    });
  });
}
let session;
try {
  session = await connectNodeMcp(rpc);
  const results = [];
  for (const query of task.queries) {
    const reply = await session.client.callTool(query);
    if (reply.isError) throw new Error(`${query.name} MCP query rejected`);
    results.push(JSON.parse(reply.content[0].text));
  }
  let staleLeaseRejected;
  if (task.stale_lease) {
    const stale = await connectNodeMcp(runtimeRpc(task.stale_lease, "", post, signal));
    try {
      const result = await stale.client.callTool(task.queries[0]);
      staleLeaseRejected = result.isError === true;
      if (!staleLeaseRejected) throw new Error("stale Runtime lease was accepted");
    } finally { await stale.close(); }
  }
  process.stdout.write(JSON.stringify({ initialized: session.client.getServerVersion()?.name, tools: session.tools.map(tool => tool.name), results, staleLeaseRejected }));
} finally {
  await session?.close();
  client?.close();
}
