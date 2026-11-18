//! Colours and widget styles for the native window.
//!
//! Everything is derived from [`argent_ui_core::Palette`], the same palette the
//! terminal and the webview use. The native frontend therefore follows the
//! `theme` setting instead of hard-coding a look of its own.

use argent_ui_core::{Palette, Rgb};
use iced::widget::overlay::menu;
use iced::widget::{button, container, pick_list, text_editor, text_input};
use iced::{Background, Border, Color, Font, Theme};

/// The UI face. Windows' Microsoft YaHei covers Latin and CJK; relying on
/// fallback for one of the two scripts would show tofu.
pub fn ui_font() -> Font {
    Font::with_name("Microsoft YaHei")
}

pub fn mono_font() -> Font {
    Font::with_name("Cascadia Mono")
}

/// Convert the frontend-neutral RGB into iced's colour.
pub fn color(rgb: Rgb) -> Color {
    Color::from_rgb8(rgb.r, rgb.g, rgb.b)
}

fn alpha(rgb: Rgb, a: f32) -> Color {
    Color::from_rgba8(rgb.r, rgb.g, rgb.b, a)
}

/// iced's stock theme only has to be close enough for the widgets whose look
/// is not overridden (menus, the scrollbar): the custom styles below carry the
/// actual palette.
pub fn theme_for(palette: &Palette) -> Theme {
    match palette.name {
        "light" => Theme::Light,
        _ => Theme::Dark,
    }
}

fn block(color: Color, radius: f32) -> Border {
    Border {
        color,
        width: 0.0,
        radius: radius.into(),
    }
}

/// The sidebar: one step darker than the canvas.
pub fn sidebar(p: Palette) -> impl Fn(&Theme) -> container::Style + 'static {
    move |_| container::Style {
        text_color: Some(color(p.text)),
        background: Some(Background::Color(color(p.surface))),
        ..Default::default()
    }
}

/// A plain container that still carries the right text colour.
pub fn plain(p: Palette) -> impl Fn(&Theme) -> container::Style + 'static {
    move |_| container::Style {
        text_color: Some(color(p.text)),
        ..Default::default()
    }
}

/// The operator's message: tinted, rounded, aligned right.
pub fn user_bubble(p: Palette) -> impl Fn(&Theme) -> container::Style + 'static {
    move |_| container::Style {
        text_color: Some(color(p.text)),
        background: Some(Background::Color(alpha(p.accent, 0.12))),
        border: Border {
            color: alpha(p.accent, 0.32),
            width: 1.0,
            radius: 12.0.into(),
        },
        ..Default::default()
    }
}

/// The first-run card: a quiet surface, not a blank canvas.
pub fn welcome_card(p: Palette) -> impl Fn(&Theme) -> container::Style + 'static {
    move |_| container::Style {
        text_color: Some(color(p.text)),
        background: Some(Background::Color(color(p.surface))),
        border: Border {
            color: color(p.border_subtle),
            width: 1.0,
            radius: 18.0.into(),
        },
        ..Default::default()
    }
}

/// A tool call.
pub fn tool_card(p: Palette) -> impl Fn(&Theme) -> container::Style + 'static {
    move |_| container::Style {
        text_color: Some(color(p.text)),
        background: Some(Background::Color(color(p.element))),
        border: Border {
            color: color(p.border_subtle),
            width: 1.0,
            radius: 10.0.into(),
        },
        ..Default::default()
    }
}

/// An approval request: visibly a decision, not a message.
pub fn warning_card(p: Palette) -> impl Fn(&Theme) -> container::Style + 'static {
    move |_| container::Style {
        text_color: Some(color(p.text)),
        background: Some(Background::Color(alpha(p.warn, 0.10))),
        border: Border {
            color: alpha(p.warn, 0.55),
            width: 1.0,
            radius: 10.0.into(),
        },
        ..Default::default()
    }
}

pub fn error_card(p: Palette) -> impl Fn(&Theme) -> container::Style + 'static {
    move |_| container::Style {
        text_color: Some(color(p.text)),
        background: Some(Background::Color(alpha(p.error, 0.10))),
        border: Border {
            color: alpha(p.error, 0.55),
            width: 1.0,
            radius: 10.0.into(),
        },
        ..Default::default()
    }
}

/// One session row in the sidebar.
pub fn row_button(
    p: Palette,
    selected: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style + 'static {
    move |_, status| {
        let background = if selected {
            Some(Background::Color(color(p.element)))
        } else {
            match status {
                button::Status::Hovered => Some(Background::Color(alpha(p.element, 0.65))),
                button::Status::Pressed => Some(Background::Color(color(p.element))),
                _ => None,
            }
        };
        button::Style {
            background,
            text_color: if selected {
                color(p.strong)
            } else {
                color(p.text)
            },
            border: block(alpha(p.accent, if selected { 0.35 } else { 0.0 }), 8.0),
            ..Default::default()
        }
    }
}

