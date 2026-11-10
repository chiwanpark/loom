use std::path::PathBuf;

use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, px, size};
use gpui_component::{Root, TitleBar};
use loom_config::ClientConfig;

use crate::backend::Backend;
use crate::connection::Env;
use crate::ui::workspace::Quit;
use crate::ui::{Workspace, bind_keys};

pub struct Startup {
    pub backend: Backend,
    pub env: Env,
    pub config: ClientConfig,
    pub config_path: PathBuf,
}

pub fn apply_appearance(config: &ClientConfig, cx: &mut App) {
    crate::themes::apply(&config.appearance, cx);
}

pub fn init(startup: Startup, cx: &mut App) -> anyhow::Result<()> {
    gpui_component::init(cx);
    crate::themes::load(startup.config_path.parent(), cx);
    apply_appearance(&startup.config, cx);
    crate::term::view::bind_keys(cx);
    bind_keys(&startup.config.keymap, cx);
    cx.on_action(|_: &Quit, cx| cx.quit());
    let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        app_id: Some("dev.loom.Loom".into()),
        window_min_size: Some(size(px(640.), px(400.))),
        ..TitleBar::window_options()
    };
    let Startup {
        backend,
        env,
        config,
        config_path,
    } = startup;
    cx.open_window(options, move |window, cx| {
        let workspace = cx.new(|cx| Workspace::new(backend, env, config, config_path, window, cx));
        cx.new(|cx| Root::new(workspace, window, cx))
    })?;
    cx.activate(true);
    Ok(())
}
