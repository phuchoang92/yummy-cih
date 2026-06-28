//! Parallel algorithm primitives gated behind the `rayon` feature.

#[cfg(feature = "rayon")]
use rayon::prelude::*;
#[cfg(feature = "rayon")]
use rand::rngs::StdRng;
#[cfg(feature = "rayon")]
use rand::seq::SliceRandom;
#[cfg(feature = "rayon")]
use rustc_hash::FxHashMap;

#[cfg(feature = "rayon")]
use crate::leiden_impl::algorithm;
#[cfg(feature = "rayon")]
use crate::leiden_impl::partition::Partition;
#[cfg(feature = "rayon")]
use crate::leiden_impl::quality::{GraphData, MoveComponents, QualityFunction};

/// Minimum number of edge slots (CSR entries) to use parallel aggregation.
#[cfg(feature = "rayon")]
pub(crate) const AGG_PARALLEL_THRESHOLD: usize = 10_000;

/// Returns (colors: Vec<usize>, num_colors: usize) where colors[node] is the color assignment.
/// Uses at most max_degree + 1 colors. O(V + E) time.
#[cfg(feature = "rayon")]
pub(crate) fn greedy_coloring(data: &GraphData, order: &[usize]) -> (Vec<usize>, usize) {
    let n = data.node_count();
    let directed = data.is_directed();
    let mut colors = vec![0usize; n];
    let mut used_colors: Vec<bool> = Vec::new();
    let mut num_colors = 1usize;

    for &node in order {
        let (targets, _) = data.out_neighbor_slices(node);

        // Compute the maximum color across ALL neighbors (out + in for directed)
        // in a single pass before any marking, so the buffer is sized once.
        let max_color = {
            let max_out = targets.iter().map(|&t| colors[t]).max().unwrap_or(0);
            if directed {
                let (in_targets, _) = data.in_neighbor_slices(node);
                max_out.max(in_targets.iter().map(|&t| colors[t]).max().unwrap_or(0))
            } else {
                max_out
            }
        };
        if used_colors.len() <= max_color {
            used_colors.resize(max_color + 1, false);
        }

        for &neighbor in targets {
            if neighbor == node {
                continue;
            }
            used_colors[colors[neighbor]] = true;
        }
        if directed {
            let (in_targets, _) = data.in_neighbor_slices(node);
            for &neighbor in in_targets {
                if neighbor == node {
                    continue;
                }
                used_colors[colors[neighbor]] = true;
            }
        }

        let mut color = 0;
        while color < used_colors.len() && used_colors[color] {
            color += 1;
        }
        colors[node] = color;
        if color + 1 > num_colors {
            num_colors = color + 1;
        }
        if color >= used_colors.len() {
            used_colors.resize(color + 1, false);
        }

        for &neighbor in targets {
            if neighbor == node {
                continue;
            }
            used_colors[colors[neighbor]] = false;
        }
        if directed {
            let (in_targets, _) = data.in_neighbor_slices(node);
            for &neighbor in in_targets {
                if neighbor == node {
                    continue;
                }
                used_colors[colors[neighbor]] = false;
            }
        }
    }

    (colors, num_colors)
}

#[cfg(feature = "rayon")]
pub(crate) fn aggregate_edges_parallel(
    data: &GraphData,
    orig_to_agg: &[usize],
    directed: bool,
) -> FxHashMap<(usize, usize), f64> {
    let n = data.node_count();
    (0..n)
        .into_par_iter()
        .fold(FxHashMap::<(usize, usize), f64>::default, |mut local, u| {
            algorithm::aggregate_node_edges_into(data, u, orig_to_agg, directed, &mut local);
            local
        })
        .reduce(
            FxHashMap::<(usize, usize), f64>::default,
            |mut acc, local| {
                for (k, v) in local {
                    *acc.entry(k).or_default() += v;
                }
                acc
            },
        )
}

