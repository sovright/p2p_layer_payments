//! The bridge's durable state: one JSON file, written atomically, guarded by a lock file.
//!
//! A request_id is written before the invoice request goes out, so a lost answer or a crash
//! is always retried with the same request_id. Nothing here deletes files: a write replaces
//! the state file by renaming a fully written and synced temporary file over it.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use paid_services_types::{hex_array, hex_bytes};

/// The state file format version.
pub const STATE_FORMAT: u32 = 1;

/// Where a purchase stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// request_id recorded; no verified invoice yet.
    Requested,
    /// A verified invoice is stored.
    Invoiced,
    /// The invoice or the provider's answer failed a check. Never shown as a payment request.
    Rejected,
    /// A verified grant is stored but not yet installed on the node.
    Granted,
    /// The node accepted the grant (in this prototype, a mock install).
    Installed,
    /// The provider reported the purchase as expired.
    Expired,
}

impl Stage {
    /// Whether the purchase still needs work.
    pub fn is_open(self) -> bool {
        matches!(
            self,
            Stage::Requested | Stage::Invoiced | Stage::Granted | Stage::Installed
        )
    }

    /// The stage's name as written in the state file.
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Requested => "requested",
            Stage::Invoiced => "invoiced",
            Stage::Rejected => "rejected",
            Stage::Granted => "granted",
            Stage::Installed => "installed",
            Stage::Expired => "expired",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// The heights of an installed grant.
pub struct Install {
    /// First height of access.
    pub start_height: u32,
    /// Exclusive end height.
    pub end_height: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// One purchase in the state file.
pub struct Purchase {
    /// The request_id (hex), written before the invoice request goes out.
    pub request_id: String,
    /// The service bought.
    pub service: String,
    /// Where the purchase stands.
    pub stage: Stage,
    /// The provider's purchase id (hex).
    #[serde(default)]
    pub purchase_id: Option<String>,
    /// Signed invoice bytes, hex, exactly as received.
    #[serde(default)]
    pub invoice: Option<String>,
    /// The provider's last reported status.
    #[serde(default)]
    pub last_status: Option<String>,
    /// Signed grant bytes, hex, exactly as received.
    #[serde(default)]
    pub grant: Option<String>,
    /// The installed grant's heights.
    #[serde(default)]
    pub install: Option<Install>,
    /// A note about the last step.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// The whole state file.
pub struct StateFile {
    /// Format version.
    pub format: u32,
    /// Every purchase, oldest first.
    pub purchases: Vec<Purchase>,
}

impl Default for StateFile {
    fn default() -> Self {
        StateFile {
            format: STATE_FORMAT,
            purchases: Vec::new(),
        }
    }
}

/// Why the state could not be opened or saved.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// Another process holds the lock.
    #[error("another sidecar process holds the lock {}; nothing was changed", .0.display())]
    Locked(PathBuf),
    /// The state file is unreadable or inconsistent; it is left untouched.
    #[error("state file {} is unreadable ({reason}); it was left untouched and nothing was changed", path.display())]
    Corrupt {
        /// The state file.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// An I/O error.
    #[error("state file error: {0}")]
    Io(String),
}

/// The lock file next to a state file.
pub fn lock_path(state: &Path) -> PathBuf {
    sibling(state, ".lock")
}

fn sibling(state: &Path, suffix: &str) -> PathBuf {
    let mut name = state
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    state.with_file_name(name)
}

/// The open state file. The lock is held for as long as this value lives.
pub struct StateStore {
    path: PathBuf,
    _lock: File,
}

impl StateStore {
    /// Takes the lock and loads the state. Fails if another process holds the lock or the
    /// file is unreadable.
    pub fn open(path: &Path) -> Result<(Self, StateFile), StateError> {
        let lock_file_path = lock_path(path);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_file_path)
            .map_err(|e| StateError::Io(format!("{}: {e}", lock_file_path.display())))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(StateError::Locked(lock_file_path)),
            Err(TryLockError::Error(e)) => {
                return Err(StateError::Io(format!("{}: {e}", lock_file_path.display())))
            }
        }
        let state = load(path)?;
        Ok((
            StateStore {
                path: path.to_path_buf(),
                _lock: lock,
            },
            state,
        ))
    }

    /// The state file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes the whole state: temporary file, fsync, rename over the state file, fsync dir.
    pub fn save(&self, state: &StateFile) -> Result<(), StateError> {
        let io = |e: std::io::Error| StateError::Io(format!("{}: {e}", self.path.display()));
        let tmp = sibling(&self.path, ".tmp");
        let mut bytes =
            serde_json::to_vec_pretty(state).map_err(|e| StateError::Io(e.to_string()))?;
        bytes.push(b'\n');
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .map_err(io)?;
        f.write_all(&bytes).map_err(io)?;
        f.sync_all().map_err(io)?;
        drop(f);
        fs::rename(&tmp, &self.path).map_err(io)?;
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            File::open(dir).and_then(|d| d.sync_all()).map_err(io)?;
        }
        Ok(())
    }
}

fn load(path: &Path) -> Result<StateFile, StateError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(StateFile::default()),
        Err(e) => return Err(StateError::Io(format!("{}: {e}", path.display()))),
    };
    let corrupt = |reason: String| StateError::Corrupt {
        path: path.to_path_buf(),
        reason,
    };
    let state: StateFile = serde_json::from_slice(&bytes)
        .map_err(|e| corrupt(format!("not valid state JSON: {e}")))?;
    if state.format != STATE_FORMAT {
        return Err(corrupt(format!("unknown format {}", state.format)));
    }
    for (i, p) in state.purchases.iter().enumerate() {
        validate(p).map_err(|e| corrupt(format!("purchase {i}: {e}")))?;
    }
    Ok(state)
}

fn validate(p: &Purchase) -> Result<(), String> {
    hex_array::<16>(&p.request_id).map_err(|e| format!("request_id: {e}"))?;
    if p.service.is_empty() {
        return Err("service is empty".into());
    }
    if let Some(pid) = &p.purchase_id {
        hex_array::<16>(pid).map_err(|e| format!("purchase_id: {e}"))?;
    }
    for (name, field) in [("invoice", &p.invoice), ("grant", &p.grant)] {
        if let Some(h) = field {
            hex_bytes(h).map_err(|e| format!("{name}: {e}"))?;
        }
    }
    let needs = |cond: bool, what: &str| {
        if cond {
            Ok(())
        } else {
            Err(format!("stage {} without {what}", p.stage.as_str()))
        }
    };
    match p.stage {
        Stage::Requested | Stage::Rejected => Ok(()),
        Stage::Invoiced => needs(p.invoice.is_some() && p.purchase_id.is_some(), "an invoice"),
        Stage::Granted => needs(p.invoice.is_some() && p.grant.is_some(), "a grant"),
        Stage::Installed => needs(
            p.invoice.is_some() && p.grant.is_some() && p.install.is_some(),
            "an installed grant",
        ),
        Stage::Expired => needs(p.purchase_id.is_some(), "a purchase_id"),
    }
}
