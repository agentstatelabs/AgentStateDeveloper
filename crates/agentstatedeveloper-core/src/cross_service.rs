//! Cross-service / cross-process edges (Plan competitive-harvest t-002).
//!
//! Call edges link two symbols by qname *within one repo*. Cross-service edges
//! are different: a client call (`requests.post("/charge")`) in one repo and a
//! route handler (`@app.post("/charge")`) in another can't be linked by qname —
//! they're linked by a shared **contract**: the HTTP method+path, the pub-sub
//! topic name. We therefore keep `symbol_id` repo-scoped and unchanged, and add
//! a separate contract-keyed layer:
//!
//!   - Each repo detects the endpoints it *exposes* (inbound: routes/listeners)
//!     and *consumes* (outbound: client calls/emits), normalizes each to a
//!     [`ServiceEndpoint`] with a stable `contract` key and a `repo_id`.
//!   - A repo exports its endpoints as a [`ServiceManifest`]; other repos import
//!     manifests to match contracts cross-repo (federated — no shared DB, no
//!     global symbol identity).
//!   - [`match_edges`] pairs outbound→inbound endpoints sharing a contract key
//!     into [`CrossServiceEdge`]s. Matching is by string equality on the
//!     normalized contract, so the normalizers below are load-bearing: a client
//!     and a server must phrase the same endpoint identically.
//!
//! HTTP and pub-sub share this machinery. Data-flow edges are a distinct
//! intra-process mechanism (arg→param / field chains) tracked separately.

use serde::{Deserialize, Serialize};

/// Transport a service endpoint communicates over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Http,
    PubSub,
    /// Intra-process data-flow (distinct mechanism; see module docs).
    DataFlow,
}

/// Whether an endpoint exposes/handles a contract (inbound) or consumes/calls
/// it (outbound).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Handles/serves/listens — the producer side (e.g. a route handler).
    Inbound,
    /// Calls/emits — the consumer side (e.g. an HTTP client call).
    Outbound,
}

/// A detected service-boundary point owned by a local symbol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceEndpoint {
    pub transport: Transport,
    pub direction: Direction,
    /// Normalized contract key, e.g. `"http:POST /users/{}"` or
    /// `"topic:payments.charged"`. Built via [`http_contract`] / [`pubsub_contract`].
    pub contract: String,
    pub repo_id: String,
    pub symbol_id: String,
    pub qname: String,
    pub file: String,
    pub line: u32,
    /// Detection confidence in `[0, 1]`: a literal route/url is high; a
    /// templated or dynamically-built target is lower.
    pub confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A lightweight reference to one end of a cross-service edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndpointRef {
    pub repo_id: String,
    pub symbol_id: String,
    pub qname: String,
    pub file: String,
    pub line: u32,
}

impl From<&ServiceEndpoint> for EndpointRef {
    fn from(e: &ServiceEndpoint) -> Self {
        EndpointRef {
            repo_id: e.repo_id.clone(),
            symbol_id: e.symbol_id.clone(),
            qname: e.qname.clone(),
            file: e.file.clone(),
            line: e.line,
        }
    }
}

/// A matched cross-service edge: an outbound endpoint linked to an inbound
/// endpoint sharing a contract key. Computed on demand, not materialized.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossServiceEdge {
    pub transport: Transport,
    pub contract: String,
    /// The consumer (outbound) side.
    pub from: EndpointRef,
    /// The producer (inbound) side.
    pub to: EndpointRef,
    /// Edge confidence — the weaker of the two endpoint detections.
    pub confidence: f64,
    /// True when the two ends live in different repos.
    pub cross_repo: bool,
}

/// A repo's exported endpoints — the unit shared for cross-repo matching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceManifest {
    pub repo_id: String,
    pub endpoints: Vec<ServiceEndpoint>,
}

/// An endpoint detected by a language adapter, before the index pipeline
/// enriches it with this repo's `repo_id` and the owning symbol's `symbol_id`.
/// Adapters work in qnames, so a [`DetectedEndpoint`] names its owner by qname.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedEndpoint {
    pub transport: Transport,
    pub direction: Direction,
    pub contract: String,
    /// qname of the symbol that owns this endpoint — the route handler, or the
    /// function containing the client call.
    pub owner_qname: String,
    pub file: String,
    pub line: u32,
    pub confidence: f64,
    pub note: Option<String>,
}

impl DetectedEndpoint {
    /// Promote to a full [`ServiceEndpoint`] once repo + symbol identity resolve.
    pub fn into_endpoint(self, repo_id: &str, symbol_id: &str) -> ServiceEndpoint {
        ServiceEndpoint {
            transport: self.transport,
            direction: self.direction,
            contract: self.contract,
            repo_id: repo_id.to_string(),
            symbol_id: symbol_id.to_string(),
            qname: self.owner_qname,
            file: self.file,
            line: self.line,
            confidence: self.confidence,
            note: self.note,
        }
    }
}

