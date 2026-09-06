//! Which palette the window is wearing.

/// app-shell ticket 12 follow-up: a third baked-in palette (Catppuccin
/// Mocha) alongside light/dark, picked from Settings rather than the old
/// binary sun/moon toggle — a third option doesn't fit a two-state icon.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Theme {
    Light,
    Dark,
    CatppuccinMocha,
}

impl Theme {
    fn attr(self) -> &'static str {
        match self {
            Theme::Light => "light",
            Theme::Dark => "dark",
            Theme::CatppuccinMocha => "catppuccin-mocha",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Theme::Light => "Light",
            Theme::Dark => "Dark",
            Theme::CatppuccinMocha => "Catppuccin Mocha",
        }
    }
}

pub(crate) fn prefers_dark() -> bool {
    web_sys::window()
        .and_then(|w| w.match_media("(prefers-color-scheme: dark)").ok().flatten())
        .map(|m| m.matches())
        .unwrap_or(false)
}

pub(crate) fn apply_theme(theme: Theme) {
    if let Some(html) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    {
        let _ = html.set_attribute("data-theme", theme.attr());
    }
}
