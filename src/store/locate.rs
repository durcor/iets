//! Where the local store keeps an input source.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use crate::error::{self, Error as CrateError};
use crate::nixbin;
use crate::remote::path_info::PathInfo;
use crate::session::Session;
use crate::store::path::{StorePath, store_dir};
use crate::store::source::{SourceKind, SourceOrigin, SrcPath};

/// A native source origin, or a registered store path whose identity and metadata
/// must survive copying even when it is not content-addressed.
#[derive(Clone, Debug)]
pub enum LocatedSource {
    Source(SourceOrigin),
    Stored { at: SrcPath, info: PathInfo },
}

#[derive(Default)]
struct Slot {
    prefetched: OnceLock<Option<BTreeSet<StorePath>>>,
    answer: OnceLock<BTreeSet<StorePath>>,
}

/// What the store answered for a path, kept for the life of the process: the
/// references of a valid store path never change.
static REFERENCES: Mutex<Option<HashMap<String, Arc<Slot>>>> = Mutex::new(None);

fn slot(full: &str) -> (Arc<Slot>, bool) {
    let mut kept = REFERENCES.lock().expect("references cache");
    let table = kept.get_or_insert_with(HashMap::new);
    match table.entry(full.to_string()) {
        Entry::Occupied(e) => (e.get().clone(), false),
        Entry::Vacant(e) => (e.insert(Arc::default()).clone(), true),
    }
}

/// The references the local store holds for `path`, empty when it cannot
/// answer. primops.cc:addPath needs them for a store path it is given with a
/// context, and swallows `queryPathInfo`'s error the same way.
pub fn references_of(path: &StorePath) -> BTreeSet<StorePath> {
    let full = path.to_string();
    let (s, _) = slot(&full);
    // a failed prefetch queries again
    s.answer
        .get_or_init(|| {
            (s.prefetched.get_or_init(|| None).clone())
                .or_else(|| query_references(&full))
                .unwrap_or_default()
        })
        .clone()
}

const MAX_PREFETCH: usize = 8;

/// Starts `references_of(path)` on a thread.
pub fn prefetch_references(path: StorePath, session: Arc<Session>) {
    static STARTED: AtomicUsize = AtomicUsize::new(0);
    if STARTED.load(Ordering::Relaxed) >= MAX_PREFETCH {
        return;
    }
    let full = path.to_string();
    let (s, new) = slot(&full);
    if new && STARTED.fetch_add(1, Ordering::Relaxed) < MAX_PREFETCH {
        thread::spawn(move || {
            let _entered = session.enter();
            s.prefetched.get_or_init(|| query_references(&full));
        });
    }
}

fn query_references(full: &str) -> Option<BTreeSet<StorePath>> {
    let out = Command::new(nixbin::nix())
        .args([
            "--extra-experimental-features",
            "nix-command",
            "path-info",
            "--json",
            full,
        ])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let info: HashMap<String, serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    info.get(full)?["references"]
        .as_array()?
        .iter()
        .map(|r| StorePath::parse(r.as_str()?).ok())
        .collect()
}

/// Only an unrelocated local namespace can authenticate bytes read directly
/// below `store_dir()`. A configured root/real/store parameter may point its
/// registration database at different physical bytes; leave those sources to
/// the native cold path instead. Never copy arbitrary settings (or secrets)
/// into a metadata subprocess's arguments.
fn local_metadata_store(session: &Session) -> bool {
    let cfg = session.nix_conf.get_or_init(|| {
        crate::config::Config::read(&[]).unwrap_or_else(|_| crate::config::Config::empty())
    });
    let store = cfg.store();
    matches!(store.as_str(), "auto" | "daemon" | "local" | "")
        || (!store.contains('?') && store.starts_with("unix://"))
}

/// Answers the plan's locator from the same local filesystem store as its bytes.
/// The session's effective selection must permit an unrelocated local store.
/// `--source` overrides remain ordinary native sources. Only genuinely
/// unavailable paths (including unsupported store namespaces) are `None`.
pub fn locate_in_store(
    paths: &[StorePath],
    overrides: &HashMap<String, PathBuf>,
    session: &Session,
) -> error::Result<Vec<Option<LocatedSource>>> {
    let local_store = local_metadata_store(session);
    let ask: Vec<String> = paths
        .iter()
        .map(|p| p.to_string())
        .filter(|p| local_store && !overrides.contains_key(p))
        .collect();
    let mut info: HashMap<String, serde_json::Value> = HashMap::new();
    if !ask.is_empty() {
        let out = Command::new(nixbin::nix())
            // Explicit CLI selection outranks both nix.conf and NIX_REMOTE.
            // `local` uses the inherited NIX_STORE_DIR/NIX_STATE_DIR namespace,
            // exactly where the filesystem origin below is read. In particular,
            // do not forward a relocated store's URL to authenticate these bytes.
            .args(["--store", "local"])
            .args([
                "--extra-experimental-features",
                "nix-command",
                "path-info",
                "--json",
            ])
            .args(&ask)
            .stderr(Stdio::inherit())
            .output()
            .map_err(|e| CrateError::io("nix path-info", e))?;
        info = serde_json::from_slice(&out.stdout)
            .map_err(|e| CrateError::Protocol(format!("nix path-info: {e}")))?;
        // Some Nix versions return nonzero for InvalidPath while still reporting
        // a complete JSON answer with null entries. Only that structured absence
        // is substitution, not a failed command or malformed metadata.
        let unavailable = ask.iter().all(|p| info.contains_key(p))
            && ask.iter().any(|p| info.get(p).is_some_and(|v| v.is_null()));
        if !out.status.success() && !unavailable {
            return Err(CrateError::Protocol("nix path-info failed".into()));
        }
    }
    paths
        .iter()
        .map(|p| {
            let full = p.to_string();
            if let Some(local) = overrides.get(&full) {
                return Ok(Some(LocatedSource::Source(SourceOrigin {
                    at: SrcPath::fs(local.clone(), None),
                    kind: SourceKind::Source,
                    nar: None,
                })));
            }
            if !local_store {
                return Ok(None);
            }
            // `null` is what `nix path-info` prints for a path that is not valid.
            let v = info
                .get(&full)
                .ok_or_else(|| CrateError::Protocol(format!("nix path-info omitted '{p}'")))?;
            if v.is_null() {
                return Ok(None);
            }
            Ok(Some(LocatedSource::Stored {
                at: SrcPath::fs(format!("{}/{}", store_dir(), p.base_name()), None),
                info: PathInfo::from_json(p.clone(), v)?,
            }))
        })
        .collect()
}