/// Parallel local moving using graph coloring.
///
/// Nodes are colored so that same-color nodes form independent sets (no edges
/// between them). Each color group is processed in parallel using Rayon. Within
/// a group, all nodes see the same snapshot of community statistics. Moves are
/// collected and applied sequentially at the end of each color group.
///
/// This relaxed consistency model may produce slightly different results than
/// [`algorithm::local_moving_generic`]. When the parallel pass does not converge
/// naturally (detected by `!converged_naturally`), the caller falls back to a
/// sequential pass for final refinement.
#[cfg(feature = "rayon")]
pub(crate) fn local_moving_parallel(
    data: &GraphData,
    partition: &mut Partition,
    quality: &(dyn QualityFunction + Sync),
    rng: &mut StdRng,
    max_comm_size: usize,
    epsilon: f64,
) -> (bool, bool) {
    let n = data.node_count();
    if n == 0 {
        return (false, true);
    }

    let directed = data.is_directed();

    let m = data.total_weight();
    if m <= 0.0 {
        return (false, true);
    }
    let two_m = 2.0 * m;
    let total_node_weight: f64 = data.total_node_weight();

    let mut order: Vec<usize> = (0..n).filter(|&node| data.degree_of(node) > 0.0).collect();
    order.shuffle(rng);
    let (colors, num_colors) = greedy_coloring(data, &order);

    let mut color_groups: Vec<Vec<usize>> = vec![Vec::new(); num_colors];
    for &node in &order {
        color_groups[colors[node]].push(node);
    }

    let mut community_total_degree: Vec<Vec<f64>> = vec![vec![0.0; n]];
    let mut community_in_degree: Vec<Vec<f64>> = vec![vec![0.0; n]];
    let mut community_size: Vec<f64> = vec![0.0; n];
    algorithm::init_community_stats_into(
        std::slice::from_ref(data),
        |node| partition.community_of(node),
        &mut community_total_degree,
        &mut community_in_degree,
        &mut community_size,
    );

    let mut changed = false;
    let mut any_move = true;
    let mut iteration = 0usize;
    let max_rounds = 100;

    while any_move && iteration < max_rounds {
        any_move = false;
        iteration += 1;

        for group in &color_groups {
            if group.is_empty() {
                continue;
            }

            let moves: Vec<(usize, usize, usize, f64, f64, f64)> = group
                .par_iter()
                .map_init(
                    || {
                        (
                            vec![0.0f64; n],  // out_neighbor_comm_weights
                            vec![0.0f64; n],  // in_neighbor_comm_weights
                            Vec::<usize>::with_capacity(64), // touched_list (out)
                            Vec::<usize>::with_capacity(64), // touched_list (in)
                        )
                    },
                    |(out_weights, in_weights, out_touched, in_touched), &node| {
                        let current_community = partition.community_of(node);
                        let k_v_out = data.out_degree_of(node);
                        let k_v_in = if directed {
                            data.in_degree_of(node)
                        } else {
                            0.0
                        };
                        let node_weight = data.node_weight(node);

                        let (targets, weights) = data.out_neighbor_slices(node);
                        let mut k_v_to_current_out = 0.0f64;

                        for i in 0..targets.len() {
                            let neighbor = targets[i];
                            let weight = weights[i];
                            if neighbor == node {
                                continue;
                            }
                            let comm = partition.community_of(neighbor);
                            if comm == current_community {
                                k_v_to_current_out += weight;
                            } else if out_weights[comm] == 0.0 {
                                out_weights[comm] = weight;
                                out_touched.push(comm);
                            } else {
                                out_weights[comm] += weight;
                            }
                        }

                        let (in_targets, in_weights_slice) = if directed {
                            data.in_neighbor_slices(node)
                        } else {
                            (&[] as &[usize], &[] as &[f64])
                        };
                        let mut k_v_to_current_in = 0.0f64;
                        if directed {
                            for i in 0..in_targets.len() {
                                let neighbor = in_targets[i];
                                let weight = in_weights_slice[i];
                                if neighbor == node {
                                    continue;
                                }
                                let comm = partition.community_of(neighbor);
                                if comm == current_community {
                                    k_v_to_current_in += weight;
                                } else if in_weights[comm] == 0.0 {
                                    in_weights[comm] = weight;
                                    in_touched.push(comm);
                                } else {
                                    in_weights[comm] += weight;
                                }
                            }
                        }

                        let sigma_tot_current_out = community_total_degree[0][current_community];
                        let sigma_tot_current_in = if directed {
                            community_in_degree[0][current_community]
                        } else {
                            0.0
                        };

                        let candidates = out_touched.iter().copied();
                        let (best_community, _) = algorithm::find_best_community(
                            candidates,
                            current_community,
                            epsilon,
                            max_comm_size,
                            &community_size,
                            node_weight,
                            |target_comm| {
                                let k_v_to_target_out = out_weights[target_comm];
                                let k_v_to_target_in = if directed {
                                    in_weights[target_comm]
                                } else {
                                    0.0
                                };
                                quality.delta_move_from_components(&MoveComponents {
                                    two_m,
                                    node_weight,
                                    total_node_weight,
                                    k_v_out,
                                    k_v_to_target_out,
                                    k_v_to_current_out,
                                    sigma_tot_target_out: community_total_degree[0][target_comm],
                                    sigma_tot_current_out,
                                    k_v_in,
                                    k_v_to_target_in,
                                    k_v_to_current_in,
                                    sigma_tot_target_in: if directed {
                                        community_in_degree[0][target_comm]
                                    } else {
                                        0.0
                                    },
                                    sigma_tot_current_in,
                                    n_target: community_size[target_comm],
                                    n_current: community_size[current_community],
                                    directed,
                                })
                            },
                        );

                        // Clear touched arrays for reuse by next node on this thread
                        for &comm in out_touched.iter() {
                            out_weights[comm] = 0.0;
                        }
                        out_touched.clear();
                        for &comm in in_touched.iter() {
                            in_weights[comm] = 0.0;
                        }
                        in_touched.clear();

                        if best_community != current_community {
                            Some((
                                node,
                                current_community,
                                best_community,
                                k_v_out,
                                k_v_in,
                                node_weight,
                            ))
                        } else {
                            None
                        }
                    },
                )
                .filter_map(|opt| opt)
                .collect();

            for (node, old_comm, new_comm, k_v_out, k_v_in, node_weight) in moves {
                algorithm::apply_move(
                    algorithm::MoveTarget::Partition(&mut *partition),
                    node,
                    old_comm,
                    new_comm,
                    algorithm::NodeContribution {
                        k_v_out: std::slice::from_ref(&k_v_out),
                        k_v_in: std::slice::from_ref(&k_v_in),
                        weight: node_weight,
                    },
                    &mut algorithm::CommunityStats {
                        total_degree_out: &mut community_total_degree,
                        sigma_in: &mut community_in_degree,
                        size: &mut community_size,
                    },
                );
                any_move = true;
                changed = true;
            }
        }
    }

    (changed, !any_move)
}

#[cfg(test)]
#[cfg(feature = "rayon")]
#[path = "parallel_tests.rs"]
mod tests;
