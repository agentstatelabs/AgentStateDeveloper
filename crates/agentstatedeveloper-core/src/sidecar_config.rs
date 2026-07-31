//! Plan K t-007: `.asd/config.toml` reader for opt-in sidecar
//! sharding. Today a single shape: per-package sharding via
//! `[sidecar] shard_by = "package"`. Default is "class" (one file
//! per conclusion class, current behavior).
//!
//! The file is intentionally tiny — most projects need no config.
//! For projects where two teams edit the same conclusion class
//! (e.g. both add Decisions in the same week), per-package sharding
//! splits the file by `package_dir(file)` so independent inserts
//! land in different shards and don't touch the same JSONL lines.
//!
//! Format:
//!
//! ```toml
//! [sidecar]
//! shard_by = "package"   # or "class" (default)
//! ```
//!
//! Schema is intentionally forgiving — unknown keys and missing
//! files both fall through to defaults. We don't want a typo in
//! config to break the commit hook.

use serde::Deserialize;
use std::path::Path;

/// How `asd conclusions export` lays out files under `.asd/conclusions/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShardBy {
    /// One file per ConclusionClass: `.asd/conclusions/<stem>.jsonl`.
    /// Default. Right for solo-dev and small-team projects.
    #[default]
    Class,
    /// One file per (ConclusionClass, package_dir): `.asd/conclusions/
    /// <stem>/<package-key>.jsonl`. Opt-in for monorepos where two
    /// teams concurrently editing the same class would textually
    /// conflict in the single-file layout.
    Package,
}

/// Plan K t-008: size budget for the committed sidecar. Defaults
/// chosen for AcmeFlow-scale projects: 1 MB total, 200 KB per
/// shard file. Configurable per-project via `.asd/config.toml`.
#[derive(Debug, Clone, Copy)]
pub struct BudgetConfig {
    /// Hard upper bound on the total size of `.asd/conclusions/`
    /// (sum across all shard files). Default 1 MiB.
    pub total_bytes: u64,
    /// Hard upper bound on any single shard file's size. Default
    /// 200 KiB.
    pub per_shard_bytes: u64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            total_bytes: 1024 * 1024,
            per_shard_bytes: 200 * 1024,
        }
    }
}

/// Top-level sidecar config.
#[derive(Debug, Clone, Default)]
pub struct SidecarConfig {
    pub shard_by: ShardBy,
    pub budget: BudgetConfig,
}

#[derive(Debug, Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    sidecar: Option<SidecarSection>,
}

#[derive(Debug, Deserialize, Default)]
struct SidecarSection {
    /// `"class"` (default) or `"package"`. Anything else falls back
    /// to the default with a logged warning.
    #[serde(default)]
    shard_by: Option<String>,
    /// Plan K t-008. Bytes for the whole `.asd/conclusions/` tree.
    /// Omitted or invalid → default (1 MiB).
    #[serde(default)]
    budget_total_bytes: Option<u64>,
    /// Plan K t-008. Bytes for any individual shard file. Omitted
    /// or invalid → default (200 KiB).
    #[serde(default)]
    budget_per_shard_bytes: Option<u64>,
}

impl SidecarConfig {
    /// Read `<project_root>/.asd/config.toml` if present. Missing
    /// file, parse errors, or unknown values all fall through to
    /// defaults — config is advisory, not load-bearing.
    pub fn load_from_project(project_root: &Path) -> Self {
        let path = project_root.join(".asd").join("config.toml");
        Self::load_from_file(&path)
    }

    /// Lower-level: read an explicit config path. Test-friendly.
    pub fn load_from_file(path: &Path) -> Self {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => return Self::default(),
        };
        let parsed: ConfigFile = match toml::from_str(&raw) {
            Ok(p) => p,
            Err(_) => return Self::default(),
        };
        let sidecar = parsed.sidecar.unwrap_or_default();
        let shard_by = sidecar
            .shard_by
            .as_deref()
            .map(|s| match s {
                "package" => ShardBy::Package,
                "class" => ShardBy::Class,
                _ => ShardBy::Class, // unknown → safe default
            })
            .unwrap_or_default();
        let default_budget = BudgetConfig::default();
        let budget = BudgetConfig {
            total_bytes: sidecar
                .budget_total_bytes
                .unwrap_or(default_budget.total_bytes),
            per_shard_bytes: sidecar
                .budget_per_shard_bytes
                .unwrap_or(default_budget.per_shard_bytes),
        };
        Self { shard_by, budget }
    }
}

