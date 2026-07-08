//! Deterministic server-side layout for the browser's whole-repository view.
//!
//! The layout is deliberately split into a cheap hierarchical seed and a
//! bounded Barnes-Hut refinement. Large projections always have a useful
//! position immediately and never run an unbounded browser-side simulation.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use cih_core::{EdgeKind, NodeKind};
use cih_graph_store::GraphOverview;
use serde::Serialize;

const GOLDEN_ANGLE: f32 = 2.399_963_1;
const BH_THETA: f32 = 1.2;
const REPULSION: f32 = 8.0;
const ATTRACTION: f32 = 0.018;
const ANCHOR_STRENGTH: f32 = 0.2;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct LayoutOverview {
    pub(crate) nodes: Vec<LayoutNode>,
    pub(crate) edges: Vec<LayoutEdge>,
    pub(crate) total_nodes: u64,
    pub(crate) total_edges: u64,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct LayoutNode {
    pub(crate) index: u32,
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) qualified_name: Option<String>,
    pub(crate) file: String,
    pub(crate) degree: u64,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) z: f32,
    pub(crate) size: f32,
    pub(crate) color: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct LayoutEdge {
    pub(crate) source: u32,
    pub(crate) target: u32,
    pub(crate) kind: String,
}

#[derive(Clone, Copy, Debug, Default)]
struct Body {
    x: f32,
    y: f32,
    z: f32,
    ax: f32,
    ay: f32,
    az: f32,
    mass: f32,
}

pub(crate) fn compute(overview: GraphOverview) -> LayoutOverview {
    if overview.nodes.is_empty() {
        return LayoutOverview {
            nodes: Vec::new(),
            edges: Vec::new(),
            total_nodes: overview.total_nodes,
            total_edges: overview.total_edges,
            truncated: overview.truncated,
        };
    }

    let mut id_to_index = HashMap::with_capacity(overview.nodes.len());
    for (index, item) in overview.nodes.iter().enumerate() {
        id_to_index.insert(item.node.id.as_str(), index);
    }

    let mut seen_edges = HashSet::with_capacity(overview.edges.len());
    let mut indexed_edges = Vec::with_capacity(overview.edges.len());
    for edge in &overview.edges {
        let (Some(&source), Some(&target)) = (
            id_to_index.get(edge.source.as_str()),
            id_to_index.get(edge.target.as_str()),
        ) else {
            continue;
        };
        if seen_edges.insert((source, target, edge.kind)) {
            indexed_edges.push((source, target, edge.kind));
        }
    }

    let depths = call_depths(&overview, &indexed_edges);
    let mut clusters = BTreeMap::<String, Vec<usize>>::new();
    for (index, item) in overview.nodes.iter().enumerate() {
        clusters
            .entry(cluster_key(item.node.kind, &item.node.file))
            .or_default()
            .push(index);
    }

    let cluster_count = clusters.len().max(1);
    let global_radius = 380.0 + (cluster_count as f32).sqrt() * 72.0;
    let mut bodies = vec![Body::default(); overview.nodes.len()];
    for (cluster_index, members) in clusters.values().enumerate() {
        let center = fibonacci_point(cluster_index, cluster_count, global_radius);
        let local_radius = (members.len() as f32).sqrt().mul_add(5.0, 24.0).min(210.0);
        for (member_index, &node_index) in members.iter().enumerate() {
            let node = &overview.nodes[node_index];
            let seed = fnv1a(node.node.id.as_str());
            let unit = fibonacci_point(
                ((seed as usize) ^ member_index) % members.len().max(1),
                members.len().max(1),
                1.0,
            );
            let radial = 0.25 + 0.75 * hash_unit(seed.rotate_left(13)).cbrt();
            let depth_offset = depths[node_index].min(24) as f32 * 4.0;
            let x = center.0 + unit.0 * local_radius * radial;
            let y = center.1 + unit.1 * local_radius * radial;
            let z = center.2 + unit.2 * local_radius * radial - depth_offset;
            let mass = 1.0 + (node.degree as f32 + 1.0).ln();
            bodies[node_index] = Body {
                x,
                y,
                z,
                ax: x,
                ay: y,
                az: z,
                mass,
            };
        }
    }

    let iterations = match bodies.len() {
        0..=10_000 => 24,
        10_001..=20_000 => 8,
        _ => 0,
    };
    if iterations > 0 {
        refine(&mut bodies, &indexed_edges, iterations);
    }

    let nodes = overview
        .nodes
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let body = bodies[index];
            LayoutNode {
                index: index as u32,
                id: item.node.id.to_string(),
                kind: item.node.kind.label().to_string(),
                name: item.node.name,
                qualified_name: item.node.qualified_name,
                file: item.node.file,
                degree: item.degree,
                x: finite(body.x),
                y: finite(body.y),
                z: finite(body.z),
                size: node_size(item.node.kind, item.degree),
                color: stellar_color(item.degree).to_string(),
            }
        })
        .collect();
    let edges = indexed_edges
        .into_iter()
        .map(|(source, target, kind)| LayoutEdge {
            source: source as u32,
            target: target as u32,
            kind: kind.cypher_label().to_string(),
        })
        .collect();

    LayoutOverview {
        nodes,
        edges,
        total_nodes: overview.total_nodes,
        total_edges: overview.total_edges,
        truncated: overview.truncated,
    }
}

