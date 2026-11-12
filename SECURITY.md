# Security policy

This is a personal fork of [dstrml/wu](https://github.com/dstrml/wu), itself a
fork of [Zed](https://github.com/zed-industries/zed). It is maintained by one
person, in their own time.

## Reporting a vulnerability

Please **do not open a public issue** for a security problem.

Use GitHub's private reporting instead: go to the **Security** tab of this
repository and choose **Report a vulnerability**. That opens a private advisory
visible only to the maintainer.

If private reporting is unavailable, open an issue that says only that you have
a security report and how to reach you — no details, no reproduction steps.

## What is in scope

Anything in this repository's own code, in particular:

- the ACP agent driver (`crates/agent_harness/src/acp.rs`) and the way it spawns
  bundled agent processes, passes environment variables, and isolates their
  data directories;
- the auto-updater (`crates/auto_update/`), which downloads and replaces the
  installed application;
- the packaging scripts under `script/`, including what they copy into a
  distributable bundle.

## What is inherited, not owned, here

Bugs in Zed or Wu itself are best reported upstream:

- Zed — https://github.com/zed-industries/zed/security
- Wu — https://github.com/dstrml/wu/security

Third-party components have their own channels:

- Node.js — https://github.com/nodejs/node/security/policy
- `@brokkai/dsh-acp` — https://github.com/BrokkAi/dsh-acp
- `pi-acp` — https://github.com/svkozak/pi-acp
- `@earendil-works/pi-coding-agent` — https://github.com/earendil-works/pi-coding-agent

## Known limitations, not vulnerabilities

These are documented trade-offs. They are still worth reporting if you think the
documentation understates the risk, but they are not surprises:

- **Releases are not code-signed.** macOS and Windows will warn on first launch.
  See `docs/INSTALL.md`.
- **Updates are integrity-checked, not authenticity-checked.** The updater
  verifies the SHA-256 digest GitHub publishes for a release asset. That catches
  corruption, not a compromised release. Anyone who can publish a release to
  this repository can push code to every install.
- **The repository must stay public** for the updater to work, since release
  metadata is fetched anonymously.
- **Bundled agent runtimes are pinned by content, not by signature.** Their
  versions are recorded in `FORK.md`.

## Supported versions

Only the latest release is supported. There are no backports.
