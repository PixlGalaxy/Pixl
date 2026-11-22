//! Registry of every child process (yt-dlp, etc.), so none outlives its track or session.
//!
//! Every child is spawned through [`ProcessRegistry::run`]: own process group, `kill_on_drop`,
//! stdin closed, bounded stderr capture and a hard timeout. Killing (by track, channel, guild or
//! everything) terminates the whole process group, escalates to SIGKILL and always reaps the child.

#![allow(dead_code)]

mod dirs;

use std::{
    ffi::OsString,
    future::Future,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use dashmap::DashMap;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use uuid::Uuid;

pub use dirs::StreamDirs;

/// Time between SIGTERM and SIGKILL.
const TERM_GRACE: Duration = Duration::from_millis(1500);
/// How long a `kill_*` call waits for the owning task to reap its child before forcing it.
const KILL_WAIT: Duration = Duration::from_secs(5);
/// Keep only the tail of stderr; errors are at the end.
const STDERR_MAX: usize = 16 * 1024;
/// Upper bound for captured stdout (yt-dlp JSON for huge playlists can be large).
const STDOUT_MAX: usize = 64 * 1024 * 1024;

/// Who a child process belongs to. Killing by owner prefix (guild, or guild + channel) must be possible.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProcOwner {
    pub guild_id: u64,
    pub voice_channel_id: u64,
    pub track_uid: Option<Uuid>,
}

impl ProcOwner {
    pub fn new(guild_id: u64, voice_channel_id: u64, track_uid: Option<Uuid>) -> Self {
        Self { guild_id, voice_channel_id, track_uid }
    }

    /// Owner for background work not tied to any voice channel (self-update, API probes).
    pub fn system() -> Self {
        Self { guild_id: 0, voice_channel_id: 0, track_uid: None }
    }

    pub fn is_system(&self) -> bool {
        self.guild_id == 0 && self.voice_channel_id == 0
    }

    pub fn with_track(&self, track_uid: Uuid) -> Self {
        Self { track_uid: Some(track_uid), ..self.clone() }
    }
}

/// A command to run through the registry.
#[derive(Debug, Clone)]
pub struct ProcSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    pub envs: Vec<(OsString, OsString)>,
    pub timeout: Duration,
    pub owner: ProcOwner,
}

impl ProcSpec {
    pub fn new(program: impl Into<PathBuf>, owner: ProcOwner, timeout: Duration) -> Self {
        Self { program: program.into(), args: Vec::new(), current_dir: None, envs: Vec::new(), timeout, owner }
    }

    pub fn arg(mut self, a: impl Into<OsString>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(it.into_iter().map(Into::into));
        self
    }

    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    pub fn env(mut self, k: impl Into<OsString>, v: impl Into<OsString>) -> Self {
        self.envs.push((k.into(), v.into()));
        self
    }
}

#[derive(Debug)]
pub struct ProcOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    /// Tail of stderr (at most `STDERR_MAX` bytes), lossily decoded.
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProcError {
    #[error("failed to start {program}: {source}")]
    Spawn { program: String, source: std::io::Error },
    #[error("process timed out after {0:?}")]
    Timeout(Duration, String),
    #[error("process was killed")]
    Killed(String),
    #[error("i/o error waiting for process: {0}")]
    Io(#[from] std::io::Error),
}

impl ProcError {
    /// Captured stderr, when the process got far enough to produce any.
    pub fn stderr(&self) -> &str {
        match self {
            Self::Timeout(_, s) | Self::Killed(s) => s,
            _ => "",
        }
    }
}

struct Entry {
    owner: ProcOwner,
    pid: Option<u32>,
    started: Instant,
    /// Asks the owning task to terminate the group.
    cancel: CancellationToken,
    /// Cancelled once the child is reaped and the entry removed.
    done: CancellationToken,
}

/// Summary of one live process, for diagnostics.
#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub id: Uuid,
    pub owner: ProcOwner,
    pub pid: Option<u32>,
    pub age: Duration,
}

#[derive(Clone, Default)]
pub struct ProcessRegistry {
    inner: Arc<DashMap<Uuid, Entry>>,
}

/// Removes the registry entry when the run future finishes or is dropped. If dropped before the
/// child was reaped, the whole group gets SIGKILL (the leader is also killed by `kill_on_drop`
/// and reaped by tokio's orphan reaper).
struct EntryGuard {
    map: Arc<DashMap<Uuid, Entry>>,
    id: Uuid,
    pid: Option<u32>,
    reaped: bool,
}

