// Run with `node --test server.test.js` from this directory. Cargo does not run
// these, but they pin the messages an agent sees when `symbols` cannot answer.
//
// `@ts-check` makes `tsc` read the annotations below; `script/check-mcp-types`
// runs it. The one that earns its keep is the `SnapshotDocV2` annotation on the
// shared fixture: it is what turns a field renamed in either language into a
// type error.
// @ts-check
"use strict";

const assert = require("node:assert/strict");
const { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join } = require("node:path");
const { after, test } = require("node:test");

const {
  DEFAULT_ORDER,
  SNAPSHOT_VERSION,
  omittedPathLimit,
  renderDiagnostics,
  renderRepoMap,
  renderSymbols,
} = require("./server.js");

const workdir = mkdtempSync(join(tmpdir(), "wu-repo-map-test-"));
mkdirSync(join(workdir, "src"));
writeFileSync(join(workdir, "src", "budget.rs"), "fn kept() {}\n");
process.chdir(workdir);

after(() => {
  process.chdir(tmpdir());
  rmSync(workdir, { recursive: true, force: true });
});

// A complete `stats` block, because `repo_wiki::snapshot::to_json` always writes
// all eight counters and the tests should model a real snapshot.
const stats = {
  considered: 10,
  parsed: 9,
  too_large: 0,
  unsupported: 0,
  unreadable: 0,
  over_budget: 0,
  file_limit: 0,
  vendored_skipped: 0,
};

/** @returns {import("./server.js").SnapshotDoc} */
function doc(overrides = {}) {
  return {
    version: 2,
    files: [],
    empty_files: [],
    stats,
    ...overrides,
  };
}

test("a path that is not on disk says so", () => {
  const text = renderSymbols(doc(), "src/typo.rs", {});
  assert.match(text, /is not on disk in this project/);
});

test("a file on disk but outside the snapshot points at the budget and files_only", () => {
  const text = renderSymbols(doc({ stats: { ...stats, over_budget: 4 } }), "src/budget.rs", {});
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

/**
 * @param {string | undefined} generatedAt
 * @param {import("./server.js").DiagnosticItem[]} items
 * @returns {import("./server.js").DiagnosticsDoc}
 */
function diagnosticsDoc(generatedAt, items) {
  return { version: 1, project: "/work", generated_at: generatedAt, items };
}

/**
 * @param {string} path
 * @param {number} line
 * @param {"error" | "warning" | "info"} severity
 * @returns {import("./server.js").DiagnosticItem}
 */
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

// The IDE renders the same page in Rust (`repo_wiki::symbol_map`) from the same
// snapshot, and both sides compare against these golden files, so a change to
// one renderer without the other fails here or in
// `crates/repo_wiki/tests/repo_map_pages.rs`.
//
// `require`, not `JSON.parse(readFileSync(...))`: a `require`d JSON keeps the
// shape it was written with, so the annotation below is a real check against
// `SnapshotDocV2` — a field renamed in Rust or in `server.js` fails
// `script/check-mcp-types` instead of quietly rendering a page without it.
const fixtures = join(__dirname, "../../../../../../crates/repo_wiki/tests/fixtures/repo-map");
/** @type {import("./server.js").SnapshotDocV2} */
const snapshot = require("../../../../../../crates/repo_wiki/tests/fixtures/repo-map/snapshot.json");

/** @type {[string, import("./server.js").RepoMapOptions][]} */
const pages = [
  ["limit-1", { limit: 1, offset: 0, filesOnly: false, includeLocals: false }],
  ["limit-10", { limit: 10, offset: 0, filesOnly: false, includeLocals: false }],
  ["limit-40", { limit: 40, offset: 0, filesOnly: false, includeLocals: false }],
  ["files-only", { limit: 10, offset: 0, filesOnly: true, includeLocals: false }],
  ["offset-past-end", { limit: 40, offset: 51, filesOnly: false, includeLocals: false }],
];

test("the fallback ordering note and the version are the ones the IDE writes", () => {
  assert.equal(snapshot.order, DEFAULT_ORDER);
  // The fixture is the only place the two implementations agree on the version;
  // without this, bumping `SNAPSHOT_VERSION` here would ship a server that
  // refuses every snapshot the IDE writes.
  assert.equal(snapshot.version, SNAPSHOT_VERSION);
});

for (const [name, options] of pages) {
  test(`renders ${name} exactly like the IDE`, () => {
    const golden = readFileSync(join(fixtures, `${name}.md`), "utf8");
    assert.equal(renderRepoMap(snapshot, options), golden);
  });
}

test("an offset past the end says so instead of reversing the range", () => {
  const text = renderRepoMap(snapshot, { limit: 40, offset: snapshot.files.length });
  assert.match(text, /showing 0 of 51 files \(offset 51 is past the end; use an offset below 51\)/);
  assert.doesNotMatch(text, /showing 52/);
});
