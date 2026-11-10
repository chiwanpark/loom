use std::path::Path;

use gpui::{App, SharedString, px};
use gpui_component::{Theme, ThemeMode, ThemeRegistry};
use loom_config::{AppearanceConfig, Theme as ConfigTheme};

const BUNDLED: [(&str, &str); 6] = [
    ("ayu", include_str!("../assets/themes/ayu.json")),
    (
        "catppuccin",
        include_str!("../assets/themes/catppuccin.json"),
    ),
    (
        "everforest",
        include_str!("../assets/themes/everforest.json"),
    ),
    ("gruvbox", include_str!("../assets/themes/gruvbox.json")),
    ("solarized", include_str!("../assets/themes/solarized.json")),
    (
        "tokyonight",
        include_str!("../assets/themes/tokyonight.json"),
    ),
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
            }
        }
    }
}
