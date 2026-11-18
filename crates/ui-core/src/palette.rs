//! Semantic colours, with RGB values.
//!
//! A renderer asks for a *role*, never a literal colour: the terminal frontend
//! maps [`Role`] onto 256-colour palette indices, the GUI maps it onto
//! `Color32`. One palette definition therefore serves every frontend.
//!
//! [`Rgb`] is what the GUI frontends use directly; the terminal one ignores it
//! and keeps its own ANSI mapping.

/// An 8-bit-per-channel colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }

    /// `#rrggbb`, for CSS and for logging.
    pub fn hex(&self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// Linear blend towards `other`; `t` is clamped to `0.0..=1.0`.
    pub fn mix(&self, other: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let blend = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgb::new(
            blend(self.r, other.r),
            blend(self.g, other.g),
            blend(self.b, other.b),
        )
    }
}

/// What a colour is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Background,
    Surface,
    Element,
    Border,
    BorderSubtle,
    Text,
    Dim,
    Accent,
    Ok,
    Error,
    Warn,
    Strong,
}

/// One named palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub name: &'static str,
    pub description: &'static str,
    pub background: Rgb,
    pub surface: Rgb,
    /// One step lighter than `surface`: cards, code blocks, the prompt box.
    pub element: Rgb,
    pub border: Rgb,
    /// A border that separates without drawing attention.
    pub border_subtle: Rgb,
    pub text: Rgb,
    pub dim: Rgb,
    pub accent: Rgb,
    pub ok: Rgb,
    pub error: Rgb,
    pub warn: Rgb,
    pub strong: Rgb,
}

impl Palette {
    pub fn get(&self, role: Role) -> Rgb {
        match role {
            Role::Background => self.background,
            Role::Surface => self.surface,
            Role::Element => self.element,
            Role::Border => self.border,
            Role::BorderSubtle => self.border_subtle,
            Role::Text => self.text,
            Role::Dim => self.dim,
            Role::Accent => self.accent,
            Role::Ok => self.ok,
            Role::Error => self.error,
            Role::Warn => self.warn,
            Role::Strong => self.strong,
        }
    }

    pub fn all() -> [Palette; 5] {
        [
            Palette::dark(),
            Palette::titanium(),
            Palette::light(),
            Palette::contrast(),
            Palette::mono(),
        ]
    }

    pub fn by_name(name: &str) -> Option<Palette> {
        let wanted = name.trim().to_ascii_lowercase();
        Palette::all()
            .into_iter()
            .find(|palette| palette.name == wanted)
    }

    pub fn from_config(value: Option<&str>) -> Palette {
        value
            .and_then(Palette::by_name)
            .unwrap_or_else(Palette::dark)
    }

    /// The default: soft blue-grey tones on a dark canvas.
    pub fn dark() -> Palette {
        Palette {
            name: "dark",
            description: "soft blue-grey canvas, periwinkle accent",
            background: Rgb::new(0x0b, 0x0e, 0x14),
            surface: Rgb::new(0x11, 0x15, 0x1c),
            element: Rgb::new(0x18, 0x20, 0x2b),
            border: Rgb::new(0x2a, 0x34, 0x45),
            border_subtle: Rgb::new(0x1d, 0x26, 0x34),
            text: Rgb::new(0xd7, 0xde, 0xe9),
            dim: Rgb::new(0x7f, 0x8b, 0x9f),
            accent: Rgb::new(0x7a, 0xa2, 0xf7),
            ok: Rgb::new(0x9e, 0xce, 0x6a),
            error: Rgb::new(0xf7, 0x76, 0x8e),
            warn: Rgb::new(0xe0, 0xaf, 0x68),
            strong: Rgb::new(0xf4, 0xf7, 0xfb),
        }
    }

    /// Titanium: the OMP-style neutral surface with a cool accent.
    pub fn titanium() -> Palette {
        Palette {
            name: "titanium",
            ..Palette::dark()
        }
    }

    /// Dark ink on white, for a bright room or a light OS theme.
    pub fn light() -> Palette {
        Palette {
            name: "light",
            description: "dark ink on a white background",
            background: Rgb::new(0xfa, 0xfa, 0xf8),
            surface: Rgb::new(0xf2, 0xf2, 0xf0),
            element: Rgb::new(0xe8, 0xe8, 0xe5),
            border: Rgb::new(0xd4, 0xd4, 0xd0),
            border_subtle: Rgb::new(0xe4, 0xe4, 0xe0),
            text: Rgb::new(0x24, 0x27, 0x2b),
            dim: Rgb::new(0x66, 0x6d, 0x78),
            accent: Rgb::new(0x1d, 0x63, 0xc8),
            ok: Rgb::new(0x1f, 0x7f, 0x3c),
            error: Rgb::new(0xb3, 0x22, 0x22),
            warn: Rgb::new(0x91, 0x62, 0x00),
            strong: Rgb::new(0x00, 0x00, 0x00),
        }
    }

