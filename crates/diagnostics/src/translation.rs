//! Translating a diagnostic into Simplified Chinese where it is displayed.
//!
//! Compiler and language server messages arrive in English and none of the
//! servers can localize them, so the translation is a single model request
//! that the user asks for by clicking a link in the diagnostic. The result
//! replaces the message in place and the original stays one click away.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use gpui::{
    App, AppContext as _, Context, Entity, Global, SharedString, Task, TaskExt as _, WeakEntity,
};
use i18n::t;
use language::LanguageRegistry;
use markdown::Markdown;
use project::project_settings::ProjectSettings;
use settings::Settings as _;
use text::BufferId;
use util::ResultExt as _;

/// A request that has not answered by now is treated as failed, so the
/// diagnostic never sits on "translating" forever.
pub(crate) const TRANSLATION_TIMEOUT: Duration = Duration::from_secs(15);

const CACHE_FILE: &str = "diagnostic_translations.json";
const MAX_CACHED_TRANSLATIONS: usize = 1000;
const CODE_CONTEXT_ROWS: u32 = 3;
const MAX_CODE_CONTEXT_CHARS: usize = 2000;

const LINK_PREFIX: &str = "file://#diagnostic-";

/// One diagnostic on screen: its buffer, its group and its position in the
/// group. The same diagnostic reaches the renderer from an inline block, the
/// Problems panel and a hover popover, and all three share this one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TranslationId {
    pub buffer_id: BufferId,
    pub group_id: usize,
    pub index: usize,
}

