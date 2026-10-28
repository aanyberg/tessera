//! The development window stays out of the app: neither `tessera-ffi` (what
//! the macOS app links) nor `tessera-app-core` may depend on it.

use std::collections::{HashSet, VecDeque};
use std::process::Command;

use serde_json::Value;

#[test]
fn ffi_and_app_core_do_not_depend_on_the_devwin() {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("metadata is JSON");

    let reachable_from_ffi = linked_closure(&metadata, "tessera-ffi");
    let reachable_from_app_core = linked_closure(&metadata, "tessera-app-core");
    let devwin = package_id(&metadata, "tessera-devwin");
    let winit = package_id(&metadata, "winit");
    // The walk is real: the app links the core the window drives.
    assert!(reachable_from_ffi.contains(&package_id(&metadata, "tessera-app-core")));
    // And the window does link what it is checked against.
    let reachable_from_devwin = linked_closure(&metadata, "tessera-devwin");
    assert!(reachable_from_devwin.contains(&package_id(&metadata, "tessera-app-core")));
    assert!(reachable_from_devwin.contains(&winit));
    // tessera-devwin is a binary-only crate, which no package can depend
    // on, so the devwin assertion below can never fail today; it guards
    // against the crate growing a library. winit is the real guard: it is
    // what the window would drag into the app.
    for (name, reachable) in [
        ("tessera-ffi", &reachable_from_ffi),
        ("tessera-app-core", &reachable_from_app_core),
    ] {
        assert!(!reachable.contains(&devwin), "{name} links tessera-devwin");
        assert!(!reachable.contains(&winit), "{name} links winit");
    }
}

/// The id of the package called `name` (the only one by that name).
fn package_id(metadata: &Value, name: &str) -> String {
    let mut ids = metadata["packages"]
        .as_array()
        .expect("packages is an array")
        .iter()
        .filter(|package| package["name"] == name)
        .filter_map(|package| package["id"].as_str());
    let id = ids
        .next()
        .unwrap_or_else(|| panic!("{name} is a package"))
        .to_owned();
    assert!(ids.next().is_none(), "{name} resolves to one version");
    id
}

/// Every package linked into `root`: the transitive closure over normal and
/// build dependencies. Dev-dependencies are not followed because they never
/// end up in the artifact.
fn linked_closure(metadata: &Value, root: &str) -> HashSet<String> {
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .expect("resolve.nodes is an array");
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([package_id(metadata, root)]);
    while let Some(id) = queue.pop_front() {
        let node = nodes
            .iter()
            .find(|node| node["id"] == id.as_str())
            .unwrap_or_else(|| panic!("{id} is in the resolve graph"));
        let deps = node["deps"].as_array().expect("deps is an array");
        for dep in deps {
            let linked = dep["dep_kinds"]
                .as_array()
                .expect("dep_kinds is an array")
                .iter()
                .any(|kind| kind["kind"].is_null() || kind["kind"] == "build");
            let pkg = dep["pkg"].as_str().expect("pkg is a string").to_owned();
            if linked && seen.insert(pkg.clone()) {
                queue.push_back(pkg);
            }
        }
    }
    seen
}