// ---------------------------------------------------------------------------
// Contract normalization
// ---------------------------------------------------------------------------

/// Build a normalized HTTP contract key from a method and a path (or full URL).
/// A client call and a server route for the same endpoint must produce the same
/// string: the method is upper-cased, any scheme+host is stripped, path
/// parameters (`{id}`, `:id`, `<id>`, `<int:id>`) collapse to `{}`, query and
/// fragment are dropped, and trailing slashes are removed.
///
/// ```
/// use agentstatedeveloper_core::cross_service::http_contract;
/// assert_eq!(http_contract("post", "/Users/{id}/"), "http:POST /users/{}");
/// assert_eq!(http_contract("GET", "https://api.svc/users/:id?x=1"), "http:GET /users/{}");
/// ```
pub fn http_contract(method: &str, path_or_url: &str) -> String {
    format!(
        "http:{} {}",
        method.trim().to_uppercase(),
        normalize_http_path(path_or_url)
    )
}

fn normalize_http_path(raw: &str) -> String {
    // Strip scheme+host if a full URL was given.
    let after_host = match raw.find("://") {
        Some(i) => {
            let rest = &raw[i + 3..];
            match rest.find('/') {
                Some(p) => &rest[p..],
                None => "/",
            }
        }
        None => raw,
    };
    // Drop query/fragment.
    let path = after_host
        .split(['?', '#'])
        .next()
        .unwrap_or(after_host)
        .trim()
        .trim_end_matches('/');

    let segs: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|seg| {
            if is_path_param(seg) {
                "{}".to_string()
            } else {
                seg.to_lowercase()
            }
        })
        .collect();

    if segs.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segs.join("/"))
    }
}

/// Does a path segment denote a parameter? Covers the common framework spellings:
/// `{id}` (OpenAPI/Spring), `:id` (Express/Rails), `<id>` / `<int:id>` (Flask),
/// `${id}` (template literals).
fn is_path_param(seg: &str) -> bool {
    (seg.starts_with('{') && seg.ends_with('}'))
        || (seg.starts_with("${") && seg.ends_with('}'))
        || seg.starts_with(':')
        || (seg.starts_with('<') && seg.ends_with('>'))
}

/// Build a normalized pub-sub contract key from a topic / queue / event name.
pub fn pubsub_contract(topic: &str) -> String {
    format!("topic:{}", topic.trim().to_lowercase())
}

/// Stable 16-hex-char hash of a contract key, for use as a filesystem-/path-safe
/// segment (raw contract keys contain spaces, `/`, and `:`).
pub fn contract_hash(contract: &str) -> String {
    blake3::hash(contract.as_bytes()).to_hex()[..16].to_string()
}

// ---------------------------------------------------------------------------
// repo_id resolution
// ---------------------------------------------------------------------------

/// Normalize a git remote URL into a stable `repo_id`. SCP-form and HTTPS-form
/// URLs for the same repo collapse to one id:
///
/// ```
/// use agentstatedeveloper_core::cross_service::normalize_repo_id;
/// assert_eq!(normalize_repo_id("git@github.com:Org/Repo.git"), "github.com/org/repo");
/// assert_eq!(normalize_repo_id("https://github.com/Org/Repo.git"), "github.com/org/repo");
/// ```
pub fn normalize_repo_id(remote_url: &str) -> String {
    let mut s = remote_url.trim().to_string();

    // Strip scheme://
    if let Some(i) = s.find("://") {
        s = s[i + 3..].to_string();
    }
    // Strip userinfo (`git@`, `user@`) that precedes the host.
    if let Some(at) = s.find('@') {
        let host_boundary = s.find('/').unwrap_or(s.len());
        if at < host_boundary {
            s = s[at + 1..].to_string();
        }
    }
    // SCP form `host:path` → `host/path`, but leave a `:port` alone.
    if let Some(colon) = s.find(':') {
        let next_is_digit = s[colon + 1..]
            .chars()
            .next()
            .map_or(false, |c| c.is_ascii_digit());
        if !next_is_digit {
            s.replace_range(colon..colon + 1, "/");
        }
    }
    // Trim trailing slash + `.git`.
    let s = s.trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    s.trim_end_matches('/').to_lowercase()
}

/// Resolve this repo's id: an explicit override wins; otherwise the normalized
/// `origin` git remote; otherwise the working-directory name as a last resort.
pub fn resolve_repo_id(override_id: Option<&str>, cwd: &std::path::Path) -> String {
    if let Some(id) = override_id {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    if let Some(url) = git_origin_url(cwd) {
        let norm = normalize_repo_id(&url);
        if !norm.is_empty() {
            return norm;
        }
    }
    cwd.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_lowercase())
        .unwrap_or_else(|| "unknown-repo".to_string())
}