impl TranslationId {
    fn link(self, action: &str) -> String {
        format!(
            "{LINK_PREFIX}{action}-{}-{}-{}",
            self.buffer_id, self.group_id, self.index
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TranslationAction {
    /// Ask the model for a translation; bypasses a cached one when forced.
    Translate { force: bool },
    /// Put the original message back on screen.
    ShowOriginal,
    /// Put the translation back after the original was shown.
    ShowTranslation,
}

pub(crate) fn parse_link(link: &str) -> Option<(TranslationAction, TranslationId)> {
    let rest = link.strip_prefix(LINK_PREFIX)?;
    let mut parts = rest.split('-');
    let action = match parts.next()? {
        "translate" => TranslationAction::Translate { force: false },
        "retranslate" => TranslationAction::Translate { force: true },
        "original" => TranslationAction::ShowOriginal,
        "translation" => TranslationAction::ShowTranslation,
        _ => return None,
    };
    let buffer_id = BufferId::new(parts.next()?.parse().ok()?).ok()?;
    let group_id = parts.next()?.parse().ok()?;
    let index = parts.next()?.parse().ok()?;
    Some((
        action,
        TranslationId {
            buffer_id,
            group_id,
            index,
        },
    ))
}

/// What the model is given: the message, the lines around it and where it
/// came from. The whole file is deliberately not included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TranslationRequest {
    pub message: String,
    pub language: Option<String>,
    pub code: String,
    pub path: Option<String>,
    pub line: u32,
}

impl TranslationRequest {
    /// Two diagnostics with the same message and the same surrounding code
    /// need the same translation, wherever they are shown.
    fn cache_key(&self) -> String {
        format!(
            "{}\u{0}{}\u{0}{}",
            self.language.as_deref().unwrap_or_default(),
            self.message,
            self.code
        )
    }
}

pub(crate) fn prompt(request: &TranslationRequest) -> String {
    let mut prompt = String::from(
        "Translate this compiler diagnostic into Simplified Chinese.\n\
         - Translate the message only: do not explain it, do not review the code and do not suggest a fix.\n\
         - Keep code identifiers, file paths, line numbers and the text inside `backticks` exactly as they are; do not translate them.\n\
         - Reply with the translated diagnostic and nothing else.\n",
    );
    if let Some(path) = &request.path {
        prompt.push_str(&format!("\nFile: {path}:{}\n", request.line));
    }
    if let Some(language) = &request.language {
        prompt.push_str(&format!("Language: {language}\n"));
    }
    prompt.push_str("\nDiagnostic:\n\n");
    prompt.push_str(&request.message);
    if !request.code.trim().is_empty() {
        prompt.push_str("\n\nCode:\n\n```");
        if let Some(language) = &request.language {
            prompt.push_str(language);
        }
        prompt.push('\n');
        prompt.push_str(&request.code);
        prompt.push_str("\n```\n");
    }
    prompt
}

/// The lines around a diagnostic, which is all the model needs to read the
/// message in context.
pub(crate) fn code_context(buffer: &text::BufferSnapshot, start: text::Point) -> String {
    let first_row = start.row.saturating_sub(CODE_CONTEXT_ROWS);
    let last_row = (start.row + CODE_CONTEXT_ROWS).min(buffer.max_point().row);
    let end = text::Point::new(last_row, buffer.line_len(last_row));
    let code = buffer
        .text_for_range(text::Point::new(first_row, 0)..end)
        .collect::<String>();
    if code.chars().count() <= MAX_CODE_CONTEXT_CHARS {
        return code;
    }
    code.chars().take(MAX_CODE_CONTEXT_CHARS).collect()
}

/// How a diagnostic's markdown is put together: the message, which a
/// translation replaces, and everything that stays next to it.
#[derive(Clone)]
pub(crate) struct TranslationChunk {
    pub message: String,
    /// The source and code tag and the links to related diagnostics.
    pub rest: String,
    pub diagnostic: Option<TranslationId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TranslationFailure {
    NoModel,
    Timeout,
    NoProject,
    Failed(SharedString),
}

impl TranslationFailure {
    fn message(&self) -> SharedString {
        match self {
            Self::NoModel => t!("Translation failed: no model configured"),
            Self::Timeout => t!("Translation failed: timed out"),
            Self::NoProject => t!("Translation failed: no project is open"),
            Self::Failed(error) => t!("Translation failed: {error}", error = error),
        }
    }
}

/// Runs the request behind the translate link. The implementation is a
/// global so that a test can answer without a model and the diagnostics crate
/// does not have to know which agent runs it.
pub(crate) trait DiagnosticTranslator: 'static {
    fn translate(
        &self,
        prompt: String,
        cwd: PathBuf,
        languages: Arc<LanguageRegistry>,
        cx: &mut App,
    ) -> Task<Result<String, TranslationFailure>>;
}

struct GlobalTranslator(Arc<dyn DiagnosticTranslator>);
impl Global for GlobalTranslator {}

struct AgentTranslator;

impl DiagnosticTranslator for AgentTranslator {
    fn translate(
        &self,
        prompt: String,
        cwd: PathBuf,
        languages: Arc<LanguageRegistry>,
        cx: &mut App,
    ) -> Task<Result<String, TranslationFailure>> {
        let work = agent_chat::run_prompt_once(prompt, cwd, languages, cx);
        cx.background_spawn(async move {
            work.await.map_err(|failure| match failure {
                agent_chat::OneShotFailure::NoAgent => TranslationFailure::NoModel,
                agent_chat::OneShotFailure::Failed(error) => {
                    TranslationFailure::Failed(error.into())
                }
            })
        })
    }
}

#[derive(Default)]
enum Status {
    #[default]
    Loading,
    Ready,
    Failed(TranslationFailure),
}

struct Entry {
    status: Status,
    translated: SharedString,
    show_original: bool,
}

struct MarkdownView {
    markdown: WeakEntity<Markdown>,
    chunks: Vec<TranslationChunk>,
}

pub(crate) struct DiagnosticTranslations {
    entries: HashMap<TranslationId, Entry>,
    /// Answers by cache key, so a diagnostic translated once is never asked
    /// for twice.
    cache: HashMap<String, SharedString>,
    views: Vec<MarkdownView>,
    pending: HashMap<TranslationId, Task<()>>,
    cache_path: Option<Arc<Path>>,
}

impl DiagnosticTranslations {
    fn new(cache_path: Option<Arc<Path>>, _cx: &mut Context<Self>) -> Self {
        let cache = cache_path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).log_err())
            .and_then(|text| {
                serde_json::from_str::<HashMap<String, String>>(&text)
                    .log_err()
                    .map(|cache| {
                        cache
                            .into_iter()
                            .map(|(key, translated)| (key, SharedString::from(translated)))
                            .collect()
                    })
            })
            .unwrap_or_default();
        Self {
            entries: HashMap::default(),
            cache,
            views: Vec::new(),
            pending: HashMap::default(),
            cache_path,
        }
    }

