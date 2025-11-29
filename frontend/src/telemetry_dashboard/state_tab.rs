// state_tab.rs
use groundstation_shared::FlightState;
use leptos::prelude::*;

/// "Flight" overview tab – currently a placeholder using the existing markup.
#[component]
pub fn StateTab(flight_state: ReadSignal<FlightState>) -> impl IntoView {
    // Local string version of the state (this replaces `flight_state_str_sig`)
    let flight_state_str_sig = Signal::derive({
        let flight_state = flight_state;
        move || flight_state.get().to_string()
    });

    view! {
        <div style="
            display:flex;
            flex-direction:column;
            gap:0.75rem;
            padding:1rem;
            border-radius:0.75rem;
            background:#020617ee;
            border:1px solid #4b5563;
            box-shadow:0 10px 25px rgba(0,0,0,0.45);
        ">
            <h2 style="margin:0; color:#38bdf8;">"Flight Overview"</h2>
            <p style="margin:0; color:#e5e7eb;">
                "Current flight state: "
                {move || flight_state_str_sig.get()}
            </p>

            <Show when=move || matches!(flight_state.get(), FlightState::Startup)>
                <p style="margin:0; color:#9ca3af;">
                    "Vehicle is in Startup state. Pre-flight checks and configuration may be in progress."
                </p>
            </Show>

            <Show when=move || matches!(flight_state.get(), FlightState::Idle)>
                <p style="margin:0; color:#9ca3af;">
                    "Vehicle is idle. Standing by for arming and launch procedures."
                </p>
            </Show>

            <Show when=move || matches!(flight_state.get(), FlightState::Armed)>
                <p style="margin:0; color:#facc15;">
                    "Vehicle is ARMED. All safety procedures should be followed before initiating launch."
                </p>
            </Show>

            <Show when=move || matches!(flight_state.get(), FlightState::Ascent | FlightState::Coast)>
                <p style="margin:0; color:#22c55e;">
                    "Vehicle is in powered ascent or coasting phase. Monitor trajectory and key telemetry closely."
                </p>
            </Show>

            <Show when=move || matches!(flight_state.get(), FlightState::Descent | FlightState::Landed)>
                <p style="margin:0; color:#93c5fd;">
                    "Vehicle is descending or has landed. Post-flight operations and data review may begin soon."
                </p>
            </Show>
        </div>
    }
}
