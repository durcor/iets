//! Registered source metadata and bytes, kept in the same store namespace.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use crate::error::{self, Error as CrateError};
use crate::fetch::spool;
use crate::nixbin;
use crate::remote::path_info::PathInfo;
use crate::remote::worker::{Connection, Transport};
use crate::session::Session;
use crate::store::hash::{Hash, HashAlgo};
use crate::store::path::{IngestionMethod, StorePath, store_dir};
use crate::store::source::{SourceKind, SourceOrigin, SrcPath};
use crate::treestore::{self, Rejected, StoredTree};

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

/// Whether the effective selection permits querying the actual local namespace.
/// Unix/default-daemon selections do not prove where that daemon keeps bytes:
/// they permit only the independently pinned `--store local` query below.
/// Parameterized/remote selections decline metadata adoption altogether. Never
/// copy arbitrary settings (or secrets) into the subprocess's arguments.
fn local_metadata_store(session: &Session) -> bool {
    let cfg = session.nix_conf.get_or_init(|| {
        crate::config::Config::read(&[]).unwrap_or_else(|_| crate::config::Config::empty())
    });
    let store = cfg.store();
    matches!(store.as_str(), "auto" | "daemon" | "local" | "")
        || (!store.contains('?') && store.starts_with("unix://"))
}

/// Actual-local metadata is optional on a multiuser installation: Nix requires
/// write access even for queries. Never open a live WAL database as immutable.
pub(crate) fn local_metadata_available() -> io::Result<bool> {
    let state = std::env::var_os("NIX_STATE_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| "/nix/var/nix".into());
    let db = state.join("db/db.sqlite");
    match fs::OpenOptions::new().read(true).write(true).open(&db) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => fs::File::open(db).map(|_| false),
        Err(e) => Err(e),
    }
}

/// Metadata-only authority for actual local filesystem bytes. This is not a
/// general source locator: an arbitrary worker cannot authenticate these bytes.
pub(crate) fn local_path_info(
    paths: &[StorePath],
    session: &Session,
) -> error::Result<Vec<Option<PathInfo>>> {
    if !local_metadata_store(session) {
        return Ok(vec![None; paths.len()]);
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let ask: Vec<String> = paths.iter().map(StorePath::to_string).collect();
    let out = Command::new(nixbin::nix())
        // Explicit selection outranks ambient configuration and NIX_REMOTE.
        .args([
            "--store",
            "local",
            "--extra-experimental-features",
            "nix-command",
            "path-info",
            "--json",
        ])
        .args(&ask)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| CrateError::io("nix path-info", e))?;
    let info: HashMap<String, serde_json::Value> = serde_json::from_slice(&out.stdout)
        .map_err(|e| CrateError::Protocol(format!("nix path-info: {e}")))?;
    let unavailable = ask.iter().all(|p| info.contains_key(p))
        && ask.iter().any(|p| info.get(p).is_some_and(|v| v.is_null()));
    if !out.status.success() && !unavailable {
        return Err(CrateError::Protocol("nix path-info failed".into()));
    }
    paths
        .iter()
        .map(|p| {
            let v = info
                .get(&p.to_string())
                .ok_or_else(|| CrateError::Protocol(format!("nix path-info omitted '{p}'")))?;
            if v.is_null() {
                Ok(None)
            } else {
                PathInfo::from_json(p.clone(), v).map(Some)
            }
        })
        .collect()
}