    /// The markdown a block shows: every chunk in order, each followed by its
    /// translation state.
    pub(crate) fn source(&self, chunks: &[TranslationChunk], enabled: bool) -> String {
        chunks
            .iter()
            .map(|chunk| self.chunk_source(chunk, enabled))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn chunk_source(&self, chunk: &TranslationChunk, enabled: bool) -> String {
        let original = || format!("{}{}", chunk.message, chunk.rest);
        let Some(id) = chunk.diagnostic.filter(|_| enabled) else {
            return original();
        };
        match self.entries.get(&id) {
            None => format!(
                "{}\n\n[{}]({})",
                original(),
                t!("Translate to Chinese"),
                id.link("translate")
            ),
            Some(entry) => match &entry.status {
                Status::Loading => format!("{}\n\n{}", original(), t!("Translating…")),
                Status::Failed(error) => format!(
                    "{}\n\n{}\n\n[{}]({})",
                    original(),
                    error.message(),
                    t!("Retry"),
                    id.link("retranslate")
                ),
                Status::Ready if entry.show_original => format!(
                    "{}\n\n[{}]({})",
                    original(),
                    t!("Translation"),
                    id.link("translation")
                ),
                Status::Ready => format!(
                    "{}{}\n\n[{}]({}) · [{}]({})",
                    entry.translated,
                    chunk.rest,
                    t!("Original"),
                    id.link("original"),
                    t!("Translate Again"),
                    id.link("retranslate")
                ),
            },
        }
    }

    /// Keeps the markdown created for a block or a hover popover in step with
    /// the translation state, since neither is rebuilt when only a
    /// translation changes.
    pub(crate) fn register_view(
        &mut self,
        markdown: &Entity<Markdown>,
        chunks: Vec<TranslationChunk>,
    ) {
        let id = markdown.entity_id();
        self.views.retain(|view| view.markdown.entity_id() != id);
        self.views.retain(|view| view.markdown.upgrade().is_some());
        self.views.push(MarkdownView {
            markdown: markdown.downgrade(),
            chunks,
        });
    }

    pub(crate) fn show_original(
        &mut self,
        id: TranslationId,
        show_original: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.show_original = show_original;
        }
        self.refresh_views(cx);
    }

    pub(crate) fn translate(
        &mut self,
        id: TranslationId,
        request: TranslationRequest,
        force: bool,
        cwd: PathBuf,
        languages: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) {
        let cache_key = request.cache_key();
        if force {
            self.cache.remove(&cache_key);
        } else if let Some(translated) = self.cache.get(&cache_key).cloned() {
            self.entries.insert(
                id,
                Entry {
                    status: Status::Ready,
                    translated,
                    show_original: false,
                },
            );
            self.refresh_views(cx);
            return;
        }

        self.entries.insert(
            id,
            Entry {
                status: Status::Loading,
                translated: SharedString::default(),
                show_original: false,
            },
        );
        self.refresh_views(cx);

        let translator = cx
            .try_global::<GlobalTranslator>()
            .map(|translator| translator.0.clone());
        let Some(translator) = translator else {
            self.fail(id, TranslationFailure::NoModel, cx);
            return;
        };
        let work = translator.translate(prompt(&request), cwd, languages, cx);
        let executor = cx.background_executor().clone();
        let timed = futures_lite::future::or(work, async move {
            executor.timer(TRANSLATION_TIMEOUT).await;
            Err(TranslationFailure::Timeout)
        });
        let task = cx.spawn(async move |this, cx| {
            let outcome = timed.await;
            this.update(cx, |this, cx| this.finish(id, cache_key, outcome, cx))
                .log_err();
        });
        if let Some(previous) = self.pending.insert(id, task) {
            drop(previous);
        }
    }

