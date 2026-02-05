use serde::{Deserialize, Serialize};

use super::types::FlightState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutConfig {
    pub version: u32,
    pub connection_tab: ConnectionTabLayout,
    pub actions_tab: ActionsTabLayout,
    pub data_tab: DataTabLayout,
    pub state_tab: StateTabLayout,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionTabLayout {
    pub sections: Vec<ConnectionSection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionSection {
    pub kind: ConnectionSectionKind,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionSectionKind {
    BoardStatus,
    Latency,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataTabLayout {
    pub tabs: Vec<DataTabSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataTabSpec {
    pub id: String,
    pub label: String,
    pub channels: Vec<String>,
    pub chart: Option<DataTabChart>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataTabChart {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionsTabLayout {
    pub actions: Vec<ActionSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionSpec {
    pub label: String,
    pub cmd: String,
    pub border: String,
    pub bg: String,
    pub fg: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateTabLayout {
    pub states: Vec<StateLayout>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateLayout {
    pub states: Vec<FlightState>,
    pub sections: Vec<StateSection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateSection {
    pub title: Option<String>,
    pub widgets: Vec<StateWidget>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateWidget {
    pub kind: StateWidgetKind,
    pub data_type: Option<String>,
    pub items: Option<Vec<SummaryItem>>,
    pub chart_title: Option<String>,
    pub width: Option<f64>,
    pub height: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryItem {
    pub label: String,
    pub index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StateWidgetKind {
    BoardStatus,
    Summary,
    Chart,
    ValveState,
    Map,
    Actions,
}

#[cfg(test)]
mod tests {
    use super::LayoutConfig;

    #[test]
    fn parses_layout_endpoint_payload() {
        let payload = r##"{
            "version": 1,
            "connection_tab": {
                "sections": [
                    { "kind": "board_status", "title": "Board Status" }
                ]
            },
            "actions_tab": {
                "actions": [
                    {
                        "label": "Launch",
                        "cmd": "Launch",
                        "border": "#22c55e",
                        "bg": "#022c22",
                        "fg": "#bbf7d0"
                    }
                ]
            },
            "data_tab": {
                "tabs": [
                    {
                        "id": "GYRO_DATA",
                        "label": "GYRO_DATA",
                        "channels": ["Roll", "Pitch", "Yaw"],
                        "chart": { "enabled": true }
                    }
                ]
            },
            "state_tab": {
                "states": [
                    {
                        "states": ["Startup"],
                        "sections": [
                            {
                                "title": "Connected Devices",
                                "widgets": [
                                    { "kind": "board_status" }
                                ]
                            }
                        ]
                    }
                ]
            }
        }"##;

        let layout: LayoutConfig = serde_json::from_str(payload).expect("valid layout payload");

        assert_eq!(layout.version, 1);
        assert_eq!(layout.connection_tab.sections.len(), 1);
        assert_eq!(layout.actions_tab.actions.len(), 1);
        assert_eq!(layout.data_tab.tabs[0].channels.len(), 3);
        assert_eq!(layout.state_tab.states.len(), 1);
    }
}
