use crossbar::*;
use std::time::Duration;

/// Helper: create a unique registry path for test isolation.
fn test_registry_path(suffix: &str) -> std::path::PathBuf {
    let pid = std::process::id();
    if cfg!(target_os = "linux") {
        std::path::PathBuf::from(format!("/dev/shm/crossbar-test-registry-{pid}-{suffix}"))
    } else {
        std::env::temp_dir().join(format!("crossbar-test-registry-{pid}-{suffix}"))
    }
}

/// Clean up a registry file.
fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

// ─── basic register + discover ──────────────────────────────────────────

#[test]
fn registry_register_and_discover() {
    let path = test_registry_path("basic");
    cleanup(&path);

    let reg = Registry::open_at(path.clone()).unwrap();
    let pid = std::process::id();

    reg.register("prices", "/tick/AAPL", pid).unwrap();
    reg.register("prices", "/tick/MSFT", pid).unwrap();
    reg.register("flow", "/flow/SPX", pid).unwrap();

    // Exact match
    let results = reg.discover("/tick/AAPL");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].region, "prices");
    assert_eq!(results[0].uri, "/tick/AAPL");
    assert_eq!(results[0].pid, pid);

    // Exact match — no result
    let results = reg.discover("/tick/GOOG");
    assert!(results.is_empty());

    cleanup(&path);
}

// ─── wildcard matching ──────────────────────────────────────────────────

#[test]
fn registry_wildcard_discover() {
    let path = test_registry_path("wildcard");
    cleanup(&path);

    let reg = Registry::open_at(path.clone()).unwrap();
    let pid = std::process::id();

    reg.register("prices", "/tick/AAPL", pid).unwrap();
    reg.register("prices", "/tick/MSFT", pid).unwrap();
    reg.register("flow", "/flow/SPX", pid).unwrap();

    // Wildcard: /tick/* matches /tick/AAPL and /tick/MSFT but NOT /flow/SPX
    let mut results = reg.discover("/tick/*");
    results.sort_by(|a, b| a.uri.cmp(&b.uri));
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].uri, "/tick/AAPL");
    assert_eq!(results[1].uri, "/tick/MSFT");

    // Wildcard: /* matches everything
    let results = reg.discover("/*");
    assert_eq!(results.len(), 3);

    // Wildcard: /flow/* matches only /flow/SPX
    let results = reg.discover("/flow/*");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].uri, "/flow/SPX");
    assert_eq!(results[0].region, "flow");

    cleanup(&path);
}

// ─── prune stale entries ────────────────────────────────────────────────

#[test]
fn registry_prune_stale() {
    let path = test_registry_path("prune");
    cleanup(&path);

    let reg = Registry::open_at(path.clone()).unwrap();
    let pid = std::process::id();

    reg.register("prices", "/tick/AAPL", pid).unwrap();
    reg.register("prices", "/tick/MSFT", pid).unwrap();

    assert_eq!(reg.discover("/tick/*").len(), 2);

    // Prune with zero timeout should remove everything (all entries are "stale")
    reg.prune_stale(Duration::ZERO);

    assert!(reg.discover("/tick/*").is_empty());

    cleanup(&path);
}

#[test]
fn registry_prune_preserves_fresh() {
    let path = test_registry_path("prune-fresh");
    cleanup(&path);

    let reg = Registry::open_at(path.clone()).unwrap();
    let pid = std::process::id();

    reg.register("prices", "/tick/AAPL", pid).unwrap();

    // Prune with a generous timeout should keep fresh entries
    reg.prune_stale(Duration::from_secs(60));

    let results = reg.discover("/tick/AAPL");
    assert_eq!(results.len(), 1);

    cleanup(&path);
}

// ─── unregister ─────────────────────────────────────────────────────────

