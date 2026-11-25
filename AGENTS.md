# Rust coding guidelines

* Prioritize code correctness and clarity. Speed and efficiency are secondary priorities unless otherwise specified.
* Do not write organizational or comments that summarize the code. Comments should only be written in order to explain "why" the code is written in some way in the case there is a reason that is tricky / non-obvious.
* Prefer implementing functionality in existing files unless it is a new logical component. Avoid creating many small files.
* Avoid using functions that panic like `unwrap()`, instead use mechanisms like `?` to propagate errors.
* Be careful with operations like indexing which may panic if the indexes are out of bounds.
* Never silently discard errors with `let _ =` on fallible operations. Always handle errors appropriately:
  - Propagate errors with `?` when the calling function should handle them
  - Use `.log_err()` or similar when you need to ignore errors but want visibility
  - Use explicit error handling with `match` or `if let Err(...)` when you need custom logic
  - Example: avoid `let _ = client.request(...).await?;` - use `client.request(...).await?;` instead
* When implementing async operations that may fail, ensure errors propagate to the UI layer so users get meaningful feedback.
* Never create files with `mod.rs` paths - prefer `src/some_module.rs` instead of `src/some_module/mod.rs`.
* When creating new crates, prefer specifying the library root path in `Cargo.toml` using `[lib] path = "...rs"` instead of the default `lib.rs`, to maintain consistent and descriptive naming (e.g., `gpui.rs` or `main.rs`).
* Avoid creative additions unless explicitly requested
* Use full words for variable names (no abbreviations like "q" for "queue")
* Use variable shadowing to scope clones in async contexts for clarity, minimizing the lifetime of borrowed references.
  Example:
  ```rust
  executor.spawn({
      let task_ran = task_ran.clone();
      async move {
          *task_ran.borrow_mut() = true;
      }
  });
  ```

# Timers in tests

* In GPUI tests, prefer GPUI executor timers over `smol::Timer::after(...)` when you need timeouts, delays, or to drive `run_until_parked()`:
  - Use `cx.background_executor().timer(duration).await` (or `cx.background_executor.timer(duration).await` in `TestAppContext`) so the work is scheduled on GPUI's dispatcher.
  - Avoid `smol::Timer::after(...)` for test timeouts when you rely on `run_until_parked()`, because it may not be tracked by GPUI's scheduler and can lead to "nothing left to run" when pumping.

# GPUI

GPUI is a UI framework which also provides primitives for state and concurrency management.

## Context

Context types allow interaction with global state, windows, entities, and system services. They are typically passed to functions as the argument named `cx`. When a function takes callbacks they come after the `cx` parameter.

* `App` is the root context type, providing access to global state and read and update of entities.
* `Context<T>` is provided when updating an `Entity<T>`. This context dereferences into `App`, so functions which take `&App` can also take `&Context<T>`.
* `AsyncApp` and `AsyncWindowContext` are provided by `cx.spawn` and `cx.spawn_in`. These can be held across await points.

## `Window`

`Window` provides access to the state of an application window. It is passed to functions as an argument named `window` and comes before `cx` when present. It is used for managing focus, dispatching actions, directly drawing, getting user input state, etc.

## Entities

An `Entity<T>` is a handle to state of type `T`. With `thing: Entity<T>`:

* `thing.entity_id()` returns `EntityId`
* `thing.downgrade()` returns `WeakEntity<T>`
* `thing.read(cx: &App)` returns `&T`.
* `thing.read_with(cx, |thing: &T, cx: &App| ...)` returns the closure's return value.
* `thing.update(cx, |thing: &mut T, cx: &mut Context<T>| ...)` allows the closure to mutate the state, and provides a `Context<T>` for interacting with the entity. It returns the closure's return value.
* `thing.update_in(cx, |thing: &mut T, window: &mut Window, cx: &mut Context<T>| ...)` takes a `AsyncWindowContext` or `VisualTestContext`. It's the same as `update` while also providing the `Window`.

