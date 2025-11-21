use super::ErrorRow;
use leptos::prelude::*;

#[component]
pub fn ErrorsTab(rows: Signal<Vec<ErrorRow>>) -> impl IntoView {
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
                <h2 style="font-size:1.1rem; color:#fecaca; margin:0;">
                    "Errors"
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
                    when=move || sorted_rows.get().is_empty()
                    fallback=move || {
                        let list = sorted_rows.get();
                        list
                            .into_iter()
                            .map(|r| view! { <ErrorRowItem row=r /> })
                            .collect_view()
                    }
                >
                    <p style="color:#f9fafb; font-size:0.85rem; margin:0;">
                        "No active errors."
                    </p>
                </Show>
            </div>
        </div>
    }
}

#[component]
fn ErrorRowItem(row: ErrorRow) -> impl IntoView {
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
            <div style="font-size:0.8rem; color:#fecaca; font-weight:600;">
                "Error"
            </div>
            <div style="font-size:0.9rem; color:#f9fafb;">
                {row.message}
            </div>
        </div>
    }
}
