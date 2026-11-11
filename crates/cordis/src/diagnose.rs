//! Dependency-cycle diagnostics — the counterpart of cordis v4 issue
//! #66 ("detect circular plugin dependencies"), which upstream leaves
//! open: a cycle there pends forever, silently.
//!
//! A pending fiber waits for each missing required dependency. A missing
//! dependency is *claimable* when some installed fiber declares it in
//! [`Plugin::provides`](crate::Plugin::provides). When the claim edges
//! close a loop, none of the members can ever activate: each one's body
//! (the thing that actually provides) cannot run until the loop is
//! broken from outside. We report such groups once per shape, and expose
//! them through [`Context::dependency_cycles`](crate::Context::dependency_cycles).
//!
//! Detection is SCC-based (Tarjan): any strongly-connected component
//! with more than one member — or a single member claiming its own
//! missing dependency — is a stuck group. Satisfied dependencies produce
//! no edges, so a group is only reported when its members are genuinely
//! stuck.

use std::collections::HashMap;
use std::sync::Arc;

use crate::context::Core;
use crate::fiber::FiberState;
use crate::logger::LogLevel;

/// Groups of installed fibers that mutually wait for each other's
/// declared provides. Each group is a list of `name#id` labels (sorted);
/// empty unless `Plugin::provides` declarations close a loop among
/// pending fibers.
pub(crate) fn dependency_cycles(core: &Arc<Core>) -> Vec<Vec<String>> {
    let fibers = {
        let registry = core.registry.lock().unwrap();
        registry.all_fibers()
    };

    // Service name → fibers claiming it in `Plugin::provides`.
    let mut claims: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, fiber) in fibers.iter().enumerate() {
        for name in fiber.declared_provides() {
            claims.entry(name).or_default().push(i);
        }
    }

    // Edge i → j for every missing required dependency of i claimed by j.
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); fibers.len()];
    for (i, fiber) in fibers.iter().enumerate() {
        for (name, required) in fiber.inject_names() {
            if !required {
                continue;
            }
            let label = fiber.ctx().isolate_label(&name);
            let satisfied = core
                .store_impl(&name, &label)
                .and_then(|impl_| impl_.owner.upgrade())
                .map(|owner| owner.state() == FiberState::Active)
                .unwrap_or(false);
            if satisfied {
                continue;
            }
            for &j in claims.get(name.as_str()).into_iter().flatten() {
                edges[i].push(j);
            }
        }
    }

    // Tarjan SCC.
    let mut index = vec![None; fibers.len()];
    let mut low = vec![0u32; fibers.len()];
    let mut on_stack = vec![false; fibers.len()];
    let mut stack: Vec<usize> = Vec::new();
    let mut counter = 0u32;
    let mut components: Vec<Vec<usize>> = Vec::new();

    for start in 0..fibers.len() {
        if index[start].is_some() {
            continue;
        }
        // Iterative DFS: (node, next edge offset).
        let mut work: Vec<(usize, usize)> = vec![(start, 0)];
        while let Some(&mut (node, ref mut offset)) = work.last_mut() {
            if *offset == 0 {
                index[node] = Some(counter);
                low[node] = counter;
                counter += 1;
                stack.push(node);
                on_stack[node] = true;
            }
            let children = &edges[node];
            if *offset < children.len() {
                let next = children[*offset];
                *offset += 1;
                if index[next].is_none() {
                    work.push((next, 0));
                } else if on_stack[next] {
                    low[node] = low[node].min(index[next].unwrap());
                }
            } else {
                work.pop();
                if let Some(&(parent, _)) = work.last() {
                    low[parent] = low[parent].min(low[node]);
                }
                if low[node] == index[node].expect("visited node has an index") {
                    let mut component = Vec::new();
                    while let Some(member) = stack.pop() {
                        on_stack[member] = false;
                        component.push(member);
                        if member == node {
                            break;
                        }
                    }
                    components.push(component);
                }
            }
        }
    }

    let label_of = |i: usize| format!("{}#{}", fibers[i].name, fibers[i].id);
    let mut cycles: Vec<Vec<String>> = components
        .into_iter()
        .filter(|component| {
            component.len() > 1 || edges[component[0]].contains(&component[0]) // self-loop
        })
        .map(|mut component| {
            component.sort_unstable();
            component.into_iter().map(label_of).collect()
        })
        .collect();
    cycles.sort();
    cycles
}

/// Report newly-detected cycles on the diagnostic logger, once per group
/// shape (so a persisting loop does not spam on every notify).
pub(crate) fn report_cycles(core: &Arc<Core>) {
    let cycles = dependency_cycles(core);
    if cycles.is_empty() {
        return;
    }
    let fresh: Vec<&Vec<String>> = {
        let mut seen = core.reported_cycles.lock().unwrap();
        cycles
            .iter()
            .filter(|group| seen.insert(group.join("|")))
            .collect()
    };
    for group in fresh {
        core.logger.log(
            LogLevel::Warn,
            "cordis/diagnose",
            &format!(
                "dependency cycle: {} cannot ever activate (mutually waiting on declared provides); \
                 break the loop by providing one of the services elsewhere",
                group.join(" -> ")
            ),
        );
    }
}
