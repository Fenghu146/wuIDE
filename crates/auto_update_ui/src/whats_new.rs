//! The release notes of this build, read from the changelog that ships with it.
//!
//! The changelog is embedded at compile time instead of fetched, so the page
//! works offline and its text always matches the binary it belongs to.

use auto_update::release_notes_url;
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, Render, SharedString, Window,
    actions, prelude::*,
};
use i18n::{Language, t};
use ui::{ListBulletItem, prelude::*};
use workspace::{Item, item::ItemEvent};

actions!(
    auto_update,
    [
        /// Shows the release notes for this build.
        ShowWhatsNew
    ]
);

const CHANGELOG: &str = include_str!("../../../.github/release/CHANGELOG.md");
const CHANGELOG_ZH: &str = include_str!("../../../.github/release/CHANGELOG.zh.md");

struct ReleaseNotes {
    version: String,
    date: Option<String>,
    entries: Vec<String>,
    /// `CHANGELOG.zh.md` has no section for this version, so its English text stands in.
    untranslated: bool,
}

fn parse_release_notes(markdown: &str) -> Vec<ReleaseNotes> {
    let mut sections: Vec<ReleaseNotes> = Vec::new();
    let mut current: Option<ReleaseNotes> = None;

    for line in markdown.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            if let Some(section) = current.take() {
                sections.push(section);
            }
            let heading = heading.trim();
            if heading.eq_ignore_ascii_case("unreleased") {
                continue;
            }
            let (version, date) = match heading.split_once(" - ") {
                Some((version, date)) => {
                    (version.trim().to_owned(), Some(date.trim().to_owned()))
                }
                None => (heading.to_owned(), None),
            };
            current = Some(ReleaseNotes {
                version,
                date,
                entries: Vec::new(),
                untranslated: false,
            });
        } else if let Some(section) = current.as_mut()
            && let Some(entry) = line.strip_prefix("- ")
        {
            section.entries.push(entry.trim().to_owned());
        }
    }

    if let Some(section) = current.take() {
        sections.push(section);
    }

    sections
}

/// The notes to show for `language`, version for version.
///
/// The English file decides which versions exist; the Chinese one only replaces the entries of the
/// versions it translates, so a version it has not caught up with still reads in English.
fn release_notes(language: Language, english: &str, chinese: &str) -> Vec<ReleaseNotes> {
    let mut sections = parse_release_notes(english);
    if language != Language::SimplifiedChinese {
        return sections;
    }

    let translated = parse_release_notes(chinese);
    for section in &mut sections {
        match translated
            .iter()
            .find(|translated| translated.version == section.version)
        {
            Some(translated) => section.entries = translated.entries.clone(),
            None => section.untranslated = true,
        }
    }
    sections
}

pub struct WhatsNew {
    focus_handle: FocusHandle,
    sections: Vec<ReleaseNotes>,
    language: Language,
    show_all: bool,
}

impl WhatsNew {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let language = i18n::language();
        Self {
            focus_handle: cx.focus_handle(),
            // The newest section is the one a version bump just added, and the
            // file is ordered newest first (`script/release` puts it first).
            sections: release_notes(language, CHANGELOG, CHANGELOG_ZH),
            language,
            show_all: false,
        }
    }

    fn render_section(&self, section: &ReleaseNotes) -> impl IntoElement {
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .items_baseline()
                    .gap_2()
                    .child(Headline::new(section.version.clone()).size(HeadlineSize::Small))
                    .children(section.date.clone().map(|date| {
                        Label::new(date)
                            .color(Color::Muted)
                            .size(LabelSize::Small)
                    })),
            )
            .when(section.untranslated, |this| {
                this.child(
                    Label::new(t!("No Chinese notes for this version."))
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
            })
            .children(
                section
                    .entries
                    .iter()
                    .map(|entry| ListBulletItem::new(entry.clone())),
            )
    }
}

impl Render for WhatsNew {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The language can change while this tab is open, and the settings
        // observer only refreshes the window instead of rebuilding the item.
        if self.language != i18n::language() {
            self.language = i18n::language();
            self.sections = release_notes(self.language, CHANGELOG, CHANGELOG_ZH);
        }

        let shown = if self.show_all {
            self.sections.len()
        } else {
            self.sections.len().min(1)
        };
        let total = self.sections.len();

