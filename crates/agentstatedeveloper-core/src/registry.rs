//! Shared ASD repo registry — `~/.config/asd/repos.toml`.
//!
//! Read/write API used by the `asd` CLI, `asd-mcp`, and CTXone. See
//! `docs/repo-registry.md` for the schema and atomic-write protocol.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Current schema version persisted in `repos.toml`.
pub const SCHEMA_VERSION: u32 = 1;

/// Env var that overrides the default registry path.
pub const ENV_REGISTRY: &str = "ASD_REGISTRY";

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(String),
    #[error("unknown registry version {0} (this build supports up to {SCHEMA_VERSION})")]
    UnknownVersion(u32),
    #[error("unknown repo \"{0}\"")]
    UnknownRepo(String),
    #[error("invalid repo name \"{0}\": must be 1-64 chars of [A-Za-z0-9_-], and not \"default\"")]
    InvalidName(String),
    #[error("repo path must be absolute (was \"{0}\")")]
    NonAbsolutePath(String),
    #[error("$HOME is not set; cannot resolve registry path")]
    NoHome,
}

pub type Result<T> = std::result::Result<T, RegistryError>;

/// One registered repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoEntry {
    pub name: String,
    pub path: PathBuf,
    pub registered_at: Option<DateTime<Utc>>,
}

/// Parsed registry. `BTreeMap` keeps disk order deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    active: Option<String>,
    repos: BTreeMap<String, RepoEntry>,
}