    /// Maximum contrast.
    pub fn contrast() -> Palette {
        Palette {
            name: "contrast",
            description: "maximum contrast",
            background: Rgb::new(0x00, 0x00, 0x00),
            surface: Rgb::new(0x10, 0x10, 0x12),
            element: Rgb::new(0x1c, 0x1c, 0x20),
            border: Rgb::new(0x5a, 0x5a, 0x62),
            border_subtle: Rgb::new(0x38, 0x38, 0x3e),
            text: Rgb::new(0xff, 0xff, 0xff),
            dim: Rgb::new(0xc8, 0xcc, 0xd4),
            accent: Rgb::new(0x54, 0xd6, 0xff),
            ok: Rgb::new(0x3c, 0xe0, 0x5a),
            error: Rgb::new(0xff, 0x40, 0x40),
            warn: Rgb::new(0xff, 0xe0, 0x30),
            strong: Rgb::new(0xff, 0xff, 0xff),
        }
    }

    /// No colour: shape and glyphs carry the meaning.
    pub fn mono() -> Palette {
        let ink = Rgb::new(0xdd, 0xdd, 0xdd);
        Palette {
            name: "mono",
            description: "no colour; relies on ✓ ✗ ▸ glyphs",
            background: Rgb::new(0x11, 0x11, 0x11),
            surface: Rgb::new(0x18, 0x18, 0x18),
            element: Rgb::new(0x22, 0x22, 0x22),
            border: Rgb::new(0x3a, 0x3a, 0x3a),
            border_subtle: Rgb::new(0x28, 0x28, 0x28),
            text: ink,
            dim: Rgb::new(0xa0, 0xa0, 0xa0),
            accent: ink,
            ok: ink,
            error: ink,
            warn: ink,
            strong: Rgb::new(0xff, 0xff, 0xff),
        }
    }
}

impl Rgb {
    /// Perceived brightness, for contrast checks in tests.
    pub fn luminance(&self) -> f32 {
        (0.2126 * self.r as f32 + 0.7152 * self.g as f32 + 0.0722 * self.b as f32) / 255.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(Rgb::new(0, 0x7a, 0xff).hex(), "#007aff");
    }

    #[test]
    fn mixing_hits_both_ends_exactly() {
        let a = Rgb::new(0, 0, 0);
        let b = Rgb::new(255, 128, 64);
        assert_eq!(a.mix(b, 0.0), a);
        assert_eq!(a.mix(b, 1.0), b);
    }

    #[test]
    fn mixing_clamps_out_of_range_weights() {
        let a = Rgb::new(0, 0, 0);
        let b = Rgb::new(10, 10, 10);
        assert_eq!(a.mix(b, -5.0), a);
        assert_eq!(a.mix(b, 5.0), b);
    }

    #[test]
    fn every_role_resolves() {
        let palette = Palette::dark();
        for role in [
            Role::Background,
            Role::Surface,
            Role::Element,
            Role::Border,
            Role::BorderSubtle,
            Role::Text,
            Role::Dim,
            Role::Accent,
            Role::Ok,
            Role::Error,
            Role::Warn,
            Role::Strong,
        ] {
            let colour = palette.get(role);
            assert!(colour != palette.background || role == Role::Background);
        }
    }

    #[test]
    fn theme_names_are_unique() {
        let mut names: Vec<&str> = Palette::all().iter().map(|p| p.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    #[test]
    fn an_unknown_name_falls_back_to_dark() {
        assert_eq!(Palette::from_config(Some("nope")).name, "dark");
        assert_eq!(Palette::from_config(None).name, "dark");
        assert_eq!(Palette::from_config(Some(" Light ")).name, "light");
    }

    #[test]
    fn light_and_dark_contrast_differently() {
        // The whole reason palettes exist.
        assert_ne!(Palette::dark().text, Palette::light().text);
        assert!(Palette::dark().background.luminance() < Palette::light().background.luminance());
    }

    #[test]
    fn mono_does_not_use_colour_for_status() {
        let palette = Palette::mono();
        assert_eq!(palette.get(Role::Ok), palette.get(Role::Error));
        assert_eq!(palette.get(Role::Accent), palette.get(Role::Text));
    }

    fn contrast(a: Rgb, b: Rgb) -> f32 {
        fn channel(value: u8) -> f32 {
            let value = value as f32 / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        fn relative(rgb: Rgb) -> f32 {
            0.2126 * channel(rgb.r) + 0.7152 * channel(rgb.g) + 0.0722 * channel(rgb.b)
        }
        let a = relative(a);
        let b = relative(b);
        let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn text_and_accent_clear_the_background() {
        for palette in [Palette::dark(), Palette::titanium(), Palette::light()] {
            assert!(
                contrast(palette.text, palette.background) >= 7.0,
                "{} body text",
                palette.name
            );
            assert!(
                contrast(palette.accent, palette.background) >= 4.5,
                "{} accent",
                palette.name
            );
        }
    }
}
