// Run with `node --test server.test.js` from this directory. Cargo does not run
// these, but they pin the messages an agent sees when `symbols` cannot answer.
"use strict";

const assert = require("node:assert/strict");
const { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { dirname, join } = require("node:path");
const { after, test } = require("node:test");

const { DEFAULT_ORDER, omittedPathLimit, renderRepoMap, renderSymbols } = require("./server.js");

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

// The IDE renders the same page in Rust (`repo_wiki::symbol_map`) from the same
// snapshot, and both sides compare against these golden files, so a change to
// one renderer without the other fails here or in
// `crates/repo_wiki/tests/repo_map_pages.rs`.
function repoRoot() {
  let directory = __dirname;
  for (;;) {
    if (existsSync(join(directory, "crates", "repo_wiki", "tests", "fixtures", "repo-map"))) {
      return directory;
    }
    const parent = dirname(directory);
    assert.notEqual(parent, directory, "no crates/repo_wiki above this test");
    directory = parent;
  }
}

const fixtures = join(repoRoot(), "crates/repo_wiki/tests/fixtures/repo-map");
const snapshot = JSON.parse(readFileSync(join(fixtures, "snapshot.json"), "utf8"));

const pages = [
  ["limit-1", { limit: 1, offset: 0, filesOnly: false, includeLocals: false }],
  ["limit-10", { limit: 10, offset: 0, filesOnly: false, includeLocals: false }],
  ["limit-40", { limit: 40, offset: 0, filesOnly: false, includeLocals: false }],
  ["files-only", { limit: 10, offset: 0, filesOnly: true, includeLocals: false }],
  ["offset-past-end", { limit: 40, offset: 51, filesOnly: false, includeLocals: false }],
];

test("the fallback ordering note is the one the IDE writes", () => {
  assert.equal(snapshot.order, DEFAULT_ORDER);
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
