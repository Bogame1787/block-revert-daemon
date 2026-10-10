// the out of band control plane. a reset is not something a guest can ask for
// and must not look like ordinary file io, so it arrives here instead, on a
// unix socket the orchestrator owns, as one json request per line.

// requests are served strictly one at a time, which is the contract rather than
// a simplification: reverts must not overlap, and the caller needs a
// synchronous acknowledgement before it lets the guest run again.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fuser::{INodeNo, Notifier, SessionUnmounter};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};

use crate::metrics::{Metrics, MetricsSnapshot};
use crate::store::{BlockStore, MarkReport, ResetReport, StoreInfo};

const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
// refuse absurd lines rather than buffering them, the largest legitimate
// request is a few hundred bytes
const MAX_LINE: u64 = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Info,
    Stats,
    // revert the image to its reference state, then drop whatever the kernel
    // had cached of it
    Reset,
    // make the current contents the new reference point. belongs at snapshot
    // time with the vm paused: the disk reference has to be taken in the same
    // quiesced window as the memory snapshot, or the guest's reverted page
    // cache ends up describing blocks the disk no longer holds.
    Mark,
    // drop the kernel's cached copy without touching the store, which is what
    // makes the caching mode measurable on its own
    Invalidate {
        #[serde(default)]
        offset: i64,
        // negative means to the end of the image
        #[serde(default = "minus_one")]
        len: i64,
    },
    Flush {
        #[serde(default = "yes")]
        datasync: bool,
    },
    Shutdown,
}