impl Drop for EntryGuard {
    fn drop(&mut self) {
        if !self.reaped {
            if let Some(pid) = self.pid {
                signal_group(pid, Signal::Kill);
            }
        }
        if let Some((_, e)) = self.map.remove(&self.id) {
            e.done.cancel();
        }
    }
}

type Target = (Uuid, Option<u32>, CancellationToken, CancellationToken);

impl ProcessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn list(&self) -> Vec<ProcInfo> {
        self.inner
            .iter()
            .map(|e| ProcInfo { id: *e.key(), owner: e.owner.clone(), pid: e.pid, age: e.started.elapsed() })
            .collect()
    }

    /// Spawn `spec`, collect its output and wait for it to exit. The process is registered for
    /// the whole call; `kill_*` on its owner ends it early with [`ProcError::Killed`].
    /// A non-zero exit status is not an error here; callers inspect `status`.
    pub async fn run(&self, spec: ProcSpec) -> Result<ProcOutput, ProcError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &spec.envs {
            cmd.env(k, v);
        }
        if let Some(dir) = &spec.current_dir {
            cmd.current_dir(dir);
        }
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .map_err(|source| ProcError::Spawn { program: spec.program.display().to_string(), source })?;
        let pid = child.id();
        let id = Uuid::new_v4();
        let cancel = CancellationToken::new();
        self.inner.insert(
            id,
            Entry {
                owner: spec.owner.clone(),
                pid,
                started: Instant::now(),
                cancel: cancel.clone(),
                done: CancellationToken::new(),
            },
        );
        let mut guard = EntryGuard { map: self.inner.clone(), id, pid, reaped: false };
        debug!(?pid, owner = ?spec.owner, program = %spec.program.display(), "spawned child");

        let stdout = tokio::spawn(read_capped(child.stdout.take(), STDOUT_MAX, false));
        let stderr = tokio::spawn(read_capped(child.stderr.take(), STDERR_MAX, true));

        #[derive(PartialEq)]
        enum End {
            Exited,
            Timeout,
            Killed,
        }
        let (end, status) = tokio::select! {
            r = child.wait() => (End::Exited, r),
            _ = tokio::time::sleep(spec.timeout) => {
                terminate(&mut child, pid).await;
                (End::Timeout, child.wait().await)
            }
            _ = cancel.cancelled() => {
                terminate(&mut child, pid).await;
                (End::Killed, child.wait().await)
            }
        };
        guard.reaped = true;
        if end == End::Exited {
            // The leader exited on its own; take down anything it left behind in its group.
            if let Some(pid) = pid {
                signal_group(pid, Signal::Kill);
            }
        }

        // Pipes close once every process in the group is gone; never hang on them.
        let collect = async { (stdout.await.unwrap_or_default(), stderr.await.unwrap_or_default()) };
        let (out, err) = tokio::time::timeout(Duration::from_secs(3), collect).await.unwrap_or_default();
        let err = String::from_utf8_lossy(&err).into_owned();
        drop(guard);

        match end {
            End::Killed => Err(ProcError::Killed(err)),
            End::Timeout => Err(ProcError::Timeout(spec.timeout, err)),
            End::Exited => Ok(ProcOutput { status: status?, stdout: out, stderr: err }),
        }
    }

    /// Kill the processes of one track.
    pub async fn kill_track(&self, guild_id: u64, voice_channel_id: u64, track_uid: Uuid) -> usize {
        self.kill_where(|o| {
            o.guild_id == guild_id && o.voice_channel_id == voice_channel_id && o.track_uid == Some(track_uid)
        })
        .await
    }

    /// Kill every process of one voice channel session.
    pub async fn kill_channel(&self, guild_id: u64, voice_channel_id: u64) -> usize {
        self.kill_where(|o| o.guild_id == guild_id && o.voice_channel_id == voice_channel_id).await
    }

    /// Kill every process of one guild.
    pub async fn kill_guild(&self, guild_id: u64) -> usize {
        self.kill_where(|o| o.guild_id == guild_id).await
    }

    /// Kill every registered process. Called on shutdown.
    pub async fn kill_all(&self) -> usize {
        self.kill_where(|_| true).await
    }

    /// Kill all processes whose owner matches `pred` and wait until they are reaped. Returns the count.
    pub async fn kill_where(&self, pred: impl Fn(&ProcOwner) -> bool) -> usize {
        let targets: Vec<Target> = self
            .inner
            .iter()
            .filter(|e| pred(&e.owner))
            .map(|e| (*e.key(), e.pid, e.cancel.clone(), e.done.clone()))
            .collect();
        self.kill_targets(targets).await
    }

    async fn kill_targets(&self, targets: Vec<Target>) -> usize {
        let n = targets.len();
        let mut set = tokio::task::JoinSet::new();
        for (id, pid, cancel, done) in targets {
            let map = self.inner.clone();
            set.spawn(async move {
                cancel.cancel();
                if tokio::time::timeout(KILL_WAIT, done.cancelled()).await.is_err() {
                    // The owning task is not being polled; force the group down ourselves.
                    warn!(?pid, "child not reaped in time, sending SIGKILL to its group");
                    if let Some(pid) = pid {
                        signal_group(pid, Signal::Kill);
                    }
                    if let Some((_, e)) = map.remove(&id) {
                        e.done.cancel();
                    }
                }
            });
        }
        while set.join_next().await.is_some() {}
        n
    }

    /// Periodic sweep: kills processes older than `max_age` or whose owner is gone.
    /// `owner_alive` is only asked about non-system owners. Spawn the returned future; it ends on `stop`.
    pub fn sweep_task<F>(
        &self,
        every: Duration,
        max_age: Duration,
        owner_alive: F,
        stop: CancellationToken,
    ) -> impl Future<Output = ()> + Send + 'static
    where
        F: Fn(&ProcOwner) -> bool + Send + Sync + 'static,
    {
        let reg = self.clone();
        async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tick.tick() => {}
                }
                let n = reg.sweep_once(max_age, &owner_alive).await;
                if n > 0 {
                    warn!(killed = n, "process sweep killed stale children");
                }
            }
        }
    }

    /// One sweep pass; returns how many processes were killed.
    pub async fn sweep_once(&self, max_age: Duration, owner_alive: &(impl Fn(&ProcOwner) -> bool + ?Sized)) -> usize {
        let targets: Vec<Target> = self
            .inner
            .iter()
            .filter(|e| e.started.elapsed() > max_age || (!e.owner.is_system() && !owner_alive(&e.owner)))
            .map(|e| (*e.key(), e.pid, e.cancel.clone(), e.done.clone()))
            .collect();
        self.kill_targets(targets).await
    }
}

