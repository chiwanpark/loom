use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::Context as _;
use loom_config::ClientConfig;
use loom_gui::app::{self, Startup};
use loom_gui::backend::Backend;
use loom_gui::connection::Env;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LOOM_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let mut args = std::env::args().skip(1);
    let mut config_path: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config_path = args.next().map(PathBuf::from),
            "--version" => {
                println!("loom {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                println!("usage: loom [--config PATH]");
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    let env = Env::from_env()?;
    let config_path = config_path.unwrap_or_else(|| ClientConfig::default_path(&env.dirs));
    let config = ClientConfig::load(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;
    let backend = Backend::new()?;
    let startup = Startup {
        backend,
        env,
        config,
        config_path,
    };
    let application = gpui_platform::application().with_assets(loom_gui::assets::AppAssets);
    let failed = Rc::new(Cell::new(false));
    let flag = failed.clone();
    application.run(move |cx| {
        if let Err(err) = app::init(startup, cx) {
            eprintln!("loom: {err:#}");
            flag.set(true);
            cx.quit();
        }
    });
    if failed.get() {
        std::process::exit(1);
    }
    Ok(())
}
