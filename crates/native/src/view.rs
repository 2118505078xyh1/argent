//! The native window's pixels.
//!
//! The layout mirrors the webview: sessions on the left, the conversation in
//! the middle, the composer at the bottom, and the state of the run in a slim
//! bar. No logic lives here; every click becomes a [`Message`] the state
//! machine already knows how to answer.

use iced::widget::text_editor::Binding;
use iced::widget::{
    button, column, container, pick_list, row, scrollable, space, stack, text, text_editor,
    text_input, Column,
};
use iced::{Alignment, Element, Length};

use argent_automation::Action;
use argent_i18n::{t, Locale};
use argent_ui_core::{format_tokens, Entry, NoticeKind, Palette, ToolState};

use crate::app::{App, Message, Screen, INPUT_ID, SCROLL_ID};
use crate::style;

const SIDEBAR_WIDTH: f32 = 248.0;

pub fn view(app: &App) -> Element<'_, Message> {
    let p = app.palette;

    let mut chat = Column::new().width(Length::Fill).height(Length::Fill);
    chat = chat.push(top_bar(app));
    chat = chat.push(transcript_area(app));
    if !app.pending.is_empty() {
        chat = chat.push(approval_card(app));
    }
    if app.error.is_some() {
        chat = chat.push(error_card(app));
    }
    chat = chat.push(composer(app));
    chat = chat.push(status_bar(app));

    let main: Element<'_, Message> = match app.screen {
        Screen::Automations => automation_page(app),
        Screen::Documents => documents_page(app),
        Screen::Chat => chat.into(),
    };

    let body = row![sidebar(app), main]
        .width(Length::Fill)
        .height(Length::Fill);

    let mut base: Element<'_, Message> = container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_| container::Style {
            background: Some(iced::Background::Color(style::color(p.background))),
            text_color: Some(style::color(p.text)),
            ..Default::default()
        })
        .into();

    if app.automation_log.is_some() {
        base = stack([base, automation_log_overlay(app)])
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }
    if app.folder_picker.is_some() {
        base = stack([base, folder_overlay(app)])
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }
    base
}

// ------------------------------------------------------------------ sidebar

