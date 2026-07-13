use super::cstr;
use super::{FalkorStore, GraphStoreError};
use std::time::Duration;

#[test]
fn cstr_escapes_backslash_and_single_quote() {
    assert_eq!(cstr("a\\b's"), "'a\\\\b\\'s'");
    assert_eq!(cstr("line\nnext\tcell\rend"), "'line\\nnext\\tcell\\rend'");
}

// Backpressure is pure semaphore logic — no FalkorDB needed. `Client::open`
// only parses the URL, it does not dial, so this stays hermetic.
#[tokio::test]
async fn query_limit_sheds_when_saturated() {
    let store = FalkorStore::connect("redis://127.0.0.1:6379", "test")
        .expect("connect parses url")
        .with_query_limit(1, Duration::from_millis(50));

    // Hold the only permit for the duration of the test.
    let _held = store
        .acquire_permit()
        .await
        .expect("first acquire succeeds");

    // The next acquire can't get a slot and sheds after the timeout.
    let err = store
        .acquire_permit()
        .await
        .expect_err("second acquire sheds");
    match err {
        GraphStoreError::Backend(msg) => assert!(
            msg.contains("overloaded"),
            "expected overloaded error, got: {msg}"
        ),
        other => panic!("expected Backend overloaded error, got: {other:?}"),
    }
}

// With slack in the limit, concurrent acquires all succeed (no false shedding).
#[tokio::test]
async fn query_limit_allows_within_capacity() {
    let store = FalkorStore::connect("redis://127.0.0.1:6379", "test")
        .expect("connect parses url")
        .with_query_limit(2, Duration::from_millis(50));

    let a = store.acquire_permit().await.expect("first slot");
    let b = store.acquire_permit().await.expect("second slot");
    drop((a, b));
}

#[test]
fn is_loading_error_detects_busy_loading() {
    // The exact message FalkorDB/redis surfaces during dataset load, as seen in
    // the field log — mapped to GraphStoreError::Backend via run()'s e.to_string().
    let loading = GraphStoreError::Backend(
        "graph backend error: An error was signalled by the server - BusyLoadingError: \
         Redis is loading the dataset in memory"
            .into(),
    );
    assert!(FalkorStore::is_loading_error(&loading));

    // Non-loading backend errors must NOT be treated as loading (fail fast).
    for other in [
        "graph backend error: syntax error",
        "graph store overloaded: concurrent query limit reached",
        "connection refused",
    ] {
        assert!(
            !FalkorStore::is_loading_error(&GraphStoreError::Backend(other.into())),
            "false positive on: {other}"
        );
    }
    // Wrong variant is never a loading error.
    assert!(!FalkorStore::is_loading_error(&GraphStoreError::NotFound(
        "x".into()
    )));
}

#[test]
fn load_wait_budget_defaults_to_600s() {
    // Only asserts the default when the env var is unset (test env doesn't set it).
    if std::env::var("CIH_FALKOR_LOAD_WAIT_SECS").is_err() {
        assert_eq!(FalkorStore::load_wait_budget(), Duration::from_secs(600));
    }
}
