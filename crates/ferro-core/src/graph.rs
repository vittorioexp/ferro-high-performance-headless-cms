use std::collections::{BTreeMap, BTreeSet};

/// Deterministic topological order. `deps` contains `(a, b)` meaning "a depends on b"
/// (b must come first). Nodes in cycles are appended in name order: the importer
/// runs with deferred constraints, so cycles are still safe.
pub fn topo_order(nodes: &[String], deps: &[(String, String)]) -> Vec<String> {
    let node_set: BTreeSet<&str> = nodes.iter().map(|s| s.as_str()).collect();
    let mut incoming: BTreeMap<&str, BTreeSet<&str>> = node_set.iter().map(|n| (*n, BTreeSet::new())).collect();
    for (a, b) in deps {
        if a != b && node_set.contains(a.as_str()) && node_set.contains(b.as_str()) {
            incoming.get_mut(a.as_str()).unwrap().insert(b.as_str());
        }
    }
    let mut out: Vec<String> = Vec::with_capacity(nodes.len());
    let mut done: BTreeSet<&str> = BTreeSet::new();
    loop {
        let ready: Vec<&str> = incoming
            .iter()
            .filter(|(n, deps)| !done.contains(*n) && deps.iter().all(|d| done.contains(d)))
            .map(|(n, _)| *n)
            .collect();
        if ready.is_empty() {
            break;
        }
        for n in ready {
            done.insert(n);
            out.push(n.to_string());
        }
    }
    for n in &node_set {
        if !done.contains(n) {
            out.push(n.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn orders_dependencies_first() {
        let order = topo_order(
            &s(&["pages_blocks", "pages", "categories", "block_hero"]),
            &[
                ("pages_blocks".into(), "pages".into()),
                ("pages".into(), "categories".into()),
                ("pages_blocks".into(), "block_hero".into()),
            ],
        );
        let pos = |n: &str| order.iter().position(|x| x == n).unwrap();
        assert!(pos("categories") < pos("pages"));
        assert!(pos("pages") < pos("pages_blocks"));
        assert!(pos("block_hero") < pos("pages_blocks"));
    }

    #[test]
    fn cycles_do_not_lose_nodes() {
        let order = topo_order(&s(&["a", "b", "c"]), &[("a".into(), "b".into()), ("b".into(), "a".into())]);
        assert_eq!(order.len(), 3);
        assert_eq!(order[0], "c");
    }
}
