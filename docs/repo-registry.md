# ASD Repo Registry — `~/.config/asd/repos.toml`

Shared registry of indexed repos used by the `asd` CLI, `asd-mcp`, and CTXone.
Lets a user (or a long-running process) switch which repo is "active" without
restarting anything.

## File location

- Default: `$XDG_CONFIG_HOME/asd/repos.toml`, falling back to
  `~/.config/asd/repos.toml` on every platform (we don't follow the
  platform-specific `dirs::config_dir()` on macOS — keep the path identical
  across Linux and macOS so docs and shell snippets work everywhere).
- Override via `ASD_REGISTRY` env var (absolute path to the TOML file).
- Parent directory is created lazily on first write. A missing file is
  equivalent to an empty registry — readers MUST NOT error on absence.

## Schema (v1)

```toml
# ~/.config/asd/repos.toml
version = 1

[active]
repo = "myapp"            # name of the currently active repo, or omitted/empty

[repos.myapp]
path = "/Users/user/code/myapp/.asd-state.db"
registered_at = "2026-05-12T23:44:00Z"

[repos.sdk]
path = "/Users/user/code/sdk/.asd-state.db"
registered_at = "2026-05-14T10:00:00Z"
```

Field rules:

- `version` — integer, currently `1`. Future-incompatible changes bump this;
  unknown versions must be rejected with a clear error rather than silently
  re-saved (which would clobber a newer client's data).
- `[active].repo` — optional. If absent, empty string, or names a repo not in
  `[repos]`, treat as "no active repo".
- `[repos.<name>]` — table keyed by repo name. Name is the registry key, not a
  field inside the table.
  - `path` — required. Absolute path to the `.asd-state.db` file. Relative
    paths and `~` MUST be rejected on write; readers tolerate them by
    canonicalizing against `$HOME`.
  - `registered_at` — optional RFC 3339 UTC timestamp. Informational only;
    consumers must not depend on it for ordering.
- Unknown fields under `[repos.<name>]` or `[active]` are preserved on
  read-modify-write so a newer client can add fields without older clients
  stripping them.

### Name rules

- 1–64 characters.
- Allowed: `[A-Za-z0-9_-]`. No dots, slashes, or whitespace.
- Case-sensitive. `myapp` and `MyApp` are distinct.
- Reserved: the empty string and the literal `default` are not allowed as
  names (they would collide with "no active repo" semantics and CLI defaults
  respectively).

## Atomic write

Every write is `write-temp + fsync + rename`:

1. Serialize the full registry to TOML.
2. Write to `repos.toml.tmp.<pid>.<nanos>` in the same directory.
3. `fsync` the temp file.
4. `rename` over `repos.toml` (atomic on POSIX same-filesystem).
5. `fsync` the directory (best-effort; ignore EPERM/ENOTSUP).

No locking. The registry is small (well under 4 KiB for realistic workloads)
and last-writer-wins is acceptable because the only mutable state is the
active-repo pointer and the per-repo path. Two clients racing to register the
same name converge on identical content; two clients racing to set the active
repo converge on whichever rename happened last, which is the user-visible
intent in both cases.

## Read path & mtime-based cache invalidation

Long-running consumers (`asd-mcp`, the CTXone process pool) cache the parsed
registry to avoid hitting the filesystem on every tool call. Invalidation is
mtime-driven:

```text
cache = { mtime: SystemTime, parsed: Registry }

on each call:
    stat = fs::metadata(path)
    if stat.is_err(): cache = empty registry; return cached
    if stat.mtime() == cache.mtime: return cached.parsed
    re-read + parse; update cache.mtime; return parsed
```

Rules:

- The cache key is the file's mtime as reported by `fs::metadata`. Do NOT use
  ctime (changes on chmod and confuses macOS) or size (a same-size edit slips
  through).
- A `stat` failure (file missing, permission denied) yields an empty
  registry. Treat this as "registry was reset" — the consumer should drop any
  cached active-repo state and require the user to set it again.
- After a successful re-read, log at `info`:
  `registry: reloaded, active = Some("sdk")` or `… active = None`.
- The mtime check itself must be cheap — one `stat`. Do NOT call `fs::read`
  on every tool invocation.
- Resolution: macOS APFS gives 1 ns mtime; Linux ext4 gives 1 ns; older FUSE
  mounts may give 1 s. The atomic rename above guarantees that any successful
  write changes the mtime, so 1 s resolution is sufficient — writes within
  the same second are still serialized by the rename, and the *last* one wins
  in both the file and the next reader's cache.

## Public API (Rust, `agentstatedeveloper-core::registry`)

```rust
pub struct Registry { /* opaque */ }

pub struct RepoEntry {
    pub name: String,
    pub path: PathBuf,
    pub registered_at: Option<OffsetDateTime>,
}

impl Registry {
    /// Read from the default path. Missing file -> empty registry.
    pub fn load() -> Result<Self>;

    /// Read from an explicit path. Missing file -> empty registry.
    pub fn load_from(path: &Path) -> Result<Self>;

    /// Atomic write to the default path.
    pub fn save(&self) -> Result<()>;
    pub fn save_to(&self, path: &Path) -> Result<()>;

    pub fn list(&self) -> Vec<&RepoEntry>;
    pub fn get(&self, name: &str) -> Option<&RepoEntry>;

    pub fn active(&self) -> Option<&RepoEntry>;
    pub fn set_active(&mut self, name: &str) -> Result<()>;
    pub fn clear_active(&mut self);

    pub fn register(&mut self, name: &str, path: &Path) -> Result<()>;
    pub fn remove(&mut self, name: &str) -> Result<()>;

    /// For consumers that want mtime-cached reads.
    pub fn path() -> PathBuf;       // resolved default path
}
```

Errors must be a typed `RegistryError` enum (not `anyhow::Error`) so callers
can distinguish "no such repo", "invalid name", "io", and "parse" cases —
the CLI surfaces these as distinct exit codes.

## Consumer expectations

- **`asd` CLI** — reads/writes directly via `Registry::load` / `save`.
  Subcommands: `asd repo add|list|use|rm|show`.
- **`asd-mcp`** — reads on startup; mtime-checks on every tool call; reopens
  the SQLite DB when the active repo changes.
- **CTXone** — reads on startup to seed the `AsdProcessPool` (one entry per
  registered repo). mtime-checks during pool maintenance to pick up newly
  added repos without restart. CTXone itself does NOT write to the registry —
  registration is a CLI/user action.

## Open questions (defer to v2)

- Per-user vs per-machine registries. Today's scope is per-user.
- Optional `[repos.<name>].label` for a human display name in Lens —
  forwards-compatible (unknown field, preserved on round-trip), add when Lens
  needs it.
- Soft-delete vs hard-delete for `asd repo rm` — currently hard.