fn cluster_key(kind: NodeKind, file: &str) -> String {
    if matches!(
        kind,
        NodeKind::Community
            | NodeKind::Process
            | NodeKind::Route
            | NodeKind::IntegrationRoute
            | NodeKind::MessageDestination
            | NodeKind::KafkaTopic
            | NodeKind::ExternalEndpoint
            | NodeKind::DbQuery
            | NodeKind::DbTable
    ) {
        return format!("@{}", kind.label());
    }
    let path = file
        .split('/')
        .filter(|part| !part.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join("/");
    if path.is_empty() {
        format!("@{}", kind.label())
    } else {
        path
    }
}

fn call_depths(overview: &GraphOverview, edges: &[(usize, usize, EdgeKind)]) -> Vec<u32> {
    let mut outgoing = vec![Vec::new(); overview.nodes.len()];
    let mut incoming = vec![0usize; overview.nodes.len()];
    for &(source, target, kind) in edges {
        if matches!(
            kind,
            EdgeKind::Calls
                | EdgeKind::HandlesRoute
                | EdgeKind::ExternalCall
                | EdgeKind::PublishesEvent
                | EdgeKind::ListensTo
                | EdgeKind::IntegrationLink
        ) {
            outgoing[source].push(target);
            incoming[target] += 1;
        }
    }

    let mut depths = vec![u32::MAX; overview.nodes.len()];
    let mut queue = VecDeque::new();
    for (index, item) in overview.nodes.iter().enumerate() {
        if matches!(
            item.node.kind,
            NodeKind::Route
                | NodeKind::Process
                | NodeKind::IntegrationRoute
                | NodeKind::MessageDestination
        ) || incoming[index] == 0
        {
            depths[index] = 0;
            queue.push_back(index);
        }
    }
    while let Some(source) = queue.pop_front() {
        let next_depth = depths[source].saturating_add(1);
        for &target in &outgoing[source] {
            if next_depth < depths[target] {
                depths[target] = next_depth;
                queue.push_back(target);
            }
        }
    }
    for depth in &mut depths {
        if *depth == u32::MAX {
            *depth = 0;
        }
    }
    depths
}

fn fibonacci_point(index: usize, count: usize, radius: f32) -> (f32, f32, f32) {
    if count <= 1 {
        return (0.0, 0.0, 0.0);
    }
    let y = 1.0 - (index as f32 / (count - 1) as f32) * 2.0;
    let ring = (1.0 - y * y).max(0.0).sqrt();
    let theta = GOLDEN_ANGLE * index as f32;
    (
        theta.cos() * ring * radius,
        y * radius,
        theta.sin() * ring * radius,
    )
}

fn node_size(kind: NodeKind, degree: u64) -> f32 {
    let base = match kind {
        NodeKind::Community | NodeKind::Process => 12.0,
        NodeKind::Route | NodeKind::IntegrationRoute => 9.0,
        NodeKind::Folder | NodeKind::File => 7.0,
        NodeKind::Class | NodeKind::Interface | NodeKind::Record => 5.0,
        _ => 3.2,
    };
    base + ((degree as f32 + 1.0).ln() * 0.75).min(8.0)
}

fn stellar_color(degree: u64) -> &'static str {
    match degree {
        0..=1 => "#ff6050",
        2..=3 => "#ff8855",
        4..=6 => "#ffc070",
        7..=12 => "#ffe080",
        13..=25 => "#fff8e8",
        26..=50 => "#c0d0ff",
        _ => "#80a0ff",
    }
}