    fn finish(
        &mut self,
        id: TranslationId,
        cache_key: String,
        outcome: Result<String, TranslationFailure>,
        cx: &mut Context<Self>,
    ) {
        self.pending.remove(&id);
        let outcome = outcome.and_then(|translated| {
            let translated = translated.trim();
            if translated.is_empty() {
                Err(TranslationFailure::Failed(t!(
                    "The model returned no translation."
                )))
            } else {
                Ok(SharedString::from(translated.to_owned()))
            }
        });
        match outcome {
            Ok(translated) => {
                if self.cache.len() >= MAX_CACHED_TRANSLATIONS
                    && let Some(evicted) = self.cache.keys().next().cloned()
                {
                    self.cache.remove(&evicted);
                }
                self.cache.insert(cache_key, translated.clone());
                if let Some(entry) = self.entries.get_mut(&id) {
                    entry.status = Status::Ready;
                    entry.translated = translated;
                    entry.show_original = false;
                }
                self.save(cx);
            }
            Err(failure) => {
                if let Some(entry) = self.entries.get_mut(&id) {
                    entry.status = Status::Failed(failure);
                }
            }
        }
        self.refresh_views(cx);
    }

    fn fail(&mut self, id: TranslationId, failure: TranslationFailure, cx: &mut Context<Self>) {
        self.entries.insert(
            id,
            Entry {
                status: Status::Failed(failure),
                translated: SharedString::default(),
                show_original: false,
            },
        );
        self.refresh_views(cx);
    }

