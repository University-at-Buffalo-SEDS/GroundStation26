use leptos::prelude::*;

use super::WarningRow;

#[component]
pub fn WarningsTab(rows: Signal<Vec<WarningRow>>) -> impl IntoView {
    // Sorted view (most recent first)
    let sorted_rows = Signal::derive(move || {
        let mut list = rows.get();
        // Newest first
        list.sort_by_key(|r| -r.timestamp_ms);
        list
    });

    view! {
        <div style="
            display:flex;
            flex-direction:column;
            gap:0.75rem;
            flex:1;
        ">
            <div style="
                display:flex;
                justify-content:space-between;
                align-items:center;
                margin-bottom:0.5rem;
            ">
                <h2 style="font-size:1.1rem; color:#facc15; margin:0;">
                    "Warnings"
                </h2>
            </div>

            <div style="
                max-height:360px;
                overflow:auto;
                display:flex;
                flex-direction:column;
                gap:0.4rem;
            ">
                <Show
                    // If there *are* rows, show the list (fallback).
                    // If empty, show the "No active warnings" message (children).
                    when=move || sorted_rows.get().is_empty()
                    fallback=move || {
                        let list = sorted_rows.get();
                        list
                            .into_iter()
                            .map(|r| view! { <WarningRowItem row=r /> })
                            .collect_view()
                    }
                >
                    <p style="color:#f9fafb; font-size:0.85rem; margin:0;">
                        "No active warnings."
                    </p>
                </Show>
            </div>
        </div>
    }
}

#[component]
fn WarningRowItem(row: WarningRow) -> impl IntoView {
    view! {
        <div style="
            padding:0.45rem 0.7rem;
            border-radius:0.5rem;
            background:#1f2937;
            border:1px solid #4b5563;
            display:flex;
            flex-direction:column;
            gap:0.25rem;
        ">
            <div style="font-size:0.8rem; color:#facc15; font-weight:600;">
                "Warning"
            </div>
            <div style="font-size:0.9rem; color:#f9fafb;">
                {row.message}
            </div>
        </div>
    }
}
