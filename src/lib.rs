// the daemon's internals, kept in a library so the fuse layer and the stores
// can be driven without going through the binary.

pub mod config;
pub mod control;
pub mod fs;
pub mod metrics;
pub mod store;

use fuser::{MountOption, SessionACL};

use crate::config::Config;

// turn our config into the mount options and session settings fuser wants
pub fn mount_config(cfg: &Config) -> fuser::Config {
    let mut options = vec![
        MountOption::FSName("blkrevertd".into()),
        MountOption::Subtype("blkrevert".into()),
        // a disk image is data: nothing here should be executable, honour a
        // setuid bit, or be read as a device node
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::NoExec,
        // we never maintain access times, so stop the kernel asking
        MountOption::NoAtime,
    ];
    if cfg.read_only {
        options.push(MountOption::RO);
    } else {
        options.push(MountOption::RW);
    }

    let acl = if cfg.allow_other {
        SessionACL::All
    } else if cfg.allow_root {
        SessionACL::RootAndOwner
    } else {
        SessionACL::Owner
    };

    // auto_unmount avoids leaving a stale mount if we die, but fusermount only
    // offers it when the mount is not owner-restricted
    if acl != SessionACL::Owner {
        options.push(MountOption::AutoUnmount);
    }

    let mut mc = fuser::Config::default();
    mc.mount_options = options;
    mc.acl = acl;
    mc.n_threads = Some(cfg.threads);
    // one descriptor per worker, or the extra threads just queue on a single
    // /dev/fuse
    mc.clone_fd = cfg.threads > 1;
    mc
}

// the kernel gates FUSE_DEV_IOC_BACKING_OPEN on CAP_SYS_ADMIN, so passthrough
// mode needs it and we would rather find out now than at the first guest open
pub fn has_cap_sys_admin() -> bool {
    const CAP_SYS_ADMIN: u32 = 21;
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return false;
    };
    status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
        .map(|bits| bits & (1 << CAP_SYS_ADMIN) != 0)
        .unwrap_or(false)
}
