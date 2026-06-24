use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cih_server_lib::search::{latest_graph_artifacts_in_dir, query_limit};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn write_artifacts(&self, version: &str) {
        let dir = self.path.join(version);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("nodes.jsonl"), "").unwrap();
        fs::write(dir.join("edges.jsonl"), "").unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn query_limit_defaults_and_clamps() {
    assert_eq!(query_limit(None), 10);
    assert_eq!(query_limit(Some(0)), 1);
    assert_eq!(query_limit(Some(8)), 8);
    assert_eq!(query_limit(Some(500)), 50);
}

#[test]
fn latest_graph_artifacts_chooses_newest_complete_dir() {
    let tmp = TempDir::new("cih-server-artifacts-test");
    tmp.write_artifacts("v1");
    std::thread::sleep(Duration::from_millis(20));
    tmp.write_artifacts("v2");
    fs::create_dir_all(tmp.path.join("v3")).unwrap();
    fs::write(tmp.path.join("v3").join("nodes.jsonl"), "").unwrap();

    let artifacts = latest_graph_artifacts_in_dir(&tmp.path).unwrap();

    assert_eq!(artifacts.version.0, "v2");
    assert!(artifacts.nodes_path.ends_with("v2/nodes.jsonl"));
    assert!(artifacts.edges_path.ends_with("v2/edges.jsonl"));
}
