// blkrevertd, a fuse daemon serving one disk image to one firecracker block
// device.
// it owns three things: the fuse session carrying guest io, a storage backend
// behind a trait, and a control socket the orchestrator uses to revert that
// backend between guests.

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use fuser::Session;
use log::{error, info, warn};

use blkrevertd::config::{CacheMode, Cli, Config, ConfigError};
use blkrevertd::control::{ControlCtx, ControlServer};
use blkrevertd::fs::{IMAGE_INO, ImageFs};
use blkrevertd::metrics::Metrics;
use blkrevertd::store::{self, BlockStore};
use blkrevertd::{has_cap_sys_admin, mount_config};

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    init_logging(&cli.log_level);

    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn init_logging(level: &str) {
    let env = env_logger::Env::default().default_filter_or(level);
    env_logger::Builder::from_env(env)
        .format_timestamp_millis()
        .format_target(false)
        .init();
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Arc::new(Config::from_cli(cli)?);

    if cfg.force_unmount {
        force_unmount(&cfg.mountpoint);
    }

    let store: Arc<dyn BlockStore> = store::open(&cfg)?;

    // passthrough hands the kernel exactly one descriptor. a store that
    // composes several files cannot express itself that way, and finding that
    // out at the first open would mean failing a guest's io instead of a
    // mount.
    if cfg.cache == CacheMode::Passthrough {
        if store.backing_fd().is_none() {
            return Err(Box::new(ConfigError::PassthroughReadWriteUnsupported));
        }
        // registering a backing descriptor is a privileged ioctl. finding that
        // out lazily means every guest open fails with EPERM and the mount
        // looks broken rather than misconfigured, so check up front.
        if !has_cap_sys_admin() {
            return Err(Box::new(ConfigError::PassthroughNeedsCapSysAdmin));
        }
    }

    let metrics = Arc::new(Metrics::default());
    let si = store.info();
    info!(
        "store {} over {} is {} bytes ({} MiB), reset {}",
        si.kind,
        cfg.served_path().display(),
        si.size,
        si.size / (1 << 20),
        if si.resettable { "supported" } else { "unsupported" },
    );

    let fs = ImageFs::new(Arc::clone(&cfg), Arc::clone(&store), Arc::clone(&metrics));
    let mut session = Session::new(fs, &cfg.mountpoint, &mount_config(&cfg))?;

    let notifier = session.notifier();
    let unmounter = session.unmount_callable();
    let shutdown = Arc::new(AtomicBool::new(false));

    let ctx = Arc::new(ControlCtx {
        store: Arc::clone(&store),
        metrics: Arc::clone(&metrics),
        notifier,
        image_ino: IMAGE_INO,
        cache_mode: cfg.cache.as_str(),
        name: cfg.name.clone(),
        mountpoint: cfg.mountpoint.clone(),
        threads: cfg.threads,
        shutdown: Arc::clone(&shutdown),
        unmounter: Mutex::new(unmounter),
    });

    let server = ControlServer::bind(
        &cfg.control,
        Arc::clone(&ctx),
        cfg.allow_other || cfg.allow_root,
    )?;
    let control_thread = std::thread::Builder::new()
        .name("blkrevertd-control".into())
        .spawn(move || server.serve())?;

    install_signal_handler(Arc::clone(&ctx), cfg.control.clone())?;

    if let Some(interval) = cfg.metrics_interval {
        spawn_metrics_ticker(Arc::clone(&metrics), Arc::clone(&shutdown), interval)?;
    }

    let pid_guard = cfg
        .pid_file
        .as_ref()
        .map(|p| PidFile::write(p))
        .transpose()?;

    info!("mounted at {}", cfg.mountpoint.display());
    info!(
        "image available at {}",
        cfg.mountpoint.join(&cfg.name).display()
    );

    let result = session.run();

    shutdown.store(true, Ordering::SeqCst);
    poke(&cfg.control);
    let _ = control_thread.join();
    drop(pid_guard);

    info!("final counters: {}", metrics.summary_line());
    result.map_err(Into::into)
}

// clear whatever is on the mountpoint. opt in, because unmounting something
// still serving a live guest is worse than refusing to start.
fn force_unmount(mountpoint: &Path) {
    let status = std::process::Command::new("fusermount3")
        .arg("-u")
        .arg("-z")
        .arg(mountpoint)
        .status();
    match status {
        Ok(s) if s.success() => info!("unmounted stale mount at {}", mountpoint.display()),
        Ok(_) => info!("nothing mounted at {}", mountpoint.display()),
        Err(e) => warn!("could not run fusermount3: {e}"),
    }
}

fn install_signal_handler(
    ctx: Arc<ControlCtx>,
    control_path: std::path::PathBuf,
) -> std::io::Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
    std::thread::Builder::new()
        .name("blkrevertd-signals".into())
        .spawn(move || {
            // one signal is enough: the first one tears the mount down and
            // the session loop exits, so there is nothing left to handle.
            if let Some(sig) = signals.forever().next() {
                info!("caught signal {sig}, unmounting");
                ctx.shutdown.store(true, Ordering::SeqCst);
                let mut u = match ctx.unmounter.lock() {
                    Ok(g) => g,
                    Err(e) => e.into_inner(),
                };
                if let Err(e) = u.unmount() {
                    error!("unmount failed: {e}");
                }
                drop(u);
                // the control thread is parked in accept, so give it a
                // connection to wake up on.
                poke(&control_path);
            }
        })?;
    Ok(())
}

fn spawn_metrics_ticker(
    metrics: Arc<Metrics>,
    shutdown: Arc<AtomicBool>,
    interval: Duration,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("blkrevertd-metrics".into())
        .spawn(move || {
            while !shutdown.load(Ordering::SeqCst) {
                std::thread::sleep(interval);
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                info!("{}", metrics.summary_line());
            }
        })?;
    Ok(())
}

// open and drop a connection, purely to unblock the accept loop
fn poke(path: &Path) {
    let _ = UnixStream::connect(path);
}

// removes the pid file on the way out so a supervisor cannot mistake a dead
// daemon for a live one
struct PidFile(std::path::PathBuf);

impl PidFile {
    fn write(path: &Path) -> std::io::Result<Self> {
        std::fs::write(path, format!("{}\n", std::process::id()))?;
        Ok(PidFile(path.to_path_buf()))
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
