#!/usr/bin/env node
// Wu's repository map and LSP diagnostics for Pi, served over MCP on stdio.
//
// The IDE writes one snapshot per project root into
// `<PI_CODING_AGENT_DIR>/repo-maps/<project-key>.json` and
// `<PI_CODING_AGENT_DIR>/diagnostics/<project-key>.json`; this server reads the
// ones belonging to its working directory. No IPC, no third-party packages:
// Pi runs us inside the project, so the directory itself is the contract.
"use strict";

const { createHash } = require("node:crypto");
const { existsSync, readFileSync } = require("node:fs");
const { join } = require("node:path");
const { createInterface } = require("node:readline");

const SNAPSHOT_VERSION = 2;
const LEGACY_SNAPSHOT_VERSION = 1;
const DEFAULT_FILE_LIMIT = 40;
const MAX_FILE_LIMIT = 400;
// Mirrors `repo_wiki::symbol_map::MAX_SYMBOLS_PER_FILE_IN_MARKDOWN`: a file with
// hundreds of definitions is summarized after its first few, and `symbols`
// returns the rest.
const MAX_SYMBOLS_PER_FILE = 40;
// Mirrors `repo_wiki::symbol_map::MAX_OMITTED_PATHS` and
// `MAX_OMITTED_PATHS_SMALL`: the list of files a page left out is summarized
// after this many names, so it cannot outgrow the map, and a page that asked
// for very few files gets the smaller list.
const MAX_OMITTED_PATHS = 20;
const MAX_OMITTED_PATHS_SMALL = 10;
// Mirrors `repo_wiki::symbol_map::MAX_FILES`: the file count one build indexes.
const MAX_FILES = 3000;
// Mirrors `repo_wiki::symbol_map::VENDORED_DIRS`: used only to explain why a
// path is absent from the snapshot, since the IDE excludes it while scanning.
const VENDORED_DIRS = new Set([
  "node_modules",
  "vendor",
  "third_party",
  "third-party",
  "dist",
  "site-packages",
  "target",
  ".venv",
  "venv",
  "bower_components",
  ".yarn",
  ".pnpm-store",
  "out",
  "Pods",
  "__pycache__",
  ".tox",
  ".next",
  ".nuxt",
  ".svelte-kit",
  ".gradle",
]);
const DIAGNOSTICS_SNAPSHOT_VERSION = 1;
const DEFAULT_DIAGNOSTICS_LIMIT = 50;
const MAX_DIAGNOSTICS_LIMIT = 500;
const DIAGNOSTIC_SEVERITIES = new Set(["error", "warning", "info"]);
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
const NO_DIAGNOSTICS_SNAPSHOT =
  "No diagnostics snapshot is available yet. Ask the user to open the Problems panel in Wu " +
  "(or trigger a diagnostic) so the IDE can write one, then call this tool again.";
const STALE_DIAGNOSTICS_SNAPSHOT =
  "The diagnostics snapshot cannot be read, probably because Wu wrote it in a different " +
  "format. Ask the user to open the Problems panel in Wu to refresh it, then call again.";

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

