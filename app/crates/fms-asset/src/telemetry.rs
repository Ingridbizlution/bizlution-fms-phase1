//! 遙測批次寫入（`/telemetry:batch-ingest`）。
//!
//! # 這支端點補上告警鏈唯一缺的 HTTP 入口
//!
//! `ingest_telemetry()` 與 `raise_alarm()` 從 006 就存在，010 的 T4 驗過整條鏈
//! —— 但**外部裝置沒有辦法把資料送進來**。
//!
//! 更根本的是 migration 057 才補上的那一段：在它之前，
//! `raise_alarm` 在整個 codebase 裡的呼叫者只有 T4，
//! **IoT 那條鏈從來沒有在生產路徑上跑過**。
//!
//! # 逐筆 savepoint 是契約的要求，不是優化
//!
//! 契約寫「伺服端逐筆處理，回應中列出失敗項目而不整批退回」。
//! 一個交易裡某一筆 SQL 失敗會讓**整個交易**進入 aborted 狀態 ——
//! 後面每一筆都會拿到 `current transaction is aborted`。
//!
//! 所以每一筆前開 `SAVEPOINT`、失敗就 `ROLLBACK TO SAVEPOINT`。
//! 沒有它的話「一批 1000 筆有 3 筆點位打錯」會變成 997 筆好資料一起被丟掉，
//! 而閘道那邊只看到一個 500。
//!
//! # 回應要說出被跳過的規則
//!
//! 057 的評估器只處理單筆讀數判斷得出來的門檻；持續型（`for_seconds`）與
//! 掃描型（`DEVICE_OFFLINE`）會計入 `skipped_sustained`。那個數字放進回應的
//! `meta`，因為「這條規則設定了但永遠不會響」必須看得見。
//!
//! `bad_rule_codes`（`op` 打錯的規則）則進 `errors[]` —— 它是設定錯誤，
//! 不是統計數字。

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, concurrency, require_permission, Caller, Problem};

use crate::handlers::AssetState;

const ENDPOINT: &str = "POST /telemetry:batch-ingest";
/// 契約寫死的上限。**在這裡擋而不是讓資料庫慢慢吃** ——
/// 一批 100 萬筆會佔住連線幾分鐘，而閘道只會看到逾時。
const MAX_READINGS: usize = 1000;
const QUALITIES: [&str; 4] = ["GOOD", "UNCERTAIN", "BAD", "STALE"];

#[derive(Debug, Deserialize)]
pub struct TelemetryBatch {
    #[allow(dead_code)]
    pub gateway_code: Option<String>,
    pub readings: Vec<Reading>,
}

