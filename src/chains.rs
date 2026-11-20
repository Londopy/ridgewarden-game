//! A team's chains: its sites joined by up links, directly or through relays (nodes that
//! aren't game sites). Only the links on some path between two of the team's sites
//! belong to a chain; relay branches and loops that lead nowhere are left off.

use std::collections::{BTreeMap, BTreeSet};

use ridgewarden_client::graph::{Edge, Graph, edge};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Chain {
    pub team: String,
    pub sites: BTreeSet<String>,
    /// the links that hold it together, relays included
    pub links: BTreeSet<Edge>,
    pub new_links: BTreeSet<Edge>,
}

/// The biconnected blocks of a graph, each as its edges (Tarjan, iterative so a long
/// chain of relays can't overflow the stack).
fn blocks(g: &Graph) -> Vec<BTreeSet<Edge>> {
    let names: Vec<&String> = g.adj.keys().collect();
    let ix: BTreeMap<&String, usize> = names.iter().enumerate().map(|(i, n)| (*n, i)).collect();
    let adj: Vec<Vec<usize>> = names
        .iter()
        .map(|n| g.adj[*n].iter().map(|m| ix[m]).collect())
        .collect();
    let mut disc = vec![usize::MAX; names.len()];
    let mut low = vec![0; names.len()];
    let mut time = 0;
    let mut edges: Vec<(usize, usize)> = vec![];
    let mut out = vec![];
    for root in 0..names.len() {
        if disc[root] != usize::MAX {
            continue;
        }
        disc[root] = time;
        low[root] = time;
        time += 1;
        let mut stack = vec![(root, usize::MAX, 0usize)];
        while let Some(&mut (u, parent, ref mut i)) = stack.last_mut() {
            if let Some(&v) = adj[u].get(*i) {
                *i += 1;
                if v == parent {
                    continue;
                }
                if disc[v] == usize::MAX {
                    edges.push((u, v));
                    disc[v] = time;
                    low[v] = time;
                    time += 1;
                    stack.push((v, u, 0));
                } else if disc[v] < disc[u] {
                    edges.push((u, v));
                    low[u] = low[u].min(disc[v]);
                }
                continue;
            }
            stack.pop();
            if parent == usize::MAX {
                continue;
            }
            low[parent] = low[parent].min(low[u]);
            if low[u] >= disc[parent] {
                let mut block = BTreeSet::new();
                while let Some((a, b)) = edges.pop() {
                    block.insert(edge(names[a], names[b]));
                    if (a, b) == (parent, u) {
                        break;
                    }
                }
                out.push(block);
            }
        }
    }
    out
}

fn vertices(block: &BTreeSet<Edge>) -> BTreeSet<&String> {
    block.iter().flat_map(|(a, b)| [a, b]).collect()
}

/// Keep only the blocks on paths between held sites: repeatedly drop a block that hangs
/// off the rest by at most one node and holds no site of its own.
fn prune(mut live: Vec<BTreeSet<Edge>>, held: &dyn Fn(&str) -> bool) -> Vec<BTreeSet<Edge>> {
    loop {
        let mut uses: BTreeMap<String, usize> = BTreeMap::new();
        for b in &live {
            for v in vertices(b) {
                *uses.entry(v.clone()).or_default() += 1;
            }
        }
        let dead = live.iter().position(|b| {
            let vs = vertices(b);
            let shared = vs.iter().filter(|v| uses[v.as_str()] > 1).count();
            let own_site = vs.iter().any(|v| uses[v.as_str()] == 1 && held(v));
            live.len() > 1 && shared <= 1 && !own_site
        });
        match dead {
            Some(i) => {
                live.swap_remove(i);
            }
            None => return live,
        }
    }
}

