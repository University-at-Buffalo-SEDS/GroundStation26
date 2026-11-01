//! Authenticated, finite report jobs. No shell, browser, or user-supplied paths.
use crate::{auth::Permission, state::AppState};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::io::AsyncWriteExt;

#[derive(Deserialize)]
pub struct ReportQuery {
    pub request: String,
}

fn valve_labels(layout: &serde_json::Value) -> serde_json::Value {
    fn walk(value: &serde_json::Value, labels: &mut std::collections::BTreeMap<u64, serde_json::Value>) {
        if value["data_type"] == "VALVE_STATE" {
            if let Some(items)=value["valves"].as_array().or_else(||value["items"].as_array()) {
                for (ordinal,item) in items.iter().enumerate() {
                    if let Some(index)=item["index"].as_u64().filter(|i|*i<7) {
                        let mut mapping=serde_json::json!({"index":index});
                        if let Some(label)=item["label"].as_str().filter(|s|s.len()<=128) {mapping["label"]=label.into();}
                        let words=value["valve_labels"].get(ordinal).filter(|v|v.is_object()).unwrap_or(&value["boolean_labels"]);
                        for field in ["true_label","false_label","unknown_label"] {if let Some(text)=words[field].as_str().filter(|s|s.len()<=128) {mapping[field]=text.into();}}
                        labels.insert(index,mapping);
                    }
                }
            }
        }
        match value {serde_json::Value::Object(map)=>{for child in map.values(){walk(child,labels);}},serde_json::Value::Array(list)=>{for child in list{walk(child,labels);}},_=>{}}
    }
    let mut labels=std::collections::BTreeMap::new();walk(layout,&mut labels);
    serde_json::Value::Array(labels.into_values().collect())
}

fn channel_metadata(layout:&serde_json::Value)->serde_json::Value {
    fn walk(value:&serde_json::Value,out:&mut Vec<serde_json::Value>) {
        if let Some(kind)=value["data_type"].as_str().or_else(||value["id"].as_str()) {
            if let Some(formatters)=value["channel_formatters"].as_array() {
                for (index,formatter) in formatters.iter().enumerate() {
                    if let Some(unit)=formatter["suffix"].as_str().map(str::trim).filter(|s|!s.is_empty() && s.len()<=64) {
                        let mut item=serde_json::json!({"data_type":kind,"index":index,"unit":unit});
                        if let Some(sender)=value["sender_id"].as_str(){item["sender_id"]=sender.into();}
                        out.push(item);
                    }
                }
            }
        }
        match value {serde_json::Value::Object(map)=>{for child in map.values(){walk(child,out);}},serde_json::Value::Array(list)=>{for child in list{walk(child,out);}},_=>{}}
    }
    let mut out=Vec::new();walk(layout,&mut out);serde_json::Value::Array(out)
}