#[derive(Debug, Deserialize)]
pub struct Reading {
    pub device_code: Option<String>,
    pub point_code: Option<String>,
    pub telemetry_point_id: Option<Uuid>,
    pub observed_at: chrono::DateTime<chrono::Utc>,
    pub value_num: Option<f64>,
    pub value_bool: Option<bool>,
    pub value_text: Option<String>,
    pub quality: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ItemError {
    pub index: usize,
    pub code: String,
    pub message: String,
}

/// `POST /telemetry:batch-ingest`
pub async fn batch_ingest(
    State(state): State<AssetState>,
    caller: Caller,
    headers: HeaderMap,
    Json(raw): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let batch: TelemetryBatch = serde_json::from_value(raw.clone())
        .map_err(|e| Problem::validation(format!("invalid TelemetryBatch: {e}")))?;

    if batch.readings.is_empty() {
        return Err(Problem::validation("readings 不能是空的"));
    }
    if batch.readings.len() > MAX_READINGS {
        return Err(Problem::validation(format!(
            "單次上限 {MAX_READINGS} 筆，這一批有 {} 筆",
            batch.readings.len()
        )));
    }

    let idem_key = concurrency::key_from(&headers)?;
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let pending = match idem_key.as_deref() {
        Some(key) => concurrency::begin(&mut tx, key, ENDPOINT, &raw)
            .await?
            .pending(),
        None => None,
    };

    // 範圍用 None：一批可以跨場域（一個閘道服務整棟樓），而每一筆的場域
    // 由 telemetry_point 決定 —— RLS 會擋掉看不到的點位，那一筆變成 rejected。
    let auth = require_permission(&mut tx, "telemetry:ingest", None, None).await?;

    if let Some(replay) = pending {
        let (code, body) = replay.release(auth);
        return Ok((code, Json(body)));
    }

    let mut accepted = 0usize;
    let mut errors: Vec<ItemError> = Vec::new();
    let mut alarms_raised = 0i32;
    let mut skipped_sustained = 0i32;

    for (i, r) in batch.readings.iter().enumerate() {
        if let Some(q) = r.quality.as_deref() {
            if !QUALITIES.contains(&q.to_uppercase().as_str()) {
                errors.push(ItemError {
                    index: i,
                    code: "INVALID_QUALITY".into(),
                    message: format!("quality 必須是 {} 其中之一", QUALITIES.join("／")),
                });
                continue;
            }
        }

        // **每一筆一個 savepoint。** 見模組檔頭：少了它，第一筆失敗之後
        // 整個交易就 aborted，後面 999 筆全部陪葬。
        sqlx::query("SAVEPOINT item").execute(tx.conn()).await?;

        match ingest_one(&mut tx, r).await {
            Ok((raised, skipped, bad)) => {
                sqlx::query("RELEASE SAVEPOINT item")
                    .execute(tx.conn())
                    .await?;
                accepted += 1;
                alarms_raised += raised;
                skipped_sustained += skipped;
                for code in bad {
                    // 設定錯誤，不是統計數字 —— 放進 errors 才看得見。
                    errors.push(ItemError {
                        index: i,
                        code: "RULE_BAD_OPERATOR".into(),
                        message: format!("規則 {code} 的 condition.op 不認得，它永遠不會觸發"),
                    });
                }
            }
            Err(e) => {
                sqlx::query("ROLLBACK TO SAVEPOINT item")
                    .execute(tx.conn())
                    .await?;
                errors.push(e.into_item(i));
            }
        }
    }

    let body = serde_json::json!({
        "accepted": accepted,
        "rejected": batch.readings.len() - accepted,
        "alarms_raised": alarms_raised,
        "errors": errors,
        "meta": {
            // 「這條規則設定了但永遠不會響」必須看得見。057 的評估器只處理
            // 單筆讀數判斷得出來的門檻；持續型與掃描型計入這裡。
            "rules_skipped_not_evaluable_per_reading": skipped_sustained,
        },
    });

    if let Some(key) = idem_key.as_deref() {
        concurrency::complete(&mut tx, key, ENDPOINT, 202, &body).await?;
    }
    tx.commit().await?;

    Ok((StatusCode::ACCEPTED, Json(body)))
}

/// 單筆的失敗原因。分成幾種是為了讓閘道能自動分類 ——
/// 「點位打錯」要修設定，「讀數重複」可以忽略。
enum ItemFail {
    NoPoint,
    PointNotFound,
    Db(String),
}

impl ItemFail {
    fn into_item(self, index: usize) -> ItemError {
        let (code, message) = match self {
            ItemFail::NoPoint => (
                "MISSING_POINT",
                "要有 telemetry_point_id，或 device_code + point_code".to_string(),
            ),
            ItemFail::PointNotFound => (
                "POINT_NOT_FOUND",
                "找不到這個計量點（或它不在你的租戶／場域範圍內）".to_string(),
            ),
            ItemFail::Db(e) => ("DB_ERROR", e),
        };
        ItemError {
            index,
            code: code.to_string(),
            message,
        }
    }
}

/// 寫入一筆並評估規則。回傳 (觸發數, 跳過數, 設定錯誤的規則代碼)。
async fn ingest_one(
    tx: &mut fms_shared::TenantTx,
    r: &Reading,
) -> Result<(i32, i32, Vec<String>), ItemFail> {
    // 解析點位。契約允許三種指定方式，而 `device_code + point_code` 是
    // 閘道最自然的一種（它不知道我們的 uuid）。
    let point_id: Option<Uuid> = match r.telemetry_point_id {
        Some(id) => Some(id),
        None => {
            let (Some(dev), Some(pt)) = (r.device_code.as_deref(), r.point_code.as_deref()) else {
                return Err(ItemFail::NoPoint);
            };
            sqlx::query_scalar(
                "SELECT p.id FROM fms.telemetry_points p
                   JOIN fms.devices d ON d.id = p.device_id
                  WHERE d.device_code = $1 AND p.point_code = $2",
            )
            .bind(dev)
            .bind(pt)
            .fetch_optional(tx.conn())
            .await
            .map_err(|e| ItemFail::Db(e.to_string()))?
        }
    };
    let point_id = point_id.ok_or(ItemFail::PointNotFound)?;

    // `coalesce($6, 'GOOD')` 而不是靠函式簽章的 `DEFAULT 'GOOD'`：
    // **SQL 的 DEFAULT 只在參數被省略時生效**，明確傳 NULL 不會套用預設 ——
    // 而 sqlx 的 bind 一定會傳一個值。少了 coalesce 的症狀是
    // `null value in column "quality" violates not-null constraint`，
    // 而那個錯誤看起來像資料表壞了，不像呼叫端傳錯。
    sqlx::query(
        "SELECT fms.ingest_telemetry($1, $2, $3::float8::numeric, $4, $5,
                                     coalesce($6::text, 'GOOD'))",
    )
    .bind(point_id)
    .bind(r.observed_at)
    .bind(r.value_num)
    .bind(r.value_bool)
    .bind(r.value_text.as_deref())
    .bind(r.quality.as_deref().map(str::to_uppercase))
    .execute(tx.conn())
    .await
    .map_err(|e| {
        // 006 對找不到的點位拋 P0002 —— 那是「設定錯」不是「系統壞」。
        if let sqlx::Error::Database(db) = &e {
            if db.code().as_deref() == Some("P0002") {
                return ItemFail::PointNotFound;
            }
        }
        ItemFail::Db(e.to_string())
    })?;

    let (raised, skipped, bad): (i32, i32, Vec<String>) = sqlx::query_as(
        "SELECT raised, skipped_sustained, bad_rule_codes
           FROM fms.evaluate_telemetry_rules($1, $2::float8::numeric, $3)",
    )
    .bind(point_id)
    .bind(r.value_num)
    .bind(r.observed_at)
    .fetch_one(tx.conn())
    .await
    .map_err(|e| ItemFail::Db(e.to_string()))?;

    Ok((raised, skipped, bad))
}
