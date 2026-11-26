//! The state directory (`--state <dir>`): the peer's identity, its stores and its rail
//! history, so a peer stopped and started again is the same wallet.
//!
//! ```text
//! <dir>/lock         held by the running process (one paykit-peer per directory)
//! <dir>/peer.json    identity secret (hex, PLAIN TEXT), identity, App ID, origin, wallet
//! <dir>/signer.json  the signer's store
//! <dir>/app.json     the Paykit app's store
//! <dir>/rail.json    the rail's history: attempt ID -> txid
//! ```
//!
//! Test tool only: the identity secret and the app's keys are on disk in plain text, protected
//! only by file modes 0600 and a 0700 directory. Never use real keys here.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use serde::{Deserialize, Serialize};

/// Open (create, truncate) `path` for writing, readable by the owner only.
pub fn private_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

/// Write `bytes` to `path` atomically, readable by the owner only.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = private_file(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// A state directory, locked for this process while the value lives.
pub struct StateDir {
    dir: PathBuf,
    /// Holds the advisory lock; released when dropped or when the process ends.
    _lock: File,
}

impl StateDir {
    /// Create `dir` if needed (mode 0700) and lock it.
    ///
    /// # Errors
    ///
    /// When it can't be created, or another process holds its lock.
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join("lock"))
            .with_context(|| format!("could not open {}/lock", dir.display()))?;
        if lock.try_lock().is_err() {
            bail!(
                "{} is in use by another paykit-peer process; send the command to that \
                 process's stdin instead",
                dir.display()
            );
        }
        Ok(Self {
            dir: dir.to_owned(),
            _lock: lock,
        })
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The peer's settings, if it was set up.
    pub fn load_config(&self) -> anyhow::Result<Option<PeerConfig>> {
        match std::fs::read(self.path("peer.json")) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("peer.json is unreadable")?,
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("could not read peer.json"),
        }
    }

    pub fn save_config(&self, config: &PeerConfig) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec_pretty(config)?;
        write_private(&self.path("peer.json"), &bytes).context("could not write peer.json")
    }
}

/// What `peer.json` holds.
#[derive(Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    /// The 32-byte identity secret, hex. Sensitive, plain text: a test shortcut.
    pub identity_secret: String,
    /// The identity's Pubky ID (z-base-32), for reading without the secret.
    pub identity: String,
    /// The Dispatch origin the signer enrolled at.
    pub origin: String,
    /// The bitcoind wallet the rail uses.
    pub wallet: String,
    /// The Paykit App ID (hex), once the app joined.
    pub app_id: Option<String>,
}

impl std::fmt::Debug for PeerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerConfig")
            .field("identity", &self.identity)
            .field("origin", &self.origin)
            .field("wallet", &self.wallet)
            .field("app_id", &self.app_id)
            .finish_non_exhaustive()
    }
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Bytes of lowercase or uppercase hex, exactly `N` of them.
pub fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}
