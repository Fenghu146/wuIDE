# Bundled agent runtimes

These files are redistributed inside the application bundle. Their licenses are
in `licenses/` and are copied next to them by the packaging scripts.

| File | Package | Version | License | Source |
| --- | --- | --- | --- | --- |
| `dsh-acp`, `dsh-acp-*.exe` | `@brokkai/dsh-acp` | 0.5.0 | MIT | https://github.com/BrokkAi/dsh-acp |
| `node`, `node.exe` | Node.js | 24.22.1 | MIT | https://nodejs.org |
| `node_modules/pi-acp` | `pi-acp` | 0.0.34 | MIT | https://github.com/svkozak/pi-acp |
| `node_modules/@earendil-works/pi-coding-agent` | `@earendil-works/pi-coding-agent` | 1.5.1 | MIT | https://github.com/earendil-works/pi |

`mcp/wu-repo-map/` is not a third-party package: it is Wu's own dependency-free
Node MCP server that lets Pi read the repository map, and it ships under this
repository's license.

## Verifying what is checked in

`dsh-acp` binaries are byte-identical to the ones published in the npm tarball
for the version above. To confirm after an upgrade:

```sh
# macOS / Linux, arm64 shown; use x86_64-apple-darwin or *-unknown-linux-gnu
curl -sL https://registry.npmjs.org/@brokkai/dsh-acp/-/dsh-acp-0.5.0.tgz \
  | tar xzO package/native/aarch64-apple-darwin/dsh-acp \
  | shasum -a 256
shasum -a 256 dsh-acp-darwin-aarch64
```

`node.exe` is the official `win-x64/node.exe` from `nodejs.org/dist/v24.22.1`,
verified against that release's `SHASUMS256.txt`. On macOS and Linux, `node` is
not checked in: `script/fetch-node` downloads the official build for the target
architecture at packaging time and verifies it the same way.

`node_modules` is installed from `package-lock.json`, which pins every one of
the 126 packages with an `integrity` hash. Reinstall it with:

```sh
npm ci
```

## Upgrading one of these

1. Bump the version in `package.json` and run `npm ci` to refresh
   `node_modules` and the lockfile.
2. Replace the `dsh-acp` binaries from the matching npm tarball.
3. Update the table above, including the new version numbers.
4. Re-verify the hashes and rebuild a bundle; `script/bundle-mac`,
   `script/bundle-linux` and `script/bundle-windows.ps1` all fail if a required
   runtime is missing.
