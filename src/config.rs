// the command line surface and the validated config derived from it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, ValueEnum};

// the most consequential knob here. it decides whether a read can be answered
// without waking us at all, and how much memory the kernel spends on clean
// blocks every vm sharing a golden image already has cached underneath us.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum CacheMode {
    /// kernel keeps a page cache over our inode. reads that hit it never reach
    /// us. the cache is per-inode, so n vms sharing one golden image hold n
    /// copies of the same clean bytes. anything we change underneath the kernel
    /// has to be followed by an explicit invalidation or the guest reads stale
    /// data.
    Cached,
    /// no page cache. every read and write is a round trip to this process.
    /// no duplication, no invalidation problem, much higher per-op cost.
    Direct,
    /// hand the kernel the backing descriptor and step out of the path
    /// entirely. reads and writes never reach userspace. this cannot serve a
    /// block-level copy-on-write view, it exists to measure what the fuse
    /// plumbing itself costs.
    Passthrough,
}

impl CacheMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheMode::Cached => "cached",
            CacheMode::Direct => "direct",
            CacheMode::Passthrough => "passthrough",
        }
    }
}

// which backend holds the image, and therefore what a reset costs. picking one
// is the decision this daemon exists to let you measure.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum StoreKind {
    /// serve a file unchanged. no reference point, so no reset. the floor
    /// every other store is measured against.
    Passthrough,
    /// private copy of a golden image. reset copies the whole thing back, so
    /// the cost is the image size no matter how little the guest touched.
    #[value(name = "fullcopy")]
    FullCopy,
    /// private copy plus a dirty bitmap. reset copies back only the blocks
    /// that moved. the read path stays unindirected, so a fleet still shares
    /// its clean pages at the ext4 layer.
    #[value(name = "dirtylog")]
    DirtyLog,
    /// golden image plus a sparse overlay file. reset punches holes over the
    /// dirty runs. superb median, worst case bounded only by fragmentation.
    Overlay,
    /// golden image plus an in-memory overlay. reset drops the map, so no
    /// syscall and no disk, but it costs one free per dirty block. bounded by
    /// RAM.
    Cow,
}

impl StoreKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StoreKind::Passthrough => "passthrough",
            StoreKind::FullCopy => "fullcopy",
            StoreKind::DirtyLog => "dirtylog",
            StoreKind::Overlay => "overlay",
            StoreKind::Cow => "cow",
        }
    }

    // whether this store needs a separate golden image to revert to
    pub fn needs_golden(self) -> bool {
        self != StoreKind::Passthrough
    }

    // whether this store needs a writable file of its own. cow keeps its
    // overlay in memory and needs none.
    pub fn needs_image(self) -> bool {
        self != StoreKind::Cow
    }
}

#[derive(Parser, Debug, Clone)]
#[command(
    name = "blkrevertd",
    about = "fuse daemon serving a single disk image to a firecracker block device",
    long_about = None,
)]
pub struct Cli {
    /// writable file this daemon serves from. for the stores that keep a
    /// separate reference it is the private copy or the overlay; for
    /// passthrough it is the image itself. not used by --store cow.
    #[arg(long, value_name = "PATH")]
    pub image: Option<PathBuf>,

    /// read-only reference the reset reverts to. required by every store
    /// except passthrough.
    #[arg(long, value_name = "PATH")]
    pub golden: Option<PathBuf>,

    /// which backend holds the image.
    #[arg(long, value_enum, default_value_t = StoreKind::Passthrough)]
    pub store: StoreKind,

    /// dirty tracking granularity, in kib. the earlier experiments found 64
    /// kib amplifies a 1 percent random write workload by about 15x, so the
    /// default is deliberately small.
    #[arg(long, default_value_t = 4, value_name = "KIB")]
    pub block_kib: usize,

    /// ceiling on what --store cow may hold in memory, in mib. 0 disables the
    /// guard, which is only sensible in a benchmark.
    #[arg(long, default_value_t = 2048, value_name = "MIB")]
    pub overlay_max_mib: u64,

    /// copy the golden over the working image at startup, so a run begins from
    /// a known state. only meaningful for fullcopy and dirtylog, which serve
    /// reads from the working file.
    #[arg(long)]
    pub reseed: bool,

    /// directory the image is exposed under.
    #[arg(long, value_name = "DIR")]
    pub mountpoint: PathBuf,

    /// name the image appears under inside the mount. defaults to the backing
    /// file's own name so paths look unsurprising.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// unix socket the control plane listens on. defaults to <mountpoint>.sock
    /// so the socket never lands inside the mount it would be controlling.
    #[arg(long, value_name = "PATH")]
    pub control: Option<PathBuf>,

    /// kernel caching strategy for the served image.
    #[arg(long, value_enum, default_value_t = CacheMode::Cached)]
    pub cache: CacheMode,

