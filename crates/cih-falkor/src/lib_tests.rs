use super::{
    run_supervised_read, FalkorStore, GraphStoreError, SupervisedReadError, REQUIRED_SYMBOL_INDEXES,
};
use crate::serialize::cstr;
use cih_graph_store::{BackendReadinessState, RetryMetadata};
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
        GraphStoreError::Overloaded { message, retry } => {
            assert!(
                message.contains("saturated"),
                "unexpected message: {message}"
            );
            assert!(retry.retryable);
            assert_eq!(retry.retry_after_ms, Some(50));
        }
        other => panic!("expected typed overloaded error, got: {other:?}"),
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

#[tokio::test]
async fn driver_timeout_retains_capacity_until_backend_task_finishes() {
    let lane = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let permit = lane.clone().acquire_owned().await.expect("first slot");
    let result = run_supervised_read(
        permit,
        Duration::from_millis(10),
        Duration::from_millis(500),
        async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok::<_, ()>(())
        },
    )
    .await;

    assert!(matches!(result, Err(SupervisedReadError::TimedOut)));
    assert!(
        lane.clone().try_acquire_owned().is_err(),
        "driver timeout must not release capacity while backend work continues"
    );
    tokio::time::sleep(Duration::from_millis(120)).await;
    let _released = lane
        .try_acquire_owned()
        .expect("backend completion releases capacity");
}

#[tokio::test]
async fn caller_cancellation_retains_capacity_until_backend_task_finishes() {
    let lane = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let permit = lane.clone().acquire_owned().await.expect("first slot");
    let started = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let task_started = started.clone();
    let task = tokio::spawn(run_supervised_read(
        permit,
        Duration::from_secs(1),
        Duration::from_millis(500),
        async move {
            task_started.wait().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok::<_, ()>(())
        },
    ));
    started.wait().await;
    task.abort();
    assert!(task
        .await
        .expect_err("caller task is cancelled")
        .is_cancelled());
    assert!(
        lane.clone().try_acquire_owned().is_err(),
        "cancelled caller must not release capacity while backend work continues"
    );
    tokio::time::sleep(Duration::from_millis(120)).await;
    let _released = lane
        .try_acquire_owned()
        .expect("backend completion releases capacity");
}