fn fnv1a(value: &str) -> u32 {
    value.bytes().fold(2_166_136_261, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16_777_619)
    })
}

fn hash_unit(value: u32) -> f32 {
    (value as f64 / u32::MAX as f64) as f32
}

fn finite(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

#[derive(Clone, Debug)]
struct OctreeNode {
    origin: [f32; 3],
    half: f32,
    center: [f32; 3],
    total_mass: f32,
    body: Option<usize>,
    body_mass: f32,
    children: [Option<usize>; 8],
}

impl OctreeNode {
    fn new(origin: [f32; 3], half: f32) -> Self {
        Self {
            origin,
            half,
            center: [0.0; 3],
            total_mass: 0.0,
            body: None,
            body_mass: 0.0,
            children: [None; 8],
        }
    }
}

fn refine(bodies: &mut [Body], edges: &[(usize, usize, EdgeKind)], iterations: usize) {
    let mut forces = vec![[0.0f32; 3]; bodies.len()];
    for _ in 0..iterations {
        forces.fill([0.0; 3]);
        let Some((origin, half)) = bounds(bodies) else {
            return;
        };
        let mut tree = vec![OctreeNode::new(origin, half)];
        for index in 0..bodies.len() {
            octree_insert(&mut tree, 0, index, bodies, 0);
        }
        for (index, force) in forces.iter_mut().enumerate() {
            octree_repulse(&tree, 0, index, bodies, force);
        }
        for &(source, target, _) in edges {
            let dx = bodies[target].x - bodies[source].x;
            let dy = bodies[target].y - bodies[source].y;
            let dz = bodies[target].z - bodies[source].z;
            forces[source][0] += dx * ATTRACTION;
            forces[source][1] += dy * ATTRACTION;
            forces[source][2] += dz * ATTRACTION;
            forces[target][0] -= dx * ATTRACTION;
            forces[target][1] -= dy * ATTRACTION;
            forces[target][2] -= dz * ATTRACTION;
        }
        for (body, force) in bodies.iter_mut().zip(&mut forces) {
            force[0] += (body.ax - body.x) * ANCHOR_STRENGTH * body.mass;
            force[1] += (body.ay - body.y) * ANCHOR_STRENGTH * body.mass;
            force[2] += (body.az - body.z) * ANCHOR_STRENGTH * body.mass;
            let magnitude = (force[0] * force[0] + force[1] * force[1] + force[2] * force[2])
                .sqrt();
            let scale = if magnitude > 8.0 { 8.0 / magnitude } else { 1.0 };
            body.x += force[0] * scale;
            body.y += force[1] * scale;
            body.z += force[2] * scale;
        }
    }
}

fn bounds(bodies: &[Body]) -> Option<([f32; 3], f32)> {
    let first = bodies.first()?;
    let mut min = [first.x, first.y, first.z];
    let mut max = min;
    for body in bodies.iter().skip(1) {
        min[0] = min[0].min(body.x);
        min[1] = min[1].min(body.y);
        min[2] = min[2].min(body.z);
        max[0] = max[0].max(body.x);
        max[1] = max[1].max(body.y);
        max[2] = max[2].max(body.z);
    }
    let origin = [
        (min[0] + max[0]) * 0.5,
        (min[1] + max[1]) * 0.5,
        (min[2] + max[2]) * 0.5,
    ];
    let half = ((max[0] - min[0]).max(max[1] - min[1]).max(max[2] - min[2]) * 0.5 + 1.0)
        .max(1.0);
    Some((origin, half))
}

fn octree_insert(
    tree: &mut Vec<OctreeNode>,
    node_index: usize,
    body_index: usize,
    bodies: &[Body],
    depth: usize,
) {
    let body = bodies[body_index];
    if tree[node_index].total_mass == 0.0 {
        let node = &mut tree[node_index];
        node.center = [body.x, body.y, body.z];
        node.total_mass = body.mass;
        node.body = Some(body_index);
        node.body_mass = body.mass;
        return;
    }

    let old_mass = tree[node_index].total_mass;
    let next_mass = old_mass + body.mass;
    for (axis, value) in [body.x, body.y, body.z].into_iter().enumerate() {
        tree[node_index].center[axis] =
            (tree[node_index].center[axis] * old_mass + value * body.mass) / next_mass;
    }
    tree[node_index].total_mass = next_mass;

    if depth >= 32 || tree[node_index].half < 0.001 {
        tree[node_index].body = None;
        return;
    }

    if let Some(previous_index) = tree[node_index].body.take() {
        let previous = bodies[previous_index];
        let child = ensure_child(tree, node_index, [previous.x, previous.y, previous.z]);
        octree_insert(tree, child, previous_index, bodies, depth + 1);
    }
    let child = ensure_child(tree, node_index, [body.x, body.y, body.z]);
    octree_insert(tree, child, body_index, bodies, depth + 1);
}

fn ensure_child(tree: &mut Vec<OctreeNode>, node_index: usize, point: [f32; 3]) -> usize {
    let origin = tree[node_index].origin;
    let octant = usize::from(point[0] >= origin[0])
        | (usize::from(point[1] >= origin[1]) << 1)
        | (usize::from(point[2] >= origin[2]) << 2);
    if let Some(child) = tree[node_index].children[octant] {
        return child;
    }
    let quarter = tree[node_index].half * 0.5;
    let child_origin = [
        origin[0] + if octant & 1 != 0 { quarter } else { -quarter },
        origin[1] + if octant & 2 != 0 { quarter } else { -quarter },
        origin[2] + if octant & 4 != 0 { quarter } else { -quarter },
    ];
    let child = tree.len();
    tree.push(OctreeNode::new(child_origin, quarter));
    tree[node_index].children[octant] = Some(child);
    child
}

fn octree_repulse(
    tree: &[OctreeNode],
    node_index: usize,
    body_index: usize,
    bodies: &[Body],
    force: &mut [f32; 3],
) {
    let node = &tree[node_index];
    if node.total_mass == 0.0 || node.body == Some(body_index) {
        return;
    }
    let body = bodies[body_index];
    let dx = body.x - node.center[0];
    let dy = body.y - node.center[1];
    let dz = body.z - node.center[2];
    let distance = (dx * dx + dy * dy + dz * dz).sqrt().max(0.01);
    let leaf = node.children.iter().all(Option::is_none);
    if leaf || node.half * 2.0 / distance < BH_THETA {
        let magnitude = REPULSION * body.mass * node.total_mass / distance;
        force[0] += magnitude * dx / distance;
        force[1] += magnitude * dy / distance;
        force[2] += magnitude * dz / distance;
        return;
    }
    for child in node.children.iter().flatten() {
        octree_repulse(tree, *child, body_index, bodies, force);
    }
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;


