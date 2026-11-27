use crate::error::KitError;
use crate::skills::SkillDiscoveryConfig;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DEBOUNCE_WINDOW: Duration = Duration::from_millis(50);
const MAX_DEBOUNCE_WAIT: Duration = Duration::from_millis(200);

#[derive(Default)]
struct WatcherSignal {
    dirty: bool,
    last_event: Option<Instant>,
    stop: bool,
}

/// Cross-platform filesystem watcher for configured skill directories.
///
/// Tracks existing skill roots (following symlinks with cycle protection) and the nearest
/// existing ancestor of roots that do not yet exist, coalescing burst events before firing
/// the registered callback. Dropping the handle stops watching and joins its worker thread.
pub struct SkillWatcher {
    signal: Arc<(Mutex<WatcherSignal>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for SkillWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillWatcher")
            .field("active", &self.worker.is_some())
            .finish()
    }
}

impl SkillWatcher {
    /// Start watching all roots in `config`, invoking `on_change` after debounced filesystem events.
    pub fn start(
        config: SkillDiscoveryConfig,
        on_change: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, KitError> {
        let signal = Arc::new((Mutex::new(WatcherSignal::default()), Condvar::new()));
        let event_signal = Arc::clone(&signal);
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            if res.is_ok() {
                let (lock, cvar) = &*event_signal;
                let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                if !state.stop {
                    state.dirty = true;
                    state.last_event = Some(Instant::now());
                    cvar.notify_one();
                }
            }
        })
        .map_err(|err| KitError::Io(std::io::Error::other(err.to_string())))?;

        let mut watched = HashSet::new();
        sync_watches(&mut watcher, &mut watched, &config);

        let worker_signal = Arc::clone(&signal);
        let worker = thread::Builder::new()
            .name("jimu-skill-watcher".into())
            .spawn(move || {
                let mut watcher = watcher;
                let (lock, cvar) = &*worker_signal;

                loop {
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while !state.dirty && !state.stop {
                        state = cvar.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                    if state.stop {
                        break;
                    }

                    let batch_start = Instant::now();
                    loop {
                        if state.stop {
                            return;
                        }
                        let since_last = state
                            .last_event
                            .map(|t| t.elapsed())
                            .unwrap_or(DEBOUNCE_WINDOW);
                        if since_last >= DEBOUNCE_WINDOW
                            || batch_start.elapsed() >= MAX_DEBOUNCE_WAIT
                        {
                            break;
                        }
                        let wait = DEBOUNCE_WINDOW
                            .saturating_sub(since_last)
                            .min(MAX_DEBOUNCE_WAIT.saturating_sub(batch_start.elapsed()));
                        let (next, _) = cvar
                            .wait_timeout(state, wait)
                            .unwrap_or_else(|e| e.into_inner());
                        state = next;
                    }
                    if state.stop {
                        break;
                    }
                    state.dirty = false;
                    state.last_event = None;
                    drop(state);

                    sync_watches(&mut watcher, &mut watched, &config);
                    on_change();
                }
            })
            .map_err(KitError::Io)?;

        Ok(Self {
            signal,
            worker: Some(worker),
        })
    }
}

impl Drop for SkillWatcher {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.signal;
        {
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            state.stop = true;
            cvar.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn sync_watches(
    watcher: &mut RecommendedWatcher,
    watched: &mut HashSet<PathBuf>,
    config: &SkillDiscoveryConfig,
) {
    let desired = collect_watch_dirs(config);
    let stale: Vec<PathBuf> = watched.difference(&desired).cloned().collect();
    for path in stale {
        let _ = watcher.unwatch(&path);
        watched.remove(&path);
    }
    for path in desired {
        if !watched.contains(&path) && watcher.watch(&path, RecursiveMode::NonRecursive).is_ok() {
            watched.insert(path);
        }
    }
}

fn is_symlink_or_junction(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt as _;
        let ft = meta.file_type();
        ft.is_symlink() || ft.is_symlink_dir()
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

fn collect_watch_dirs(config: &SkillDiscoveryConfig) -> HashSet<PathBuf> {
    let mut dirs = HashSet::new();
    let mut ancestry = HashSet::new();
    for root in config.roots() {
        if root.is_dir() {
            // If the root itself is a symlink (or Windows NTFS junction), also watch its parent
            // so retargeting or removing the link emits an event.
            if fs::symlink_metadata(&root).is_ok_and(|meta| is_symlink_or_junction(&meta))
                && let Some(parent) = root.parent()
                && let Ok(canonical_parent) = fs::canonicalize(parent)
            {
                dirs.insert(canonical_parent);
            }
            scan_watch_dir(&root, &mut dirs, &mut ancestry);
        } else if root.is_file() {
            if let Some(parent) = root.parent()
                && let Ok(canonical_parent) = fs::canonicalize(parent)
            {
                dirs.insert(canonical_parent);
            }
        } else if let Some(ancestor) = root.ancestors().skip(1).find(|p| p.is_dir())
            && let Ok(canonical_ancestor) = fs::canonicalize(ancestor)
        {
            dirs.insert(canonical_ancestor);
        }
    }
    dirs
}

fn scan_watch_dir(dir: &Path, dirs: &mut HashSet<PathBuf>, ancestry: &mut HashSet<PathBuf>) {
    let Ok(canonical) = fs::canonicalize(dir) else {
        return;
    };
    if !ancestry.insert(canonical.clone()) {
        return;
    }
    dirs.insert(canonical.clone());

    if dir.join("SKILL.md").is_file() {
        ancestry.remove(&canonical);
        return;
    }

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            if path.is_dir() {
                scan_watch_dir(&path, dirs, ancestry);
            }
        }
    }

    ancestry.remove(&canonical);
}
