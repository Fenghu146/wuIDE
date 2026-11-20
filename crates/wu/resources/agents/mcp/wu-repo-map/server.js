#!/usr/bin/env node
// Wu's repository map for Pi, served over MCP on stdio.
//
// The IDE writes one snapshot per project root into
// `<PI_CODING_AGENT_DIR>/repo-maps/<project-key>.json`; this server reads the
// one belonging to its working directory. No IPC, no third-party packages:
// Pi runs us inside the project, so the directory itself is the contract.
"use strict";

const { createHash } = require("node:crypto");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { createInterface } = require("node:readline");

const SNAPSHOT_VERSION = 1;
const DEFAULT_FILE_LIMIT = 60;
const MAX_FILE_LIMIT = 400;
const MAX_CHARS = 12_000;

const NO_SNAPSHOT =
  "No repository map snapshot is available yet. Ask the user to open the Repository Map " +
  "panel in Wu so the IDE can build one, then call this tool again.";
const STALE_SNAPSHOT =
  "The repository map snapshot cannot be read. Ask the user to refresh the Repository Map " +
  "panel in Wu, then call this tool again.";

// Must match `repo_wiki::snapshot::project_key`: forward slashes, no trailing
// slash, lowercased on Windows, first 16 hex digits of SHA-256.
function projectKey(root) {
  let path = String(root).replace(/\\/g, "/");
  while (path.length > 1 && path.endsWith("/")) path = path.slice(0, -1);
  if (process.platform === "win32") path = path.toLowerCase();
  return createHash("sha256").update(path, "utf8").digest("hex").slice(0, 16);
}

function snapshotPath() {
  const agentDir = process.env.PI_CODING_AGENT_DIR;
  if (!agentDir) return null;
  return join(agentDir, "repo-maps", `${projectKey(process.cwd())}.json`);
}

function loadSnapshot() {
  const path = snapshotPath();
  if (!path) return { error: NO_SNAPSHOT };

  let text;
  try {
    text = readFileSync(path, "utf8");
  } catch {
    return { error: NO_SNAPSHOT };
  }

  let doc;
  try {
    doc = JSON.parse(text);
  } catch {
    return { error: STALE_SNAPSHOT };
  }
  if (!doc || doc.version !== SNAPSHOT_VERSION || !Array.isArray(doc.files)) {
    return { error: STALE_SNAPSHOT };
  }
  return { doc };
}

function normalizePath(path) {
  return String(path)
    .replace(/\\/g, "/")
    .replace(/^\.\//, "")
    .replace(/\/+$/, "");
}

function renderRepoMap(doc, limit) {
  const files = doc.files.slice(0, limit);
  const out = [
    "## Repository map",
    "",
    `${doc.files.length} files with definitions, most-referenced first.`,
    "",
  ];
  let budget = MAX_CHARS;
  let written = 0;
  for (const file of files) {
    const lines = [`\`${file.path}\``];
    for (const symbol of file.symbols || []) {
      lines.push(`- ${symbol.kind} ${symbol.name} (line ${symbol.line})`);
    }
    const block = `${lines.join("\n")}\n`;
    if (block.length > budget) break;
    budget -= block.length;
    out.push(block);
    written += 1;
  }
  if (written < files.length) {
    out.push(`(${files.length - written} more files omitted to stay within the size limit.)\n`);
  }
  return out.join("\n");
}

function findFile(doc, requested) {
  const wanted = normalizePath(requested);
  if (!wanted) return undefined;
  // The agent may pass a project-relative path or the absolute one it saw in
  // its working directory; accept both without guessing further.
  return (
    doc.files.find((file) => file.path === wanted) ||
    doc.files.find((file) => file.path.endsWith(`/${wanted}`)) ||
    doc.files.find((file) => wanted.endsWith(`/${file.path}`))
  );
}

function renderSymbols(doc, requested) {
  if (typeof requested !== "string" || requested.trim() === "") {
    return "Pass the file path, relative to the project root.";
  }
  const file = findFile(doc, requested);
  if (!file) {
    return (
      `\`${normalizePath(requested)}\` is not in the repository map. It may have no ` +
      "definitions, or the map may predate it; the Repository Map panel rebuilds on refresh."
    );
  }
  if (!file.symbols || file.symbols.length === 0) {
    return `\`${file.path}\` has no definitions in the repository map.`;
  }
  const lines = [`\`${file.path}\``];
  for (const symbol of file.symbols) {
    lines.push(`- ${symbol.kind} ${symbol.name} (line ${symbol.line})`);
  }
  return lines.join("\n");
}

const TOOLS = [
  {
    name: "repo_map",
    description:
      "The project's repository map: files ranked by how much the rest of the repository " +
      "refers to them, with their definitions. Use it for structural orientation instead of " +
      "guessing from directory listings or search results.",
    inputSchema: {
      type: "object",
      properties: {
        limit: {
          type: "integer",
          minimum: 1,
          maximum: MAX_FILE_LIMIT,
          description: `How many of the top-ranked files to include (default ${DEFAULT_FILE_LIMIT}).`,
        },
      },
      additionalProperties: false,
    },
  },
  {
    name: "symbols",
    description: "Every definition the IDE found in one file of this project.",
    inputSchema: {
      type: "object",
      properties: {
        path: {
          type: "string",
          description: "File path relative to the project root, as shown by repo_map.",
        },
      },
      required: ["path"],
      additionalProperties: false,
    },
  },
];

function textResult(text) {
  return { content: [{ type: "text", text }] };
}

function callTool(name, args) {
  if (name !== "repo_map" && name !== "symbols") {
    return undefined;
  }
  const { doc, error } = loadSnapshot();
  if (error) return textResult(error);

  if (name === "repo_map") {
    const requested = Number(args && args.limit);
    const limit =
      Number.isFinite(requested) && requested >= 1
        ? Math.min(Math.floor(requested), MAX_FILE_LIMIT)
        : DEFAULT_FILE_LIMIT;
    return textResult(renderRepoMap(doc, limit));
  }
  return textResult(renderSymbols(doc, args && args.path));
}

function send(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function handle(message) {
  const method = message.method || "";
  if (message.id === undefined) return; // notification

  const reply = (result) => send({ jsonrpc: "2.0", id: message.id, result });
  const fail = (code, text) =>
    send({ jsonrpc: "2.0", id: message.id, error: { code, message: text } });

  switch (method) {
    case "initialize":
      reply({
        protocolVersion: (message.params && message.params.protocolVersion) || "2025-06-18",
        capabilities: { tools: {} },
        serverInfo: { name: "wu-repo-map", version: "1.0.0" },
      });
      return;
    case "ping":
      reply({});
      return;
    case "tools/list":
      reply({ tools: TOOLS });
      return;
    case "tools/call": {
      const name = message.params && message.params.name;
      const result = callTool(name, (message.params && message.params.arguments) || {});
      if (!result) {
        fail(-32602, `unknown tool: ${name}`);
        return;
      }
      reply(result);
      return;
    }
    default:
      fail(-32601, `method not supported: ${method}`);
  }
}

const input = createInterface({ input: process.stdin, crlfDelay: Infinity });
input.on("line", (line) => {
  const trimmed = line.trim();
  if (!trimmed) return;
  let message;
  try {
    message = JSON.parse(trimmed);
  } catch {
    return;
  }
  handle(message);
});