fn git_origin_url(cwd: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if url.is_empty() { None } else { Some(url) }
}

/// Flatten the on-disk endpoint registry tree
/// (`contract_hash → repo_id → symbol_id → ServiceEndpoint`) into a flat list.
/// Malformed entries are skipped.
pub fn endpoints_from_tree(tree: &serde_json::Value) -> Vec<ServiceEndpoint> {
    let mut out = Vec::new();
    let Some(by_contract) = tree.as_object() else {
        return out;
    };
    for by_repo in by_contract.values() {
        let Some(by_repo) = by_repo.as_object() else {
            continue;
        };
        for by_sym in by_repo.values() {
            let Some(by_sym) = by_sym.as_object() else {
                continue;
            };
            for ep in by_sym.values() {
                if let Ok(e) = serde_json::from_value::<ServiceEndpoint>(ep.clone()) {
                    out.push(e);
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

/// Pair outbound endpoints with inbound endpoints sharing a contract key,
/// producing cross-service edges. Endpoints may come from any repo (local +
/// imported manifests). Edge confidence is the weaker of the two detections;
/// `cross_repo` is set when the ends live in different repos.
pub fn match_edges(endpoints: &[ServiceEndpoint]) -> Vec<CrossServiceEdge> {
    use std::collections::HashMap;

    let mut inbound_by_contract: HashMap<&str, Vec<&ServiceEndpoint>> = HashMap::new();
    for e in endpoints
        .iter()
        .filter(|e| e.direction == Direction::Inbound)
    {
        inbound_by_contract
            .entry(e.contract.as_str())
            .or_default()
            .push(e);
    }

    let mut edges = Vec::new();
    for out in endpoints
        .iter()
        .filter(|e| e.direction == Direction::Outbound)
    {
        let Some(ins) = inbound_by_contract.get(out.contract.as_str()) else {
            continue;
        };
        for inb in ins {
            // The contract prefix already encodes the transport, but guard anyway.
            if inb.transport != out.transport {
                continue;
            }
            edges.push(CrossServiceEdge {
                transport: out.transport,
                contract: out.contract.clone(),
                from: EndpointRef::from(out),
                to: EndpointRef::from(*inb),
                confidence: out.confidence.min(inb.confidence),
                cross_repo: out.repo_id != inb.repo_id,
            });
        }
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- HTTP contract normalization ------------------------------------

    #[test]
    fn http_method_uppercased_and_path_lowercased() {
        assert_eq!(http_contract("post", "/Charge"), "http:POST /charge");
    }

    #[test]
    fn http_path_params_collapse_across_framework_spellings() {
        let want = "http:GET /users/{}/posts/{}";
        assert_eq!(http_contract("get", "/users/{id}/posts/{postId}"), want);
        assert_eq!(http_contract("get", "/users/:id/posts/:postId"), want);
        assert_eq!(http_contract("get", "/users/<id>/posts/<int:postId>"), want);
    }

    #[test]
    fn http_client_url_and_server_route_match() {
        // A client calling a full URL must phrase the same contract as the
        // server's path-only route.
        let client = http_contract("GET", "https://payments.svc/users/:id?expand=1#frag");
        let server = http_contract("get", "/users/{id}");
        assert_eq!(client, server);
        assert_eq!(server, "http:GET /users/{}");
    }

    #[test]
    fn http_trailing_slash_and_root_normalized() {
        assert_eq!(http_contract("GET", "/charge/"), "http:GET /charge");
        assert_eq!(http_contract("GET", "/"), "http:GET /");
        assert_eq!(http_contract("GET", ""), "http:GET /");
    }

    #[test]
    fn pubsub_contract_normalizes_case() {
        assert_eq!(
            pubsub_contract("  Payments.Charged "),
            "topic:payments.charged"
        );
    }

    #[test]
    fn contract_hash_is_stable_and_path_safe() {
        let c = http_contract("POST", "/charge");
        let h = contract_hash(&c);
        assert_eq!(h, contract_hash(&c), "hash must be deterministic");
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    // --- repo_id ---------------------------------------------------------

    #[test]
    fn repo_id_scp_and_https_collapse() {
        assert_eq!(
            normalize_repo_id("git@github.com:Org/Repo.git"),
            "github.com/org/repo"
        );
        assert_eq!(
            normalize_repo_id("https://github.com/Org/Repo.git"),
            "github.com/org/repo"
        );
        assert_eq!(
            normalize_repo_id("https://github.com/Org/Repo"),
            "github.com/org/repo"
        );
    }

    #[test]
    fn repo_id_strips_userinfo_keeps_port() {
        // A `:port` is preserved (only an SCP `host:path` colon converts to `/`),
        // so the two URL forms for the same host stay distinct from a portless one.
        assert_eq!(
            normalize_repo_id("ssh://git@host.example:22/team/svc.git"),
            "host.example:22/team/svc"
        );
        assert_eq!(
            normalize_repo_id("https://user@gitlab.com/team/svc.git"),
            "gitlab.com/team/svc"
        );
    }

    #[test]
    fn repo_id_override_wins_and_blank_ignored() {
        let cwd = std::path::Path::new("/tmp/whatever");
        assert_eq!(resolve_repo_id(Some("payments-svc"), cwd), "payments-svc");
        // Blank override falls through (here to the dir name, since /tmp/whatever has no git origin).
        assert_eq!(resolve_repo_id(Some("   "), cwd), "whatever");
    }

    // --- matching --------------------------------------------------------

    fn ep(dir: Direction, contract: &str, repo: &str, conf: f64) -> ServiceEndpoint {
        ServiceEndpoint {
            transport: Transport::Http,
            direction: dir,
            contract: contract.to_string(),
            repo_id: repo.to_string(),
            symbol_id: format!("sym_{repo}_{dir:?}"),
            qname: "q".into(),
            file: "f".into(),
            line: 1,
            confidence: conf,
            note: None,
        }
    }

    #[test]
    fn match_pairs_outbound_to_inbound_on_contract() {
        let c = http_contract("POST", "/charge");
        let eps = vec![
            ep(Direction::Outbound, &c, "billing", 0.9),
            ep(Direction::Inbound, &c, "payments", 0.8),
        ];
        let edges = match_edges(&eps);
        assert_eq!(edges.len(), 1);
        let e = &edges[0];
        assert_eq!(e.from.repo_id, "billing");
        assert_eq!(e.to.repo_id, "payments");
        assert!(e.cross_repo);
        // Confidence is the weaker of the two detections.
        assert_eq!(e.confidence, 0.8);
    }

    #[test]
    fn match_marks_same_repo_edges_not_cross_repo() {
        let c = http_contract("GET", "/health");
        let eps = vec![
            ep(Direction::Outbound, &c, "svc", 1.0),
            ep(Direction::Inbound, &c, "svc", 1.0),
        ];
        let edges = match_edges(&eps);
        assert_eq!(edges.len(), 1);
        assert!(!edges[0].cross_repo);
    }

    #[test]
    fn no_edge_without_a_matching_inbound() {
        let c = http_contract("POST", "/charge");
        let edges = match_edges(&[ep(Direction::Outbound, &c, "billing", 0.9)]);
        assert!(edges.is_empty());
    }

    #[test]
    fn different_contracts_do_not_match() {
        let eps = vec![
            ep(
                Direction::Outbound,
                &http_contract("POST", "/charge"),
                "a",
                1.0,
            ),
            ep(
                Direction::Inbound,
                &http_contract("POST", "/refund"),
                "b",
                1.0,
            ),
        ];
        assert!(match_edges(&eps).is_empty());
    }

    #[test]
    fn one_inbound_matches_many_outbound_consumers() {
        let c = http_contract("GET", "/users/{id}");
        let eps = vec![
            ep(Direction::Inbound, &c, "users", 0.9),
            ep(Direction::Outbound, &c, "web", 0.7),
            ep(Direction::Outbound, &c, "mobile", 0.6),
        ];
        let edges = match_edges(&eps);
        assert_eq!(edges.len(), 2, "both consumers link to the one handler");
    }

    #[test]
    fn endpoints_from_tree_flattens_registry() {
        // Mirror the on-disk shape: contract_hash → repo_id → symbol_id → endpoint.
        let e = ep(
            Direction::Inbound,
            &http_contract("POST", "/charge"),
            "pay",
            0.9,
        );
        let tree = serde_json::json!({
            contract_hash(&e.contract): { "pay": { "sym_1": serde_json::to_value(&e).unwrap() } }
        });
        let got = endpoints_from_tree(&tree);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].contract, "http:POST /charge");
        // Malformed/empty trees yield nothing rather than panicking.
        assert!(endpoints_from_tree(&serde_json::Value::Null).is_empty());
        assert!(endpoints_from_tree(&serde_json::json!({"h": {"r": {"s": 42}}})).is_empty());
    }

    #[test]
    fn manifest_round_trips() {
        let m = ServiceManifest {
            repo_id: "payments".into(),
            endpoints: vec![ep(
                Direction::Inbound,
                &http_contract("POST", "/charge"),
                "payments",
                0.9,
            )],
        };
        let back: ServiceManifest =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back.repo_id, "payments");
        assert_eq!(back.endpoints.len(), 1);
        assert_eq!(back.endpoints[0].contract, "http:POST /charge");
    }
}