/// A team's chains, given who holds what, the game's sites, and the links that are up.
pub fn chains(
    team: &str,
    holders: &BTreeMap<String, String>,
    sites: &[String],
    up: &BTreeSet<Edge>,
) -> Vec<Chain> {
    let held = |n: &str| holders.get(n).is_some_and(|t| t == team);
    let open = |n: &str| held(n) || !sites.iter().any(|s| s == n);
    let mut g = Graph::default();
    for (a, b) in up {
        if open(a) && open(b) {
            g.add_edge(a, b);
        }
    }
    let none = BTreeSet::new();
    let mut out = vec![];
    for comp in g.components(&none, &BTreeSet::new()) {
        let mine: BTreeSet<String> = comp.iter().filter(|n| held(n)).cloned().collect();
        if mine.len() < 2 {
            continue;
        }
        let mut sub = Graph::default();
        for (a, b) in up {
            if comp.contains(a) && comp.contains(b) && open(a) && open(b) {
                sub.add_edge(a, b);
            }
        }
        let links = prune(blocks(&sub), &held).into_iter().flatten().collect();
        out.push(Chain {
            team: team.to_string(),
            sites: mine,
            links,
            new_links: BTreeSet::new(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges(xs: &[(&str, &str)]) -> BTreeSet<Edge> {
        xs.iter().map(|(a, b)| edge(a, b)).collect()
    }

    fn holders(xs: &[(&str, &str)]) -> BTreeMap<String, String> {
        xs.iter()
            .map(|(s, t)| (s.to_string(), t.to_string()))
            .collect()
    }

    fn sites() -> Vec<String> {
        ["a", "b", "c"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn relays_join_a_chain_but_another_teams_site_breaks_it() {
        let h = holders(&[("a", "Red"), ("b", "Red"), ("c", "Blue")]);
        let up = edges(&[("a", "r"), ("r", "b"), ("r", "x")]);
        let ch = chains("Red", &h, &sites(), &up);
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].sites.len(), 2);
        assert_eq!(
            ch[0].links,
            edges(&[("a", "r"), ("r", "b")]),
            "dead-end relay x left off"
        );

        let through_blue = edges(&[("a", "c"), ("c", "b")]);
        assert!(chains("Red", &h, &sites(), &through_blue).is_empty());
        let unheld = holders(&[("a", "Red"), ("b", "Red")]);
        assert!(
            chains("Red", &unheld, &sites(), &through_blue).is_empty(),
            "a site nobody holds isn't a relay"
        );
        assert!(
            chains("Blue", &h, &sites(), &up).is_empty(),
            "a lone site is no chain"
        );
    }

    #[test]
    fn loops_that_lead_nowhere_are_left_off_but_real_loops_stay() {
        let h = holders(&[("a", "Red"), ("b", "Red")]);
        // a - r - b, with a dead-end loop r - x - y - r and a real second path a - s - b
        let up = edges(&[
            ("a", "r"),
            ("r", "b"),
            ("r", "x"),
            ("x", "y"),
            ("y", "r"),
            ("a", "s"),
            ("s", "b"),
        ]);
        let ch = chains("Red", &h, &sites(), &up);
        assert_eq!(
            ch[0].links,
            edges(&[("a", "r"), ("r", "b"), ("a", "s"), ("s", "b")])
        );
    }

    #[test]
    fn a_held_site_in_the_middle_keeps_both_sides() {
        let h = holders(&[("a", "Red"), ("b", "Red"), ("c", "Red")]);
        let up = edges(&[("a", "b"), ("b", "c"), ("c", "z")]);
        let ch = chains("Red", &h, &sites(), &up);
        assert_eq!(ch[0].links, edges(&[("a", "b"), ("b", "c")]));
        assert_eq!(ch[0].sites.len(), 3);
    }

    /// Every edge on some simple path between two held nodes, by brute force.
    fn on_paths(
        adj: &BTreeMap<String, BTreeSet<String>>,
        held: &BTreeSet<String>,
    ) -> BTreeSet<Edge> {
        fn walk(
            adj: &BTreeMap<String, BTreeSet<String>>,
            held: &BTreeSet<String>,
            path: &mut Vec<String>,
            out: &mut BTreeSet<Edge>,
        ) {
            let last = path.last().unwrap().clone();
            if path.len() > 1 && held.contains(&last) {
                for w in path.windows(2) {
                    out.insert(edge(&w[0], &w[1]));
                }
            }
            for n in adj.get(&last).into_iter().flatten() {
                if !path.contains(n) {
                    path.push(n.clone());
                    walk(adj, held, path, out);
                    path.pop();
                }
            }
        }
        let mut out = BTreeSet::new();
        for h in held {
            walk(adj, held, &mut vec![h.clone()], &mut out);
        }
        out
    }

    #[test]
    fn chain_links_are_exactly_the_links_on_paths_between_held_sites() {
        let mut seed: u64 = 7;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        let names: Vec<String> = "abcdefg".chars().map(|c| c.to_string()).collect();
        for _ in 0..500 {
            let mut up = BTreeSet::new();
            for i in 0..names.len() {
                for j in i + 1..names.len() {
                    if next() % 3 == 0 {
                        up.insert(edge(&names[i], &names[j]));
                    }
                }
            }
            // each node: held by Red, a site nobody holds, or a relay
            let mut holders = BTreeMap::new();
            let mut sites = vec![];
            for n in &names {
                match next() % 3 {
                    0 => {
                        holders.insert(n.clone(), "Red".to_string());
                        sites.push(n.clone());
                    }
                    1 => sites.push(n.clone()),
                    _ => {}
                }
            }
            let open = |n: &String| holders.contains_key(n) || !sites.contains(n);
            let mut adj: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for (a, b) in up.iter().filter(|(a, b)| open(a) && open(b)) {
                adj.entry(a.clone()).or_default().insert(b.clone());
                adj.entry(b.clone()).or_default().insert(a.clone());
            }
            let held: BTreeSet<String> = holders.keys().cloned().collect();
            let got: BTreeSet<Edge> = chains("Red", &holders, &sites, &up)
                .into_iter()
                .flat_map(|c| c.links)
                .collect();
            assert_eq!(got, on_paths(&adj, &held), "{up:?} {holders:?} {sites:?}");
        }
    }
}