pub fn accent_button(p: Palette) -> impl Fn(&Theme, button::Status) -> button::Style + 'static {
    move |_, status| {
        let (background, ink) = match status {
            button::Status::Hovered => (color(p.accent.mix(p.strong, 0.18)), color(p.background)),
            button::Status::Pressed => {
                (color(p.accent.mix(p.background, 0.18)), color(p.background))
            }
            button::Status::Disabled => (color(p.element), color(p.dim)),
            button::Status::Active => (color(p.accent), color(p.background)),
        };
        button::Style {
            background: Some(Background::Color(background)),
            text_color: ink,
            border: block(background, 9.0),
            ..Default::default()
        }
    }
}

pub fn ghost_button(p: Palette) -> impl Fn(&Theme, button::Status) -> button::Style + 'static {
    move |_, status| {
        let (background, ink) = match status {
            button::Status::Hovered => (Some(Background::Color(color(p.element))), color(p.strong)),
            button::Status::Pressed => (
                Some(Background::Color(color(p.element.mix(p.border, 0.5)))),
                color(p.strong),
            ),
            button::Status::Disabled => (None, color(p.dim)),
            button::Status::Active => (None, color(p.dim)),
        };
        button::Style {
            background,
            text_color: ink,
            border: block(color(p.border_subtle), 9.0),
            ..Default::default()
        }
    }
}

pub fn danger_button(p: Palette) -> impl Fn(&Theme, button::Status) -> button::Style + 'static {
    move |_, status| {
        let background = match status {
            button::Status::Hovered => color(p.error.mix(p.strong, 0.12)),
            button::Status::Pressed => color(p.error.mix(p.background, 0.2)),
            button::Status::Disabled => color(p.element),
            button::Status::Active => color(p.error),
        };
        button::Style {
            background: Some(Background::Color(background)),
            text_color: if matches!(status, button::Status::Disabled) {
                color(p.dim)
            } else {
                Color::WHITE
            },
            border: block(background, 6.0),
            ..Default::default()
        }
    }
}

/// The model and effort pickers, styled as chips rather than as form fields.
pub fn picker(p: Palette) -> impl Fn(&Theme, pick_list::Status) -> pick_list::Style + 'static {
    move |_, status| {
        let opened = matches!(status, pick_list::Status::Opened { .. });
        pick_list::Style {
            text_color: color(p.text),
            placeholder_color: color(p.dim),
            handle_color: color(p.dim),
            background: Background::Color(color(if opened { p.element } else { p.surface })),
            border: Border {
                color: if opened {
                    alpha(p.accent, 0.55)
                } else {
                    color(p.border)
                },
                width: 1.0,
                radius: 6.0.into(),
            },
        }
    }
}

/// The picker's popup: a floating panel, not a system menu.
pub fn menu(p: Palette) -> impl Fn(&Theme) -> menu::Style + 'static {
    move |_| menu::Style {
        background: Background::Color(color(p.surface)),
        border: Border {
            color: color(p.border),
            width: 1.0,
            radius: 7.0.into(),
        },
        text_color: color(p.text),
        selected_text_color: color(p.accent),
        selected_background: Background::Color(alpha(p.accent, 0.12)),
        shadow: iced::Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.45),
            offset: iced::Vector { x: 0.0, y: 6.0 },
            blur_radius: 18.0,
        },
    }
}

/// The strip the composer lives on.
pub fn composer_bar(p: Palette) -> impl Fn(&Theme) -> container::Style + 'static {
    move |_| container::Style {
        text_color: Some(color(p.text)),
        background: Some(Background::Color(color(p.surface))),
        border: Border {
            color: color(p.border_subtle),
            width: 1.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

/// A floating card: the folder picker, and any future dialog.
pub fn raised(p: Palette) -> impl Fn(&Theme) -> container::Style + 'static {
    move |_| container::Style {
        text_color: Some(color(p.text)),
        background: Some(Background::Color(color(p.surface))),
        border: Border {
            color: color(p.border),
            width: 1.0,
            radius: 12.0.into(),
        },
        ..Default::default()
    }
}

/// A single-line path field in the folder picker.
pub fn path_input(
    p: Palette,
) -> impl Fn(&Theme, text_input::Status) -> text_input::Style + 'static {
    move |_, status| text_input::Style {
        background: Background::Color(color(p.background)),
        border: Border {
            color: match status {
                text_input::Status::Focused { .. } => color(p.accent),
                _ => color(p.border),
            },
            width: 1.0,
            radius: 7.0.into(),
        },
        placeholder: color(p.dim),
        value: color(p.text),
        selection: alpha(p.accent, 0.30),
        icon: color(p.dim),
    }
}

/// The composer.
pub fn editor(p: Palette) -> impl Fn(&Theme, text_editor::Status) -> text_editor::Style + 'static {
    move |_, status| text_editor::Style {
        background: Background::Color(color(if matches!(status, text_editor::Status::Hovered) {
            p.element.mix(p.border_subtle, 0.6)
        } else {
            p.element
        })),
        border: Border {
            color: match status {
                text_editor::Status::Focused { .. } => color(p.accent),
                _ => color(p.border),
            },
            width: 1.0,
            radius: 12.0.into(),
        },
        placeholder: color(p.dim),
        value: color(p.text),
        selection: alpha(p.accent, 0.30),
    }
}
