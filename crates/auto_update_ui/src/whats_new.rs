//! The release notes of this build, read from the changelog that ships with it.
//!
//! The changelog is embedded at compile time instead of fetched, so the page
//! works offline and its text always matches the binary it belongs to.

use auto_update::release_notes_url;
use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, Render, SharedString, Window,
    actions, prelude::*,
};
use i18n::t;
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

struct ReleaseNotes {
    version: String,
    date: Option<String>,
    entries: Vec<String>,
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

pub struct WhatsNew {
    focus_handle: FocusHandle,
    sections: Vec<ReleaseNotes>,
    show_all: bool,
}

impl WhatsNew {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            // The newest section is the one a version bump just added, and the
            // file is ordered newest first (`script/release` puts it first).
            sections: parse_release_notes(CHANGELOG),
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
}
