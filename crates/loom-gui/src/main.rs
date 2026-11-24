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
                println!("loom {}", loom_version::VERSION);
                return Ok(());
            }
            "-h" | "--help" => {
                println!(
                    "usage: loom [--config PATH]\n       loom update [--check] [--version VERSION]"
                );
                return Ok(());
            }
            "update" => std::process::exit(update(args.collect())?),
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    let env = Env::from_env()?;
    let config_path = config_path.unwrap_or_else(|| ClientConfig::default_path(&env.dirs));
    let config = ClientConfig::load(&config_path)
        .with_context(|| format!("loading {}", config_path.display()))?;
    if !config.removed_hosts.is_empty() {
        tracing::warn!(
            hosts = %config.removed_hosts.join(", "),
            "Loom no longer has local, SSH, and relay hosts; the client ignores them and removes them from the file when it saves it"
        );
    }
    let backend = Backend::new()?;
    // An updated client restarts with the same configuration.
    let restart_arguments = vec!["--config".into(), config_path.clone().into_os_string()];
    let startup = Startup {
        backend,
        env,
        config,
        config_path,
    };
    let application = gpui_platform::application()
        .with_assets(loom_gui::assets::AppAssets)
        .with_restart_arguments(restart_arguments);
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

/// `loom update`: installs the latest release of Loom, or another one, in
/// place of this one, with its agent. Returns the exit code.
fn update(args: Vec<String>) -> anyhow::Result<i32> {
    use loom_update::{Package, Updater, Version, current_version, install_dir};
    let mut check = false;
    let mut version: Option<Version> = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => check = true,
            "--version" => {
                let value = args
                    .next()
                    .context("--version needs a version, such as 0.4.0")?;
                version =
                    Some(Version::parse(&value).with_context(|| format!("version {value:?}"))?);
            }
            "-h" | "--help" => {
                println!("usage: loom update [--check] [--version VERSION]");
                println!();
                println!("Installs the latest release of Loom and its agent where loom is.");
                println!("  --check            Only tell whether a newer release is out.");
                println!("  --version VERSION  Install this release instead of the latest.");
                return Ok(0);
            }
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let updater = Updater::new();
        let current = current_version();
        let release = updater.release(Package::Desktop, version.as_ref()).await?;
        if check {
            if release.version > current {
                println!("Loom {} is out; this is {current}", release.version);
                println!("Run `loom update` to install it.");
                return Ok(1);
            }
            println!("Loom {current} is the latest release");
            return Ok(0);
        }
        if version.is_none() && release.version <= current {
            println!("Loom {current} is up to date");
            return Ok(0);
        }
        let dir = install_dir()?;
        eprintln!("Downloading {}", release.archive);
        let installed = updater
            .install(Package::Desktop, &release, &dir, |_, _| {})
            .await?;
        for item in &installed {
            let old = item.old.as_deref().unwrap_or(item.binary);
            println!(
                "Replaced {old} at {} with {}",
                item.path.display(),
                item.new
            );
        }
        println!("Restart Loom to use the new version.");
        Ok(0)
    })
}
