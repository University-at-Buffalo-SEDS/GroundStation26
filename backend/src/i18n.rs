use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use whatlang::{Lang, detect};

#[derive(Serialize, Deserialize, Default)]
struct TranslationCacheFile {
    entries: HashMap<String, HashMap<String, String>>,
}

#[derive(Serialize)]
pub struct TranslationCatalogResponse {
    pub lang: String,
    pub translations: HashMap<String, String>,
}

#[derive(Deserialize)]
pub struct TranslateRequest {
    pub target_lang: String,
    pub texts: Vec<String>,
}

#[derive(Serialize)]
pub struct TranslateResponse {
    pub lang: String,
    pub translations: HashMap<String, String>,
}

#[derive(Serialize)]
struct LibreTranslateRequest<'a> {
    q: &'a str,
    source: &'a str,
    target: &'a str,
    format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
}

#[derive(Deserialize)]
struct LibreTranslateResponse {
    #[serde(rename = "translatedText")]
    translated_text: String,
}

static TRANSLATION_CACHE: LazyLock<Mutex<TranslationCacheFile>> =
    LazyLock::new(|| Mutex::new(load_cache_file()));

const KNOWN_TRANSLATIONS: &[(&str, &str, &str)] = &[
    (
        "Telemetry Dashboard",
        "Panel de Telemetría",
        "Tableau Télémétrie",
    ),
    ("Settings", "Ajustes", "Paramètres"),
    ("Map", "Mapa", "Carte"),
    ("Warnings", "Avisos", "Alertes"),
    ("Errors", "Errores", "Erreurs"),
    ("Notifications", "Notificaciones", "Notifications"),
    (
        "Notifications History",
        "Historial de notificaciones",
        "Historique des notifications",
    ),
    ("Connection Status", "Estado de Conexión", "État Connexion"),
    ("Detailed Info", "Info Detallada", "Infos Détaillées"),
    ("Network Topology", "Topología Red", "Topologie Réseau"),
    ("Actions", "Acciones", "Actions"),
    ("Calibration", "Calibración", "Calibration"),
    ("Data", "Datos", "Données"),
    ("Flight", "Vuelo", "Vol"),
    ("Dismiss", "Cerrar", "Fermer"),
    ("No warnings.", "No hay avisos.", "Aucune alerte."),
    ("No errors.", "No hay errores.", "Aucune erreur."),
    (
        "No notifications yet.",
        "Todavía no hay notificaciones.",
        "Aucune notification pour l'instant.",
    ),
    (
        "Acknowledge warnings",
        "Confirmar avisos",
        "Acquitter alertes",
    ),
    (
        "Acknowledge errors",
        "Confirmar errores",
        "Acquitter erreurs",
    ),
    ("Nominal", "Nominal", "Nominal"),
    ("Connected", "Conectado", "Connecté"),
    ("Disconnected", "Desconectado", "Déconnecté"),
    ("Unavailable", "No disponible", "Indisponible"),
    ("None", "Ninguno", "Aucun"),
    (
        "Waiting for telemetry…",
        "Esperando telemetría…",
        "Attente de télémétrie…",
    ),
    ("Collapse", "Contraer", "Réduire"),
    ("Expand", "Expandir", "Développer"),
    ("Fullscreen", "Pantalla completa", "Plein écran"),
    (
        "Exit Fullscreen",
        "Salir de pantalla completa",
        "Quitter plein écran",
    ),
    ("Data Graph", "Gráfica de datos", "Graphique de données"),
    (
        "Scaled per series",
        "Escalado por serie",
        "Échelle par série",
    ),
    ("min", "mín", "min"),
    ("max", "máx", "max"),
    ("Type", "Tipo", "Type"),
    ("type", "tipo", "type"),
    ("linear", "lineal", "linéaire"),
    ("now", "ahora", "maintenant"),
    ("Open", "Abierto", "Ouvert"),
    ("Closed", "Cerrado", "Fermé"),
    ("Unknown", "Desconocido", "Inconnu"),
    ("yes", "sí", "oui"),
    ("no", "no", "non"),
    ("Startup", "Inicio", "Démarrage"),
    ("Abort", "Abortar", "Abandon"),
    (
        "Current Flight State",
        "Estado actual de vuelo",
        "État de vol actuel",
    ),
    (
        "Fuel Tank Pressure",
        "Presión del tanque de combustible",
        "Pression du réservoir carburant",
    ),
    (
        "FUEL TANK PRESSURE",
        "PRESIÓN DEL TANQUE DE COMBUSTIBLE",
        "PRESSION DU RÉSERVOIR CARBURANT",
    ),
    (
        "Tank Pressure",
        "Presión del tanque",
        "Pression du réservoir",
    ),
    ("Percent", "Porcentaje", "Pourcentage"),
    (
        "Drop Rate (V/min)",
        "Tasa de caída (V/min)",
        "Taux de chute (V/min)",
    ),
    (
        "Remaining Minutes",
        "Minutos restantes",
        "Minutes restantes",
    ),
    (
        "AV Bay Battery (Power Board)",
        "Batería AV Bay (placa de potencia)",
        "Batterie AV Bay (carte puissance)",
    ),
    (
        "Ground Station Battery (Gateway Board)",
        "Batería de estación base (placa gateway)",
        "Batterie station sol (carte passerelle)",
    ),
    ("Gyro Data", "Datos de giroscopio", "Données gyroscope"),
    (
        "Accel Data",
        "Datos de acelerómetro",
        "Données accéléromètre",
    ),
    ("Barometer Data", "Datos de barómetro", "Données baromètre"),
    (
        "Kalman Filter Data",
        "Datos del filtro Kalman",
        "Données filtre Kalman",
    ),
    ("GPS Data", "Datos GPS", "Données GPS"),
    (
        "Vehicle Speed",
        "Velocidad del vehículo",
        "Vitesse du véhicule",
    ),
    (
        "Battery Voltage",
        "Voltaje de batería",
        "Tension de batterie",
    ),
    (
        "Battery Current",
        "Corriente de batería",
        "Courant de batterie",
    ),
    (
        "Battery Runtime",
        "Autonomía de batería",
        "Autonomie de batterie",
    ),
    ("Fuel Flow", "Flujo de combustible", "Débit de carburant"),
    ("Valve State", "Estado de válvulas", "État des vannes"),
    ("Loadcell", "Celda de carga", "Cellule de charge"),
    ("Pressure", "Presión", "Pression"),
    ("Temp", "Temperatura", "Température"),
    ("Temperature", "Temperatura", "Température"),
    ("Altitude", "Altitud", "Altitude"),
    ("Dump", "Vaciado", "Purge"),
    ("NormallyOpen", "Normalmente abierto", "Normalement ouvert"),
    ("Igniter", "Ignitor", "Allumeur"),
    ("Nitrogen", "Nitrógeno", "Azote"),
    ("Nitrous", "Óxido nitroso", "Protoxyde d'azote"),
    ("Fill Lines", "Líneas de carga", "Lignes de remplissage"),
    ("Pilot", "Piloto", "Pilote"),
    ("Idle", "Inactivo", "Repos"),
    (
        "Disable Actions is enabled. All action and flight-state buttons except Abort are disabled.",
        "Desactivar acciones está habilitado. Todos los botones de acciones y estado de vuelo excepto Abortar están desactivados.",
        "Désactiver actions est activé. Tous les boutons d'action et d'état de vol sauf Abandon sont désactivés.",
    ),
];