Within the closures, the inner `cx` provided to the closure must be used instead of the outer `cx` to avoid issues with multiple borrows.

Trying to update an entity while it's already being updated must be avoided as this will cause a panic.

`WeakEntity<T>` is a weak handle. It has `read_with`, `update`, and `update_in` methods that work the same, but always return an `anyhow::Result` so that they can fail if the entity no longer exists. This can be useful to avoid memory leaks - if entities have mutually recursive handles to each other they will never be dropped.

## Concurrency

All use of entities and UI rendering occurs on a single foreground thread.

`cx.spawn(async move |cx| ...)` runs an async closure on the foreground thread. Within the closure, `cx` is `&mut AsyncApp`.

When the outer cx is a `Context<T>`, the use of `spawn` instead looks like `cx.spawn(async move |this, cx| ...)`, where `this: WeakEntity<T>` and `cx: &mut AsyncApp`.

To do work on other threads, `cx.background_spawn(async move { ... })` is used. Often this background task is awaited on by a foreground task which uses the results to update state.

Both `cx.spawn` and `cx.background_spawn` return a `Task<R>`, which is a future that can be awaited upon. If this task is dropped, then its work is cancelled. To prevent this one of the following must be done:

* Awaiting the task in some other async context.
* Detaching the task via `task.detach()` or `task.detach_and_log_err(cx)`, allowing it to run indefinitely.
* Storing the task in a field, if the work should be halted when the struct is dropped.

A task which doesn't do anything but provide a value can be created with `Task::ready(value)`.

## Elements

The `Render` trait is used to render some state into an element tree that is laid out using flexbox layout. An `Entity<T>` where `T` implements `Render` is sometimes called a "view".

Example:

```
struct TextWithBorder(SharedString);

impl Render for TextWithBorder {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().border_1().child(self.0.clone())
    }
}
```

Since `impl IntoElement for SharedString` exists, it can be used as an argument to `child`. `SharedString` is used to avoid copying strings, and is either an `&'static str` or `Arc<str>`.

UI components that are constructed just to be turned into elements can instead implement the `RenderOnce` trait, which is similar to `Render`, but its `render` method takes ownership of `self` and receives `&mut App` instead of `&mut Context<Self>`. Types that implement this trait can use `#[derive(IntoElement)]` to use them directly as children.

The style methods on elements are similar to those used by Tailwind CSS.

If some attributes or children of an element tree are conditional, `.when(condition, |this| ...)` can be used to run the closure only when `condition` is true. Similarly, `.when_some(option, |this, value| ...)` runs the closure when the `Option` has a value.

## Input events

Input event handlers can be registered on an element via methods like `.on_click(|event, window, cx: &mut App| ...)`.

Often event handlers will want to update the entity that's in the current `Context<T>`. The `cx.listener` method provides this - its use looks like `.on_click(cx.listener(|this: &mut T, event, window, cx: &mut Context<T>| ...)`.

## Actions

Actions are dispatched via user keyboard interaction or in code via `window.dispatch_action(SomeAction.boxed_clone(), cx)` or `focus_handle.dispatch_action(&SomeAction, window, cx)`.

Actions with no data are defined with the `actions!(some_namespace, [SomeAction, AnotherAction])` macro call. Otherwise the `Action` derive macro is used. Doc comments on actions are displayed to the user.

Action handlers can be registered on an element via the event handler `.on_action(|action, window, cx| ...)`. Like other event handlers, this is often used with `cx.listener`.

## Notify

When a view's state has changed in a way that may affect its rendering, it should call `cx.notify()`. This will cause the view to be rerendered. It will also cause any observe callbacks registered for the entity with `cx.observe` to be called.

## Entity events

While updating an entity (`cx: Context<T>`), it can emit an event using `cx.emit(event)`. Entities register which events they can emit by declaring `impl EventEmitter<EventType> for EntityType {}`.