    /// let the kernel buffer writes and flush them back in larger batches.
    /// only meaningful with --cache cached, and it puts dirty pages above us
    /// that must be flushed before any revert.
    #[arg(long)]
    pub writeback: bool,

    /// serve the image read only.
    #[arg(long)]
    pub read_only: bool,

    /// event loop threads. each gets its own cloned /dev/fuse descriptor.
    #[arg(long, default_value_t = 4)]
    pub threads: usize,

    /// largest write the kernel may send in one request, in kib.
    #[arg(long, default_value_t = 1024, value_name = "KIB")]
    pub max_write_kib: u32,

    /// readahead window the kernel may use, in kib.
    #[arg(long, default_value_t = 256, value_name = "KIB")]
    pub max_readahead_kib: u32,

    /// outstanding background requests the kernel may have in flight.
    #[arg(long, default_value_t = 64)]
    pub max_background: u16,

    /// how long the kernel may trust attributes, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    pub attr_ttl_ms: u64,

    /// how long the kernel may trust directory entries, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    pub entry_ttl_ms: u64,

    /// allow users other than the one running the daemon to reach the mount.
    /// needed when firecracker runs jailed under a different uid.
    #[arg(long)]
    pub allow_other: bool,

    /// allow root in addition to the owner.
    #[arg(long, conflicts_with = "allow_other")]
    pub allow_root: bool,

    /// unmount whatever is already on the mountpoint before starting.
    #[arg(long)]
    pub force_unmount: bool,

    /// write the daemon pid here once the mount is live.
    #[arg(long, value_name = "PATH")]
    pub pid_file: Option<PathBuf>,

    /// log a metrics line this often. 0 turns it off.
    #[arg(long, default_value_t = 0)]
    pub metrics_interval_s: u64,

    /// log level: error, warn, info, debug, trace.
    #[arg(long, default_value = "info")]
    pub log_level: String,
}

// config after validation, with paths absolute and durations concrete
#[derive(Debug, Clone)]
pub struct Config {
    pub image: Option<PathBuf>,
    pub golden: Option<PathBuf>,
    pub store: StoreKind,
    pub block_size: usize,
    pub overlay_max_bytes: u64,
    pub reseed: bool,
    pub mountpoint: PathBuf,
    pub name: String,
    pub control: PathBuf,
    pub cache: CacheMode,
    pub writeback: bool,
    pub read_only: bool,
    pub threads: usize,
    pub max_write: u32,
    pub max_readahead: u32,
    pub max_background: u16,
    pub attr_ttl: Duration,
    pub entry_ttl: Duration,
    pub allow_other: bool,
    pub allow_root: bool,
    pub force_unmount: bool,
    pub pid_file: Option<PathBuf>,
    pub metrics_interval: Option<Duration>,
}