fn minus_one() -> i64 {
    -1
}
fn yes() -> bool {
    true
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Response {
    Ok { ok: bool, result: ResultBody },
    Err { ok: bool, error: String },
}

impl Response {
    fn ok(body: ResultBody) -> Self {
        Response::Ok {
            ok: true,
            result: body,
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        Response::Err {
            ok: false,
            error: msg.into(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ResultBody {
    Pong { pong: bool },
    Info(Box<InfoBody>),
    Stats(Box<MetricsSnapshot>),
    Reset(Box<ResetBody>),
    Mark(Box<MarkBody>),
    Invalidate { invalidated: bool, micros: u64 },
    Flush { micros: u64 },
    Shutdown { stopping: bool },
}

#[derive(Debug, Serialize)]
pub struct InfoBody {
    pub store: StoreInfo,
    pub cache_mode: &'static str,
    pub name: String,
    pub mountpoint: PathBuf,
    pub threads: usize,
    pub pid: u32,
}

#[derive(Debug, Serialize)]
pub struct MarkBody {
    #[serde(flatten)]
    pub report: MarkReport,
    // folding the overlay into golden changes what an offset reads as, so mark
    // needs the same invalidation a reset does
    pub invalidated: bool,
    pub total_micros: u64,
}

#[derive(Debug, Serialize)]
pub struct ResetBody {
    #[serde(flatten)]
    pub report: ResetReport,
    // a reset that reverts the store but fails to invalidate has left the guest
    // reading stale data, so this is reported separately rather than inside ok
    pub invalidated: bool,
    pub total_micros: u64,
}

// everything a control request may act on
pub struct ControlCtx {
    pub store: Arc<dyn BlockStore>,
    pub metrics: Arc<Metrics>,
    pub notifier: Notifier,
    pub image_ino: INodeNo,
    pub cache_mode: &'static str,
    pub name: String,
    pub mountpoint: PathBuf,
    pub threads: usize,
    pub shutdown: Arc<AtomicBool>,
    pub unmounter: Mutex<SessionUnmounter>,
}

impl ControlCtx {
    fn invalidate(&self, offset: i64, len: i64) -> std::io::Result<()> {
        self.notifier.inval_inode(self.image_ino, offset, len)
    }

    fn handle(&self, req: Request) -> Response {
        match req {
            Request::Ping => Response::ok(ResultBody::Pong { pong: true }),

            Request::Info => Response::ok(ResultBody::Info(Box::new(InfoBody {
                store: self.store.info(),
                cache_mode: self.cache_mode,
                name: self.name.clone(),
                mountpoint: self.mountpoint.clone(),
                threads: self.threads,
                pid: std::process::id(),
            }))),

            Request::Stats => Response::ok(ResultBody::Stats(Box::new(self.metrics.snapshot()))),

            Request::Reset => {
                let started = Instant::now();
                let report = match self.store.reset() {
                    Ok(r) => r,
                    Err(e) => {
                        self.metrics.reset.record(started, 0, false);
                        return Response::err(format!("reset failed: {e}"));
                    }
                };
                // the store is now back at its reference state, but the kernel
                // still holds pages it read before that happened. until those
                // are dropped the guest will keep reading pre-reset bytes.
                let invalidated = match self.invalidate(0, -1) {
                    Ok(()) => true,
                    Err(e) => {
                        error!("reverted the store but could not invalidate the page cache: {e}");
                        false
                    }
                };
                self.metrics
                    .reset
                    .record(started, report.bytes_reverted, invalidated);
                if !invalidated {
                    return Response::err(
                        "store reverted but the kernel cache could not be invalidated; \
                         the guest may read stale data",
                    );
                }
                Response::ok(ResultBody::Reset(Box::new(ResetBody {
                    report,
                    invalidated,
                    total_micros: started.elapsed().as_micros() as u64,
                })))
            }

            Request::Mark => {
                let started = Instant::now();
                let report = match self.store.mark() {
                    Ok(r) => r,
                    Err(e) => return Response::err(format!("mark failed: {e}")),
                };
                // the composite view just changed underneath the kernel in
                // exactly the way a reset changes it, so the cached copy is
                // just as stale.
                let invalidated = match self.invalidate(0, -1) {
                    Ok(()) => true,
                    Err(e) => {
                        error!("marked but could not invalidate the page cache: {e}");
                        false
                    }
                };
                if !invalidated {
                    return Response::err(
                        "reference point moved but the kernel cache could not be \
                         invalidated; the guest may read stale data",
                    );
                }
                Response::ok(ResultBody::Mark(Box::new(MarkBody {
                    report,
                    invalidated,
                    total_micros: started.elapsed().as_micros() as u64,
                })))
            }

            Request::Invalidate { offset, len } => {
                let started = Instant::now();
                match self.invalidate(offset, len) {
                    Ok(()) => {
                        self.metrics.invalidate.record(started, 0, true);
                        Response::ok(ResultBody::Invalidate {
                            invalidated: true,
                            micros: started.elapsed().as_micros() as u64,
                        })
                    }
                    Err(e) => {
                        self.metrics.invalidate.record(started, 0, false);
                        Response::err(format!("invalidate failed: {e}"))
                    }
                }
            }

            Request::Flush { datasync } => {
                let started = Instant::now();
                match self.store.flush(datasync) {
                    Ok(()) => Response::ok(ResultBody::Flush {
                        micros: started.elapsed().as_micros() as u64,
                    }),
                    Err(e) => Response::err(format!("flush failed: {e}")),
                }
            }

            Request::Shutdown => {
                info!("shutdown requested over the control socket");
                self.shutdown.store(true, Ordering::SeqCst);
                let mut u = match self.unmounter.lock() {
                    Ok(g) => g,
                    Err(e) => e.into_inner(),
                };
                if let Err(e) = u.unmount() {
                    warn!("unmount during shutdown failed: {e}");
                }
                Response::ok(ResultBody::Shutdown { stopping: true })
            }
        }
    }
}

pub struct ControlServer {
    listener: UnixListener,
    path: PathBuf,
    ctx: Arc<ControlCtx>,
}

impl ControlServer {
    // a stale socket from a crashed daemon is removed, but only after checking
    // nothing is listening, so two daemons cannot steal each other's socket
    pub fn bind(path: &Path, ctx: Arc<ControlCtx>, group_readable: bool) -> std::io::Result<Self> {
        if path.exists() {
            match UnixStream::connect(path) {
                Ok(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        format!("another daemon is already listening on {}", path.display()),
                    ));
                }
                Err(_) => {
                    debug!("removing stale control socket {}", path.display());
                    std::fs::remove_file(path)?;
                }
            }
        }
        let listener = UnixListener::bind(path)?;
        let mode = if group_readable { 0o660 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        info!("control socket at {} mode {mode:o}", path.display());
        Ok(ControlServer {
            listener,
            path: path.to_path_buf(),
            ctx,
        })
    }

    // accept forever, one client at a time
    pub fn serve(self) {
        for stream in self.listener.incoming() {
            if self.ctx.shutdown.load(Ordering::SeqCst) {
                break;
            }
            match stream {
                Ok(s) => {
                    if let Err(e) = self.serve_one(s) {
                        debug!("control client ended: {e}");
                    }
                }
                Err(e) => {
                    error!("control accept failed: {e}");
                    break;
                }
            }
            if self.ctx.shutdown.load(Ordering::SeqCst) {
                break;
            }
        }
        let _ = std::fs::remove_file(&self.path);
        debug!("control server stopped");
    }

    fn serve_one(&self, stream: UnixStream) -> std::io::Result<()> {
        stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
        stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;
        let mut out = stream.try_clone()?;
        // the limit is reset per line rather than applied to the whole
        // connection, so a long lived control session is not cut off after its
        // first few requests.
        let mut reader = BufReader::new(stream).take(MAX_LINE);
        let mut line = String::new();
        loop {
            line.clear();
            reader.set_limit(MAX_LINE);
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            if line.trim().is_empty() {
                continue;
            }
            let resp = match serde_json::from_str::<Request>(line.trim()) {
                Ok(req) => self.ctx.handle(req),
                Err(e) => Response::err(format!("bad request: {e}")),
            };
            let mut body = serde_json::to_string(&resp).unwrap_or_else(|e| {
                format!("{{\"ok\":false,\"error\":\"could not encode response: {e}\"}}")
            });
            body.push('\n');
            out.write_all(body.as_bytes())?;
            out.flush()?;
            if self.ctx.shutdown.load(Ordering::SeqCst) {
                break;
            }
        }
        Ok(())
    }
}
