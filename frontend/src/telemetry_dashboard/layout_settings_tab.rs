use super::{layout::ThemeConfig, localized_copy, set_preferred_language};
use dioxus::prelude::*;
use dioxus_signals::Signal;

#[component]
pub fn SettingsPage(
    distance_units_metric: Signal<bool>,
    theme_preset: Signal<String>,
    language_code: Signal<String>,
    network_flow_animation_enabled: Signal<bool>,
    theme: ThemeConfig,
    #[props(default)] title: Option<String>,
) -> Element {
    let language = language_code.read().clone();
    let title = title
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| localized_copy(&language, "Settings", "Ajustes", "Parametres"));
    let metric_enabled = *distance_units_metric.read();
    let selected_theme = theme_preset.read().clone();
    let selected_language = language_code.read().clone();
    let flow_animation_enabled = *network_flow_animation_enabled.read();

    let card_style = format!(
        "padding:16px; border-radius:14px; border:1px solid {}; background:{}; display:flex; flex-direction:column; gap:12px;",
        theme.border, theme.panel_background
    );
    let chip_selected = format!(
        "padding:8px 12px; border-radius:999px; border:1px solid {}; background:{}; color:{}; font-family:system-ui, -apple-system, BlinkMacSystemFont; font-size:0.9rem; font-weight:700; cursor:pointer;",
        theme.info_accent, theme.info_background, theme.text_primary
    );
    let chip_idle = format!(
        "padding:8px 12px; border-radius:999px; border:1px solid {}; background:{}; color:{}; font-family:system-ui, -apple-system, BlinkMacSystemFont; font-size:0.9rem; font-weight:600; cursor:pointer;",
        theme.border, theme.button_background, theme.text_secondary
    );

    let section_general = localized_copy(&language, "General", "General", "General");
    let section_appearance = localized_copy(&language, "Appearance", "Apariencia", "Apparence");
    let section_map = localized_copy(&language, "Map", "Mapa", "Carte");
    let section_network = localized_copy(&language, "Network", "Red", "Reseau");
    let language_title = localized_copy(&language, "Language", "Idioma", "Langue");
    let language_desc = localized_copy(
        &language,
        "Localizes dashboard tab labels, settings copy, and core chrome.",
        "Localiza las pestanas, los textos de ajustes y partes clave de la interfaz.",
        "Localise les onglets, les textes de configuration et les elements principaux.",
    );
    let theme_title = localized_copy(&language, "Theme Preset", "Tema", "Theme");
    let theme_desc = localized_copy(
        &language,
        "Choose between the built-in default theme, the backend theme, or local overrides.",
        "Elige entre el tema por defecto, el tema del backend o variantes locales.",
        "Choisissez entre le theme par defaut, le theme backend ou des variantes locales.",
    );
    let units_title = localized_copy(
        &language,
        "Distance Units",
        "Unidades de distancia",
        "Unites de distance",
    );
    let units_desc = localized_copy(
        &language,
        "Controls the rocket distance label and the live guide line readout on the map.",
        "Controla la distancia al cohete y la lectura de la linea guia en el mapa.",
        "Controle la distance vers la fusee et la lecture de la ligne guide sur la carte.",
    );
    let metric_label = localized_copy(&language, "Metric", "Metrico", "Metrique");
    let imperial_label = localized_copy(&language, "Imperial", "Imperial", "Imperial");
    let metric_hint = localized_copy(
        &language,
        "Meters below 1 km, then kilometers.",
        "Metros hasta 1 km y luego kilometros.",
        "Metres jusqu'a 1 km puis kilometres.",
    );
    let imperial_hint = localized_copy(
        &language,
        "Feet below 1000 ft, then miles.",
        "Pies hasta 1000 ft y luego millas.",
        "Pieds jusqu'a 1000 ft puis miles.",
    );
    let network_anim_title = localized_copy(
        &language,
        "Flow Animations",
        "Animaciones de flujo",
        "Animations de flux",
    );
    let network_anim_desc = localized_copy(
        &language,
        "Controls animated directional lanes on the network graph.",
        "Controla los carriles animados direccionales en el grafo de red.",
        "Controle les voies directionnelles animees sur le graphe reseau.",
    );
    let flow_on_label = localized_copy(&language, "On", "Activado", "Active");
    let flow_off_label = localized_copy(&language, "Off", "Desactivado", "Desactive");
    let english_label = "English".to_string();
    let spanish_label = "Español".to_string();
    let french_label = "Français".to_string();
    let default_theme_label = localized_copy(
        &language,
        "Default Theme",
        "Tema por defecto",
        "Theme par defaut",
    );
    let backend_theme_label = localized_copy(
        &language,
        "Backend Theme",
        "Tema del backend",
        "Theme backend",
    );
    let sunset_theme_label = localized_copy(&language, "Sunset", "Atardecer", "Coucher");
    let forest_theme_label = localized_copy(&language, "Forest", "Bosque", "Foret");
    let contrast_theme_label = localized_copy(
        &language,
        "High Contrast",
        "Alto contraste",
        "Contraste fort",
    );

    rsx! {
        div { style: "padding:16px; overflow:visible; font-family:system-ui, -apple-system, BlinkMacSystemFont; color:{theme.text_primary};",
            h2 { style: "margin:0 0 14px 0; color:{theme.text_primary};", "{title}" }

            div { style: "display:grid; grid-template-columns:repeat(auto-fit, minmax(280px, 1fr)); gap:12px;",
                div { style: "{card_style}",
                    div { style: "font-size:15px; color:{theme.text_primary}; font-weight:700;", "{section_general}" }
                    div { style: "font-size:13px; color:{theme.text_muted};", "{language_title}" }
                    div { style: "font-size:13px; color:{theme.text_soft};", "{language_desc}" }
                    div { style: "display:flex; gap:8px; flex-wrap:wrap;",
                        button {
                            style: if selected_language == "en" { chip_selected.clone() } else { chip_idle.clone() },
                            onclick: move |_| {
                                let code = "en".to_string();
                                language_code.set(code.clone());
                                set_preferred_language(&code);
                            },
                            "{english_label}"
                        }
                        button {
                            style: if selected_language == "es" { chip_selected.clone() } else { chip_idle.clone() },
                            onclick: move |_| {
                                let code = "es".to_string();
                                language_code.set(code.clone());
                                set_preferred_language(&code);
                            },
                            "{spanish_label}"
                        }
                        button {
                            style: if selected_language == "fr" { chip_selected.clone() } else { chip_idle.clone() },
                            onclick: move |_| {
                                let code = "fr".to_string();
                                language_code.set(code.clone());
                                set_preferred_language(&code);
                            },
                            "{french_label}"
                        }
                    }
                }

                div { style: "{card_style}",
                    div { style: "font-size:15px; color:{theme.text_primary}; font-weight:700;", "{section_appearance}" }
                    div { style: "font-size:13px; color:{theme.text_muted};", "{theme_title}" }
                    div { style: "font-size:13px; color:{theme.text_soft};", "{theme_desc}" }
                    div { style: "display:flex; gap:8px; flex-wrap:wrap;",
                        button {
                            style: if selected_theme == "backend" { chip_selected.clone() } else { chip_idle.clone() },
                            onclick: move |_| theme_preset.set("backend".to_string()),
                            "{backend_theme_label}"
                        }
                        button {
                            style: if selected_theme == "default" { chip_selected.clone() } else { chip_idle.clone() },
                            onclick: move |_| theme_preset.set("default".to_string()),
                            "{default_theme_label}"
                        }
                        button {
                            style: if selected_theme == "sunset" { chip_selected.clone() } else { chip_idle.clone() },
                            onclick: move |_| theme_preset.set("sunset".to_string()),
                            "{sunset_theme_label}"
                        }
                        button {
                            style: if selected_theme == "forest" { chip_selected.clone() } else { chip_idle.clone() },
                            onclick: move |_| theme_preset.set("forest".to_string()),
                            "{forest_theme_label}"
                        }
                        button {
                            style: if selected_theme == "high_contrast" { chip_selected.clone() } else { chip_idle.clone() },
                            onclick: move |_| theme_preset.set("high_contrast".to_string()),
                            "{contrast_theme_label}"
                        }
                    }
                }
            }

            div { style: "margin-top:12px; {card_style}",
                div { style: "font-size:15px; color:{theme.text_primary}; font-weight:700;", "{section_map}" }
                div { style: "font-size:13px; color:{theme.text_muted};", "{units_title}" }
                div { style: "font-size:13px; color:{theme.text_soft};", "{units_desc}" }
                div { style: "display:flex; align-items:center; gap:12px; flex-wrap:wrap;",
                    button {
                        style: if metric_enabled { chip_selected.clone() } else { chip_idle.clone() },
                        onclick: move |_| distance_units_metric.set(true),
                        "{metric_label}"
                    }
                    button {
                        style: if !metric_enabled { chip_selected.clone() } else { chip_idle.clone() },
                        onclick: move |_| distance_units_metric.set(false),
                        "{imperial_label}"
                    }
                    div { style: "font-size:13px; color:{theme.text_secondary};",
                        if metric_enabled { "{metric_hint}" } else { "{imperial_hint}" }
                    }
                }
            }

            div { style: "margin-top:12px; {card_style}",
                div { style: "font-size:15px; color:{theme.text_primary}; font-weight:700;", "{section_network}" }
                div { style: "font-size:13px; color:{theme.text_muted};", "{network_anim_title}" }
                div { style: "font-size:13px; color:{theme.text_soft};", "{network_anim_desc}" }
                div { style: "display:flex; align-items:center; gap:12px; flex-wrap:wrap;",
                    button {
                        style: if flow_animation_enabled { chip_selected.clone() } else { chip_idle.clone() },
                        onclick: move |_| network_flow_animation_enabled.set(true),
                        "{flow_on_label}"
                    }
                    button {
                        style: if !flow_animation_enabled { chip_selected.clone() } else { chip_idle.clone() },
                        onclick: move |_| network_flow_animation_enabled.set(false),
                        "{flow_off_label}"
                    }
                }
            }
        }
    }
}
