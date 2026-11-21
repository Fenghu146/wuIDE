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

const SNAPSHOT_VERSION = 2;
const LEGACY_SNAPSHOT_VERSION = 1;
const DEFAULT_FILE_LIMIT = 40;
const MAX_FILE_LIMIT = 400;
// Only a fallback for snapshots written before `max_chars` existed; the IDE
// records the real budget in the snapshot so both sides render identically.
const FALLBACK_MAX_CHARS = 48_000;
const FOLD_THRESHOLD = 3;

const NO_SNAPSHOT =
  "No repository map snapshot is available yet. Ask the user to open the Repository Map " +
  "panel in Wu so the IDE can build one, then call this tool again.";
const STALE_SNAPSHOT =
  "The repository map snapshot cannot be read. Ask the user to refresh the Repository Map " +
  "panel in Wu, then call this tool again.";
const LEGACY_SNAPSHOT =
  "This snapshot was written by an older Wu build, so it has no signatures, access levels or " +
  "include counts. Ask the user to open the Repository Map panel in Wu to rebuild it.";

const DEFAULT_ORDER =
  "Order: include fan-in (desc), then reference PageRank (desc), then path.";

// Languages the IDE can index; used only to explain why a path is missing.
const SOURCE_EXTENSIONS = new Set([
  ".c",
  ".cc",
  ".cpp",
  ".cs",
  ".cxx",
  ".go",
  ".h",
  ".hh",
  ".hpp",
  ".hxx",
  ".java",
  ".js",
  ".jsx",
  ".kt",
  ".mjs",
  ".py",
  ".rb",
  ".rs",
  ".ts",
  ".tsx",
]);

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
  if (!doc || !Array.isArray(doc.files)) {
    return { error: STALE_SNAPSHOT };
  }
  // Version 1 keeps working with the fields it has; anything newer than this
  // server understands would render fields it does not know about.
  if (doc.version === LEGACY_SNAPSHOT_VERSION) {
    return { doc, legacy: true };
  }
  if (doc.version !== SNAPSHOT_VERSION) {
    return { error: STALE_SNAPSHOT };
  }
  return { doc, legacy: false };
}