        v_flex()
            .id("whats-new")
            .key_context("WhatsNew")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().colors().editor_background)
            .p_8()
            .gap_6()
            .child(Headline::new(t!("What's New")))
            .child(
                Label::new(t!(
                    "These notes ship with this build, so they read the same offline."
                ))
                .color(Color::Muted),
            )
            .when(self.sections.is_empty(), |this| {
                this.child(Label::new(t!("No release notes for this version.")))
            })
            .children(
                self.sections
                    .iter()
                    .take(shown)
                    .map(|section| self.render_section(section)),
            )
            .when(total > shown, |this| {
                this.child(
                    Button::new(
                        "whats-new-show-all",
                        t!("Show all {count} versions", count = total),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_all = true;
                        cx.notify();
                    })),
                )
            })
            .when_some(release_notes_url(cx), |this, url| {
                this.child(
                    Button::new("whats-new-open-github", t!("Open on GitHub"))
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
            })
    }
}

impl Focusable for WhatsNew {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for WhatsNew {}

impl Item for WhatsNew {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        t!("What's New")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENGLISH_1_4_7: &str = "\
## 1.4.7 - 2026-11-24

- **Stop** no longer ends the whole application.
- Program output in the terminal is decoded as UTF-8 on Windows.

## 1.4.6 - 2026-11-24

- Run, Debug and Stop are now separate buttons.
";

    const CHINESE_1_4_7: &str = "\
## 1.4.7 - 2026-11-24

- **Stop** 不再把整个应用一起带走。
- Windows 上终端里的程序输出按 UTF-8 解码。

## 1.4.6 - 2026-11-24

- Run、Debug、Stop 现在是三个独立按钮。
";

    #[test]
    fn the_embedded_changelog_parses_into_newest_first_sections() {
        let sections = parse_release_notes(CHANGELOG);

        assert!(
            !sections.is_empty(),
            "a changelog with no `## <version>` heading would leave the page empty"
        );
        assert!(
            !sections[0].version.eq_ignore_ascii_case("unreleased"),
            "`Unreleased` describes work no user has, so it must not be shown first"
        );
        assert!(
            !sections[0].entries.is_empty(),
            "the newest section should list what changed in it"
        );
        assert!(
            sections.iter().all(|section| section.date.is_some()),
            "every released section carries a date in its heading"
        );
    }

    #[test]
    fn chinese_reads_the_chinese_entries() {
        let sections = release_notes(Language::SimplifiedChinese, ENGLISH_1_4_7, CHINESE_1_4_7);

        assert_eq!(sections[0].version, "1.4.7");
        assert_eq!(
            sections[0].entries,
            [
                "**Stop** 不再把整个应用一起带走。",
                "Windows 上终端里的程序输出按 UTF-8 解码。"
            ]
        );
        assert!(
            !sections[0].untranslated,
            "a translated version must not carry the fallback note"
        );
    }

    #[test]
    fn english_reads_the_english_entries_even_when_chinese_exists() {
        let sections = release_notes(Language::English, ENGLISH_1_4_7, CHINESE_1_4_7);

        assert_eq!(
            sections[0].entries[0],
            "**Stop** no longer ends the whole application."
        );
        assert!(!sections[0].untranslated);
    }

    #[test]
    fn a_version_the_chinese_file_lacks_falls_back_to_english_with_a_note() {
        let chinese_without_1_4_7 = "\
## 1.4.6 - 2026-11-24

- Run、Debug、Stop 现在是三个独立按钮。
";

        let sections = release_notes(
            Language::SimplifiedChinese,
            ENGLISH_1_4_7,
            chinese_without_1_4_7,
        );

        assert_eq!(sections[0].version, "1.4.7");
        assert_eq!(
            sections[0].entries[0],
            "**Stop** no longer ends the whole application."
        );
        assert!(
            sections[0].untranslated,
            "the page has to say this version has no Chinese notes"
        );
        assert!(
            !sections[1].untranslated,
            "1.4.6 is translated, so it keeps its Chinese entries"
        );
    }

    #[test]
    fn the_chinese_changelog_matches_the_english_one_entry_for_entry() {
        let english = parse_release_notes(CHANGELOG);
        let chinese = parse_release_notes(CHANGELOG_ZH);

        assert!(
            !chinese.is_empty(),
            "CHANGELOG.zh.md has no `## <version>` section"
        );
        for translated in &chinese {
            let original = english
                .iter()
                .find(|section| section.version == translated.version)
                .unwrap_or_else(|| {
                    panic!(
                        "CHANGELOG.zh.md has a section for {}, which CHANGELOG.md does not",
                        translated.version
                    )
                });
            assert_eq!(
                translated.date, original.date,
                "{} is dated differently in the two files",
                translated.version
            );
            assert_eq!(
                translated.entries.len(),
                original.entries.len(),
                "{} has {} entries in Chinese but {} in English; translate them one for one",
                translated.version,
                translated.entries.len(),
                original.entries.len()
            );
        }
    }
}