Other entities can then register a callback to handle these events by doing `cx.subscribe(other_entity, |this, other_entity, event, cx| ...)`. This will return a `Subscription` which deregisters the callback when dropped.  Typically `cx.subscribe` happens when creating a new entity and the subscriptions are stored in a `_subscriptions: Vec<Subscription>` field.

## Build guidelines

- Use `./script/clippy` instead of `cargo clippy`

# Threads and workspaces

Several agent threads and people work in this repository at once, sharing one main checkout and one `.git`. Every rule below carries its reason, because a rule whose cost is invisible is a rule that gets dropped.

* **Give every thread its own worktree**: `git worktree add ..\<name> -b <branch> origin/main`. The main checkout (`D:\wuIDE-main\wuIDE-main`) is only for merging, releasing, and tagging; no thread may edit files in it. It is a shared resource: when two threads switch branches in the same checkout, a commit lands on the other thread's branch. This has happened: a merge commit ended up on the wrong feature branch and only turned out harmless because `main` was fast-forwarded to it.
* **Build with your own target dir**: `CARGO_TARGET_DIR=D:\cargo-target\<thread>` or `cargo --target-dir`, and delete it when you are done. Concurrent builds sharing one target dir overwrite each other's intermediate artifacts, and the result reads like a code error: a field that exists in the source is reported missing, which costs a long detour to diagnose.
* **Before committing, `git status --short` must list only files you changed.** If someone else's files appear, stop and report them; do not `checkout`, `stash`, or `revert` them, because that is work they have not committed yet and it is gone once you clean it. `git add -A` is the usual way other people's work gets carried into a commit; add paths explicitly.
* **New or changed user-visible text must be in both English and Simplified Chinese.** `script/check-translations` gates releases: `.github/workflows/mac-dmg.yml` and `release.yml` both run it, and a missing Chinese string fails the macOS build.
* **A new enum setting value needs three edits at once**: the arm in `translated_setting_value` (`crates/settings_ui/src/components/dropdown.rs`), a `"setting value|<displayed value>"` entry in `assets/locales/zh-CN/strings/settings_ui.json`, and the setting's schema description. Values that deliberately stay English go into `UNTRANSLATED_SETTING_VALUES` instead. The `settings_ui` translation test walks every dropdown value and looks up its Chinese text, so missing any one of the three turns it red.
* **Do not rustfmt a file you only touched incidentally.** A whole-file reformat drags unrelated lines into your diff, widens the conflict surface, and hides the real change from review.
* **Take a lease before you start and release it when you finish**: `script/lease claim <path|glob>... --note "..."`, then `script/lease release --all-mine`; a long task should `script/lease heartbeat <token>` rather than claim a huge ttl. A claim that overlaps another thread's active lease is refused with the holder, its note and the time left, which is the only moment where a collision is cheap to discover. Leases live in `.git/amp-leases` and expire (two hours by default), so a dead thread does not block the repository forever.
* **A `release-fast` build or packaging takes the build lock**: `script/lease build claim` before it, `script/lease build release` after. Two heavy builds sharing target dirs reproduce the stale-artifact failures above, and the lock is what `script/pre-merge-check --release` waits for.
* **Commit hooks are turned on by whoever runs the merge, not by each thread.** `script/setup-hooks` sets `core.hooksPath` for the whole clone, so enabling them mid-flight changes every running thread's commits at once, and threads that never claimed paths start hitting other threads' leases. They belong to the merge and release window: the orchestrator turns them on there. Once on, they refuse a hand-made commit in the main checkout and a commit that stages files another thread has leased. `AMP_MAIN_COMMIT=1` is the escape hatch for the first of those, and it is needed exactly when you commit to finish a conflicted merge: a clean `git merge` never runs this hook.
* **Run `./script/pre-merge-check` before merging, tagging, or releasing.** It prints PASS, FAIL, or WARN per check. A dirty main checkout, a `main` that differs from `origin/main`, and a lease on files the branch changes block; leftover worktrees, stale worktree records, a branch that does not contain `origin/main`, a running `cargo`/`rustc`, low disk, a held build lock and missing commit hooks only warn. `--release` also blocks on the 60 GB disk requirement and on a held build lock, because a release build genuinely needs the room and must not share target dirs. See `docs/PARALLEL-DEV.md`.

