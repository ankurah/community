//! Event-DAG layout and SVG rendering for the L1 inspector.
//!
//! ankurah-core's own `event_dag` module (compare / EventLayer /
//! topo_sort_events) is `pub(crate)` in 0.9.0, so x-ray re-derives a display
//! layout client-side: Kahn topo-sort with longest-path layering (x axis) and
//! greedy lane assignment (y axis). Events are content-addressed and acyclic
//! by construction; a message's history is 1–50 nodes, so this is
//! microseconds, not a rendering problem.
//!
//! Rendered as inline SVG (not canvas): it inherits the CSS token system for
//! light/dark, gives hover/`<title>`/text/a11y for free, and SVG is untroubled
//! by thousands of static elements (we cap render at 200 anyway).

use std::collections::{BTreeMap, HashMap};

use leptos::prelude::*;

use ankurah::proto::{Clock, EventId};

use super::decode::OpBadge;

/// Beyond this many events we render only the newest layers (linear-chain
/// compression is a v1 nicety; messages are tens of events at most).
const MAX_RENDER: usize = 200;

const DX: f64 = 92.0; // layer spacing (x)
const DY: f64 = 48.0; // lane spacing (y)
const PAD_X: f64 = 40.0;
const PAD_Y: f64 = 34.0;

/// Everything the inspector knows about one event, pre-layout.
#[derive(Clone, Debug, PartialEq)]
pub struct DagNodeInput {
    pub id: EventId,
    pub parent: Clock,
    pub badges: Vec<OpBadge>,
    /// `None` when the event body arrived via the remote walker (payload only).
    pub attestations: Option<usize>,
    /// True if fetched from a durable peer rather than found locally.
    pub fetched: bool,
    /// LWW properties whose *current* value this event wrote (message-only
    /// cross-reference via `LWWBackend::get_event_id`).
    pub wrote_current: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DagNodeVis {
    pub input: DagNodeInput,
    pub x: f64,
    pub y: f64,
    pub short: String,
    pub is_create: bool,
    pub is_tip: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DagEdgeVis {
    /// child id + parent id (keying), plus the cubic Bézier path.
    pub from: EventId,
    pub to: EventId,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct DagModel {
    pub nodes: Vec<DagNodeVis>,
    pub edges: Vec<DagEdgeVis>,
    pub width: f64,
    pub height: f64,
    /// Events omitted because of the render cap (0 in any sane message).
    pub omitted: usize,
    /// Parent ids referenced but not present (unfetched history).
    pub dangling: usize,
}

/// Topo-layer + lane layout. `head` marks the current tips (mint ring).
pub fn layout(mut inputs: Vec<DagNodeInput>, head: &Clock) -> DagModel {
    if inputs.is_empty() {
        return DagModel::default();
    }

    // Deterministic base order (content hashes — arbitrary but stable).
    inputs.sort_by(|a, b| a.id.cmp(&b.id));
    let mut present: HashMap<EventId, usize> = inputs.iter().enumerate().map(|(i, n)| (n.id.clone(), i)).collect();

    // Kahn over present-parent edges; layer = longest path from any root.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); inputs.len()];
    let mut pending_parents: Vec<usize> = vec![0; inputs.len()];
    let mut dangling = 0usize;
    for (i, node) in inputs.iter().enumerate() {
        for parent in node.parent.iter() {
            match present.get(parent) {
                Some(&p) => {
                    children[p].push(i);
                    pending_parents[i] += 1;
                }
                None => {
                    if !node.parent.is_empty() {
                        dangling += 1;
                    }
                }
            }
        }
    }

    let mut layer: Vec<usize> = vec![0; inputs.len()];
    let mut queue: Vec<usize> = (0..inputs.len()).filter(|&i| pending_parents[i] == 0).collect();
    let mut order: Vec<usize> = Vec::with_capacity(inputs.len());
    while let Some(i) = queue.pop() {
        order.push(i);
        for &c in &children[i] {
            layer[c] = layer[c].max(layer[i] + 1);
            pending_parents[c] -= 1;
            if pending_parents[c] == 0 {
                queue.push(c);
            }
        }
    }
    // Content addressing makes cycles impossible; if data were corrupt enough
    // to produce one, render what resolved and count the rest as omitted.
    let mut omitted = inputs.len() - order.len();

    // Render cap: keep the layers closest to the head (newest history).
    let keep_from_layer = if inputs.len() > MAX_RENDER {
        let mut layers: Vec<usize> = order.iter().map(|&i| layer[i]).collect();
        layers.sort_unstable_by(|a, b| b.cmp(a));
        layers.get(MAX_RENDER - 1).copied().unwrap_or(0)
    } else {
        0
    };

    // Greedy lanes, processing layer-by-layer in topo order: inherit the
    // first free parent lane; forks take fresh lanes; merges join the
    // smallest parent lane.
    let mut by_layer: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &i in &order {
        if layer[i] >= keep_from_layer {
            by_layer.entry(layer[i]).or_default().push(i);
        } else {
            omitted += 1;
        }
    }
    let mut lane: HashMap<EventId, usize> = HashMap::new();
    let mut max_lane = 0usize;
    for indices in by_layer.values() {
        let mut used: Vec<usize> = Vec::new();
        for &i in indices {
            let parent_lanes: Vec<usize> =
                inputs[i].parent.iter().filter_map(|p| lane.get(p).copied()).collect();
            let mut candidate = parent_lanes.iter().copied().min().unwrap_or(0);
            while used.contains(&candidate) {
                candidate += 1;
            }
            used.push(candidate);
            max_lane = max_lane.max(candidate);
            lane.insert(inputs[i].id.clone(), candidate);
        }
    }

    // Drop entries for nodes outside the cap so edges don't reference them.
    present.retain(|_, &mut i| layer[i] >= keep_from_layer && lane.contains_key(&inputs[i].id));

    let min_layer = keep_from_layer;
    let mut nodes: Vec<DagNodeVis> = Vec::new();
    let mut edges: Vec<DagEdgeVis> = Vec::new();
    for indices in by_layer.values() {
        for &i in indices {
            let node = &inputs[i];
            let x = PAD_X + ((layer[i] - min_layer) as f64) * DX;
            let y = PAD_Y + (lane[&node.id] as f64) * DY;
            for parent in node.parent.iter() {
                if let Some(&p) = present.get(parent) {
                    let px = PAD_X + ((layer[p] - min_layer) as f64) * DX;
                    let py = PAD_Y + (lane[&inputs[p].id] as f64) * DY;
                    edges.push(DagEdgeVis {
                        from: parent.clone(),
                        to: node.id.clone(),
                        path: format!(
                            "M {:.1} {:.1} C {:.1} {:.1}, {:.1} {:.1}, {:.1} {:.1}",
                            px,
                            py,
                            px + DX * 0.5,
                            py,
                            x - DX * 0.5,
                            y,
                            x,
                            y
                        ),
                    });
                }
            }
            nodes.push(DagNodeVis {
                short: node.id.to_base64_short(),
                is_create: node.parent.is_empty(),
                is_tip: head.contains(&node.id),
                x,
                y,
                input: node.clone(),
            });
        }
    }

    let max_layer = by_layer.keys().last().copied().unwrap_or(min_layer);
    DagModel {
        nodes,
        edges,
        width: PAD_X * 2.0 + ((max_layer - min_layer) as f64) * DX,
        height: PAD_Y * 2.0 + (max_lane as f64) * DY + 14.0,
        omitted,
        dangling,
    }
}

/// Pure SVG rendering of a laid-out DAG. Scrolls horizontally inside its own
/// container; clicking a node selects it for the detail pane.
#[component]
pub fn DagView(model: DagModel, selected: RwSignal<Option<EventId>>) -> impl IntoView {
    let DagModel { nodes, edges, width, height, omitted, dangling } = model;

    view! {
        <div class="xrayDagWrap">
            <svg
                class="xrayDag"
                width=format!("{:.0}", width)
                height=format!("{:.0}", height)
                viewBox=format!("0 0 {:.0} {:.0}", width, height)
                role="img"
                aria-label="Event DAG"
            >
                {edges
                    .into_iter()
                    .map(|edge| view! { <path class="xrayEdge" d=edge.path /> })
                    .collect_view()}
                {nodes
                    .into_iter()
                    .map(|node| {
                        let id = node.input.id.clone();
                        let id_for_click = id.clone();
                        let id_for_class = id.clone();
                        let title = format!(
                            "{}\nparents: {}\n{}",
                            id.to_base64(),
                            if node.input.parent.is_empty() {
                                "(none — creation event)".to_string()
                            } else {
                                node.input.parent.to_base64_short()
                            },
                            node.input
                                .badges
                                .iter()
                                .map(|b| format!("{} {} ({} B)", b.backend, b.summary, b.bytes))
                                .collect::<Vec<_>>()
                                .join(" · "),
                        );
                        let summary_line = node
                            .input
                            .badges
                            .iter()
                            .map(|b| b.summary.clone())
                            .collect::<Vec<_>>()
                            .join(" ");
                        view! {
                            <g
                                class="xrayNode"
                                class:xrayNodeCreate=node.is_create
                                class:xrayNodeTip=node.is_tip
                                class:xrayNodeFetched=node.input.fetched
                                class:xrayNodeSelected=move || selected.get().as_ref() == Some(&id_for_class)
                                transform=format!("translate({:.1},{:.1})", node.x, node.y)
                                on:click=move |_| selected.set(Some(id_for_click.clone()))
                            >
                                <title>{title}</title>
                                <circle r="8" />
                                {node.is_create.then(|| view! {
                                    // Tiny sprout mark inside the creation node.
                                    <path class="xraySprout" d="M0 3 C 2 -1 1 -3 -1 -4 M0 3 C -2 0 -3 -1 -4 -1" />
                                })}
                                <text class="xrayNodeOps" y="-14" text-anchor="middle">{summary_line}</text>
                                <text class="xrayNodeLabel" y="24" text-anchor="middle">{node.short.clone()}</text>
                            </g>
                        }
                    })
                    .collect_view()}
            </svg>
            {(omitted > 0 || dangling > 0)
                .then(|| view! {
                    <p class="xrayDagNote">
                        {(omitted > 0).then(|| format!("{} older events not rendered. ", omitted))}
                        {(dangling > 0)
                            .then(|| format!("{} parent reference(s) not fetched (older history).", dangling))}
                    </p>
                })}
        </div>
    }
}