/// Retain provider bytes once in the existing verified content cache. The cache
/// key describes only the NAR, never the original object's IA/CA identity or
/// references; callers retain its complete PathInfo separately for registration.
pub(crate) fn cache_nar(
    conn: &mut Connection,
    info: &PathInfo,
) -> error::Result<(StorePath, Arc<StoredTree>)> {
    let hash = Hash::parse_any(&info.nar_hash, Some(HashAlgo::Sha256))?;
    if info.nar_size == 0 {
        return Err(CrateError::Protocol(format!(
            "'{}' has an empty registered NAR",
            info.path
        )));
    }
    let key = treestore::store_path("source", &hash)?;
    let _fetching = treestore::fetch_lock(key.base_name(), || {});
    let entry = if let Some(entry) = treestore::reopen(key.base_name()) {
        entry
    } else {
        let staged = treestore::stage(key.base_name()).ok_or_else(|| {
            CrateError::Protocol("iets has no cache directory to hold provider bytes".into())
        })?;
        let mut file = spool::spool_file(staged.dir())
            .map_err(|e| CrateError::io("staging provider NAR", e))?;
        let mut root = None;
        conn.nar_from_path(&info.path.to_string(), &mut |_| {}, &mut |r| {
            let (node, size) = spool::index_nar(&mut r.take(info.nar_size), &mut file)?;
            if size != info.nar_size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "'{}' NAR size mismatch: expected {}, got {size}",
                        info.path, info.nar_size
                    ),
                ));
            }
            root = Some(node);
            Ok(())
        })?;
        let tree = spool::Tree::spooled(file, root.expect("successful NAR callback"));
        treestore::put("source", IngestionMethod::Recursive, &hash, staged, tree).map_err(|e| {
            match e {
                Rejected::Mismatch(actual) => CrateError::Protocol(format!(
                    "NAR hash mismatch for '{}': expected '{}', got '{}'",
                    info.path,
                    hash.to_sri(),
                    actual.to_sri()
                )),
                Rejected::Io(e) => CrateError::io("keeping provider NAR", e),
                Rejected::Unstorable => CrateError::Protocol(
                    "iets cannot keep provider bytes in the input store".into(),
                ),
            }
        })?
    };
    if entry.nar_hash() != hash || entry.nar_size() != info.nar_size {
        return Err(CrateError::Protocol(format!(
            "'{}' has inconsistent canonical NAR metadata",
            info.path
        )));
    }
    Ok((key, entry))
}

/// Locate unknown registered sources for every upload shape. Worker metadata
/// and bytes always come from one connection; verified cache accessors can be
/// replayed by worker/gRPC writers and retries without contacting that provider.
pub fn locate_in_store(
    paths: &[StorePath],
    overrides: &HashMap<String, PathBuf>,
    session: &Session,
) -> error::Result<Vec<Option<LocatedSource>>> {
    let cfg = session.nix_conf.get_or_init(|| {
        crate::config::Config::read(&[]).unwrap_or_else(|_| crate::config::Config::empty())
    });
    let unknown: Vec<StorePath> = paths
        .iter()
        .filter(|p| !overrides.contains_key(&p.to_string()))
        .cloned()
        .collect();
    let mut found = HashMap::new();
    if !unknown.is_empty() {
        let transport = Transport::parse(&cfg.store());
        let opened = transport
            .as_ref()
            .filter(|t| !matches!(t, Transport::Grpc(_)))
            .map(|t| Connection::open(t, &mut |_| {}))
            .transpose();
        match opened {
            Ok(Some(mut conn)) => {
                for p in &unknown {
                    let located = match conn.query_path_info(&p.to_string(), &mut |_| {})? {
                        None => None,
                        Some(info) => {
                            if info.path != *p {
                                return Err(CrateError::Protocol(format!(
                                    "provider returned '{}' for '{p}'",
                                    info.path
                                )));
                            }
                            let (key, _) = cache_nar(&mut conn, &info)?;
                            Some(LocatedSource::Stored {
                                at: SrcPath::spool(key.base_name(), b""),
                                info,
                            })
                        }
                    };
                    found.insert(p.clone(), located);
                }
                // A reachable worker may use another namespace. Its absence
                // does not invalidate a genuinely registered actual-local
                // custom output; only that local registration can bind its FS.
                let absent: Vec<_> = unknown.iter()
                    .filter(|p| found.get(*p).is_some_and(Option::is_none))
                    .cloned().collect();
                if !absent.is_empty() && local_metadata_store(session)
                    && local_metadata_available().map_err(|e| CrateError::io("opening local registration metadata", e))?
                {
                    for (p, info) in absent.iter().zip(local_path_info(&absent, session)?) {
                        found.insert(p.clone(), info.map(|info| LocatedSource::Stored {
                            at: SrcPath::fs(format!("{}/{}", store_dir(), p.base_name()), None), info,
                        }));
                    }
                }
            }
            opened => {
                if local_metadata_store(session)
                    && local_metadata_available()
                        .map_err(|e| CrateError::io("opening local registration metadata", e))?
                {
                    for (p, info) in unknown.iter().zip(local_path_info(&unknown, session)?) {
                        found.insert(
                            p.clone(),
                            info.map(|info| LocatedSource::Stored {
                                at: SrcPath::fs(format!("{}/{}", store_dir(), p.base_name()), None),
                                info,
                            }),
                        );
                    }
                } else {
                    opened?;
                }
            }
        }
    }
    Ok(paths
        .iter()
        .map(|p| {
            if let Some(local) = overrides.get(&p.to_string()) {
                Some(LocatedSource::Source(SourceOrigin {
                    at: SrcPath::fs(local.clone(), None),
                    kind: SourceKind::Source,
                    nar: None,
                }))
            } else {
                found.get(p).cloned().flatten()
            }
        })
        .collect())
}