#[test]
fn is_loading_error_detects_busy_loading() {
    let redis_error = redis::RedisError::from((
        redis::ErrorKind::BusyLoadingError,
        "Redis is loading the dataset in memory",
    ));
    let loading = FalkorStore::map_redis_error(redis_error, None);
    assert!(FalkorStore::is_loading_error(&loading));
    let GraphStoreError::Loading { retry, .. } = loading else {
        panic!("BusyLoadingError must remain structurally classified");
    };
    assert!(retry.retryable);

    // Arbitrary strings containing "loading" must NOT trigger readiness waits.
    for other in [
        "graph backend error: syntax error",
        "graph store overloaded: concurrent query limit reached",
        "connection refused",
        "application is loading configuration",
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
fn backend_readiness_uses_restore_metadata_without_a_graph_query() {
    let command = String::from_utf8(FalkorStore::backend_readiness_command().get_packed_command())
        .expect("RESP command is utf-8 for this fixture");
    assert!(command.contains("INFO"));
    assert!(command.contains("persistence"));
    assert!(!command.contains("GRAPH.QUERY"));
    assert!(!command.contains("CREATE INDEX"));
}

#[test]
fn backend_readiness_parses_ready_loading_and_malformed_metadata() {
    let ready =
        FalkorStore::parse_backend_readiness("# Persistence\r\nloading:0\r\naof_enabled:1\r\n")
            .expect("loading:0 is ready");
    assert_eq!(ready.state, BackendReadinessState::Ready);
    assert_eq!(ready.retry_after_ms, None);

    let loading = FalkorStore::parse_backend_readiness(
        "# Persistence\r\nloading:1\r\nloading_loaded_perc:42.75\r\n",
    )
    .expect("loading:1 is a reportable state");
    assert_eq!(loading.state, BackendReadinessState::Loading);
    assert_eq!(loading.retry_after_ms, Some(1_000));
    assert!(loading
        .detail
        .as_deref()
        .is_some_and(|detail| detail.contains("42.75%")));

    for malformed in [
        "# Persistence\r\naof_enabled:1\r\n",
        "# Persistence\r\nloading:unknown\r\n",
    ] {
        let error = FalkorStore::parse_backend_readiness(malformed)
            .expect_err("missing or invalid loading metadata must not report ready");
        let GraphStoreError::Unavailable { retry, .. } = error else {
            panic!("malformed readiness metadata must be unavailable");
        };
        assert_eq!(retry, RetryMetadata::non_retryable());
    }
}

#[test]
fn query_timeout_is_classified_without_stringly_retry() {
    let redis_error = redis::RedisError::from((
        redis::ErrorKind::ResponseError,
        "FalkorDB query error",
        "Query timed out".to_string(),
    ));
    let error = FalkorStore::map_redis_error(redis_error, Some(Duration::from_millis(2_500)));
    match error {
        GraphStoreError::ExecutionTimeout {
            timeout_ms, retry, ..
        } => {
            assert_eq!(timeout_ms, 2_500);
            assert!(
                !retry.retryable,
                "same bounded query should be degraded, not retried"
            );
        }
        other => panic!("expected typed execution timeout, got: {other:?}"),
    }
}

#[test]
fn read_timeout_builder_validates_order_and_injects_backend_timeout() {
    let store = FalkorStore::connect("redis://127.0.0.1:6379", "test")
        .expect("connect parses url")
        .with_read_timeouts(Duration::from_millis(750), Duration::from_secs(1))
        .expect("valid read deadlines");
    let command = String::from_utf8(store.read_query_command("RETURN 1").get_packed_command())
        .expect("RESP command is utf-8 for this fixture");
    assert!(command.contains("GRAPH.QUERY"));
    assert!(command.contains("TIMEOUT"));
    assert!(command.contains("750"));

    for (backend, driver) in [
        (Duration::ZERO, Duration::from_secs(1)),
        (Duration::from_secs(1), Duration::from_secs(1)),
        (Duration::from_secs(2), Duration::from_secs(1)),
    ] {
        let result = FalkorStore::connect("redis://127.0.0.1:6379", "test")
            .expect("connect parses url")
            .with_read_timeouts(backend, driver);
        let Err(error) = result else {
            panic!("invalid deadline pair must fail");
        };
        assert!(matches!(error, GraphStoreError::InvalidInput(_)));
    }
}

#[test]
fn duplicate_index_candidate_and_index_tokens_are_exact() {
    assert!(FalkorStore::is_possible_duplicate_index_error(
        &GraphStoreError::Backend("Index already exists".into())
    ));
    assert!(!FalkorStore::is_possible_duplicate_index_error(
        &GraphStoreError::Backend("index allocation failed".into())
    ));
    assert!(super::cell_has_token("[id, kind]", "id"));
    assert!(super::cell_has_token("Symbol", "Symbol"));
    assert!(!super::cell_has_token("candidateId", "id"));
    assert_eq!(super::duration_millis(Duration::from_nanos(1)), 1);
    assert_eq!(REQUIRED_SYMBOL_INDEXES, ["id", "kind", "name", "file"]);
}

#[test]
fn load_wait_budget_defaults_to_600s() {
    // Only asserts the default when the env var is unset (test env doesn't set it).
    if std::env::var("CIH_FALKOR_LOAD_WAIT_SECS").is_err() {
        assert_eq!(FalkorStore::load_wait_budget(), Duration::from_secs(600));
    }
}

#[test]
fn server_can_disable_per_request_loading_sleep() {
    let store = FalkorStore::connect("redis://127.0.0.1:6379", "test")
        .expect("connect parses url")
        .with_read_load_wait(Duration::ZERO);

    assert_eq!(store.read_load_wait, Some(Duration::ZERO));
}