// Same key, same directory scheme as the repository map, so both snapshots for
// one project sit next to each other.
function diagnosticsSnapshotPath() {
  const agentDir = process.env.PI_CODING_AGENT_DIR;
  if (!agentDir) return null;
  return join(agentDir, "diagnostics", `${projectKey(process.cwd())}.json`);
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

function loadDiagnosticsSnapshot() {
  const path = diagnosticsSnapshotPath();
  if (!path) return { error: NO_DIAGNOSTICS_SNAPSHOT };

  let text;
  try {
    text = readFileSync(path, "utf8");
  } catch {
    return { error: NO_DIAGNOSTICS_SNAPSHOT };
  }

  let doc;
  try {
    doc = JSON.parse(text);
  } catch {
    return { error: STALE_DIAGNOSTICS_SNAPSHOT };
  }
  if (!doc || !Array.isArray(doc.items)) {
    return { error: STALE_DIAGNOSTICS_SNAPSHOT };
  }
  if (doc.version !== DIAGNOSTICS_SNAPSHOT_VERSION) {
    return { error: STALE_DIAGNOSTICS_SNAPSHOT };
  }
  return { doc };
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

// Mirrors the cap in `SymbolMap::file_block`: the first visible definitions, then
// one line saying how many were left for the `symbols` tool.
function cappedSymbolLines(symbols, includeLocals) {
  const visible = (symbols || []).filter((symbol) => includeLocals || !symbol.local);
  const kept = Math.min(visible.length, MAX_SYMBOLS_PER_FILE);
  const lines = symbolLines(visible.slice(0, kept), includeLocals);
  if (kept < visible.length) lines.push(`… +${visible.length - kept} more symbols`);
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

// Renders the file blocks of one page, best-ranked first, until the budget for
// the body runs out. Mirrors `render_ranked` in `repo_wiki::symbol_map`.
function renderPageBody(files, start, end, budget, options) {
  const body = [];
  let written = start;
  for (const file of files.slice(start, end)) {
    const lines = [fileHeader(file)];
    if (!options.filesOnly) lines.push(...cappedSymbolLines(file.symbols, options.includeLocals));
    const block = `\n${lines.join("\n")}\n`;
    if (block.length > budget && written > start) break;
    budget = Math.max(0, budget - block.length);
    body.push(block);
    written += 1;
  }
  return { body: body.join(""), written };
}

// Paths one page may list after its body, mirroring `omitted_path_limit` in
// `repo_wiki::symbol_map`.
function omittedPathLimit(limit) {
  return limit <= 5 ? MAX_OMITTED_PATHS_SMALL : MAX_OMITTED_PATHS;
}

// Honest coverage for the whole build, mirroring `SymbolMap::coverage_notes`:
// how many candidates became map entries, why the rest did not, and what never
// became a candidate at all.
function coverageNotes(stats) {
  const notes = [];
  if (Number.isFinite(stats.considered) && stats.considered > 0) {
    const reasons = [];
    if (stats.unsupported > 0) reasons.push(`${stats.unsupported} unsupported language`);
    if (stats.too_large > 0) reasons.push(`${stats.too_large} over 256 KiB`);
    if (stats.over_budget > 0) reasons.push(`${stats.over_budget} over the byte budget`);
    if (stats.file_limit > 0) reasons.push(`${stats.file_limit} beyond the ${MAX_FILES}-file limit`);
    if (stats.unreadable > 0) reasons.push(`${stats.unreadable} unreadable`);
    const suffix = reasons.length === 0 ? "" : ` (${reasons.join(", ")})`;
    notes.push(`Indexed ${stats.parsed} / ${stats.considered} candidate files${suffix}.`);
  }
  if (stats.vendored_skipped > 0) {
    notes.push(`Excluded before scanning: ${stats.vendored_skipped} vendored or generated paths.`);
  }
  return notes;
}

// Everything before the body: the header, the ordering note and the coverage
// of the build the page was cut from.
function renderPageHead(doc, written, start, end, total, options) {
  let header = `showing ${start + 1}\u2013${written} of ${total} files with definitions`;
  // Only the budget needs an explanation: a page cut by `limit` is the page the
  // caller asked for, and the paging hint below covers it.
  if (written < end) {
    header +=
      ` (output budget reached; use offset=${written} for the next page, or call ` +
      "symbols(path) for one file)";
  }
  const head = ["## Repository map", "", `${header}.`];
  head.push(typeof doc.order === "string" ? doc.order : DEFAULT_ORDER);

  const stats = doc.stats;
  if (stats && Number.isFinite(stats.considered) && Number.isFinite(stats.parsed)) {
    head.push(...coverageNotes(stats));
  }
  if (options.legacy) head.push(LEGACY_SNAPSHOT);
  return `${head.join("\n")}\n`;
}

// Everything after the body, with the widest "+N more omitted" line its count
// can produce, so the body budget never eats that room.
function renderPageTailReserve(written, start, end, total, options) {
  let tail = "";
  const skipped = total - written;
  if (skipped > 0) {
    tail += `\nOmitted files (${skipped}, paths only):\n`;
    tail += `… +${skipped} more omitted (call files_only or raise offset to list them)\n`;
  }
  if (written > start && written === end) {
    tail += `\nNext page: offset=${written}, limit=${options.limit}.\n`;
  }
  if (options.includeLocals) tail += "\nIncludes local variables.\n";
  return tail;
}

function renderRepoMap(doc, options) {
  const files = doc.files;
  const total = files.length;
  const start = Math.min(Math.max(0, options.offset), total);
  const end = Math.min(start + options.limit, total);

  if (total === 0) {
    return "## Repository map\n\nNo definitions were found in this repository.";
  }

  // The whole page shares one budget: the body is measured against what the
  // framing and the omitted list leave behind. Each block starts with a blank
  // line, exactly like the IDE's renderer, so both surfaces produce the same
  // text for the same snapshot.
  const maxChars =
    Number.isFinite(doc.max_chars) && doc.max_chars > 0 ? doc.max_chars : FALLBACK_MAX_CHARS;
  let bodyBudget = maxChars;
  let attempts = 0;
  let head = "";
  let body = "";
  let written = start;
  let reserveLen = 0;
  for (;;) {
    const rendered = renderPageBody(files, start, end, bodyBudget, options);
    head = renderPageHead(doc, rendered.written, start, end, total, options);
    reserveLen = renderPageTailReserve(rendered.written, start, end, total, options).length;
    body = rendered.body;
    written = rendered.written;
    const used = head.length + body.length + reserveLen;
    // A page always keeps its first file, so a budget too small for the framing
    // alone cannot be satisfied and is left as best effort.
    if (used <= maxChars || written <= start + 1 || attempts >= 8) break;
    bodyBudget = Math.max(0, bodyBudget - Math.max(1, used - maxChars));
    attempts += 1;
  }

  let text = head + body;
  // The whole tail is reserved before any path is printed, so the list can only
  // use what the header, the body and the trailing notes leave.
  let room = Math.max(0, maxChars - (text.length + reserveLen));
  const skipped = total - written;
  if (skipped > 0) {
    text += `\nOmitted files (${skipped}, paths only):\n`;
    // Show whole paths while they fit, never more than the page's cap, and name
    // the rest in one line.
    let shown = 0;
    for (const file of files.slice(written, written + omittedPathLimit(options.limit))) {
      const line = `${file.path}\n`;
      if (line.length > room) break;
      room -= line.length;
      text += line;
      shown += 1;
    }
    if (shown < skipped) {
      text += `… +${skipped - shown} more omitted (call files_only or raise offset to list them)\n`;
    }
  }
  if (written > start && written === end) {
    text += `\nNext page: offset=${written}, limit=${options.limit}.\n`;
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

// Same rule as `is_vendored_path` in `repo_wiki::symbol_map`: a hidden or
// known vendored directory anywhere before the file name.
function isVendoredPath(path) {
  const parts = normalizePath(path).split("/");
  return parts
    .slice(0, -1)
    .some((part) => part.startsWith(".") || VENDORED_DIRS.has(part));
}

function renderSymbols(doc, requested, options) {
  if (typeof requested !== "string" || requested.trim() === "") {
    return "Pass the file path, relative to the project root.";
  }
  const wanted = normalizePath(requested);
  const file = findFile(doc, requested);
  if (!file) {
    if (isVendoredPath(wanted)) {
      return (
        `\`${wanted}\` is inside a vendored or generated directory, which the repository map ` +
        "deliberately skips (for example `node_modules`, `dist`, `target`, or `vendor`), so it " +
        "has no symbols."
      );
    }
    if (!isSourcePath(wanted)) {
      return (
        `\`${wanted}\` is not a source file the repository map indexes, so it has no symbols. ` +
        "Only source files with a supported grammar are parsed; documents, data files and " +
        "generated files are not."
      );
    }
    let onDisk = false;
    try {
      onDisk = existsSync(wanted);
    } catch {
      onDisk = false;
    }
    if (!onDisk) {
      return (
        `\`${wanted}\` is not on disk in this project. Check the path (it is relative to the ` +
        "project root) and call again."
      );
    }
    const budget =
      doc.stats && (Number(doc.stats.over_budget) > 0 || Number(doc.stats.file_limit) > 0);
    return (
      `\`${wanted}\` exists on disk but is not in the repository map snapshot. ` +
      (budget
        ? "The build skipped it because the scan reached its output budget, which covers the " +
          "byte budget and the file limit. Ask the user to rebuild the Repository Map with a " +
          "larger budget, or call `repo_map` with `files_only` to list the files it did index."
        : "It may be new, ignored by `.gitignore`, or unreadable. Ask the user to open the " +
          "Repository Map panel in Wu to rebuild the snapshot, or call `repo_map` with " +
          "`files_only` to list the files it did index.")
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

const TRUNCATED_DIAGNOSTICS =
  "The snapshot was capped when the IDE wrote it, so older diagnostics are missing from this list.";

// A snapshot older than this is called out instead of quietly reported, so an
// agent can tell a fresh list from one that predates the user's last edit.
const DIAGNOSTICS_STALE_AFTER_SECONDS = 5 * 60;

function plural(count, noun) {
  return `${count} ${noun}${count === 1 ? "" : "s"}`;
}

function snapshotAgeSeconds(generatedAt, now) {
  if (typeof generatedAt !== "string") return null;
  const written = Date.parse(generatedAt);
  if (!Number.isFinite(written)) return null;
  return Math.max(0, Math.round((now - written) / 1000));
}

function formatAge(seconds) {
  if (seconds < 90) return `${seconds}s`;
  if (seconds < 90 * 60) return `${Math.round(seconds / 60)}m`;
  if (seconds < 48 * 60 * 60) return `${Math.round(seconds / 3600)}h`;
  return `${Math.round(seconds / (24 * 60 * 60))}d`;
}

// The first line of the tool output: how old the snapshot is, so the list below
// is never mistaken for live state, plus what it covers.
function freshnessLine(doc, matched, fileCount, now) {
  const age = snapshotAgeSeconds(doc.generated_at, now);
  if (age === null) return null;
  const size = `${plural(matched.length, "diagnostic")} in ${plural(fileCount, "file")}`;
  if (age >= DIAGNOSTICS_STALE_AFTER_SECONDS) {
    return `Snapshot is ${formatAge(age)} old; ask the user to refresh the Problems panel · ${size}`;
  }
  return `Snapshot written ${formatAge(age)} ago · ${size}`;
}

function clampDiagnosticsLimit(requested) {
  return Number.isFinite(requested) && requested >= 1
    ? Math.min(Math.floor(requested), MAX_DIAGNOSTICS_LIMIT)
    : DEFAULT_DIAGNOSTICS_LIMIT;
}

function matchesPath(item, wanted) {
  return (
    !wanted ||
    item.path === wanted ||
    item.path.endsWith(`/${wanted}`) ||
    wanted.endsWith(`/${item.path}`)
  );
}

// One line per diagnostic, grouped under the file it belongs to, so an agent
// can read the list and open the exact position.
function renderDiagnostics(doc, options) {
  const wanted = options.path ? normalizePath(options.path) : "";
  const matched = (doc.items || []).filter(
    (item) =>
      (!options.severity || item.severity === options.severity) &&
      matchesPath(item, wanted),
  );
  const counts = { error: 0, warning: 0, info: 0 };
  for (const item of matched) {
    if (item.severity in counts) counts[item.severity] += 1;
  }
  const fileCount = new Set(matched.map((item) => item.path)).size;

  const head = ["## Diagnostics", ""];
  const freshness = freshnessLine(doc, matched, fileCount, Date.now());
  if (freshness) head.push(freshness);
  if (matched.length === 0) {
    const scope =
      (options.severity ? ` with severity \`${options.severity}\`` : "") +
      (wanted ? ` in \`${wanted}\`` : "");
    head.push(`No diagnostics${scope}.`);
    if (doc.truncated) head.push(TRUNCATED_DIAGNOSTICS);
    return head.join("\n");
  }

  const shown = matched
    .slice()
    .sort(
      (left, right) =>
        String(left.path).localeCompare(String(right.path)) ||
        Number(left.line) - Number(right.line) ||
        Number(left.column) - Number(right.column),
    )
    .slice(0, options.limit);

  const showing =
    shown.length === matched.length
      ? `showing all ${matched.length}`
      : `showing ${shown.length} of ${matched.length}`;
  head.push(
    `${plural(counts.error, "error")}, ${plural(counts.warning, "warning")}, ` +
      `${counts.info} info, ${showing}.`,
  );
  if (doc.truncated) head.push(TRUNCATED_DIAGNOSTICS);

  const body = [];
  let currentPath = null;
  for (const item of shown) {
    if (item.path !== currentPath) {
      currentPath = item.path;
      body.push("", `\`${item.path}\``);
    }
    const code = item.code ? ` (${item.code})` : "";
    const message = String(item.message).replace(/\s+/g, " ").trim();
    body.push(`- ${item.path}:${item.line}:${item.column} ${item.severity}${code} ${message}`);
  }

  let text = `${head.join("\n")}\n${body.join("\n")}`;
  if (matched.length > shown.length) {
    text +=
      `\n\nShowing ${shown.length} of ${matched.length}. Raise \`limit\` ` +
      `(max ${MAX_DIAGNOSTICS_LIMIT}), or filter by \`path\` or \`severity\`.`;
  }
  return `${text}\n`;
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
          description:
            `How many files to include at most (default ${DEFAULT_FILE_LIMIT}). The page can ` +
            "end earlier when the output budget runs out; the header says so and `offset` " +
            "pages on.",
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
  {
    name: "diagnostics",
    description:
      "The language-server diagnostics Wu currently holds for this project: compile errors, " +
      "warnings and info, each with its file, line, column, code and message. Use it to find " +
      "out what is broken instead of asking the user to paste errors. Filter by `path` or " +
      "`severity`, and page with `limit`.",
    inputSchema: {
      type: "object",
      properties: {
        path: {
          type: "string",
          description: "Only diagnostics in this file; project-relative or absolute.",
        },
        severity: {
          type: "string",
          enum: ["error", "warning", "info"],
          description: "Only diagnostics with this severity.",
        },
        limit: {
          type: "integer",
          minimum: 1,
          maximum: MAX_DIAGNOSTICS_LIMIT,
          description: `How many diagnostics to list (default ${DEFAULT_DIAGNOSTICS_LIMIT}).`,
        },
      },
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
  if (name === "diagnostics") {
    const { doc, error } = loadDiagnosticsSnapshot();
    if (error) return textResult(error);
    const severity =
      typeof args.severity === "string" && DIAGNOSTIC_SEVERITIES.has(args.severity)
        ? args.severity
        : undefined;
    return textResult(
      renderDiagnostics(doc, {
        path: typeof args.path === "string" ? args.path : "",
        severity,
        limit: clampDiagnosticsLimit(Number(args.limit)),
      }),
    );
  }
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
        serverInfo: { name: "wu-repo-map", version: "1.2.0" },
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

// Pi starts this file directly; the guard keeps `require` from a test from
// taking over its stdin.
if (require.main === module) {
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
}

module.exports = {
  renderDiagnostics,
  renderRepoMap,
  renderSymbols,
  omittedPathLimit,
  coverageNotes,
};