#[test]
fn registry_unregister() {
    let path = test_registry_path("unregister");
    cleanup(&path);

    let reg = Registry::open_at(path.clone()).unwrap();
    let pid = std::process::id();

    reg.register("prices", "/tick/AAPL", pid).unwrap();
    reg.register("prices", "/tick/MSFT", pid).unwrap();
    reg.register("flow", "/flow/SPX", pid).unwrap();

    // Unregister all entries for region "prices" with our PID
    reg.unregister("prices", pid);

    // /tick/* should be empty now
    assert!(reg.discover("/tick/*").is_empty());

    // /flow/* should still be there
    let results = reg.discover("/flow/*");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].uri, "/flow/SPX");

    cleanup(&path);
}

// ─── multiple regions with overlapping URIs ─────────────────────────────

#[test]
fn registry_multiple_regions_overlapping_uris() {
    let path = test_registry_path("overlap");
    cleanup(&path);

    let reg = Registry::open_at(path.clone()).unwrap();
    let pid = std::process::id();

    // Same URI in different regions
    reg.register("region-a", "/tick/AAPL", pid).unwrap();
    reg.register("region-b", "/tick/AAPL", pid).unwrap();

    let mut results = reg.discover("/tick/AAPL");
    results.sort_by(|a, b| a.region.cmp(&b.region));
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].region, "region-a");
    assert_eq!(results[1].region, "region-b");

    cleanup(&path);
}

// ─── duplicate registration updates timestamp ──────────────────────────

#[test]
fn registry_duplicate_register_does_not_create_new_entry() {
    let path = test_registry_path("dedup");
    cleanup(&path);

    let reg = Registry::open_at(path.clone()).unwrap();
    let pid = std::process::id();

    reg.register("prices", "/tick/AAPL", pid).unwrap();
    reg.register("prices", "/tick/AAPL", pid).unwrap();
    reg.register("prices", "/tick/AAPL", pid).unwrap();

    let results = reg.discover("/tick/AAPL");
    assert_eq!(
        results.len(),
        1,
        "duplicate registrations should not create extra entries"
    );

    cleanup(&path);
}

// ─── reopen existing registry ───────────────────────────────────────────

#[test]
fn registry_reopen_existing() {
    let path = test_registry_path("reopen");
    cleanup(&path);

    {
        let reg = Registry::open_at(path.clone()).unwrap();
        reg.register("prices", "/tick/AAPL", std::process::id())
            .unwrap();
    }

    // Reopen — should see the existing entry
    let reg2 = Registry::open_at(path.clone()).unwrap();
    let results = reg2.discover("/tick/AAPL");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].region, "prices");

    cleanup(&path);
}

// ─── integration with Publisher ─────────────────────────────────────────

#[test]
fn publisher_registers_in_global_registry() {
    let name = &format!("test-disc-pub-{}", std::process::id());
    let mut pub_ = Publisher::create(name, Config::default()).unwrap();
    let _topic = pub_.register("/tick/AAPL").unwrap();
    let _topic2 = pub_.register("/flow/SPX").unwrap();

    // Open the global registry and discover
    let reg = Registry::open().unwrap();
    let results = reg.discover("/tick/AAPL");

    // The publisher should have registered in the global registry
    let found = results
        .iter()
        .any(|t| t.region == *name && t.uri == "/tick/AAPL");
    assert!(found, "Publisher should register topics in global registry");

    let results = reg.discover("/flow/SPX");
    let found = results
        .iter()
        .any(|t| t.region == *name && t.uri == "/flow/SPX");
    assert!(
        found,
        "Publisher should register all topics in global registry"
    );

    // Drop the publisher — should unregister
    drop(pub_);

    let reg2 = Registry::open().unwrap();
    let results = reg2.discover("/tick/AAPL");
    let found = results.iter().any(|t| t.region == *name);
    assert!(!found, "Publisher should unregister topics on drop");
}

// ─── top-level discover() convenience function ──────────────────────────

#[test]
fn top_level_discover_function() {
    let name = &format!("test-disc-top-{}", std::process::id());
    let mut pub_ = Publisher::create(name, Config::default()).unwrap();
    let _topic = pub_.register("/disc-test/AAPL").unwrap();

    let results = crossbar::discover("/disc-test/*").unwrap();
    let found = results
        .iter()
        .any(|t| t.region == *name && t.uri == "/disc-test/AAPL");
    assert!(found, "Top-level discover() should find registered topics");

    drop(pub_);
}