function normalizePath(path) {
  return String(path)
    .replace(/\\/g, "/")
    .replace(/^\.\//, "")
    .replace(/\/+$/, "");
}

function indent(depth) {
  return "  ".repeat(Math.max(0, Number(depth) || 0));
}

function symbolLine(symbol) {
  const signature = typeof symbol.signature === "string" ? symbol.signature : "";
  return `${indent(symbol.depth)}- ${symbol.kind} ${symbol.name}${signature} (line ${symbol.line})`;
}

function folds(entry) {
  // Only variables fold: a struct's fields are its layout and must stay named.
  return entry.kind === "variable";
}

// Mirrors `repo_wiki::symbol_map::symbol_lines`: locals are hidden unless
// asked for, access levels become their own line, and runs of variables fold
// into one summary line so fields do not bury methods.
function symbolLines(symbols, includeLocals) {
  const visible = (symbols || []).filter((symbol) => includeLocals || !symbol.local);
  const lines = [];
  let lastAccess = null;
  let index = 0;
  while (index < visible.length) {
    const symbol = visible[index];
    const accessKey =
      symbol.access == null
        ? null
        : `${symbol.owner || ""}\u0000${symbol.access}\u0000${symbol.depth || 0}`;
    if (symbol.access != null && accessKey !== lastAccess) {
      lines.push(`${indent((symbol.depth || 0) - 1)}- \`${symbol.access}:\``);
    }
    lastAccess = accessKey;

    let end = index + 1;
    if (folds(symbol)) {
      while (end < visible.length) {
        const next = visible[end];
        if (
          next.kind !== symbol.kind ||
          (next.owner || "") !== (symbol.owner || "") ||
          (next.access || "") !== (symbol.access || "") ||
          Boolean(next.local) !== Boolean(symbol.local)
        ) {
          break;
        }
        end += 1;
      }
    }
    if (end - index >= FOLD_THRESHOLD) {
      lines.push(
        `${indent(symbol.depth)}- ${symbol.kind} x${end - index} ` +
          `(lines ${symbol.line}\u2013${visible[end - 1].line})`,
      );
    } else {
      for (const entry of visible.slice(index, end)) lines.push(symbolLine(entry));
    }
    index = end;
  }
  return lines;
}

function fileHeader(file) {
  if (file.in == null && file.out == null && file.lines == null) {
    // Version 1 snapshots carry no counts; do not invent zeros.
    return `\`${file.path}\``;
  }
  return (
    `\`${file.path}\` (in ${file.in || 0} / out ${file.out || 0}, ` +
    `${file.lines || 0} lines)`
  );
}

function clampLimit(requested) {
  return Number.isFinite(requested) && requested >= 1
    ? Math.min(Math.floor(requested), MAX_FILE_LIMIT)
    : DEFAULT_FILE_LIMIT;
}

function renderRepoMap(doc, options) {
  const files = doc.files;
  const total = files.length;
  const start = Math.min(Math.max(0, options.offset), total);
  const end = Math.min(start + options.limit, total);

  if (total === 0) {
    return "## Repository map\n\nNo definitions were found in this repository.";
  }

  // The character budget is a last resort: `limit` decides how many files are
  // shown, so even an oversized first file is rendered and the rest are listed.
  // The body is built first so the header states what was really written.
  // Each block starts with a blank line, exactly like the IDE's renderer, so
  // both surfaces produce the same text for the same snapshot.
  const body = [];
  let budget =
    Number.isFinite(doc.max_chars) && doc.max_chars > 0 ? doc.max_chars : FALLBACK_MAX_CHARS;
  let written = start;
  for (const file of files.slice(start, end)) {
    const lines = [fileHeader(file)];
    if (!options.filesOnly) lines.push(...symbolLines(file.symbols, options.includeLocals));
    const block = `\n${lines.join("\n")}\n`;
    if (block.length > budget && written > start) break;
    budget -= block.length;
    body.push(block);
    written += 1;
  }

  const head = ["## Repository map", ""];
  head.push(`${total} files with definitions, showing ${start + 1}\u2013${written}.`);
  head.push(typeof doc.order === "string" ? doc.order : DEFAULT_ORDER);

  const stats = doc.stats;
  if (stats && Number.isFinite(stats.considered) && Number.isFinite(stats.parsed)) {
    const unindexed = stats.considered - stats.parsed;
    if (unindexed > 0) {
      head.push(
        `${unindexed} candidate files were not indexed (${stats.too_large || 0} too large, ` +
          `${stats.unsupported || 0} unsupported language).`,
      );
    }
  }
  if (options.legacy) head.push(LEGACY_SNAPSHOT);

  let text = `${head.join("\n")}\n${body.join("")}`;
  const skipped = total - written;
  if (skipped > 0) {
    text += `\nOmitted files (${skipped}, paths only):\n`;
    for (const file of files.slice(written)) text += `${file.path}\n`;
    if (written > start) text += `\nNext page: offset=${written}, limit=${options.limit}.\n`;
  }
  if (options.includeLocals) text += "\nIncludes local variables.\n";
  return text;
}

function findFile(doc, requested) {
  const wanted = normalizePath(requested);
  if (!wanted) return undefined;
  // The agent may pass a project-relative path or the absolute one it saw in
  // its working directory; accept both without guessing further.
  return (
    doc.files.find((file) => file.path === wanted) ||
    doc.files.find((file) => file.path.endsWith(`/${wanted}`)) ||
    doc.files.find((file) => wanted.endsWith(`/${file.path}`)) ||
    (Array.isArray(doc.empty_files)
      ? doc.empty_files.find((path) => path === wanted)
      : undefined)
  );
}

function isSourcePath(path) {
  const name = String(path).toLowerCase();
  const dot = name.lastIndexOf(".");
  return dot >= 0 && SOURCE_EXTENSIONS.has(name.slice(dot));
}

function renderSymbols(doc, requested, options) {
  if (typeof requested !== "string" || requested.trim() === "") {
    return "Pass the file path, relative to the project root.";
  }
  const wanted = normalizePath(requested);
  const file = findFile(doc, requested);
  if (!file) {
    if (!isSourcePath(wanted)) {
      return (
        `\`${wanted}\` is not a source file the repository map indexes, so it has no symbols. ` +
        "Only source files with a supported grammar are parsed; documents, data files and " +
        "generated files are not."
      );
    }
    return (
      `\`${wanted}\` is not in the repository map snapshot. It may be new, excluded by ` +
      ".gitignore, or skipped because it is too large. Ask the user to open the Repository " +
      "Map panel in Wu to rebuild the snapshot."
    );
  }
  const path = typeof file === "string" ? file : file.path;
  if (typeof file === "string" || !file.symbols || file.symbols.length === 0) {
    return (
      `\`${path}\` is a source file the map parsed, but it defines nothing ` +
      "(only statements, declarations it does not own, or comments)."
    );
  }

  const head = [fileHeader(file)];
  if (options.legacy) head.push(LEGACY_SNAPSHOT);
  head.push("");
  const lines = symbolLines(file.symbols, options.includeLocals);
  if (lines.length === 0) {
    head.push(
      "Only local variables were found; call again with include_locals=true to list them.",
    );
  } else {
    head.push(...lines);
  }
  return head.join("\n");
}

const TOOLS = [
  {
    name: "repo_map",
    description:
      "The project's repository map: files ordered by include fan-in, then by how much the " +
      "rest of the repository refers to them, with their top-level definitions. Use `offset` " +
      "to page and `files_only` to list paths and line counts. Files that are not shown are " +
      "listed by path at the end.",
    inputSchema: {
      type: "object",
      properties: {
        limit: {
          type: "integer",
          minimum: 1,
          maximum: MAX_FILE_LIMIT,
          description: `How many files to include (default ${DEFAULT_FILE_LIMIT}).`,
        },
        offset: {
          type: "integer",
          minimum: 0,
          description: "How many ranked files to skip, for paging (default 0).",
        },
        files_only: {
          type: "boolean",
          description: "List files with their line counts and include counts, without symbols.",
        },
        include_locals: {
          type: "boolean",
          description:
            "Also list variables declared inside function bodies (default false).",
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
        include_locals: {
          type: "boolean",
          description:
            "Also list variables declared inside function bodies (default false).",
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

function intOption(value, fallback) {
  const number = Number(value);
  return Number.isFinite(number) && number >= 0 ? Math.floor(number) : fallback;
}

function callTool(name, args) {
  if (name !== "repo_map" && name !== "symbols") {
    return undefined;
  }
  const { doc, error, legacy } = loadSnapshot();
  if (error) return textResult(error);

  const includeLocals = Boolean(args && args.include_locals);
  if (name === "repo_map") {
    return textResult(
      renderRepoMap(doc, {
        limit: clampLimit(Number(args && args.limit)),
        offset: intOption(args && args.offset, 0),
        filesOnly: Boolean(args && args.files_only),
        includeLocals,
        legacy: Boolean(legacy),
      }),
    );
  }
  return textResult(
    renderSymbols(doc, args && args.path, {
      includeLocals,
      legacy: Boolean(legacy),
    }),
  );
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
        serverInfo: { name: "wu-repo-map", version: "1.1.0" },
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
