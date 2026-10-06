//! Opt-in evidence helper. Requires an explicit scratch config and a copied baseline.
use std::collections::BTreeMap;
use std::path::Path;

use neutronsync::config;
use neutronsync::driveapi::DriveApi;
use neutronsync::protoncli::Remote;

#[test]
#[ignore = "requires the signed-in sidecar and explicit scratch paths"]
fn compare_documents_with_copied_baseline() {
    let config_path = std::env::var("NEUTRONSYNC_PHASE1_CONFIG").expect("scratch config required");
    let baseline_dir =
        std::env::var("NEUTRONSYNC_PHASE1_BASELINE_COPY").expect("baseline copy required");
    let baseline_pair =
        std::env::var("NEUTRONSYNC_PHASE1_BASELINE_PAIR").unwrap_or_else(|_| "Documents".into());
    let cfg = config::load(Some(&config_path)).unwrap();
    let api = DriveApi::new(&cfg).unwrap();
    let tree = api
        .list_tree("/my-files/Documents", &|_| false, &|_| {})
        .unwrap();
    assert!(!tree.root_missing);
    assert!(tree.failed.is_empty());
    let baseline =
        neutronsync::state::load_baseline_read_only(Path::new(&baseline_dir), &baseline_pair)
            .unwrap();
    assert!(!baseline.is_empty(), "wrong baseline pair or empty copy");
    let remote: BTreeMap<_, _> = tree.entries.into_iter().collect();
    let only_api: Vec<_> = remote
        .keys()
        .filter(|p| !baseline.contains_key(*p))
        .collect();
    let only_baseline: Vec<_> = baseline
        .keys()
        .filter(|p| !remote.contains_key(*p))
        .collect();
    let only_api_details: Vec<_> = only_api.iter().map(|path| {
        let entry = &remote[*path];
        let matching_baseline: Vec<_> = baseline.iter().filter(|(_, old)| {
            !entry.is_dir && !old.is_dir && entry.size == old.size && entry.sha1.is_some() && entry.sha1 == old.sha1
        }).map(|(path, _)| path).collect();
        serde_json::json!({"path":path,"is_dir":entry.is_dir,"size":entry.size,"mtime":entry.mtime,"sha1":entry.sha1,"ignored_junk":neutronsync::ignore::is_ignored_junk(path),"same_content_baseline_paths":matching_baseline})
    }).collect();
    let changed: Vec<_> = remote.iter().filter_map(|(path, entry)| {
        let old = baseline.get(path)?;
        if entry.is_dir == old.is_dir && entry.size == old.size && entry.mtime == old.mtime && entry.sha1 == old.sha1 { return None; }
        Some(serde_json::json!({"path":path,"baseline":{"is_dir":old.is_dir,"size":old.size,"mtime":old.mtime,"sha1":old.sha1},"api":{"is_dir":entry.is_dir,"size":entry.size,"mtime":entry.mtime,"sha1":entry.sha1}}))
    }).collect();
    println!(
        "PHASE1_EQUIVALENCE {}",
        serde_json::json!({"baseline_pair":baseline_pair,"baseline_entries":baseline.len(),"api_entries":remote.len(),"api_folders":remote.values().filter(|e| e.is_dir).count(),"failed":tree.failed,"only_api":only_api,"only_api_details":only_api_details,"only_baseline":only_baseline,"changed":changed})
    );
}
