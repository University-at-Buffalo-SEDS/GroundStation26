//! Project observed radio paths without altering the SDK's learned routes.
use crate::rrls::TopologyObservation;
use crate::types::{
    NetworkTopologyLink, NetworkTopologyNode, NetworkTopologyNodeKind, NetworkTopologyStatus,
};

pub(crate) fn project(
    nodes: &mut Vec<NetworkTopologyNode>,
    links: &mut Vec<NetworkTopologyLink>,
    now_ms: u64,
    observed: TopologyObservation,
) {
    let Some(gs) = nodes
        .iter()
        .find(|n| n.group == "local")
        .map(|n| n.id.clone())
    else {
        return;
    };
    let rf = nodes
        .iter()
        .find(|n| n.sender_id.as_deref() == Some("RF"))
        .map(|n| n.id.clone());
    // A logical SDK connection is not evidence of a working direct radio path.
    if observed.relaying || !observed.direct_age_ms.is_some_and(|age| age < 3_000) {
        if let Some(rf) = &rf {
            links.retain(|l| {
                !((l.source == gs && l.target == *rf) || (l.target == gs && l.source == *rf))
            });
        }
    }
    let Some(age) = observed.peer_age_ms else {
        return;
    };
    let online = age < 15_000;
    let status = if online {
        NetworkTopologyStatus::Online
    } else {
        NetworkTopologyStatus::Offline
    };
    let mode = if !online {
        "Offline"
    } else if observed.relaying {
        "Relaying"
    } else {
        "Standby"
    };
    let id = "board_rrls".to_string();
    let node = NetworkTopologyNode {
        id: id.clone(),
        label: format!("RRLS · {mode}"),
        kind: NetworkTopologyNodeKind::Board,
        status,
        group: "recovery".into(),
        sender_id: Some("RRLS".into()),
        last_seen_ms: Some(now_ms.saturating_sub(age)),
        age_ms: Some(age),
        endpoints: Vec::new(),
        show_in_details: true,
        detail: Some(if observed.relaying && !observed.command_ready {
            "Relayed telemetry received; command readiness unconfirmed".into()
        } else {
            format!("Recovery radio: {}", mode.to_lowercase())
        }),
        stats: None,
    };
    nodes.retain(|n| n.id != id);
    nodes.push(node);
    links.retain(|l| l.source != id && l.target != id);
    links.push(NetworkTopologyLink {
        source: gs,
        target: id.clone(),
        label: Some(mode.to_lowercase()),
        status,
    });
    if online && observed.relaying {
        if let Some(rf) = rf {
            links.push(NetworkTopologyLink {
                source: id,
                target: rf,
                label: Some("relay".into()),
                status,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn node(sender: &str, local: bool) -> NetworkTopologyNode {
        NetworkTopologyNode {
            id: format!("board_{}", sender.to_lowercase()),
            label: sender.into(),
            kind: if local {
                NetworkTopologyNodeKind::Router
            } else {
                NetworkTopologyNodeKind::Board
            },
            status: NetworkTopologyStatus::Online,
            group: if local { "local" } else { "board" }.into(),
            sender_id: Some(sender.into()),
            last_seen_ms: None,
            age_ms: None,
            endpoints: vec![],
            show_in_details: true,
            detail: None,
            stats: None,
        }
    }
    fn snapshot(o: TopologyObservation) -> (Vec<NetworkTopologyNode>, Vec<NetworkTopologyLink>) {
        let mut nodes = vec![node("GS", true), node("RF", false), node("FC", false)];
        let mut links = vec![("gs", "rf"), ("rf", "fc")]
            .into_iter()
            .map(|(a, b)| NetworkTopologyLink {
                source: format!("board_{a}"),
                target: format!("board_{b}"),
                label: None,
                status: NetworkTopologyStatus::Online,
            })
            .collect();
        project(&mut nodes, &mut links, 30_000, o);
        (nodes, links)
    }
    fn edge(links: &[NetworkTopologyLink], a: &str, b: &str) -> bool {
        links
            .iter()
            .any(|l| l.source == format!("board_{a}") && l.target == format!("board_{b}"))
    }
    #[test]
    fn direct_relay_restore_and_peer_expiry_follow_observations() {
        let mut o = TopologyObservation {
            peer_age_ms: None,
            direct_age_ms: Some(0),
            relaying: false,
            command_ready: false,
        };
        let (nodes, links) = snapshot(o);
        assert!(edge(&links, "gs", "rf"));
        assert!(!nodes.iter().any(|n| n.id == "board_rrls"));
        o.peer_age_ms = Some(0);
        o.direct_age_ms = None;
        o.relaying = true;
        let (nodes, links) = snapshot(o);
        assert!(!edge(&links, "gs", "rf"));
        assert!(edge(&links, "gs", "rrls"));
        assert!(edge(&links, "rrls", "rf"));
        assert!(edge(&links, "rf", "fc"));
        assert!(nodes.iter().any(|n| {
            n.detail
                .as_deref()
                .is_some_and(|d| d.contains("unconfirmed"))
        }));
        o.direct_age_ms = Some(0);
        o.relaying = false;
        let (nodes, links) = snapshot(o);
        assert!(edge(&links, "gs", "rf"));
        assert!(!edge(&links, "rrls", "rf"));
        assert!(nodes.iter().any(|n| n.label == "RRLS · Standby"));
        o.peer_age_ms = Some(15_000);
        o.direct_age_ms = None;
        let (nodes, links) = snapshot(o);
        assert!(!edge(&links, "gs", "rf"));
        assert!(!edge(&links, "rrls", "rf"));
        assert!(
            nodes
                .iter()
                .any(|n| n.id == "board_rrls" && n.status == NetworkTopologyStatus::Offline)
        );
    }
}