pub fn catalog_for_lang(lang: &str) -> TranslationCatalogResponse {
    let lang = normalize_lang(lang);
    let translations = KNOWN_TRANSLATIONS
        .iter()
        .map(|(key, es, fr)| {
            let value = match lang {
                "es" => *es,
                "fr" => *fr,
                _ => *key,
            };
            ((*key).to_string(), value.to_string())
        })
        .collect();
    TranslationCatalogResponse {
        lang: lang.to_string(),
        translations,
    }
}

pub async fn translate_texts(req: TranslateRequest) -> TranslateResponse {
    let target_lang = normalize_lang(&req.target_lang).to_string();
    let known = catalog_for_lang(&target_lang).translations;
    let mut translations = HashMap::new();

    for raw in req.texts {
        let text = raw.trim();
        if text.is_empty() {
            continue;
        }
        if let Some(value) = known.get(text) {
            translations.insert(text.to_string(), value.clone());
            continue;
        }
        let translated = translate_unknown(text, &target_lang)
            .await
            .unwrap_or_else(|| text.to_string());
        translations.insert(text.to_string(), translated);
    }

    TranslateResponse {
        lang: target_lang,
        translations,
    }
}

async fn translate_unknown(text: &str, target_lang: &str) -> Option<String> {
    let source_lang = detect_lang_code(text);
    if source_lang == target_lang {
        return Some(text.to_string());
    }

    if let Some(cached) = cached_translation(&source_lang, target_lang, text) {
        return Some(cached);
    }

    let url = std::env::var("GS_TRANSLATION_BACKEND_URL").ok()?;
    let client = Client::builder().build().ok()?;
    let api_key = std::env::var("GS_TRANSLATION_API_KEY").ok();
    let request = LibreTranslateRequest {
        q: text,
        source: if source_lang.is_empty() {
            "auto"
        } else {
            &source_lang
        },
        target: target_lang,
        format: "text",
        api_key,
    };

    let response = client
        .post(url)
        .json(&request)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<LibreTranslateResponse>()
        .await
        .ok()?;

    save_cached_translation(&source_lang, target_lang, text, &response.translated_text);
    Some(response.translated_text)
}

fn normalize_lang(lang: &str) -> &str {
    match lang {
        "es" | "fr" | "en" => lang,
        _ => "en",
    }
}

fn detect_lang_code(text: &str) -> String {
    match detect(text).map(|info| info.lang()) {
        Some(Lang::Spa) => "es".to_string(),
        Some(Lang::Fra) => "fr".to_string(),
        Some(Lang::Eng) => "en".to_string(),
        _ => "auto".to_string(),
    }
}

fn cache_file_path() -> PathBuf {
    PathBuf::from("backend/data/translation_cache.json")
}

fn load_cache_file() -> TranslationCacheFile {
    let path = cache_file_path();
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<TranslationCacheFile>(&raw).ok())
        .unwrap_or_default()
}

fn cached_translation(source_lang: &str, target_lang: &str, text: &str) -> Option<String> {
    let key = format!("{source_lang}->{target_lang}");
    TRANSLATION_CACHE.lock().ok().and_then(|cache| {
        cache
            .entries
            .get(&key)
            .and_then(|bucket| bucket.get(text).cloned())
    })
}

fn save_cached_translation(source_lang: &str, target_lang: &str, text: &str, translated: &str) {
    let key = format!("{source_lang}->{target_lang}");
    let Ok(mut cache) = TRANSLATION_CACHE.lock() else {
        return;
    };
    cache
        .entries
        .entry(key)
        .or_default()
        .insert(text.to_string(), translated.to_string());

    let path = cache_file_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(raw) = serde_json::to_string_pretty(&*cache) {
        let _ = fs::write(path, raw);
    }
}
