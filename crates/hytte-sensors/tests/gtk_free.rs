//! Pins hytte-sensors' GTK-free-by-construction guarantee (#1249): this leaf
//! exists so a future plugin (#1250, `hytte-plugin-stats`) can reuse the
//! same procfs/sysfs samplers `hytte-services::sensors` wraps, without
//! pulling in GTK, D-Bus, or the reactive registry the way linking
//! `hytte-services` itself would. #1249's own pin names `cargo tree
//! -p hytte-sensors -e normal` as the check; this test runs `cargo metadata`
//! itself and walks the resolved dependency graph, which is the same fact
//! `cargo tree -e normal` reports, pinned in a way `cargo test` reruns on
//! every build instead of only "verified by hand for this PR".
//!
//! A manifest-text scan (this file's first version, in review on #1258) was
//! rejected: the workspace declares GTK under **renamed aliases**
//! (`gtk = { package = "gtk4" }`, `adw = libadwaita`, `webkit = webkit6`,
//! root `Cargo.toml`), so a text scan for the real package names never
//! fires for `gtk.workspace = true` — the one spelling every GTK-linking
//! member in this tree actually writes — and it can't see a
//! `[target."cfg(unix)".dependencies]` table at all, since it only reads the
//! unqualified `[dependencies]` block. Walking the resolved graph is
//! alias-proof (`cargo metadata` reports real crate names regardless of how
//! a manifest renamed them) and target-proof (`cargo metadata` resolves
//! dependencies across every cfg target unless asked to filter one out, so
//! a `target."cfg(unix)"` table is an ordinary edge here).
//!
//! Only **normal**-kind edges are walked — `dep_kinds` entries with a null
//! `kind` — at every node, not just the root: a dev/build dependency never
//! lands in a consumer's link line, so `tests/gtk_free.rs`'s own
//! `serde_json` dev-dependency (used to parse `cargo metadata`'s output)
//! must never itself trip this pin, and doesn't.
//!
//! Falsification (all three verified for this fix round, each restored):
//! - `gtk = { workspace = true }` under `[dependencies]` → reds on `gtk4`.
//! - `zbus = { workspace = true }` under a
//!   `[target."cfg(unix)".dependencies]` table → reds on `zbus`, proving the
//!   per-target table is actually walked.
//! - `glib = "0.22"` under `[dependencies]` (the original PR's own
//!   falsification spelling) → reds on `glib`.

use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Command;

/// Real crate names (not this workspace's renamed aliases) that must never
/// be reachable from `hytte-sensors` through a normal dependency edge.
const FORBIDDEN: &[&str] = &[
    "gtk4",
    "gtk4-sys",
    "glib",
    "glib-sys",
    "gio",
    "gio-sys",
    "gdk4",
    "libadwaita",
    "webkit6",
    "zbus",
    "tokio",
];

#[test]
fn resolved_dependency_graph_has_no_gtk_or_hytte_dependency() {
    let manifest_path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version=1",
            "--locked",
            "--manifest-path",
        ])
        .arg(manifest_path)
        .output()
        .expect("failed to spawn `cargo metadata`");
    assert!(
        output.status.success(),
        "`cargo metadata` failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata did not print valid JSON");

    let packages = metadata["packages"]
        .as_array()
        .expect("metadata.packages must be an array");
    let name_by_id: HashMap<&str, &str> = packages
        .iter()
        .map(|pkg| {
            let id = pkg["id"].as_str().expect("package.id must be a string");
            let name = pkg["name"].as_str().expect("package.name must be a string");
            (id, name)
        })
        .collect();

    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .expect("metadata.resolve.nodes must be an array");
    let node_by_id: HashMap<&str, &Value> = nodes
        .iter()
        .map(|node| (node["id"].as_str().expect("node.id must be a string"), node))
        .collect();

    let root_id = metadata["resolve"]["root"]
        .as_str()
        .expect(
            "metadata.resolve.root must name hytte-sensors — was cargo metadata \
             invoked without --manifest-path pointing at this crate?",
        )
        .to_string();

    // BFS over normal-kind edges only, starting at hytte-sensors itself.
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    visited.insert(root_id.clone());
    queue.push_back(root_id.clone());

    while let Some(id) = queue.pop_front() {
        let Some(node) = node_by_id.get(id.as_str()) else {
            continue;
        };
        let deps = node["deps"].as_array().expect("node.deps must be an array");
        for dep in deps {
            let dep_kinds = dep["dep_kinds"]
                .as_array()
                .expect("dep.dep_kinds must be an array");
            let is_normal = dep_kinds.iter().any(|dk| dk["kind"].is_null());
            if !is_normal {
                continue;
            }
            let dep_id = dep["pkg"]
                .as_str()
                .expect("dep.pkg must be a string")
                .to_string();
            if visited.insert(dep_id.clone()) {
                queue.push_back(dep_id);
            }
        }
    }

    let mut reachable: Vec<&str> = visited
        .iter()
        .filter(|id| id.as_str() != root_id)
        .map(|id| {
            *name_by_id
                .get(id.as_str())
                .expect("every resolved id must have a matching package entry")
        })
        .collect();
    reachable.sort_unstable();
    reachable.dedup();

    for name in &reachable {
        assert!(
            !FORBIDDEN.contains(name),
            "hytte-sensors reaches `{name}` through a normal dependency edge \
             — this leaf must stay GTK-free (#1249). Reachable set: {reachable:?}"
        );
        assert!(
            *name == "hytte-sensors" || !name.starts_with("hytte-"),
            "hytte-sensors reaches `{name}` through a normal dependency edge \
             — this leaf must not depend on another hytte-* crate (#1249). \
             Reachable set: {reachable:?}"
        );
    }
}
