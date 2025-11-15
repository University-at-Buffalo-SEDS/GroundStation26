use leptos::prelude::*;

#[component]
pub fn App() -> impl IntoView {
    view! {
        <main
            style="
                min-height: 100vh;
                margin: 0;
                padding: 1.5rem;
                background-color: #020617;  /* near-black navy */
                color: #e5e7eb;             /* light gray text */
                font-family: system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
            "
        >
            <h1 style="color: #f97316; margin-bottom: 0.75rem;">
                "Ground Station 26"
            </h1>
            <p style="margin-bottom: 0.5rem;">
                "Dummy frontend is running in dark mode. "
                "Backend and database can be wired in later."
            </p>
            <p style="color: #9ca3af;">
                "Telemetry router / sedsprintf decoding is not hooked up yet, "
                "so this UI is just a placeholder."
            </p>
        </main>
    }
}