#[derive(Debug)]
pub enum ConfigError {
    ImageMissing(PathBuf),
    ImageNotFile(PathBuf),
    MountpointUnusable(PathBuf, String),
    BadName(String),
    BadThreads(usize),
    WritebackWithoutCache(CacheMode),
    PassthroughReadWriteUnsupported,
    PassthroughNeedsCapSysAdmin,
    MissingImage(StoreKind),
    MissingGolden(StoreKind),
    GoldenMissing(PathBuf),
    BadBlockSize(usize),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::ImageMissing(p) => {
                write!(f, "backing image {} does not exist", p.display())
            }
            ConfigError::ImageNotFile(p) => {
                write!(f, "backing image {} is not a regular file", p.display())
            }
            ConfigError::MountpointUnusable(p, why) => {
                write!(f, "mountpoint {} is unusable: {why}", p.display())
            }
            ConfigError::BadName(n) => {
                write!(f, "exposed name {n:?} must be a single path component")
            }
            ConfigError::BadThreads(n) => write!(f, "thread count {n} must be at least 1"),
            ConfigError::WritebackWithoutCache(m) => {
                write!(f, "--writeback needs --cache cached, got {}", m.as_str())
            }
            ConfigError::PassthroughReadWriteUnsupported => write!(
                f,
                "passthrough hands the kernel one whole descriptor, so it cannot serve a \
                 copy-on-write view. it is a measurement mode only"
            ),
            ConfigError::MissingImage(k) => write!(
                f,
                "--store {} needs --image, the writable file it serves from",
                k.as_str()
            ),
            ConfigError::MissingGolden(k) => write!(
                f,
                "--store {} needs --golden, the reference a reset reverts to",
                k.as_str()
            ),
            ConfigError::GoldenMissing(p) => {
                write!(f, "golden image {} does not exist", p.display())
            }
            ConfigError::BadBlockSize(n) => write!(
                f,
                "--block-kib {n} must be a power of two between 4 and 1024"
            ),
            ConfigError::PassthroughNeedsCapSysAdmin => write!(
                f,
                "passthrough mode needs CAP_SYS_ADMIN because the kernel gates \
                 FUSE_DEV_IOC_BACKING_OPEN on it. run the daemon with that capability, \
                 or pick --cache cached or --cache direct"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn from_cli(cli: Cli) -> Result<Self, ConfigError> {
        if cli.threads == 0 {
            return Err(ConfigError::BadThreads(cli.threads));
        }
        if cli.writeback && cli.cache != CacheMode::Cached {
            return Err(ConfigError::WritebackWithoutCache(cli.cache));
        }

        if !cli.block_kib.is_power_of_two() || cli.block_kib < 4 || cli.block_kib > 1024 {
            return Err(ConfigError::BadBlockSize(cli.block_kib));
        }

        // the golden has to exist already; it is the reference, not something
        // we are allowed to invent. the working image may not, because the
        // stores that need one create it sized from the golden.
        let golden = cli.golden.as_deref().map(absolutise);
        if cli.store.needs_golden() {
            let Some(g) = golden.as_ref() else {
                return Err(ConfigError::MissingGolden(cli.store));
            };
            if !std::fs::metadata(g).map(|m| m.is_file()).unwrap_or(false) {
                return Err(ConfigError::GoldenMissing(g.clone()));
            }
        }

        let image = cli.image.as_deref().map(absolutise);
        if cli.store.needs_image() {
            let Some(i) = image.as_ref() else {
                return Err(ConfigError::MissingImage(cli.store));
            };
            // passthrough serves it as-is, so it must already be there. the
            // others create or resize it from the golden.
            if cli.store == StoreKind::Passthrough {
                match std::fs::metadata(i) {
                    Ok(m) if m.is_file() => {}
                    Ok(_) => return Err(ConfigError::ImageNotFile(i.clone())),
                    Err(_) => return Err(ConfigError::ImageMissing(i.clone())),
                }
            }
        }

        let mountpoint = prepare_mountpoint(&cli.mountpoint)?;

        // the exposed name follows the golden where there is one, so a guest
        // sees the same path regardless of which store is behind it
        let name = match cli.name {
            Some(n) => n,
            None => golden
                .as_ref()
                .or(image.as_ref())
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "image".to_string()),
        };
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(ConfigError::BadName(name));
        }

        let control = cli.control.map(|c| absolutise(&c)).unwrap_or_else(|| {
            let mut p = mountpoint.clone().into_os_string();
            p.push(".sock");
            PathBuf::from(p)
        });

        Ok(Config {
            image,
            golden,
            store: cli.store,
            block_size: cli.block_kib * 1024,
            overlay_max_bytes: cli.overlay_max_mib.saturating_mul(1024 * 1024),
            reseed: cli.reseed,
            mountpoint,
            name,
            control,
            cache: cli.cache,
            writeback: cli.writeback,
            read_only: cli.read_only,
            threads: cli.threads,
            max_write: cli.max_write_kib.saturating_mul(1024),
            max_readahead: cli.max_readahead_kib.saturating_mul(1024),
            max_background: cli.max_background,
            attr_ttl: Duration::from_millis(cli.attr_ttl_ms),
            entry_ttl: Duration::from_millis(cli.entry_ttl_ms),
            allow_other: cli.allow_other,
            allow_root: cli.allow_root,
            force_unmount: cli.force_unmount,
            pid_file: cli.pid_file.map(|p| absolutise(&p)),
            metrics_interval: match cli.metrics_interval_s {
                0 => None,
                n => Some(Duration::from_secs(n)),
            },
        })
    }

    // the writable file, for stores that have one
    pub fn image_path(&self) -> std::io::Result<&Path> {
        self.image.as_deref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("--store {} needs --image", self.store.as_str()),
            )
        })
    }

    // the reference a reset reverts to
    pub fn golden_path(&self) -> std::io::Result<&Path> {
        self.golden.as_deref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("--store {} needs --golden", self.store.as_str()),
            )
        })
    }

    // what to put in a log line, whichever store is in play
    pub fn served_path(&self) -> &Path {
        self.golden
            .as_deref()
            .or(self.image.as_deref())
            .unwrap_or(Path::new("<none>"))
    }
}

fn absolutise(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

// create the mountpoint if missing and refuse anything that is not an empty
// directory. a non-empty one is almost always a stale mount or a typo, and
// silently shadowing its contents is worse than failing.
fn prepare_mountpoint(p: &Path) -> Result<PathBuf, ConfigError> {
    let p = absolutise(p);
    match std::fs::metadata(&p) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return Err(ConfigError::MountpointUnusable(
                p.clone(),
                "not a directory".into(),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&p)
                .map_err(|e| ConfigError::MountpointUnusable(p.clone(), e.to_string()))?;
        }
        Err(e) => return Err(ConfigError::MountpointUnusable(p.clone(), e.to_string())),
    }
    Ok(p)
}