fn sidebar(app: &App) -> Element<'_, Message> {
    let p = app.palette;
    let locale = app.locale;

    let brand = row![
        text("argent").size(18).color(style::color(p.accent)),
        space().width(Length::Fill),
        text(t(locale, "ui.sessions"))
            .size(10)
            .color(style::color(p.dim)),
    ]
    .align_y(Alignment::Center);

    let nav = row![
        button(
            text(t(locale, "ui.tasks"))
                .size(11)
                .width(Length::Fill)
                .align_x(Alignment::Center),
        )
        .width(Length::FillPortion(1))
        .padding([4, 6])
        .style(style::row_button(p, app.screen == Screen::Chat))
        .on_press(Message::ShowChat),
        button(
            text(t(locale, "ui.automation"))
                .size(11)
                .width(Length::Fill)
                .align_x(Alignment::Center),
        )
        .width(Length::FillPortion(1))
        .padding([4, 6])
        .style(style::row_button(p, app.screen == Screen::Automations))
        .on_press(Message::ShowAutomations),
        button(
            text(t(locale, "ui.docs"))
                .size(11)
                .width(Length::Fill)
                .align_x(Alignment::Center),
        )
        .width(Length::FillPortion(1))
        .padding([4, 6])
        .style(style::row_button(p, app.screen == Screen::Documents))
        .on_press(Message::ShowDocuments),
    ]
    .spacing(4);

    let new_task = button(
        row![
            text("＋").size(13).color(style::color(p.accent)),
            text(t(locale, "ui.new_task"))
                .size(12)
                .color(style::color(p.text)),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .padding([6, 9])
    .style(style::ghost_button(p))
    .on_press(Message::NewSession);

    let mut list = Column::new().spacing(2).width(Length::Fill);

    // The task that has been given a folder but not yet a first prompt. It
    // has no file yet; if a minute passes it is dropped without leaving an
    // empty entry behind.
    if let Some(task) = &app.pending_task {
        let content = column![
            text(t(locale, "ui.new_task_title"))
                .size(12)
                .color(style::color(p.strong)),
            text(meta(&task.folder.display().to_string(), &now_stamp()))
                .size(9)
                .color(style::color(p.dim)),
        ]
        .spacing(2);
        list = list.push(
            button(content)
                .width(Length::Fill)
                .padding([6, 8])
                .style(style::row_button(p, app.active.is_none()))
                .on_press(Message::NewSession),
        );
    }

    if app.sessions.is_empty() && app.pending_task.is_none() {
        list = list.push(
            text(t(locale, "ui.no_sessions"))
                .size(11)
                .color(style::color(p.dim)),
        );
    } else {
        for (index, row) in app.sessions.iter().enumerate() {
            let selected = app.active.as_ref() == Some(&row.path);
            let content = column![
                text(clip(&row.title, 26))
                    .size(12)
                    .color(style::color(if selected { p.strong } else { p.text })),
                text(meta(&row.cwd, &row.updated_local))
                    .size(9)
                    .color(style::color(p.dim)),
            ]
            .spacing(2);
            list = list.push(
                button(content)
                    .width(Length::Fill)
                    .padding([6, 8])
                    .style(style::row_button(p, selected))
                    .on_press(Message::Open(index)),
            );
        }
    }

    let body = scrollable(list).height(Length::Fill).width(Length::Fill);

    container(column![brand, nav, new_task, body].spacing(10).padding(12))
        .width(SIDEBAR_WIDTH)
        .height(Length::Fill)
        .style(style::sidebar(p))
        .into()
}

// -------------------------------------------------------------- automations

fn automation_page(app: &App) -> Element<'_, Message> {
    let p = app.palette;
    let locale = app.locale;
    let now = argent_automation::now();

    let header = container(
        row![
            column![
                text(t(locale, "ui.automation")).size(13),
                text(short_path(&app.cwd))
                    .size(10)
                    .color(style::color(p.dim)),
            ]
            .spacing(1),
            space().width(Length::Fill),
            match &app.automation_status {
                Some(status) => text(status.clone()).size(10).color(style::color(p.dim)),
                None => text(""),
            },
        ]
        .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fixed(50.0))
    .padding([0, 16])
    .style(move |_| container::Style {
        background: Some(iced::Background::Color(style::color(p.surface))),
        text_color: Some(style::color(p.text)),
        border: iced::Border {
            color: style::color(p.border_subtle),
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    });

    let mut form = Column::new().spacing(7);
    form = form.push(text(t(locale, "ui.new_automation")).size(12));
    form = form.push(
        text_input(t(locale, "ui.automation_name"), &app.automation_name)
            .size(11)
            .padding(7)
            .width(Length::Fill)
            .style(style::path_input(p))
            .on_input(Message::AutomationName),
    );
    form = form.push(
        row![
            text_input(
                t(locale, "ui.automation_schedule"),
                &app.automation_schedule
            )
            .size(11)
            .padding(7)
            .width(Length::Fixed(210.0))
            .style(style::path_input(p))
            .on_input(Message::AutomationSchedule),
            text_input(t(locale, "ui.automation_prompt"), &app.automation_prompt)
                .size(11)
                .padding(7)
                .width(Length::Fill)
                .style(style::path_input(p))
                .on_input(Message::AutomationPrompt),
        ]
        .spacing(8),
    );
    form = form.push(
        text(t(locale, "ui.automation_schedule_hint"))
            .size(9)
            .color(style::color(p.dim)),
    );
    if let Some(error) = &app.automation_error {
        form = form.push(text(error.clone()).size(10).color(style::color(p.error)));
    }
    form = form.push(
        row![
            space().width(Length::Fill),
            button(text(t(locale, "ui.automation_add")).size(11))
                .padding([4, 12])
                .style(style::accent_button(p))
                .on_press(Message::AutomationAdd),
        ]
        .spacing(6),
    );
    let form = container(form.padding(12)).style(style::raised(p));

    let mut list = Column::new().spacing(8);
    if app.automations.is_empty() {
        list = list.push(
            text(t(locale, "ui.automation_empty"))
                .size(11)
                .color(style::color(p.dim)),
        );
    }
    for (index, task) in app.automations.iter().enumerate() {
        let next = argent_automation::format_iso(task.next_run(now).max(0));
        let last = task.last_status.clone().unwrap_or_else(|| "-".to_string());
        let action = match &task.action {
            Action::Prompt(text) => clip(text, 90),
            Action::Workflow(path) => path.display().to_string(),
        };
        let toggle = if task.enabled {
            t(locale, "ui.disable")
        } else {
            t(locale, "ui.enable")
        };
        let controls = row![
            button(text(t(locale, "ui.run")).size(10))
                .padding([3, 9])
                .style(style::accent_button(p))
                .on_press(Message::AutomationRun(index)),
            button(text(toggle).size(10))
                .padding([3, 9])
                .style(style::ghost_button(p))
                .on_press(Message::AutomationToggle(index)),
            button(text(t(locale, "ui.logs")).size(10))
                .padding([3, 9])
                .style(style::ghost_button(p))
                .on_press(Message::AutomationLog(index)),
            button(text(t(locale, "ui.delete")).size(10))
                .padding([3, 9])
                .style(style::danger_button(p))
                .on_press(Message::AutomationDelete(index)),
        ]
        .spacing(5);

        let card = column![
            row![
                text(if task.enabled { "●" } else { "○" })
                    .size(9)
                    .color(style::color(if task.enabled { p.ok } else { p.dim })),
                text(task.name.clone()).size(12),
                space().width(Length::Fill),
                text(format!("{} {}", t(locale, "ui.next_run"), next))
                    .size(9)
                    .color(style::color(p.dim)),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
            text(format!("{} · {}", task.schedule.display(), action))
                .size(10)
                .color(style::color(p.dim)),
            row![
                text(format!("{}: {}", t(locale, "ui.last_status"), last))
                    .size(9)
                    .color(style::color(p.dim)),
                space().width(Length::Fill),
                controls,
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        ]
        .spacing(5);
        list = list.push(container(card.padding(10)).style(style::tool_card(p)));
    }

    let body = scrollable(
        Column::new()
            .spacing(12)
            .padding([14, 18])
            .push(form)
            .push(list),
    )
    .width(Length::Fill)
    .height(Length::Fill);

    column![header, body]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn automation_log_overlay(app: &App) -> Element<'_, Message> {
    let Some(log) = &app.automation_log else {
        return space().into();
    };
    let p = app.palette;

    let card = container(
        column![
            row![
                text(log.title.clone()).size(13),
                space().width(Length::Fill),
                button(text("×").size(14))
                    .padding([1, 8])
                    .style(style::ghost_button(p))
                    .on_press(Message::CloseAutomationLog),
            ]
            .align_y(Alignment::Center),
            scrollable(
                text(log.body.clone())
                    .size(10)
                    .font(style::mono_font())
                    .width(Length::Fill)
            )
            .height(Length::Fixed(430.0))
            .width(Length::Fill),
        ]
        .spacing(8)
        .padding(14),
    )
    .width(Length::Fixed(760.0))
    .height(Length::Fixed(500.0))
    .style(style::raised(p));

    container(card)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(move |_| container::Style {
            background: Some(iced::Background::Color(iced::Color::from_rgba(
                0.0, 0.0, 0.0, 0.55,
            ))),
            ..Default::default()
        })
        .into()
}

// ----------------------------------------------------------------- documents

fn documents_page(app: &App) -> Element<'_, Message> {
    let p = app.palette;
    let locale = app.locale;

    let header = container(
        column![
            text(t(locale, "ui.docs")).size(13),
            text(short_path(&app.cwd))
                .size(10)
                .color(style::color(p.dim)),
        ]
        .spacing(1),
    )
    .width(Length::Fill)
    .height(Length::Fixed(50.0))
    .padding([0, 16])
    .style(move |_| container::Style {
        background: Some(iced::Background::Color(style::color(p.surface))),
        text_color: Some(style::color(p.text)),
        border: iced::Border {
            color: style::color(p.border_subtle),
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    });

    let mut files = Column::new().spacing(2).width(Length::Fill);
    if app.office_files.is_empty() {
        files = files.push(
            text(t(locale, "ui.no_documents"))
                .size(11)
                .color(style::color(p.dim)),
        );
    }
    for (index, document) in app.office_files.iter().enumerate() {
        let selected = app
            .document
            .as_ref()
            .is_some_and(|open| open.title == document.name);
        files = files.push(
            button(text(clip(&document.name, 30)).size(11))
                .width(Length::Fill)
                .padding([5, 8])
                .style(style::row_button(p, selected))
                .on_press(Message::OpenDocument(index)),
        );
    }
    let sidebar = container(scrollable(files).height(Length::Fill))
        .width(Length::Fixed(270.0))
        .padding(10)
        .style(style::plain(p));

    let body = match &app.document {
        Some(document) => container(
            scrollable(
                text(document.body.clone())
                    .size(11)
                    .font(style::mono_font())
                    .width(Length::Fill),
            )
            .height(Length::Fill)
            .width(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(12)
        .style(style::tool_card(p)),
        None => container(
            text(t(locale, "ui.select_document"))
                .size(11)
                .color(style::color(p.dim)),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center),
    };

    let content = row![sidebar, container(body).padding([12, 14])]
        .width(Length::Fill)
        .height(Length::Fill);

    column![header, content]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

// ------------------------------------------------------------ folder picker

fn folder_overlay(app: &App) -> Element<'_, Message> {
    let Some(picker) = &app.folder_picker else {
        return space().into();
    };
    let p = app.palette;
    let locale = app.locale;

    let current = app.cwd.display().to_string();
    let mut recent: Vec<String> = Vec::new();
    for row in &app.sessions {
        if !row.cwd.is_empty() && !recent.iter().any(|known| known == &row.cwd) {
            recent.push(row.cwd.clone());
        }
    }
    if !recent.iter().any(|known| known == &current) {
        recent.insert(0, current.clone());
    }
    recent.truncate(6);

    let mut folders = Column::new().spacing(2).width(Length::Fill);
    for folder in &recent {
        let name = std::path::Path::new(folder)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| folder.clone());
        let content = column![
            text(name).size(12),
            text(clip(folder, 48)).size(9).color(style::color(p.dim)),
        ]
        .spacing(1);
        folders = folders.push(
            button(content)
                .width(Length::Fill)
                .padding([5, 8])
                .style(style::row_button(p, folder == &current))
                .on_press(Message::ChooseFolder(folder.clone())),
        );
    }

    let mut form = Column::new().spacing(9);
    form = form.push(text(t(locale, "ui.choose_folder")).size(15));
    form = form.push(
        text(t(locale, "ui.folder_hint"))
            .size(11)
            .color(style::color(p.dim)),
    );
    form = form.push(
        text(t(locale, "ui.recent_folders"))
            .size(9)
            .color(style::color(p.dim)),
    );
    form = form.push(
        scrollable(folders)
            .height(Length::Fixed(146.0))
            .width(Length::Fill),
    );
    form = form.push(
        text(t(locale, "ui.folder_path"))
            .size(9)
            .color(style::color(p.dim)),
    );
    form = form.push(
        text_input(t(locale, "ui.folder_path"), &picker.path)
            .size(12)
            .padding(8)
            .width(Length::Fill)
            .style(style::path_input(p))
            .on_input(Message::FolderInput)
            .on_submit(Message::ChooseFolder(picker.path.clone())),
    );
    let default_profile = t(locale, "ui.default_profile").to_string();
    let mut profile_options = vec![default_profile.clone()];
    profile_options.extend(app.profiles.iter().map(|profile| profile.name.clone()));
    let selected_profile = picker.profile.clone().unwrap_or(default_profile);
    form = form.push(
        text(t(locale, "ui.profile"))
            .size(9)
            .color(style::color(p.dim)),
    );
    form = form.push(
        pick_list(profile_options, Some(selected_profile), Message::Profile)
            .width(Length::Fill)
            .text_size(12)
            .padding([5, 9])
            .style(style::picker(p))
            .menu_style(style::menu(p)),
    );
    if let Some(profile) = &picker.profile {
        if let Some(profile) = app
            .profiles
            .iter()
            .find(|candidate| &candidate.name == profile)
        {
            if !profile.description.is_empty() {
                form = form.push(
                    text(profile.description.clone())
                        .size(10)
                        .color(style::color(p.dim)),
                );
            }
        }
    }
    if let Some(error) = &picker.error {
        form = form.push(text(error.clone()).size(11).color(style::color(p.error)));
    }
    form = form.push(
        row![
            space().width(Length::Fill),
            button(text(t(locale, "ui.cancel")).size(11))
                .padding([5, 13])
                .style(style::ghost_button(p))
                .on_press(Message::CancelFolder),
            button(text(t(locale, "ui.create_task")).size(11))
                .padding([5, 13])
                .style(style::accent_button(p))
                .on_press(Message::ChooseFolder(picker.path.clone())),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    );

    let card = container(form.padding(16))
        .width(Length::Fixed(540.0))
        .style(style::raised(p));

    container(card)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(move |_| container::Style {
            background: Some(iced::Background::Color(iced::Color::from_rgba(
                0.0, 0.0, 0.0, 0.55,
            ))),
            ..Default::default()
        })
        .into()
}

/// `folder · 11-14 20:06`, clipped to fit the sidebar.
fn meta(folder: &str, when: &str) -> String {
    if folder.is_empty() {
        when.to_string()
    } else {
        clip(&format!("{} · {}", folder, when), 38)
    }
}

fn now_stamp() -> String {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    argent_session::local_stamp(epoch)
}

// ------------------------------------------------------------------- top bar

fn top_bar(app: &App) -> Element<'_, Message> {
    let p = app.palette;
    let locale = app.locale;

    let heading = column![
        text(app.title()).size(13),
        text(short_path(&app.cwd))
            .size(10)
            .color(style::color(p.dim)),
    ]
    .spacing(1);

    let selected_model = if app.models.iter().any(|id| id == &app.model) {
        Some(app.model.clone())
    } else {
        None
    };
    let model = pick_list(app.models.clone(), selected_model, Message::Model)
        .width(176)
        .text_size(11)
        .padding([5, 9])
        .style(style::picker(p))
        .menu_style(style::menu(p));
    let selected_level = if app.levels.iter().any(|level| level == &app.thinking) {
        Some(app.thinking.clone())
    } else {
        None
    };
    let level = pick_list(app.levels.clone(), selected_level, Message::Thinking)
        .width(96)
        .text_size(11)
        .padding([5, 9])
        .style(style::picker(p))
        .menu_style(style::menu(p));

    let bar = row![
        heading,
        space().width(Length::Fill),
        text(t(locale, "tui.models.title"))
            .size(9)
            .color(style::color(p.dim)),
        model,
        text(t(locale, "tui.thinking.title"))
            .size(9)
            .color(style::color(p.dim)),
        level,
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    container(bar)
        .width(Length::Fill)
        .height(Length::Fixed(50.0))
        .padding([0, 16])
        .style(move |_| container::Style {
            background: Some(iced::Background::Color(style::color(p.surface))),
            text_color: Some(style::color(p.text)),
            border: iced::Border {
                color: style::color(p.border_subtle),
                width: 1.0,
                radius: 0.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn short_path(path: &std::path::Path) -> String {
    path.display().to_string()
}

// ---------------------------------------------------------------- transcript

fn transcript_area(app: &App) -> Element<'_, Message> {
    let p = app.palette;
    if app.transcript.is_empty() {
        return empty_state(app);
    }

    let mut list = Column::new().spacing(12).width(Length::Fill);
    for entry in app.transcript.entries() {
        list = list.push(entry_view(entry, p, app.locale));
    }

    container(
        scrollable(list.padding([14, 20]))
            .id(SCROLL_ID)
            .width(Length::Fill)
            .height(Length::Fill)
            .on_scroll(|viewport| Message::Scrolled(at_end(&viewport))),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(style::plain(p))
    .into()
}

fn at_end(viewport: &iced::widget::scrollable::Viewport) -> bool {
    let bounds = viewport.bounds();
    let content = viewport.content_bounds();
    let offset = viewport.absolute_offset();
    (content.height - bounds.height - offset.y) <= 12.0
}

fn empty_state(app: &App) -> Element<'_, Message> {
    let p = app.palette;
    let locale = app.locale;
    let body = column![
        text("argent").size(30).color(style::color(p.accent)),
        text(t(locale, "tui.empty"))
            .size(13)
            .color(style::color(p.text)),
        text(t(locale, "ui.placeholder"))
            .size(11)
            .color(style::color(p.dim)),
    ]
    .spacing(6)
    .align_x(Alignment::Center);

    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(style::plain(p))
        .into()
}

fn entry_view<'a>(entry: &'a Entry, p: Palette, locale: Locale) -> Element<'a, Message> {
    match entry {
        Entry::User(message) => {
            let bubble = container(
                column![
                    text(t(locale, "ui.you")).size(9).color(style::color(p.dim)),
                    text(message.clone()).size(13),
                ]
                .spacing(2),
            )
            .padding([7, 11])
            .style(style::user_bubble(p));

            row![
                space().width(Length::FillPortion(1)),
                container(bubble).width(Length::FillPortion(4)),
            ]
            .width(Length::Fill)
            .into()
        }
        Entry::Assistant(message) => container(
            column![
                text("argent").size(9).color(style::color(p.accent)),
                text(message.clone()).size(13),
            ]
            .spacing(2),
        )
        .width(Length::Fill)
        .style(style::plain(p))
        .into(),
        Entry::Thinking(message) => container(
            column![
                text(t(locale, "ui.thinking"))
                    .size(9)
                    .color(style::color(p.dim)),
                text(message.clone()).size(12).color(style::color(p.dim)),
            ]
            .spacing(2),
        )
        .padding([1, 2])
        .style(style::plain(p))
        .into(),
        Entry::Tool(run) => {
            let (glyph, tint) = match run.state {
                ToolState::Running => ("·", p.warn),
                ToolState::Ok { .. } => ("✓", p.ok),
                ToolState::Failed { .. } => ("✗", p.error),
            };
            let mut body = Column::new().spacing(4);
            body = body.push(
                row![
                    text(glyph).size(12).color(style::color(tint)),
                    text(run.name.clone()).size(11).font(style::mono_font()),
                    text(run.summary())
                        .size(10)
                        .color(style::color(p.dim))
                        .width(Length::Fill)
                        .align_x(Alignment::End),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            );
            if !run.arguments.trim().is_empty() {
                body = body.push(
                    text(clip(run.arguments.trim(), 240))
                        .size(10)
                        .font(style::mono_font())
                        .color(style::color(p.dim)),
                );
            }
            if !run.output.trim().is_empty() {
                body = body.push(
                    text(clip(&clean(&run.output), 6000))
                        .size(11)
                        .font(style::mono_font()),
                );
            }
            container(body)
                .width(Length::Fill)
                .padding(9)
                .style(style::tool_card(p))
                .into()
        }
        Entry::Notice(kind, message) => {
            let (glyph, tint) = match kind {
                NoticeKind::Info => ("ⓘ", p.dim),
                NoticeKind::Warning => ("!", p.warn),
                NoticeKind::Error => ("✗", p.error),
            };
            row![
                text(glyph).size(11).color(style::color(tint)),
                text(message.clone()).size(11).color(style::color(tint)),
            ]
            .spacing(5)
            .align_y(Alignment::Center)
            .into()
        }
    }
}

fn clean(output: &str) -> String {
    output.replace("\r\n", "\n").replace('\r', "\n")
}

// ------------------------------------------------------------------ approval

fn approval_card(app: &App) -> Element<'_, Message> {
    let Some(pending) = app.pending.first() else {
        return space().into();
    };
    let p = app.palette;
    let locale = app.locale;
    let more = if app.pending.len() > 1 {
        format!(" (+{})", app.pending.len() - 1)
    } else {
        String::new()
    };

    let actions = row![
        button(text(t(locale, "ui.allow")).size(11))
            .padding([4, 12])
            .style(style::accent_button(p))
            .on_press(Message::Allow(pending.id)),
        button(text(t(locale, "ui.reject")).size(11))
            .padding([4, 12])
            .style(style::ghost_button(p))
            .on_press(Message::Deny(pending.id)),
    ]
    .spacing(6);

    let body = column![
        text(format!(
            "{}{} · {}",
            t(locale, "ui.allow_tool"),
            more,
            pending.name
        ))
        .size(11)
        .color(style::color(p.warn)),
        text(pending.detail.clone()).size(11),
        actions,
    ]
    .spacing(6);

    container(body)
        .width(Length::Fill)
        .padding([8, 10])
        .style(style::warning_card(p))
        .into()
}

fn error_card(app: &App) -> Element<'_, Message> {
    let Some(error) = &app.error else {
        return space().into();
    };
    let p = app.palette;
    let body = row![
        text(error.clone())
            .size(11)
            .width(Length::Fill)
            .color(style::color(p.error)),
        button(text("×").size(13))
            .padding([1, 7])
            .style(style::ghost_button(p))
            .on_press(Message::DismissError),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    container(body)
        .width(Length::Fill)
        .padding([6, 10])
        .style(style::error_card(p))
        .into()
}

// ------------------------------------------------------------------ composer

fn composer(app: &App) -> Element<'_, Message> {
    let p = app.palette;
    let locale = app.locale;
    let can_send = !app.input.text().trim().is_empty();

    let editor = text_editor(&app.input)
        .id(INPUT_ID)
        .placeholder(t(locale, "ui.placeholder"))
        .font(style::ui_font())
        .size(13)
        .on_action(Message::Edit)
        .key_binding(|press| {
            let enter = matches!(
                press.key,
                iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter)
            );
            if enter && !press.modifiers.shift() && !press.modifiers.control() {
                return Some(Binding::Custom(Message::Send));
            }
            Binding::from_key_press(press)
        })
        .padding(9)
        .height(Length::Fixed(68.0))
        .style(style::editor(p));

    let action: Element<'_, Message> = if app.working {
        button(text(t(locale, "ui.stop")).size(11))
            .padding([5, 13])
            .style(style::danger_button(p))
            .on_press(Message::Abort)
            .into()
    } else {
        button(text(t(locale, "ui.send")).size(11))
            .padding([5, 13])
            .style(style::accent_button(p))
            .on_press_maybe(can_send.then_some(Message::Send))
            .into()
    };

    let hint = match locale {
        Locale::En => "Enter send · Shift+Enter newline",
        Locale::Zh => "Enter 发送 · Shift+Enter 换行",
    };

    let rewind = button(text(t(locale, "ui.rewind")).size(10))
        .padding([5, 10])
        .style(style::ghost_button(p))
        .on_press_maybe((!app.working).then_some(Message::Rewind));

    let footer = row![
        text(hint).size(9).color(style::color(p.dim)),
        space().width(Length::Fill),
        rewind,
        action,
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    container(
        column![editor, footer]
            .spacing(6)
            .padding([8, 16])
            .width(Length::Fill),
    )
    .width(Length::Fill)
    .style(style::composer_bar(p))
    .into()
}

// ---------------------------------------------------------------- status bar

fn status_bar(app: &App) -> Element<'_, Message> {
    let p = app.palette;
    let locale = app.locale;
    let dot = if app.working {
        text("●").size(9).color(style::color(p.warn))
    } else {
        text("●").size(9).color(style::color(p.ok))
    };
    let state = if app.working {
        t(locale, "tui.working")
    } else {
        t(locale, "ui.ready")
    };

    let mut bar = row![
        dot,
        text(state).size(10).color(style::color(p.dim)),
        text("·").size(10).color(style::color(p.dim)),
        text(app.model.clone()).size(10).color(style::color(p.dim)),
        text("·").size(10).color(style::color(p.dim)),
        text(app.thinking.clone())
            .size(10)
            .color(style::color(p.dim)),
    ]
    .spacing(5)
    .align_y(Alignment::Center);

    if let Some(profile) = &app.profile {
        bar = bar.push(text("·").size(10).color(style::color(p.dim)));
        bar = bar.push(text(profile.clone()).size(10).color(style::color(p.accent)));
    }

    bar = bar.push(space().width(Length::Fill));

    if app.prompt_tokens > 0 {
        bar = bar.push(
            text(format!(
                "ctx {} / {}",
                format_tokens(app.prompt_tokens),
                format_tokens(app.context_max as u64)
            ))
            .size(10)
            .color(style::color(p.dim)),
        );
    }
    if app.requests > 0 {
        bar = bar.push(
            text(format!(
                "↑ {} ↓ {}",
                format_tokens(app.prompt_tokens),
                format_tokens(app.completion_tokens)
            ))
            .size(10)
            .color(style::color(p.dim)),
        );
    }
    if app.cached_tokens > 0 && app.prompt_tokens > 0 {
        let hit = (app.cached_tokens.min(app.prompt_tokens) * 100) / app.prompt_tokens;
        bar = bar.push(
            text(format!("cache {}%", hit))
                .size(10)
                .color(style::color(p.dim)),
        );
    }
    if let Some(cost) = app.cost {
        bar = bar.push(
            text(format!("${:.4}", cost))
                .size(10)
                .color(style::color(p.accent)),
        );
    }

    container(bar)
        .width(Length::Fill)
        .padding([4, 16])
        .style(move |_| container::Style {
            background: Some(iced::Background::Color(style::color(p.surface))),
            text_color: Some(style::color(p.dim)),
            ..Default::default()
        })
        .into()
}

// -------------------------------------------------------------------- helpers

/// Cap a piece of text without cutting a character in half.
fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

impl App {
    pub fn view(&self) -> Element<'_, Message> {
        crate::view::view(self)
    }

    pub fn theme(&self) -> iced::Theme {
        style::theme_for(&self.palette)
    }
}
