//! Phase-2 smoke test: build a sample graph → write canonical artifacts →
//! bulk_load into FalkorDB → run impact/context through the GraphStore.
//!
//! Run (FalkorDB on 6380 per the dev setup):
//!   FALKOR_URL=redis://127.0.0.1:6380 cargo run -p cih-falkor --example load_sample

use cih_core::{Edge, EdgeKind, GraphArtifacts, Node, NodeId, NodeKind, Range, VersionId};
use cih_falkor::FalkorStore;
use cih_graph_store::{Direction, GraphStore};

fn method(id: &str, name: &str, file: &str) -> Node {
    Node {
        id: NodeId::new(id),
        kind: NodeKind::Method,
        name: name.to_string(),
        qualified_name: Some(id.trim_start_matches("Method:").to_string()),
        file: file.to_string(),
        range: Range {
            start_line: 1,
            ..Range::default()
        },
        props: None,
    }
}

fn calls(src: &str, dst: &str) -> Edge {
    Edge {
        src: NodeId::new(src),
        dst: NodeId::new(dst),
        kind: EdgeKind::Calls,
        confidence: 0.95,
        reason: "sample".to_string(),
        props: None,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::var("FALKOR_URL").unwrap_or_else(|_| "redis://127.0.0.1:6380".into());
    let store = FalkorStore::connect(&url, "cih_sample")?;
    store.ensure_schema().await?;

    // register -> save -> persist  (a 3-hop call chain)
    let nodes = vec![
        method(
            "Method:UserController#register",
            "register",
            "UserController.java",
        ),
        method("Method:UserService#save", "save", "UserService.java"),
        method(
            "Method:UserRepository#persist",
            "persist",
            "UserRepository.java",
        ),
    ];
    let edges = vec![
        calls("Method:UserController#register", "Method:UserService#save"),
        calls("Method:UserService#save", "Method:UserRepository#persist"),
    ];

    let dir = std::env::temp_dir().join("cih_sample_artifacts");
    let artifacts = GraphArtifacts::write(&dir, VersionId::new("v1"), &nodes, &edges)?;
    println!("artifacts: {:?}", artifacts.nodes_path);

    let stats = store.bulk_load(&artifacts).await?;
    println!("bulk_load → {} nodes, {} edges", stats.nodes, stats.edges);

    let imp = store
        .impact(
            &NodeId::new("Method:UserRepository#persist"),
            Direction::Upstream,
            4,
        )
        .await?;
    println!(
        "impact(persist, upstream) risk={} affected={:?}",
        imp.risk,
        imp.affected
            .iter()
            .map(|a| format!("{}@{}", a.id, a.depth))
            .collect::<Vec<_>>()
    );

    let ctx = store
        .context(&NodeId::new("Method:UserService#save"))
        .await?;
    println!(
        "context(save): callers={:?} callees={:?}",
        ctx.callers
            .iter()
            .map(|n| n.id.to_string())
            .collect::<Vec<_>>(),
        ctx.callees
            .iter()
            .map(|n| n.id.to_string())
            .collect::<Vec<_>>(),
    );

    Ok(())
}
