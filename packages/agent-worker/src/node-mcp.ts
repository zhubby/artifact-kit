import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import type { Rpc } from "./runtime.js";

const properties = {
  os: { type: "string", enum: ["linux", "darwin", "windows"] },
  arch: { type: "string", enum: ["amd64", "arm64"] },
  metadata: { type: "object", additionalProperties: true, maxProperties: 32 },
  tools: { type: "array", items: { type: "string" }, maxItems: 32 },
  capabilities: { type: "array", items: { type: "string" }, maxItems: 32 },
  limit: { type: "integer", minimum: 1, maximum: 100, default: 50 },
  cursor: { type: "string", format: "uuid" },
};
const definitions = [
  { name: "summarize_nodes", description: "Summarize authorized device capabilities, online status and task slots. Details are available through query_nodes and get_node.", inputSchema: { type: "object" as const, properties: {}, additionalProperties: false } },
  { name: "query_nodes", description: "Find authorized devices by native platform, metadata key/value containment and tools. Returns ordered IDs, metadata digests, fresh resource summaries and a continuation cursor; default 50, max 100.", inputSchema: { type: "object" as const, properties, additionalProperties: false } },
  { name: "get_node", description: "Read one authorized device's complete startup metadata and latest resources. Unknown or stale resources cannot satisfy placement requirements.", inputSchema: { type: "object" as const, properties: { node_id: { type: "string", format: "uuid" } }, required: ["node_id"], additionalProperties: false } },
];

/** A real SDK MCP session scoped to one authenticated planning task, without a network listener. */
export async function connectNodeMcp(rpc: Rpc) {
  const server = new Server({ name: "artifactory-nodes", version: "1.0.0" }, { capabilities: { tools: {} } });
  server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: definitions }));
  server.setRequestHandler(CallToolRequestSchema, async request => {
    if (!definitions.some(tool => tool.name === request.params.name)) return { content: [{ type: "text", text: "tool not permitted" }], isError: true };
    try {
      const args = JSON.stringify(request.params.arguments ?? {});
      if (Buffer.byteLength(args) > 16 * 1024) throw new Error("node filter exceeds limit");
      const result = await rpc("tool", { name: request.params.name, arguments_json: args });
      const text = JSON.stringify(result);
      if (Buffer.byteLength(text) > 128 * 1024) throw new Error("node response exceeds limit");
      return { content: [{ type: "text", text }], isError: !!result.error };
    } catch {
      return { content: [{ type: "text", text: "authorized node query unavailable" }], isError: true };
    }
  });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  const client = new Client({ name: "artifactory-agent-worker", version: "1.0.0" });
  await server.connect(serverTransport);
  try {
    await client.connect(clientTransport);
    const { tools } = await client.listTools();
    return { client, tools, close: async () => { await client.close(); await server.close(); } };
  } catch (error) { await server.close(); throw error; }
}
