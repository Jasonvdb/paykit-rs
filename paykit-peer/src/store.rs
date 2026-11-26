//! The host store: the client's in-memory store rules, optionally written to one file after
//! every commit, so a peer stopped and started again keeps its identity, its signer and its
//! app.
//!
//! Test tool only. It meets the `StateStore` guarantees well enough for one local process:
//!
//! - atomic and durable: a commit is applied to a copy, the copy is written to a temporary file
//!   that is synced and renamed over the store's file, and only then does the commit return
//!   `Ok`; a failed write leaves the store as it was;
//! - exclusive ownership holds within this process; across processes, the state directory's
//!   lock ([`crate::state::StateDir`]) keeps a second peer process out;
//! - the file is NOT encrypted at rest. It holds the app's keys and private Paykit records, so
//!   it is written with mode 0600 in a 0700 directory. Never point it at real keys.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pubky_dispatch_client::store::{
    check_owned_batch, AcquireOutcome, CommitOutcome, KeyValue, StateStore, StoreError, StoreLease,
    StoreOwner, Write, WriteBatch,
};
use pubky_dispatch_core::enc::{b64, unb64};
use serde::{Deserialize, Serialize};

type Entries = BTreeMap<String, BTreeMap<Vec<u8>, Vec<u8>>>;

/// A store in memory, written to `path` after every commit when it has one.
pub struct FileStore {
    path: Option<PathBuf>,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    entries: Entries,
    owners: BTreeMap<String, StoreOwner>,
}

/// The file's form: every namespace, with keys and values in base64url.
#[derive(Default, Serialize, Deserialize)]
struct Snapshot {
    namespaces: BTreeMap<String, BTreeMap<String, String>>,
}

impl FileStore {
    /// A store that lives in memory only.
    pub fn in_memory() -> Self {
        Self {
            path: None,
            state: Arc::default(),
        }
    }

    /// The store kept in `path`, read back when the file exists.
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let mut entries = Entries::new();
        match std::fs::read(&path) {
            Ok(bytes) => {
                let snapshot: Snapshot = serde_json::from_slice(&bytes).map_err(|error| {
                    anyhow::anyhow!("{} is unreadable: {error}", path.display())
                })?;
                for (ns, keys) in snapshot.namespaces {
                    let mut decoded = BTreeMap::new();
                    for (key, value) in keys {
                        decoded.insert(
                            unb64(&key).map_err(|_| anyhow::anyhow!("a key in {ns} is corrupt"))?,
                            unb64(&value)
                                .map_err(|_| anyhow::anyhow!("a value in {ns} is corrupt"))?,
                        );
                    }
                    entries.insert(ns, decoded);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => anyhow::bail!("could not read {}: {error}", path.display()),
        }
        Ok(Self {
            path: Some(path),
            state: Arc::new(Mutex::new(State {
                entries,
                owners: BTreeMap::new(),
            })),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Apply `batch` to a copy, write the copy to the file, then keep it. Nothing changes when
    /// the write fails.
    fn apply(&self, state: &mut State, batch: WriteBatch) -> Result<(), StoreError> {
        let mut next = state.entries.clone();
        for write in batch.into_writes() {
            match write {
                Write::Put { ns, key, value } => {
                    next.entry(ns).or_default().insert(key, value);
                }
                Write::Delete { ns, key } => {
                    if let Some(keys) = next.get_mut(&ns) {
                        keys.remove(&key);
                    }
                }
                Write::DeletePrefix { ns, prefix } => {
                    if let Some(keys) = next.get_mut(&ns) {
                        keys.retain(|key, _| !key.starts_with(&prefix));
                    }
                }
            }
        }
        next.retain(|_, keys| !keys.is_empty());
        if let Some(path) = &self.path {
            write_snapshot(path, &next)?;
        }
        state.entries = next;
        Ok(())
    }
}

/// Write `entries` to `path` atomically: a synced temporary file renamed over it. Any failure
/// before the rename leaves the old file, so the commit is refused as not applied.
fn write_snapshot(path: &PathBuf, entries: &Entries) -> Result<(), StoreError> {
    let snapshot = Snapshot {
        namespaces: entries
            .iter()
            .map(|(ns, keys)| {
                (
                    ns.clone(),
                    keys.iter().map(|(k, v)| (b64(k), b64(v))).collect(),
                )
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&snapshot).map_err(|_| StoreError::Unavailable)?;
    let tmp = path.with_extension("tmp");
    let written = (|| -> std::io::Result<()> {
        let mut file = crate::state::private_file(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    written.map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        StoreError::Unavailable
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Lease {
    state: Arc<Mutex<State>>,
    namespace: String,
    owner: StoreOwner,
}

impl StoreLease for Lease {}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = lock(&self.state);
        if state.owners.get(&self.namespace) == Some(&self.owner) {
            state.owners.remove(&self.namespace);
        }
    }
}

impl std::fmt::Debug for FileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileStore({} namespaces)", self.lock().entries.len())
    }
}

impl StateStore for FileStore {
    fn get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self
            .lock()
            .entries
            .get(ns)
            .and_then(|keys| keys.get(key))
            .cloned())
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<KeyValue>, StoreError> {
        let state = self.lock();
        let Some(keys) = state.entries.get(ns) else {
            return Ok(Vec::new());
        };
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        Ok(keys
            .range(prefix.to_vec()..)
            .take_while(|(key, _)| key.starts_with(prefix))
            .filter(|(key, _)| after.is_none_or(|after| key.as_slice() > after))
            .take(limit)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect())
    }

    fn commit(&self, batch: WriteBatch) -> Result<(), StoreError> {
        let mut state = self.lock();
        if batch
            .writes()
            .iter()
            .any(|write| state.owners.contains_key(write.ns()))
        {
            return Err(StoreError::InUse);
        }
        self.apply(&mut state, batch)
    }

    fn acquire(&self, ns: &str, owner: StoreOwner) -> Result<AcquireOutcome, StoreError> {
        let mut state = self.lock();
        if state.owners.contains_key(ns) {
            return Ok(AcquireOutcome::Busy);
        }
        state.owners.insert(ns.to_owned(), owner);
        Ok(AcquireOutcome::Acquired(Arc::new(Lease {
            state: Arc::clone(&self.state),
            namespace: ns.to_owned(),
            owner,
        })))
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: StoreOwner,
        batch: WriteBatch,
    ) -> Result<CommitOutcome, StoreError> {
        check_owned_batch(ns, &batch)?;
        let mut state = self.lock();
        if state.owners.get(ns) != Some(&owner) {
            return Ok(CommitOutcome::NotOwner);
        }
        self.apply(&mut state, batch)?;
        Ok(CommitOutcome::Applied)
    }
}
