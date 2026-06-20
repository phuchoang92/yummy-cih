//! Community partition representation.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Tracks community membership for each node in the graph.
/// `community[i]` = community ID of node i.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Partition {
    /// `community[node_index]` = community_id
    community: Vec<usize>,
    /// Number of distinct communities
    num_communities: usize,
}

impl Partition {
    /// Create a singleton partition where each node is in its own community.
    #[must_use = "constructor returns a new instance"]
    pub fn new(n: usize) -> Self {
        Self {
            community: (0..n).collect(),
            num_communities: n,
        }
    }

    /// Create a partition from an existing membership mapping.
    #[must_use = "constructor returns a new instance"]
    pub fn from_membership(membership: Vec<usize>) -> Self {
        let num_communities = if membership.is_empty() {
            0
        } else {
            *membership.iter().max().expect("membership is non-empty (checked above)") + 1
        };
        Self {
            community: membership,
            num_communities,
        }
    }

    /// Get the community ID of a node.
    #[inline]
    pub fn community_of(&self, node: usize) -> usize {
        self.community[node]
    }

    /// Move a node to a different community.
    pub fn move_node(&mut self, node: usize, new_community: usize) {
        self.community[node] = new_community;
        if new_community >= self.num_communities {
            self.num_communities = new_community + 1;
        }
    }

    /// Number of distinct communities.
    pub fn num_communities(&self) -> usize {
        self.num_communities
    }

    /// Collect all nodes in a given community.
    pub fn nodes_in_community(&self, community: usize) -> Vec<usize> {
        self.community
            .iter()
            .enumerate()
            .filter_map(|(node, &comm)| if comm == community { Some(node) } else { None })
            .collect()
    }

    /// Get the size of each community as a vector.
    /// `sizes[c]` = number of nodes in community c.
    pub fn community_sizes(&self) -> Vec<usize> {
        if self.community.is_empty() {
            return vec![];
        }
        let max_comm = *self.community.iter().max().expect("partition is non-empty (checked above)");
        let mut sizes = vec![0usize; max_comm + 1];
        for &comm in &self.community {
            sizes[comm] += 1;
        }
        sizes
    }

    /// Renumber communities to be contiguous 0..k.
    pub fn renumber(&mut self) {
        if self.community.is_empty() {
            self.num_communities = 0;
            return;
        }

        let max_comm = self.community.iter().copied().max().unwrap_or(0);
        let mut mapping: Vec<usize> = vec![usize::MAX; max_comm + 1];
        let mut next_id = 0usize;

        for comm in self.community.iter_mut() {
            if mapping[*comm] == usize::MAX {
                mapping[*comm] = next_id;
                next_id += 1;
            }
            *comm = mapping[*comm];
        }

        self.num_communities = next_id;
    }

    /// Get the raw membership slice.
    pub fn as_slice(&self) -> &[usize] {
        &self.community
    }

    /// Number of nodes in the partition.
    pub fn len(&self) -> usize {
        self.community.len()
    }

    /// Whether the partition is empty.
    pub fn is_empty(&self) -> bool {
        self.community.is_empty()
    }

    /// Group nodes by community, returning `(community_id, Vec<node>)` pairs.
    pub fn communities(&self) -> Vec<(usize, Vec<usize>)> {
        if self.community.is_empty() {
            return Vec::new();
        }
        let max_comm = *self.community.iter().max().unwrap_or(&0);
        let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); max_comm + 1];
        for (node, &comm) in self.community.iter().enumerate() {
            buckets[comm].push(node);
        }
        buckets
            .into_iter()
            .enumerate()
            .filter(|(_, nodes)| !nodes.is_empty())
            .collect()
    }

    /// Iterate over all `(node, community)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.community.iter().copied().enumerate()
    }
}

impl AsRef<[usize]> for Partition {
    fn as_ref(&self) -> &[usize] {
        &self.community
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_membership_empty() {
        let p = Partition::from_membership(vec![0usize; 0]);
        assert_eq!(p.num_communities(), 0);
        assert!(p.community.is_empty());
    }

    #[test]
    fn from_membership_singleton() {
        let p = Partition::from_membership(vec![0]);
        assert_eq!(p.num_communities(), 1);
        assert_eq!(p.community_of(0), 0);
    }

    #[test]
    fn from_membership_multiple_communities() {
        let p = Partition::from_membership(vec![0, 0, 1, 1, 2]);
        assert_eq!(p.num_communities(), 3);
        assert_eq!(p.nodes_in_community(0), vec![0, 1]);
        assert_eq!(p.nodes_in_community(1), vec![2, 3]);
        assert_eq!(p.nodes_in_community(2), vec![4]);
    }

    #[test]
    fn community_sizes_empty() {
        let p = Partition::from_membership(vec![0usize; 0]);
        assert_eq!(p.community_sizes(), Vec::<usize>::new());
    }

    #[test]
    fn community_sizes_all_in_one() {
        let p = Partition::from_membership(vec![0, 0, 0]);
        assert_eq!(p.community_sizes(), vec![3]);
    }

    #[test]
    fn nodes_in_community_gap() {
        // Community 1 exists in the range (0..3) but has no members
        let p = Partition::from_membership(vec![0, 0, 2]);
        assert_eq!(p.nodes_in_community(1), Vec::<usize>::new());
    }

    #[test]
    fn nodes_in_community_out_of_range() {
        // Query a community ID beyond the partition's range
        let p = Partition::from_membership(vec![0, 0, 1, 1]);
        assert_eq!(p.nodes_in_community(99), Vec::<usize>::new());
    }
}