/// Sanitize a package directory path (`crates/foo/src`) into a
/// filename-safe key (`crates--foo--src`). Empty string (root-
/// level file with no parent directory) becomes `"_root"`.
///
/// Plan K t-007: stable transformation so the same package always
/// produces the same shard filename across runs and machines.
pub fn package_key_for_filename(package_dir: &str) -> String {
    if package_dir.is_empty() {
        return "_root".to_string();
    }
    package_dir.replace('/', "--").replace('\\', "--")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn defaults_when_no_config_file() {
        let tmp = tempdir().unwrap();
        let cfg = SidecarConfig::load_from_project(tmp.path());
        assert_eq!(cfg.shard_by, ShardBy::Class);
    }

    #[test]
    fn reads_shard_by_package() {
        let tmp = tempdir().unwrap();
        let asd = tmp.path().join(".asd");
        std::fs::create_dir_all(&asd).unwrap();
        std::fs::write(
            asd.join("config.toml"),
            "[sidecar]\nshard_by = \"package\"\n",
        )
        .unwrap();
        let cfg = SidecarConfig::load_from_project(tmp.path());
        assert_eq!(cfg.shard_by, ShardBy::Package);
    }

    #[test]
    fn reads_shard_by_class_explicitly() {
        let tmp = tempdir().unwrap();
        let asd = tmp.path().join(".asd");
        std::fs::create_dir_all(&asd).unwrap();
        std::fs::write(asd.join("config.toml"), "[sidecar]\nshard_by = \"class\"\n").unwrap();
        let cfg = SidecarConfig::load_from_project(tmp.path());
        assert_eq!(cfg.shard_by, ShardBy::Class);
    }

    #[test]
    fn unknown_value_falls_back_to_default() {
        let tmp = tempdir().unwrap();
        let asd = tmp.path().join(".asd");
        std::fs::create_dir_all(&asd).unwrap();
        std::fs::write(
            asd.join("config.toml"),
            "[sidecar]\nshard_by = \"banana\"\n",
        )
        .unwrap();
        let cfg = SidecarConfig::load_from_project(tmp.path());
        assert_eq!(
            cfg.shard_by,
            ShardBy::Class,
            "unknown shard_by value must fall back to default"
        );
    }

    #[test]
    fn malformed_toml_falls_back_to_default() {
        let tmp = tempdir().unwrap();
        let asd = tmp.path().join(".asd");
        std::fs::create_dir_all(&asd).unwrap();
        std::fs::write(asd.join("config.toml"), "this is not toml {{{").unwrap();
        let cfg = SidecarConfig::load_from_project(tmp.path());
        assert_eq!(cfg.shard_by, ShardBy::Class);
    }

    #[test]
    fn reads_budget_overrides() {
        let tmp = tempdir().unwrap();
        let asd = tmp.path().join(".asd");
        std::fs::create_dir_all(&asd).unwrap();
        std::fs::write(
            asd.join("config.toml"),
            "[sidecar]\nbudget_total_bytes = 5000\nbudget_per_shard_bytes = 1000\n",
        )
        .unwrap();
        let cfg = SidecarConfig::load_from_project(tmp.path());
        assert_eq!(cfg.budget.total_bytes, 5000);
        assert_eq!(cfg.budget.per_shard_bytes, 1000);
    }

    #[test]
    fn budget_defaults_when_not_specified() {
        let tmp = tempdir().unwrap();
        let cfg = SidecarConfig::load_from_project(tmp.path());
        assert_eq!(cfg.budget.total_bytes, 1024 * 1024);
        assert_eq!(cfg.budget.per_shard_bytes, 200 * 1024);
    }

    #[test]
    fn package_key_sanitizes_slashes() {
        assert_eq!(
            package_key_for_filename("crates/foo/src"),
            "crates--foo--src"
        );
        assert_eq!(package_key_for_filename("src"), "src");
        assert_eq!(package_key_for_filename(""), "_root");
    }
}
