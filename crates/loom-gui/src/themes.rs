use std::path::Path;

use gpui::{App, SharedString, px};
use gpui_component::{Theme, ThemeMode, ThemeRegistry};
use loom_config::{AppearanceConfig, Theme as ConfigTheme};

const BUNDLED: &[(&str, &str)] = &[
    (
        "apprentice",
        include_str!("../assets/themes/apprentice.json"),
    ),
    ("ayu", include_str!("../assets/themes/ayu.json")),
    (
        "ayu-mirage",
        include_str!("../assets/themes/ayu-mirage.json"),
    ),
    (
        "catppuccin",
        include_str!("../assets/themes/catppuccin.json"),
    ),
    ("cobalt2", include_str!("../assets/themes/cobalt2.json")),
    ("deus", include_str!("../assets/themes/deus.json")),
    ("dracula", include_str!("../assets/themes/dracula.json")),
    (
        "everforest",
        include_str!("../assets/themes/everforest.json"),
    ),
    ("github", include_str!("../assets/themes/github.json")),
    ("gotham", include_str!("../assets/themes/gotham.json")),
    ("gruvbox", include_str!("../assets/themes/gruvbox.json")),
    (
        "gruvbox-contrast",
        include_str!("../assets/themes/gruvbox-contrast.json"),
    ),
    ("iceberg", include_str!("../assets/themes/iceberg.json")),
    (
        "jellybeans",
        include_str!("../assets/themes/jellybeans.json"),
    ),
    ("kanagawa", include_str!("../assets/themes/kanagawa.json")),
    ("lucario", include_str!("../assets/themes/lucario.json")),
    ("miasma", include_str!("../assets/themes/miasma.json")),
    ("moonfly", include_str!("../assets/themes/moonfly.json")),
    ("night-owl", include_str!("../assets/themes/night-owl.json")),
    ("nightfly", include_str!("../assets/themes/nightfly.json")),
    ("nightfox", include_str!("../assets/themes/nightfox.json")),
    ("noctis", include_str!("../assets/themes/noctis.json")),
    ("nord", include_str!("../assets/themes/nord.json")),
    ("nordic", include_str!("../assets/themes/nordic.json")),
    ("one", include_str!("../assets/themes/one.json")),
    ("one-half", include_str!("../assets/themes/one-half.json")),
    ("panda", include_str!("../assets/themes/panda.json")),
    (
        "posterpole",
        include_str!("../assets/themes/posterpole.json"),
    ),
    ("rose-pine", include_str!("../assets/themes/rose-pine.json")),
    ("seoul256", include_str!("../assets/themes/seoul256.json")),
    (
        "shades-of-purple",
        include_str!("../assets/themes/shades-of-purple.json"),
    ),
    ("solarized", include_str!("../assets/themes/solarized.json")),
    ("sonokai", include_str!("../assets/themes/sonokai.json")),
    ("srcery", include_str!("../assets/themes/srcery.json")),
    ("tender", include_str!("../assets/themes/tender.json")),
    (
        "tokyonight",
        include_str!("../assets/themes/tokyonight.json"),
    ),
    (
        "tokyonight-day",
        include_str!("../assets/themes/tokyonight-day.json"),
    ),
    ("tomorrow", include_str!("../assets/themes/tomorrow.json")),
    ("zenbones", include_str!("../assets/themes/zenbones.json")),
];

pub fn load(config_dir: Option<&Path>, cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    for (name, content) in BUNDLED {
        if let Err(err) = registry.load_themes_from_str(content) {
            tracing::warn!("bundled theme {name} is invalid: {err}");
        }
    }
    let Some(dir) = config_dir.map(|d| d.join("themes")) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    for path in paths {
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                if let Err(err) = registry.load_themes_from_str(&content) {
                    tracing::warn!("ignoring theme file {}: {err}", path.display());
                }
            }
            Err(err) => tracing::warn!("cannot read theme file {}: {err}", path.display()),
        }
    }
}

pub fn names(cx: &App) -> Vec<(SharedString, ThemeMode)> {
    ThemeRegistry::global(cx)
        .sorted_themes()
        .into_iter()
        .map(|config| (config.name.clone(), config.mode))
        .collect()
}

pub fn apply(appearance: &AppearanceConfig, cx: &mut App) {
    let registry = ThemeRegistry::global(cx);
    let named = appearance
        .theme_name
        .as_deref()
        .and_then(|name| registry.themes().get(name).cloned());
    if appearance.theme_name.is_some() && named.is_none() {
        tracing::warn!(
            "theme {:?} is not installed; using the default theme",
            appearance.theme_name
        );
    }
    let light = registry.default_light_theme().clone();
    let dark = registry.default_dark_theme().clone();
    let mode = match appearance.theme {
        ConfigTheme::Dark => ThemeMode::Dark,
        ConfigTheme::Light => ThemeMode::Light,
    };
    Theme::change(mode, None, cx);
    Theme::update(cx, |theme| {
        theme.light_theme = light;
        theme.dark_theme = dark;
        match &named {
            Some(config) => theme.apply_config(config),
            None => {
                let config = if mode.is_dark() {
                    theme.dark_theme.clone()
                } else {
                    theme.light_theme.clone()
                };
                theme.apply_config(&config);
            }
        }
        if let Some(font) = &appearance.ui_font {
            theme.font_family = font.clone().into();
        }
        theme.font_size = px(appearance.ui_font_size);
        if let Some(font) = &appearance.mono_font {
            theme.mono_font_family = font.clone().into();
        }
        theme.mono_font_size = px(appearance.mono_font_size);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_themes_parse_with_syntax_colors() {
        let mut names = std::collections::HashSet::new();
        for (name, content) in BUNDLED {
            let set: gpui_component::ThemeSet = serde_json::from_str(content)
                .unwrap_or_else(|err| panic!("{name} does not parse: {err}"));
            assert!(!set.themes.is_empty(), "{name} has no themes");
            for theme in set.themes {
                assert!(
                    theme.highlight.is_some(),
                    "{} has no syntax colors",
                    theme.name
                );
                assert!(
                    names.insert(theme.name.clone()),
                    "{} is bundled twice",
                    theme.name
                );
            }
        }
    }
}