# Runtime traps

These look like unrelated bugs, and each one has already cost a debugging session.

* **A panic must not cross an `unsafe extern "system" fn`.** Window and input callbacks on Windows run inside wndproc, and a panic that unwinds out of them is reported as `panic_cannot_unwind` and aborts the process with `0xC0000409`. There is no recovery point: the `catch_unwind` in `gpui_windows` is not a net, because unwinding through the callback drops gpui's `Lease` and takes the entity with it. Remove the panic at its source — no `unwrap`, `expect`, indexing, or slice range on a fallible path in a wndproc callback. One "Stop" click has ended the whole application this way.
* **When you verify a stop or cancel, assert that the caller survives, not that the target disappeared.** An editor aborting also kills its child processes, so "the subprocess is gone" looks exactly like a successful stop and proves nothing. Check that the view, pane, or app is still alive and responsive afterwards. A false positive is worse than no test, because it convinces the next person there is nothing left to check.
* **Before you verify a GUI, make sure no other instance is running**, or start it with its own `--user-data-dir`. The single-instance check forwards the invocation to the running IDE and exits, so the instance you meant to look at never draws a window and the screenshot waits forever. A thread has lost time to exactly this; a separate data dir makes the instance its own and reachable.
* **A heredoc that shows a command must not expand it.** Write `<<'EOF'`; if the body also interpolates a variable, escape the backticks instead of unquoting the delimiter. An unquoted `<<EOF` runs anything inside backticks or `$(...)`: the main-checkout guard ran `git merge` on every refused commit because its message quoted that command, which would fast-forward a stale `main`. `script/hook-test` fails when a hook runs a git subcommand it does not need.

# Pull request hygiene

When an agent opens or updates a pull request, it must:

- Use a clear, correctly capitalized, imperative PR title (for example, `Fix crash in project panel`).
- Avoid conventional commit prefixes in PR titles (`fix:`, `feat:`, `docs:`, etc.).
- Avoid trailing punctuation in PR titles.
- Optionally prefix the title with a crate name when one crate is the clear scope (for example, `git_ui: Add history view`).
- Include a `Release Notes:` section as the final section in the PR body.
- Use one bullet under `Release Notes:`:
  - `- Added ...`, `- Fixed ...`, or `- Improved ...` for user-facing changes, or
  - `- N/A` for docs-only and other non-user-facing changes.
- Format release notes exactly with a blank line after the heading, for example:

```
Release Notes:

- N/A
```

# Rules Hygiene

These rules are read by every agent session. Keep them high-signal.

## After any agentic session
If you discover a non-obvious pattern that would help future sessions, include a **"Suggested rules additions"** heading in your PR description with the proposed text. Do **not** edit rules inline during normal feature/fix work. Reviewers decide what gets merged.

## High bar for new rules
Editing or clarifying existing rules is always welcome. New rules must meet **all three** criteria:
1. **Non-obvious** — someone familiar with the codebase would still get it wrong without the rule.
2. **Repeatedly encountered** — it came up more than once (multiple hits in one session counts).
3. **Specific enough to act on** — a concrete instruction, not a vague principle.

## What NOT to put in rules
Avoid architectural descriptions of a crate (module layout, data flow, key types). These go stale fast and the agent can gather them by reading the code. Rules should be **traps to avoid**, not **maps to follow**.

## No drive-by additions
Rules emerge from validated patterns, not one-off observations. The workflow is:
1. Agent notes a pattern during a session.
2. Team validates the pattern in code review.
3. A dedicated commit adds the rule with context on *why* it exists.