pub async fn report(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ReportQuery>,
) -> Response {
    if let Err(e) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return e;
    }
    if query.request.len() > 24_000 {
        return (StatusCode::BAD_REQUEST, "Report selection is too large.").into_response();
    }
    let mut request: serde_json::Value = match serde_json::from_str(&query.request) {
        Ok(serde_json::Value::Object(map)) => serde_json::Value::Object(map),
        _ => return (StatusCode::BAD_REQUEST, "Invalid report selection.").into_response(),
    };
    let format = request["format"].as_str().unwrap_or("preview").to_owned();
    let content_type = match format.as_str() {
        "preview" => "application/json",
        "csv" | "daq_csv" => "text/csv; charset=utf-8",
        "daq_tsv" => "text/tab-separated-values; charset=utf-8",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pdf" => "application/pdf",
        _ => return (StatusCode::BAD_REQUEST, "Choose CSV, DAQ SD CSV/TSV, Excel or PDF.").into_response(),
    };
    // These names are export-time station configuration, not historical state.
    // Override any client-supplied mapping; numeric saved values remain intact.
    let export_layout=crate::layout::load_layout().ok().and_then(|layout|serde_json::to_value(layout).ok());
    request["valve_mappings"]=export_layout.as_ref().map(valve_labels).unwrap_or_else(||serde_json::json!([]));
    request["channel_metadata"]=export_layout.as_ref().map(channel_metadata).unwrap_or_else(||serde_json::json!([]));
    // The root is always server-controlled, regardless of request JSON.
    request["root"] = crate::recording_export::directory(&state)
        .to_string_lossy()
        .into_owned()
        .into();
    static JOBS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let Ok(_permit) = JOBS.try_acquire() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "Another report is being prepared. Retry when it finishes.",
        )
            .into_response();
    };
    let output = tokio::time::timeout(Duration::from_secs(120), async {
        let python = std::env::var("GS_REPORT_PYTHON").unwrap_or_else(|_| {
            let local = if cfg!(windows) {
                ".venv-reports/Scripts/python.exe"
            } else {
                ".venv-reports/bin/python"
            };
            if std::path::Path::new(local).is_file() {
                local.into()
            } else {
                "python3".into()
            }
        });
        let mut child = tokio::process::Command::new(python)
            .args(["-c", include_str!("recording_report.py")])
            .env("MPLBACKEND", "Agg")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(request.to_string().as_bytes()).await?;
        drop(stdin);
        child.wait_with_output().await
    })
    .await;
    let output = match output {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            log::error!("Report worker: {e}");
            return (StatusCode::SERVICE_UNAVAILABLE,"Report worker unavailable. Install Python 3 or set GS_REPORT_PYTHON on the backend.").into_response();
        }
        Err(_) => {
            return (
                StatusCode::REQUEST_TIMEOUT,
                "Report exceeded two minutes. Select a smaller time window or fewer channels.",
            )
                .into_response();
        }
    };
    if !output.status.success() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(1500)
                .collect::<String>(),
        )
            .into_response();
    }
    let mut response = (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
        ],
        output.stdout,
    )
        .into_response();
    if format != "preview" {
        response.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"",match format.as_str(){"daq_csv"=>"telemetry-daq-sd.csv".to_owned(),"daq_tsv"=>"telemetry-daq-sd.tsv".to_owned(),_=>format!("telemetry-report.{format}")})
                .parse()
                .unwrap(),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording_export::tests::{TestDirectory, export_state};

    #[tokio::test]
    async fn reports_require_data_permission_before_spawning_worker() {
        let root = TestDirectory::new();
        let response = report(
            State(export_state(&root.0, false).await),
            HeaderMap::new(),
            Query(ReportQuery {
                request: "{}".into(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn report_worker_reads_saved_data_and_ignores_user_root() {
        let root = TestDirectory::new();
        let state = export_state(&root.0, true).await;
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(root.0.join("groundstation_recording_1.db"))
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::query("CREATE TABLE telemetry(id INTEGER PRIMARY KEY,timestamp_ms INTEGER,source_timestamp_ms INTEGER,sender_id TEXT,data_type TEXT,values_json TEXT,payload_json TEXT)").execute(&db).await.unwrap();
        sqlx::query("INSERT INTO telemetry VALUES(1,1000,20,'DAQ','KG1000','[2.5]','[]')")
            .execute(&db)
            .await
            .unwrap();
        db.close().await;
        let response = report(
            State(state),
            HeaderMap::new(),
            Query(ReportQuery {
                request: r#"{"root":"/does-not-exist","format":"preview"}"#.into(),
            }),
        )
        .await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(result["recorded_rows"], 1);
        assert_eq!(result["channels"][0]["points"][0][1], 2.5);
    }
    #[tokio::test]
    async fn daq_formatter_formats_require_view_data_before_worker() {
        let root=TestDirectory::new();
        let state=export_state(&root.0,false).await;
        for format in ["daq_csv","daq_tsv"] {
            let response=report(State(state.clone()),HeaderMap::new(),Query(ReportQuery{
                request:serde_json::json!({"recording":"groundstation_recording_1.db","format":format,"channels":["secret"]}).to_string(),
            })).await;
            assert_eq!(response.status(),StatusCode::UNAUTHORIZED);
        }
    }
    #[test]
    fn units_are_only_explicit_layout_suffixes() {
        let metadata=channel_metadata(&serde_json::json!({"tabs":[
            {"id":"KG1000","channels":["Volts"],"channel_formatters":[{"suffix":" V"}]},
            {"id":"UNKNOWN","channels":["Pressure (psi)"]}
        ]}));
        assert_eq!(metadata,serde_json::json!([{"data_type":"KG1000","index":0,"unit":"V"}]));
    }

}