/// SIGTERM the group, wait up to `TERM_GRACE`, then SIGKILL the group. Does not reap; the caller waits.
async fn terminate(child: &mut Child, pid: Option<u32>) {
    match pid {
        Some(pid) => {
            signal_group(pid, Signal::Term);
            let _ = tokio::time::timeout(TERM_GRACE, child.wait()).await;
            // Either the leader ignored TERM or it is gone; any survivor in the group gets KILL.
            signal_group(pid, Signal::Kill);
        }
        None => {
            let _ = child.start_kill();
        }
    }
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
fn signal_group(pgid: u32, sig: Signal) {
    let sig = match sig {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: killpg only sends a signal; ESRCH (group already gone) is fine to ignore.
    unsafe {
        libc::killpg(pgid as libc::pid_t, sig);
    }
}

#[cfg(not(unix))]
fn signal_group(_pgid: u32, _sig: Signal) {}

async fn read_capped<R: AsyncRead + Unpin>(src: Option<R>, cap: usize, keep_tail: bool) -> Vec<u8> {
    let Some(mut src) = src else { return Vec::new() };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match src.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if keep_tail {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > cap {
                        let cut = buf.len() - cap;
                        buf.drain(..cut);
                    }
                } else if buf.len() < cap {
                    let take = n.min(cap - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                }
                // Past the cap we keep draining so the child never blocks on a full pipe.
            }
        }
    }
    buf
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn owner(g: u64, c: u64, t: Option<Uuid>) -> ProcOwner {
        ProcOwner::new(g, c, t)
    }

    fn sh(script: &str, owner: ProcOwner, timeout: Duration) -> ProcSpec {
        ProcSpec::new("/bin/sh", owner, timeout).arg("-c").arg(script)
    }

    fn running(pid: i32) -> bool {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // Field 3 is the state; a zombie ("Z") is not running.
        !stat.is_empty() && !stat.split(") ").nth(1).is_some_and(|s| s.starts_with('Z'))
    }

    async fn wait_len(reg: &ProcessRegistry, n: usize) {
        for _ in 0..200 {
            if reg.len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("registry never reached {n} entries");
    }

    #[tokio::test]
    async fn captures_output_and_unregisters() {
        let reg = ProcessRegistry::new();
        let out = reg
            .run(sh("echo hello; echo oops >&2; exit 3", owner(1, 2, None), Duration::from_secs(5)))
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
        assert_eq!(out.stderr.trim(), "oops");
        assert_eq!(out.status.code(), Some(3));
        assert!(reg.is_empty());
    }

    #[tokio::test]
    async fn stderr_is_bounded_to_the_tail() {
        let reg = ProcessRegistry::new();
        let out = reg
            .run(sh(
                "i=0; while [ $i -lt 3000 ]; do echo line$i >&2; i=$((i+1)); done; echo ERROR: last >&2",
                owner(1, 2, None),
                Duration::from_secs(10),
            ))
            .await
            .unwrap();
        assert!(out.stderr.len() <= STDERR_MAX);
        assert!(out.stderr.trim_end().ends_with("ERROR: last"));
    }

    #[tokio::test]
    async fn timeout_kills_group() {
        let reg = ProcessRegistry::new();
        let err = reg.run(sh("sleep 30", owner(1, 2, None), Duration::from_millis(200))).await.unwrap_err();
        assert!(matches!(err, ProcError::Timeout(..)), "{err:?}");
        assert!(reg.is_empty());
    }

    #[tokio::test]
    async fn kill_channel_kills_whole_group_including_grandchildren() {
        let reg = ProcessRegistry::new();
        let dir = std::env::temp_dir().join(format!("lynx-proc-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        // A grandchild that ignores SIGTERM: only the group SIGKILL can stop it.
        let script = format!("(trap '' TERM; sleep 60) & echo $! > {}; wait", pidfile.display());
        let t = Uuid::new_v4();
        let r = reg.clone();
        let task = tokio::spawn(async move { r.run(sh(&script, owner(7, 8, Some(t)), Duration::from_secs(60))).await });
        let r = reg.clone();
        let other = tokio::spawn(async move { r.run(sh("sleep 2", owner(7, 9, None), Duration::from_secs(60))).await });
        wait_len(&reg, 2).await;
        for _ in 0..100 {
            if std::fs::read_to_string(&pidfile).is_ok_and(|s| s.ends_with('\n')) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let gpid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        assert!(running(gpid));
        assert_eq!(reg.kill_channel(7, 8).await, 1);
        let res = task.await.unwrap();
        assert!(matches!(res, Err(ProcError::Killed(_))), "{res:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!running(gpid), "grandchild survived the group kill");
        // The other channel is untouched.
        assert!(other.await.unwrap().is_ok());
        assert!(reg.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn kill_track_and_guild_match_only_their_owner() {
        let reg = ProcessRegistry::new();
        let t1 = Uuid::new_v4();
        let t2 = Uuid::new_v4();
        let mut tasks = Vec::new();
        for o in [owner(1, 1, Some(t1)), owner(1, 1, Some(t2)), owner(2, 1, None)] {
            let r = reg.clone();
            tasks.push(tokio::spawn(async move { r.run(sh("sleep 30", o, Duration::from_secs(60))).await }));
        }
        wait_len(&reg, 3).await;
        assert_eq!(reg.kill_track(1, 1, t1).await, 1);
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.kill_guild(1).await, 1);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.kill_all().await, 1);
        assert!(reg.is_empty());
        for t in tasks {
            assert!(matches!(t.await.unwrap(), Err(ProcError::Killed(_))));
        }
    }

    #[tokio::test]
    async fn dropping_the_future_kills_the_child() {
        let reg = ProcessRegistry::new();
        let fut = reg.run(sh("sleep 30", owner(3, 3, None), Duration::from_secs(60)));
        let _ = tokio::time::timeout(Duration::from_millis(200), fut).await;
        assert!(reg.is_empty());
    }

    #[tokio::test]
    async fn sweep_kills_orphans_and_old_processes() {
        let reg = ProcessRegistry::new();
        let mut tasks = Vec::new();
        for o in [owner(5, 1, None), owner(6, 1, None), ProcOwner::system()] {
            let r = reg.clone();
            tasks.push(tokio::spawn(async move { r.run(sh("sleep 30", o, Duration::from_secs(60))).await }));
        }
        wait_len(&reg, 3).await;
        // Guild 6 is gone; system owners are never considered orphaned.
        let n = reg.sweep_once(Duration::from_secs(3600), &|o: &ProcOwner| o.guild_id != 6).await;
        assert_eq!(n, 1);
        assert_eq!(reg.len(), 2);
        let n = reg.sweep_once(Duration::ZERO, &|_: &ProcOwner| true).await;
        assert_eq!(n, 2);
        assert!(reg.is_empty());
        for t in tasks {
            let _ = t.await;
        }
    }
}