impl Registry {
    /// Read from the default path. A missing file yields an empty registry.
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        parse(&raw)
    }

    /// Atomic write to the default path.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let body = serialize(self);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = tmp_sibling(path);
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(body.as_bytes())?;
            f.sync_all()?;
        }
        // Atomic on POSIX same-filesystem; on error, best-effort cleanup of tmp.
        if let Err(e) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        // Best-effort dir fsync — ignore failures, they're informational at most.
        if let Some(parent) = path.parent() {
            if let Ok(d) = fs::File::open(parent) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    }

    pub fn list(&self) -> Vec<&RepoEntry> {
        self.repos.values().collect()
    }

    pub fn get(&self, name: &str) -> Option<&RepoEntry> {
        self.repos.get(name)
    }

    /// Active repo, if any. Returns `None` when the active pointer is unset or
    /// names a repo no longer registered.
    pub fn active(&self) -> Option<&RepoEntry> {
        self.active.as_ref().and_then(|n| self.repos.get(n))
    }

    /// Set the active repo. Errors if no such repo is registered.
    pub fn set_active(&mut self, name: &str) -> Result<()> {
        if !self.repos.contains_key(name) {
            return Err(RegistryError::UnknownRepo(name.to_string()));
        }
        self.active = Some(name.to_string());
        Ok(())
    }

    pub fn clear_active(&mut self) {
        self.active = None;
    }

    /// Register (or update) a repo. Absolute path required; name must pass
    /// the rules in the schema doc.
    pub fn register(&mut self, name: &str, path: &Path) -> Result<()> {
        validate_name(name)?;
        if !path.is_absolute() {
            return Err(RegistryError::NonAbsolutePath(
                path.display().to_string(),
            ));
        }
        let registered_at = self
            .repos
            .get(name)
            .and_then(|e| e.registered_at)
            .or_else(|| Some(now_utc()));
        self.repos.insert(
            name.to_string(),
            RepoEntry {
                name: name.to_string(),
                path: path.to_path_buf(),
                registered_at,
            },
        );
        Ok(())
    }

    /// Remove a repo. Clears the active pointer if it pointed at this name.
    pub fn remove(&mut self, name: &str) -> Result<()> {
        if self.repos.remove(name).is_none() {
            return Err(RegistryError::UnknownRepo(name.to_string()));
        }
        if self.active.as_deref() == Some(name) {
            self.active = None;
        }
        Ok(())
    }

    /// Resolved default registry path: `$ASD_REGISTRY` if set, else
    /// `$HOME/.config/asd/repos.toml`. Matches the spec — does NOT defer to
    /// platform-specific config dirs.
    pub fn path() -> Result<PathBuf> {
        if let Some(p) = std::env::var_os(ENV_REGISTRY) {
            return Ok(PathBuf::from(p));
        }
        let home = std::env::var_os("HOME").ok_or(RegistryError::NoHome)?;
        Ok(PathBuf::from(home).join(".config/asd/repos.toml"))
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// On-disk shape. Kept private so callers can't accidentally bypass invariants.
#[derive(Serialize, Deserialize, Default)]
struct DiskRoot {
    #[serde(default)]
    version: Option<u32>,
    #[serde(default)]
    active: Option<DiskActive>,
    #[serde(default)]
    repos: BTreeMap<String, DiskRepo>,
}

#[derive(Serialize, Deserialize, Default)]
struct DiskActive {
    #[serde(default)]
    repo: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct DiskRepo {
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    registered_at: Option<String>,
}

fn parse(raw: &str) -> Result<Registry> {
    let root: DiskRoot =
        toml::from_str(raw).map_err(|e| RegistryError::Parse(e.to_string()))?;
    let version = root.version.unwrap_or(SCHEMA_VERSION);
    if version > SCHEMA_VERSION {
        return Err(RegistryError::UnknownVersion(version));
    }
    let mut repos = BTreeMap::new();
    for (name, r) in root.repos.into_iter() {
        // Tolerate ~ and relative paths on read by canonicalizing against $HOME.
        let path = canonicalize_lenient(&r.path);
        let registered_at = r
            .registered_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        repos.insert(
            name.clone(),
            RepoEntry {
                name,
                path,
                registered_at,
            },
        );
    }
    let active = root
        .active
        .and_then(|a| a.repo)
        .filter(|s| !s.is_empty() && repos.contains_key(s));
    Ok(Registry { active, repos })
}

fn serialize(reg: &Registry) -> String {
    let root = DiskRoot {
        version: Some(SCHEMA_VERSION),
        active: Some(DiskActive {
            repo: reg.active.clone(),
        }),
        repos: reg
            .repos
            .iter()
            .map(|(n, e)| {
                (
                    n.clone(),
                    DiskRepo {
                        path: e.path.display().to_string(),
                        registered_at: e
                            .registered_at
                            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
                    },
                )
            })
            .collect(),
    };
    toml::to_string_pretty(&root).unwrap_or_else(|_| String::new())
}

fn validate_name(name: &str) -> Result<()> {
    let len = name.len();
    if !(1..=64).contains(&len) {
        return Err(RegistryError::InvalidName(name.to_string()));
    }
    if name == "default" {
        return Err(RegistryError::InvalidName(name.to_string()));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(RegistryError::InvalidName(name.to_string()));
    }
    Ok(())
}

fn canonicalize_lenient(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    if raw == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else if let Some(home) = std::env::var_os("HOME") {
        // Relative paths in a global registry are almost always a mistake;
        // resolve against $HOME so the resulting path is at least usable from
        // a deterministic root.
        PathBuf::from(home).join(p)
    } else {
        p
    }
}

fn tmp_sibling(path: &Path) -> PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{pid}.{nanos}"));
    PathBuf::from(tmp)
}

fn now_utc() -> DateTime<Utc> {
    // Truncate to seconds so an in-memory timestamp round-trips through TOML
    // (we serialize with `SecondsFormat::Secs`) without a sub-second mismatch.
    let now = chrono::Utc::now();
    DateTime::from_timestamp(now.timestamp(), 0).unwrap_or(now)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "asd-registry-test-{}-{}-{}.toml",
            std::process::id(),
            now_utc().timestamp_nanos_opt().unwrap_or(0),
            name
        ));
        p
    }

    #[test]
    fn missing_file_is_empty() {
        let p = temp_path("missing");
        let r = Registry::load_from(&p).unwrap();
        assert!(r.list().is_empty());
        assert!(r.active().is_none());
    }

    #[test]
    fn register_then_round_trip() {
        let p = temp_path("rt");
        let mut r = Registry::default();
        r.register("myapp", &PathBuf::from("/tmp/x/.asd-state.db"))
            .unwrap();
        r.register("sdk", &PathBuf::from("/tmp/y/.asd-state.db"))
            .unwrap();
        r.set_active("sdk").unwrap();
        r.save_to(&p).unwrap();

        let loaded = Registry::load_from(&p).unwrap();
        assert_eq!(loaded.list().len(), 2);
        assert_eq!(loaded.active().unwrap().name, "sdk");
        assert_eq!(
            loaded.get("myapp").unwrap().path,
            PathBuf::from("/tmp/x/.asd-state.db")
        );
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn set_active_unknown_errors() {
        let mut r = Registry::default();
        assert!(matches!(
            r.set_active("ghost"),
            Err(RegistryError::UnknownRepo(_))
        ));
    }

    #[test]
    fn remove_clears_active() {
        let mut r = Registry::default();
        r.register("a", &PathBuf::from("/tmp/a.db")).unwrap();
        r.set_active("a").unwrap();
        r.remove("a").unwrap();
        assert!(r.active().is_none());
    }

    #[test]
    fn rejects_non_absolute_path() {
        let mut r = Registry::default();
        assert!(matches!(
            r.register("a", &PathBuf::from("relative/path.db")),
            Err(RegistryError::NonAbsolutePath(_))
        ));
    }

    #[test]
    fn rejects_bad_names() {
        let mut r = Registry::default();
        for bad in ["", "default", "has space", "has/slash", "has.dot", &"x".repeat(65)] {
            assert!(
                matches!(
                    r.register(bad, &PathBuf::from("/tmp/x.db")),
                    Err(RegistryError::InvalidName(_))
                ),
                "expected InvalidName for {:?}",
                bad
            );
        }
    }

    #[test]
    fn unknown_future_version_rejected() {
        let toml = "version = 99\n";
        assert!(matches!(
            parse(toml),
            Err(RegistryError::UnknownVersion(99))
        ));
    }

    #[test]
    fn active_pointing_at_missing_repo_treated_as_none() {
        let toml = "version = 1\n[active]\nrepo = \"ghost\"\n";
        let r = parse(toml).unwrap();
        assert!(r.active().is_none());
    }

    #[test]
    fn registered_at_preserved_on_round_trip() {
        let p = temp_path("ts");
        let mut r = Registry::default();
        r.register("a", &PathBuf::from("/tmp/a.db")).unwrap();
        let t0 = r.get("a").unwrap().registered_at;
        assert!(t0.is_some());
        r.save_to(&p).unwrap();
        let loaded = Registry::load_from(&p).unwrap();
        assert_eq!(loaded.get("a").unwrap().registered_at, t0);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn re_register_keeps_original_timestamp() {
        let mut r = Registry::default();
        r.register("a", &PathBuf::from("/tmp/a.db")).unwrap();
        let t0 = r.get("a").unwrap().registered_at;
        r.register("a", &PathBuf::from("/tmp/a-moved.db")).unwrap();
        assert_eq!(r.get("a").unwrap().registered_at, t0);
        assert_eq!(r.get("a").unwrap().path, PathBuf::from("/tmp/a-moved.db"));
    }
}
