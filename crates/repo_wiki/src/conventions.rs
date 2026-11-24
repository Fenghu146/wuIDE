//! Project conventions (AGENTS.md): the shared human/agent contract.
//!
//! Claude Code, Codex and OpenCode read a worktree-root AGENTS.md natively
//! when they run there, so WuIDE does not inject it again for those
//! harnesses. The `/conventions` chat command and the repo map panel make the
//! same files reachable for every other harness and for the user, and the
//! user-global AGENTS.md (`paths::agents_file`) finally gets a consumer.

use std::path::PathBuf;

use gpui::{App, AppContext as _, Task};

/// Per-project conventions file name.
pub const AGENTS_FILE_NAME: &str = "AGENTS.md";

/// Per-file cap, so a runaway conventions file cannot silently consume the
/// chat context.
const MAX_FILE_BYTES: usize = 64 * 1024;

/// One conventions file, gathered for injection into a chat session.
pub struct ConventionsDoc {
    /// Absolute path of the file this block came from, shown to the user and
    /// the agent so both sides can trace the source.
    pub path: PathBuf,
    pub content: String,
}

/// Starter template written when creating a missing project AGENTS.md.
pub const AGENTS_TEMPLATE: &str = r#"# 项目约定（AGENTS.md）

本文件是人与 AI 智能体共同遵守的项目约定。Claude Code、Codex、OpenCode
在项目根目录运行时会自动读取本文件；其他智能体可在聊天中使用
/conventions 命令注入本文件内容。

## 项目结构

- （描述主要模块及其职责）

## 构建与测试

- 构建：cargo check -p <crate>
- 测试：cargo test -p <crate>

## 代码约定

- （填写本项目特有的约定；通用风格放在全局 AGENTS.md）

## 注意事项

- （填写陷阱、不可触碰的区域、容易出错的地方）
"#;

/// Reads AGENTS.md from each project root plus the user-global file, in that
/// order. Missing files are skipped.
pub fn gather_conventions(project_roots: &[PathBuf]) -> Vec<ConventionsDoc> {
    let mut paths: Vec<PathBuf> = project_roots
        .iter()
        .map(|root| root.join(AGENTS_FILE_NAME))
        .collect();
    paths.push(paths::agents_file().to_path_buf());
    gather_from_paths(&paths)
}

/// Reads the given conventions files, skipping missing ones and dropping
/// duplicates that resolve to the same file.
pub fn gather_from_paths(paths: &[PathBuf]) -> Vec<ConventionsDoc> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut docs = Vec::new();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.clone());
        if seen.iter().any(|seen| seen == &canonical) {
            continue;
        }
        seen.push(canonical);
        match std::fs::read_to_string(path) {
            Ok(content) => docs.push(ConventionsDoc {
                path: path.clone(),
                content: truncate(content, MAX_FILE_BYTES),
            }),
            Err(error) => log::warn!("failed to read {}: {error}", path.display()),
        }
    }
    docs
}

/// Formats the gathered files as one markdown block, `None` when there is
/// nothing to inject.
pub fn conventions_markdown(docs: &[ConventionsDoc]) -> Option<String> {
    if docs.is_empty() {
        return None;
    }
    let mut markdown = String::from(
        "## Project conventions (AGENTS.md)\n\nFollow these unless the user explicitly overrides them.\n",
    );
    for doc in docs {
        markdown.push_str(&format!(
            "\n### {}\n\n{}\n",
            doc.path.display(),
            doc.content.trim_end()
        ));
    }
    Some(markdown)
}

fn truncate(mut content: String, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content;
    }
    let mut end = max_bytes;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    content.truncate(end);
    content.push_str("\n\n[…truncated]");
    content
}

/// Reads the conventions files off the foreground thread.
pub fn conventions_task(project_roots: Vec<PathBuf>, cx: &App) -> Task<Vec<ConventionsDoc>> {
    cx.background_spawn(async move { gather_conventions(&project_roots) })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("repo_wiki_{}_{}", std::process::id(), name));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn gathers_existing_files_in_order_and_skips_missing() {
        let first = TempDir::new("first");
        let second = TempDir::new("second");
        std::fs::write(first.join(AGENTS_FILE_NAME), "one").unwrap();
        std::fs::write(second.join(AGENTS_FILE_NAME), "two").unwrap();

        let docs = gather_from_paths(&[
            first.join(AGENTS_FILE_NAME),
            second.join(AGENTS_FILE_NAME),
            second.0.join("missing/AGENTS.md"),
        ]);
        let contents: Vec<&str> = docs.iter().map(|doc| doc.content.as_str()).collect();
        assert_eq!(contents, ["one", "two"]);
    }

    #[test]
    fn deduplicates_repeated_paths() {
        let dir = TempDir::new("dedup");
        let path = dir.join(AGENTS_FILE_NAME);
        std::fs::write(&path, "same").unwrap();

        let docs = gather_from_paths(&[path.clone(), path]);
        assert_eq!(docs.len(), 1);
    }

    #[test]
    fn truncates_on_char_boundary_and_adds_a_note() {
        // Each '哈' is three bytes, so a two-thirds cap cannot land on a
        // boundary.
        let content = "哈".repeat(30);
        assert_eq!(content.len(), 90);

        let truncated = truncate(content, 40);
        assert!(truncated.starts_with('哈'));
        assert!(truncated.ends_with("[…truncated]"));
        assert!(truncated.len() < 40 + 20);
    }

    #[test]
    fn markdown_is_none_without_docs_and_labels_sources() {
        assert!(conventions_markdown(&[]).is_none());

        let doc = ConventionsDoc {
            path: PathBuf::from("/tmp/AGENTS.md"),
            content: "be kind".into(),
        };
        let markdown = conventions_markdown(&[doc]).unwrap();
        assert!(markdown.contains("Project conventions"));
        assert!(markdown.contains("/tmp/AGENTS.md"));
        assert!(markdown.contains("be kind"));
    }

    #[test]
    fn template_is_utf8_markdown() {
        // The template ships in the binary and is written as-is to disk.
        assert!(AGENTS_TEMPLATE.starts_with("# "));
        assert!(std::str::from_utf8(AGENTS_TEMPLATE.as_bytes()).is_ok());
    }
}
