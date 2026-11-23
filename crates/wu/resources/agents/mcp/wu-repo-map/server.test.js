// Run with `node --test server.test.js` from this directory. Cargo does not run
// these, but they pin the messages an agent sees when `symbols` cannot answer.
"use strict";

const assert = require("node:assert/strict");
const { mkdirSync, mkdtempSync, rmSync, writeFileSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join } = require("node:path");
const { after, test } = require("node:test");

const { omittedPathLimit, renderDiagnostics, renderSymbols } = require("./server.js");

const workdir = mkdtempSync(join(tmpdir(), "wu-repo-map-test-"));
mkdirSync(join(workdir, "src"));
writeFileSync(join(workdir, "src", "budget.rs"), "fn kept() {}\n");
process.chdir(workdir);

after(() => {
  process.chdir(tmpdir());
  rmSync(workdir, { recursive: true, force: true });
});

function doc(overrides = {}) {
  return {
    files: [],
    empty_files: [],
    stats: { considered: 10, parsed: 9, too_large: 0, unsupported: 0 },
    ...overrides,
  };
}

test("a path that is not on disk says so", () => {
  const text = renderSymbols(doc(), "src/typo.rs", {});
  assert.match(text, /is not on disk in this project/);
});

test("a file on disk but outside the snapshot points at the budget and files_only", () => {
  const text = renderSymbols(doc({ stats: { considered: 10, parsed: 9, over_budget: 4 } }), "src/budget.rs", {});
  assert.match(text, /exists on disk but is not in the repository map snapshot/);
  assert.match(text, /output budget/);
  assert.match(text, /larger budget/);
  assert.match(text, /`files_only`/);
});

test("a non-source path is rejected without touching the disk", () => {
  const text = renderSymbols(doc(), "docs/notes.md", {});
  assert.match(text, /is not a source file the repository map indexes/);
});

test("a parsed file with no definitions says it defines nothing", () => {
  const text = renderSymbols(doc({ empty_files: ["src/empty.rs"] }), "src/empty.rs", {});
  assert.match(text, /defines nothing/);
});

test("a vendored path explains that the map skips it", () => {
  const text = renderSymbols(doc(), "crates/wu/resources/agents/node_modules/pi/index.js", {});
  assert.match(text, /vendored or generated directory/);
});

test("the omitted-path cap mirrors the Rust renderer", () => {
  assert.equal(omittedPathLimit(1), 10);
  assert.equal(omittedPathLimit(5), 10);
  assert.equal(omittedPathLimit(6), 20);
  assert.equal(omittedPathLimit(40), 20);
});

function diagnosticsDoc(generatedAt, items) {
  return { version: 1, project: "/work", generated_at: generatedAt, items };
}

function diagnosticItem(path, line, severity) {
  return { path, line, column: 3, severity, code: "", message: `${severity} here`, source: "rust-analyzer" };
}

test("the header says how old the diagnostics snapshot is", () => {
  const age = 12 * 1000;
  const text = renderDiagnostics(
    diagnosticsDoc(new Date(Date.now() - age).toISOString(), [
      diagnosticItem("src/a.rs", 2, "error"),
      diagnosticItem("src/a.rs", 4, "warning"),
      diagnosticItem("src/a.rs", 5, "info"),
    ]),
    { limit: 50 },
  );
  assert.match(text, /^Snapshot written 1[12]s ago · 3 diagnostics in 1 file$/m);
  assert.match(text, /^1 error, 1 warning, 1 info, showing all 3\.$/m);
});

test("an old snapshot is called out instead of passed off as current", () => {
  const age = 7 * 60 * 1000;
  const text = renderDiagnostics(
    diagnosticsDoc(new Date(Date.now() - age).toISOString(), [
      diagnosticItem("src/a.rs", 2, "warning"),
    ]),
    { limit: 50 },
  );
  assert.match(text, /^Snapshot is 7m old; ask the user to refresh the Problems panel · 1 diagnostic in 1 file$/m);
});

test("a snapshot without a timestamp keeps listing its diagnostics", () => {
  const text = renderDiagnostics(
    diagnosticsDoc(undefined, [diagnosticItem("src/a.rs", 2, "error")]),
    { limit: 50 },
  );
  assert.doesNotMatch(text, /Snapshot/);
  assert.match(text, /^- src\/a\.rs:2:3 error error here$/m);
});