    fn refresh_views(&mut self, cx: &mut Context<Self>) {
        if self.views.is_empty() {
            return;
        }
        let enabled = ProjectSettings::get_global(cx)
            .diagnostics
            .translate_with_model;
        let views = std::mem::take(&mut self.views);
        for view in views {
            let Some(markdown) = view.markdown.upgrade() else {
                continue;
            };
            let source = self.source(&view.chunks, enabled);
            if markdown.read(cx).source().as_ref() != source {
                markdown.update(cx, |markdown, cx| markdown.replace(source, cx));
            }
            self.views.push(view);
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.cache_path.clone() else {
            return;
        };
        let cache = self
            .cache
            .iter()
            .map(|(key, translated)| (key.clone(), translated.to_string()))
            .collect::<HashMap<_, _>>();
        cx.background_spawn(async move {
            let text = serde_json::to_string(&cache)?;
            let parent = path.parent();
            if let Some(parent) = parent {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, text)
        })
        .detach_and_log_err(cx);
    }
}

struct GlobalTranslations(Entity<DiagnosticTranslations>);
impl Global for GlobalTranslations {}

pub(crate) fn translations(cx: &App) -> Option<Entity<DiagnosticTranslations>> {
    cx.try_global::<GlobalTranslations>()
        .map(|translations| translations.0.clone())
}

/// Applies a click on one of the links the translation section adds.
pub(crate) fn handle_link(
    editor: &mut editor::Editor,
    action: TranslationAction,
    id: TranslationId,
    cx: &mut App,
) {
    let Some(translations) = translations(cx) else {
        return;
    };
    match action {
        TranslationAction::ShowOriginal => {
            translations.update(cx, |translations, cx| {
                translations.show_original(id, true, cx)
            });
        }
        TranslationAction::ShowTranslation => {
            translations.update(cx, |translations, cx| {
                translations.show_original(id, false, cx)
            });
        }
        TranslationAction::Translate { force } => {
            let request = translation_request(editor, id, cx);
            let Some((cwd, languages)) = project_context(editor, cx) else {
                translations.update(cx, |translations, cx| {
                    translations.fail(id, TranslationFailure::NoProject, cx)
                });
                return;
            };
            let Some(request) = request else {
                translations.update(cx, |translations, cx| {
                    translations.fail(
                        id,
                        TranslationFailure::Failed(t!("The diagnostic is no longer on screen.")),
                        cx,
                    )
                });
                return;
            };
            translations.update(cx, |translations, cx| {
                translations.translate(id, request, force, cwd, languages, cx)
            });
        }
    }
}

fn translation_request(
    editor: &editor::Editor,
    id: TranslationId,
    cx: &mut App,
) -> Option<TranslationRequest> {
    let multibuffer = editor.buffer().read(cx).snapshot(cx);
    let entry = multibuffer
        .diagnostic_group(id.buffer_id, id.group_id)
        .nth(id.index)?;
    let start = entry.range.start;
    let message = entry.diagnostic.message.as_str().to_owned();
    let buffer = editor.buffer().read(cx).buffer(id.buffer_id)?;
    let buffer = buffer.read(cx);
    let snapshot = buffer.snapshot();
    Some(TranslationRequest {
        message,
        language: buffer
            .language()
            .map(|language| language.name().to_string()),
        code: code_context(&snapshot, start),
        path: buffer.file().map(|file| file.path().to_string()),
        line: start.row + 1,
    })
}

fn project_context(
    editor: &editor::Editor,
    cx: &mut App,
) -> Option<(PathBuf, Arc<LanguageRegistry>)> {
    if let Some(project) = editor.project() {
        let languages = project.read(cx).languages().clone();
        let cwd = project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .unwrap_or_else(|| std::env::temp_dir());
        return Some((cwd, languages));
    }
    let state = workspace::AppState::try_global(cx)?;
    Some((std::env::temp_dir(), state.languages.clone()))
}

pub(crate) fn init(cx: &mut App) {
    // A test never gets past a model request, so it has no business writing to
    // the user's data directory.
    let cache_path = if cfg!(test) {
        None
    } else {
        Some(paths::data_dir().join(CACHE_FILE).into())
    };
    let translations = cx.new(|cx| DiagnosticTranslations::new(cache_path, cx));
    cx.set_global(GlobalTranslations(translations));
    cx.set_global(GlobalTranslator(Arc::new(AgentTranslator)));
}

#[cfg(test)]
pub(crate) fn set_translator(translator: impl DiagnosticTranslator, cx: &mut App) {
    cx.set_global(GlobalTranslator(Arc::new(translator)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> TranslationRequest {
        TranslationRequest {
            message: "use of undeclared identifier 'x'".to_owned(),
            language: Some("c".to_owned()),
            code: "int main() {\n  x = 1;\n}".to_owned(),
            path: Some("src/main.c".to_owned()),
            line: 2,
        }
    }

    #[test]
    fn the_prompt_asks_for_a_translation_and_nothing_else() {
        let text = prompt(&request());

        for constraint in [
            "Simplified Chinese",
            "do not explain",
            "do not review the code",
            "do not suggest a fix",
            "do not translate them",
            "nothing else",
        ] {
            assert!(text.contains(constraint), "{constraint:?} missing:\n{text}");
        }
        assert!(text.contains("use of undeclared identifier 'x'"));
        assert!(text.contains("  x = 1;"));
        assert!(text.contains("src/main.c:2"));
    }

    #[test]
    fn the_prompt_carries_no_more_code_than_the_lines_it_was_given() {
        let mut request = request();
        request.code = "only the context".to_owned();
        let text = prompt(&request);
        assert!(!text.contains("int main"), "{text}");
    }

    #[test]
    fn translation_links_round_trip_and_leave_hint_links_alone() {
        let id = TranslationId {
            buffer_id: BufferId::new(7).unwrap(),
            group_id: 3,
            index: 1,
        };
        for (action, name) in [
            (TranslationAction::Translate { force: false }, "translate"),
            (TranslationAction::Translate { force: true }, "retranslate"),
            (TranslationAction::ShowOriginal, "original"),
            (TranslationAction::ShowTranslation, "translation"),
        ] {
            assert_eq!(parse_link(&id.link(name)), Some((action, id)));
        }
        assert_eq!(parse_link("file://#diagnostic-7-3-1"), None);
    }

    #[test]
    fn a_translation_is_cached_by_its_message_code_and_language() {
        let request = request();
        let key = request.cache_key();

        let mut other_language = request.clone();
        other_language.language = Some("cpp".to_owned());
        assert_ne!(key, other_language.cache_key());

        let mut other_code = request.clone();
        other_code.code = "void f() {}".to_owned();
        assert_ne!(key, other_code.cache_key());

        // Where the diagnostic sits does not change the translation it needs.
        let mut moved = request;
        moved.path = Some("elsewhere/main.c".to_owned());
        moved.line = 40;
        assert_eq!(key, moved.cache_key());
    }
}
