// frontend/src/telemetry_dashboard/actions_tab.rs

use dioxus::prelude::*;

use super::layout::ActionsTabLayout;

fn btn_style(border: &str, bg: &str, fg: &str) -> String {
    format!(
        "padding:0.65rem 1rem; border-radius:0.75rem; cursor:pointer; width:100%; \
         text-align:left; border:1px solid {border}; background:{bg}; color:{fg}; \
         font-weight:800; box-shadow:0 10px 25px rgba(0,0,0,0.25);"
    )
}

#[component]
pub fn ActionsTab(layout: ActionsTabLayout) -> Element {
    rsx! {
        div {
            style: "
                padding:16px;
                display:flex;
                flex-direction:column;
                gap:12px;
            ",
            h2 { style: "margin:0 0 8px 0; color:#e5e7eb;", "Actions" }
            p  { style: "margin:0 0 12px 0; color:#9ca3af; font-size:0.9rem;",
                "All available actions are available all the time, use with caution as improper use \
                can and will damage the system."
            }

            div {
                style: "
                    display:grid;
                    grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
                    gap:12px;
                ",

                for action in layout.actions.iter() {
                    button {
                        style: "{btn_style(&action.border, &action.bg, &action.fg)}",
                        onclick: {
                            let cmd = action.cmd.clone();
                            move |_| crate::telemetry_dashboard::send_cmd(&cmd)
                        },
                        "{action.label}"
                    }
                }
            }
        }
    }
}
