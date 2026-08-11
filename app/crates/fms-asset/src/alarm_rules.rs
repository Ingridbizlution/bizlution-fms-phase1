//! 告警規則（`/alarm-rules`）。
//!
//! # 在這三支之前，規則只能手寫 SQL 建立
//!
//! `fms.alarm_rules` 從 006 就存在，057 補上了評估器，#17 補上了讀數入口。
//! 但**唯一寫過那張表的是 seed 009** —— 也就是說一條新的門檻要上線，
//! 得有人連進資料庫。而 `auto_create_work_order` 那個開關
//! （告警自動變工單，整條 IoT 鏈的價值所在）也只有 seed 設得動。
//!
//! # 試跑與真跑共用同一套判斷
//!
//! `POST /alarm-rules/{id}/test` 不能呼叫 `evaluate_telemetry_rules`
//! —— 那支會真的 `raise_alarm`，試跑不該有副作用。
//!
//! 所以 migration 061 把兩個述詞抽成 `fms.telemetry_rule_fires` 與
//! `fms.alarm_rule_covers_point`，**並改寫 057 的評估器去呼叫它們**。
//! 這裡的試跑用的是同兩支函式。
//!
//! 少了這一步，試跑會是同一套語意的第二份實作，而漂移的症狀最糟：
//! 試跑說「會響 3 次」，上線之後響 0 次 —— 使用者對這個系統的信任
//! 正是建立在那個預覽上。
//!
//! # 試跑會回報「判斷不出來」的規則
//!
//! `telemetry_rule_fires` 回 NULL 代表 condition 判斷不出來（op 打錯、缺 value）。
//! 試跑把那個數字單獨列出來 —— 一條 op 打錯的規則在真跑時是
//! 「永遠不會響而沒有人知道」，而試跑正是該把它抓出來的地方。
//!
//! # 非 THRESHOLD 與持續型會被明說跳過
//!
//! 057 只實作單筆讀數判斷得出來的門檻。`for_seconds`（持續型）與
//! `DEVICE_OFFLINE`（掃描型）在真跑時計入 `skipped_sustained`；
//! 試跑則直接回 `evaluable: false` 並說明原因 ——
//! 「試跑結果 0 次」與「這種規則現在根本不會被評估」必須分得開。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, page, require_permission, Caller, Cursor, PageMeta, Problem,
    SortSpec,
};

use crate::handlers::AssetState;

const RULE_TYPES: [&str; 7] = [
    "THRESHOLD",
    "RATE_OF_CHANGE",
    "DEVIATION",
    "FLATLINE",
    "DEVICE_OFFLINE",
    "BOOLEAN_STATE",
    "COMPOSITE",
];
const SEVERITIES: [&str; 5] = ["INFO", "WARNING", "MINOR", "MAJOR", "CRITICAL"];
const WO_PRIORITIES: [&str; 5] = ["LOW", "MEDIUM", "HIGH", "URGENT", "CRITICAL"];
/// 061 的 `telemetry_rule_fires` 認得的運算子。**這份清單必須與那支函式一致**
/// —— 不一致的方向只有一種會出事：這裡放行、那裡回 NULL，
/// 於是一條「通過驗證」的規則永遠不會響。所以 `alarm_rules_slice.rs`
/// 有一條測試把兩邊對起來。
const OPS: [&str; 6] = [">", ">=", "<", "<=", "=", "!="];
/// 試跑掃描的讀數上限。一年的 1 Hz 資料是 3100 萬筆，
/// 而試跑是互動式的（使用者按下按鈕在等）。
const MAX_DRY_RUN_READINGS: i64 = 200_000;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RuleDto {
    pub id: Uuid,
    pub facility_id: Option<Uuid>,
    pub facility_name: Option<String>,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub telemetry_point_id: Option<Uuid>,
    pub point_code: Option<String>,
    pub asset_category_id: Option<Uuid>,
    pub rule_type: String,
    pub condition: serde_json::Value,
    pub severity: String,
    pub debounce_seconds: i32,
    pub auto_clear: bool,
    pub auto_create_work_order: bool,
    pub wo_work_order_type: Option<String>,
    pub wo_priority: Option<String>,
    pub wo_team_id: Option<Uuid>,
    pub wo_sla_policy_id: Option<Uuid>,
    pub dedupe_window_minutes: i32,
    pub notify_role_codes: Vec<String>,
    pub is_active: bool,
    /// 這條規則現在管到幾個點位。**0 是最重要的一個值** ——
    /// 一條 `point_code` 打錯的規則會安靜地什麼都不管。
    pub covered_point_count: i64,
    /// 這條規則現在能不能被單筆讀數評估。false 代表 057 會跳過它
    /// （持續型或掃描型），也就是**它現在不會響**。
    pub evaluable: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub facility_id: Option<Uuid>,
    pub rule_type: Option<String>,
    pub severity: Option<String>,
    pub is_active: Option<bool>,
    pub auto_create_work_order: Option<bool>,
    /// 只回傳**現在不會響**的規則：管不到任何點位，或 057 評估不了。
    /// 這是「哪些規則設了等於沒設」那個問題 —— 而那正是這個系統
    /// 最容易靜默失效的地方。
    pub ineffective_only: Option<bool>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RuleCreate {
    pub facility_id: Option<Uuid>,
    pub code: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub telemetry_point_id: Option<Uuid>,
    pub point_code: Option<String>,
    pub asset_category_id: Option<Uuid>,
    pub rule_type: Option<String>,
    pub condition: Option<serde_json::Value>,
    pub severity: Option<String>,
    pub debounce_seconds: Option<i32>,
    pub auto_clear: Option<bool>,
    pub auto_create_work_order: Option<bool>,
    pub wo_work_order_type: Option<String>,
    pub wo_priority: Option<String>,
    pub wo_team_id: Option<Uuid>,
    pub wo_sla_policy_id: Option<Uuid>,
    pub dedupe_window_minutes: Option<i32>,
    pub notify_role_codes: Option<Vec<String>>,
    pub is_active: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct DryRunQuery {
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
}

/// 試跑的統計結果。
///
/// 與 `DryRunTarget` 同一個理由具名（clippy 的 `type_complexity`），
/// 而且六個同型別的 `i64`／`Option<...>` 靠位置對應太容易接錯 ——
/// `matched` 與 `unknown` 互換不會有任何編譯錯誤，只會讓
/// 「判斷不出來的筆數」被當成「會觸發的次數」回報。
#[derive(Debug, sqlx::FromRow)]
struct DryRunOutcome {
    scanned: i64,
    matched: i64,
    unknown: i64,
    first_at: Option<chrono::DateTime<chrono::Utc>>,
    last_at: Option<chrono::DateTime<chrono::Utc>>,
    peak: Option<f64>,
}

/// 試跑要讀的那幾個欄位。
///
/// 具名而不是六元組：CI 的 clippy（比本機新）對那個元組報
/// `type_complexity`。而具名之後 `let Struct { .. } = ` 的解構也讓
/// 「哪個 String 是 code、哪個是 name」看得出來 —— 元組版本要靠位置記。
#[derive(Debug, sqlx::FromRow)]
struct DryRunTarget {
    code: String,
    name: String,
    rule_type: String,
    condition: serde_json::Value,
    telemetry_point_id: Option<Uuid>,
    point_code: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DryRunBody {
    /// 覆寫 condition 來試不同門檻，**不寫入**。這是「28 度太敏感，
    /// 改成 30 會少響幾次」那個問題的答案。
    pub condition: Option<serde_json::Value>,
}

/// `covered_point_count` 與 `evaluable` 的 SQL。兩者都是算出來的 ——
/// 存成欄位的話，改一個點位的 `point_code` 就會讓它們開始說謊。
const DERIVED: &str = "
  (SELECT count(*) FROM fms.telemetry_points p
    WHERE fms.alarm_rule_covers_point(r.telemetry_point_id, r.point_code::text,
                                      p.id, p.point_code::text)) AS covered_point_count,
  (r.rule_type = 'THRESHOLD' AND NOT (r.condition ? 'for_seconds')) AS evaluable";

const COLUMNS: &str = "r.id, r.facility_id, f.name::text AS facility_name,
                       r.code::text AS code, r.name::text AS name, r.description,
                       r.telemetry_point_id, r.point_code::text AS point_code,
                       r.asset_category_id, r.rule_type, r.condition, r.severity,
                       r.debounce_seconds, r.auto_clear, r.auto_create_work_order,
                       r.wo_work_order_type::text AS wo_work_order_type,
                       r.wo_priority, r.wo_team_id, r.wo_sla_policy_id,
                       r.dedupe_window_minutes, r.notify_role_codes, r.is_active,
                       r.created_at";

const FROM: &str = "FROM fms.alarm_rules r
                    LEFT JOIN fms.facilities f ON f.id = r.facility_id";

/// `GET /alarm-rules`
pub async fn list(
    State(state): State<AssetState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    if let Some(t) = q.rule_type.as_deref() {
        if !RULE_TYPES.contains(&t.to_uppercase().as_str()) {
            return Err(Problem::validation(format!(
                "rule_type 必須是 {} 其中之一",
                RULE_TYPES.join("／")
            )));
        }
    }
    if let Some(s) = q.severity.as_deref() {
        if !SEVERITIES.contains(&s.to_uppercase().as_str()) {
            return Err(Problem::validation(format!(
                "severity 必須是 {} 其中之一",
                SEVERITIES.join("／")
            )));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 060 補的 `alarm_rule:read`。在它之前只有 `alarm_rule:write`，
    // 於是技師看得到告警卻看不到產生它的門檻。
    require_permission(&mut tx, "alarm_rule:read", q.facility_id, None).await?;

    let limit = clamp_limit(q.limit);
    let sort = SortSpec {
        column: "code".to_string(),
        desc: false,
    };
    let (ckey, cid) = match q.cursor.as_deref() {
        Some(raw) => {
            let c = Cursor::decode(raw, &sort.column)?;
            (Some(c.key.clone()), Some(c.uuid_id()?))
        }
        None => (None, None),
    };

    let rows: Vec<RuleDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS}, {DERIVED} {FROM}
          WHERE ($1::uuid IS NULL OR r.facility_id = $1::uuid)
            AND ($2::text IS NULL OR r.rule_type = upper($2::text))
            AND ($3::text IS NULL OR r.severity = upper($3::text))
            AND ($4::bool IS NULL OR r.is_active = $4::bool)
            AND ($5::bool IS NULL OR r.auto_create_work_order = $5::bool)
            AND (NOT $6::bool OR (
                  -- 「設了等於沒設」的兩種：管不到任何點位，或 057 評估不了。
                  r.rule_type <> 'THRESHOLD'
                  OR r.condition ? 'for_seconds'
                  OR NOT EXISTS (
                       SELECT 1 FROM fms.telemetry_points p
                        WHERE fms.alarm_rule_covers_point(
                                r.telemetry_point_id, r.point_code::text,
                                p.id, p.point_code::text))))
            AND ($7::text IS NULL OR (r.code, r.id) > ($7::text, $8::uuid))
          ORDER BY r.code, r.id
          LIMIT $9"
    ))
    .bind(q.facility_id)
    .bind(q.rule_type.as_deref())
    .bind(q.severity.as_deref())
    .bind(q.is_active)
    .bind(q.auto_create_work_order)
    .bind(q.ineffective_only.unwrap_or(false))
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.code.clone(), r.id));
    Ok(Json(serde_json::json!({
        "data": paged.data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
    })))
}

/// `POST /alarm-rules`
pub async fn create(
    State(state): State<AssetState>,
    caller: Caller,
    Json(body): Json<RuleCreate>,
) -> Result<(StatusCode, Json<RuleDto>), Problem> {
    let code = required(&body.code, "code")?;
    let name = required(&body.name, "name")?;
    let rule_type = body
        .rule_type
        .as_deref()
        .map(str::to_uppercase)
        .unwrap_or_else(|| "THRESHOLD".to_string());
    if !RULE_TYPES.contains(&rule_type.as_str()) {
        return Err(Problem::validation(format!(
            "rule_type 必須是 {} 其中之一",
            RULE_TYPES.join("／")
        )));
    }
    let condition = body
        .condition
        .clone()
        .ok_or_else(|| Problem::validation("condition 為必填"))?;
    validate_condition(&rule_type, &condition)?;

    // 006 的 `ck_alarm_rule_scope`。先在這裡擋是為了給出說得出理由的訊息。
    if body.telemetry_point_id.is_none()
        && body
            .point_code
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        && rule_type != "DEVICE_OFFLINE"
    {
        return Err(Problem::validation(
            "要有 telemetry_point_id 或 point_code —— \
             一條不知道自己管哪些點位的規則永遠不會響",
        ));
    }

    if let Some(s) = body.severity.as_deref() {
        if !SEVERITIES.contains(&s.to_uppercase().as_str()) {
            return Err(Problem::validation(format!(
                "severity 必須是 {} 其中之一",
                SEVERITIES.join("／")
            )));
        }
    }
    if let Some(p) = body.wo_priority.as_deref() {
        if !WO_PRIORITIES.contains(&p.to_uppercase().as_str()) {
            return Err(Problem::validation(format!(
                "wo_priority 必須是 {} 其中之一",
                WO_PRIORITIES.join("／")
            )));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "alarm_rule:write", body.facility_id, None).await?;

    // `notify_role_codes` 指向不存在的角色，等於「設了通知但沒有人會收到」
    // —— 這個 repo 反覆出現的缺陷。在這裡擋掉，而不是等到告警響了才發現。
    if let Some(codes) = body.notify_role_codes.as_deref() {
        let unknown: Vec<String> = sqlx::query_scalar(
            "SELECT c FROM unnest($1::text[]) AS c
              WHERE NOT EXISTS (SELECT 1 FROM fms.roles r WHERE r.code = c)",
        )
        .bind(codes)
        .fetch_all(tx.conn())
        .await?;
        if !unknown.is_empty() {
            return Err(Problem::validation(format!(
                "notify_role_codes 裡這些角色不存在：{} —— \
                 留著它們等於設了通知而沒有人會收到",
                unknown.join("、")
            )));
        }
    }

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.alarm_rules
           (tenant_id, facility_id, code, name, description,
            telemetry_point_id, point_code, asset_category_id,
            rule_type, condition, severity, debounce_seconds, auto_clear,
            auto_create_work_order, wo_work_order_type, wo_priority,
            wo_team_id, wo_sla_policy_id, dedupe_window_minutes,
            notify_role_codes, is_active)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5, $6, $7, $8, $9,
                 coalesce(upper($10), 'WARNING'), coalesce($11, 60),
                 coalesce($12, true), coalesce($13, false),
                 coalesce($14, 'CORRECTIVE'), coalesce(upper($15), 'HIGH'),
                 $16, $17, coalesce($18, 120),
                 coalesce($19::text[], '{}'::text[]), coalesce($20, true))
         RETURNING id",
    )
    .bind(body.facility_id)
    .bind(code)
    .bind(name)
    .bind(body.description.as_deref())
    .bind(body.telemetry_point_id)
    .bind(
        body.point_code
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    )
    .bind(body.asset_category_id)
    .bind(&rule_type)
    .bind(&condition)
    .bind(body.severity.as_deref())
    .bind(body.debounce_seconds)
    .bind(body.auto_clear)
    .bind(body.auto_create_work_order)
    .bind(body.wo_work_order_type.as_deref())
    .bind(body.wo_priority.as_deref())
    .bind(body.wo_team_id)
    .bind(body.wo_sla_policy_id)
    .bind(body.dedupe_window_minutes)
    .bind(body.notify_role_codes.as_deref())
    .bind(body.is_active)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;

    let row: RuleDto = sqlx::query_as(&format!(
        "SELECT {COLUMNS}, {DERIVED} {FROM} WHERE r.id = $1"
    ))
    .bind(id)
    .fetch_one(tx.conn())
    .await?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(row)))
}

/// `POST /alarm-rules/{ruleId}/test`
///
/// 以歷史讀數試跑。**沒有副作用** —— 不建告警、不建工單、不寫任何東西。
pub async fn dry_run(
    State(state): State<AssetState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Query(q): Query<DryRunQuery>,
    body: Option<Json<DryRunBody>>,
) -> Result<Json<serde_json::Value>, Problem> {
    let to = q.to.unwrap_or_else(chrono::Utc::now);
    // 預設回看七天：夠長到能看出這條門檻平常會不會響，
    // 又不至於讓一次試跑掃過整個歷史。
    let from = q.from.unwrap_or(to - chrono::Duration::days(7));
    if from >= to {
        return Err(Problem::validation("from 必須早於 to"));
    }
    let override_condition = body.and_then(|Json(b)| b.condition);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 試跑沒有副作用，但它讀得到別人設的門檻與歷史讀數，
    // 而「改門檻前先試」是寫入者的動作 —— 用 write 權限。
    require_permission(&mut tx, "alarm_rule:write", None, None).await?;

    let rule: Option<DryRunTarget> = sqlx::query_as(
        "SELECT code::text AS code, name::text AS name, rule_type,
                condition, telemetry_point_id, point_code::text AS point_code
           FROM fms.alarm_rules WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(tx.conn())
    .await?;
    let DryRunTarget {
        code,
        name,
        rule_type,
        condition: stored_condition,
        telemetry_point_id: rule_point_id,
        point_code: rule_point_code,
    } = rule.ok_or_else(|| Problem::not_found("找不到這條規則（或它不在你的範圍內）"))?;

    let condition = match override_condition {
        Some(c) => {
            validate_condition(&rule_type, &c)?;
            c
        }
        None => stored_condition,
    };

    // 「這種規則現在根本不會被評估」與「試跑結果 0 次」必須分得開。
    let evaluable = rule_type == "THRESHOLD" && condition.get("for_seconds").is_none();
    if !evaluable {
        let reason = if rule_type != "THRESHOLD" {
            format!("rule_type = {rule_type}：057 的評估器只處理 THRESHOLD")
        } else {
            "condition 有 for_seconds（持續型）：單筆讀數判斷不出來".to_string()
        };
        tx.commit().await?;
        return Ok(Json(serde_json::json!({
            "rule": { "id": id, "code": code, "name": name, "rule_type": rule_type },
            "evaluable": false,
            // 不是 0 次，是**評估不了** —— 回 0 會讓人以為門檻很安全。
            "reason": reason,
            "meta": { "from": from, "to": to },
        })));
    }

    // 試跑本體。判斷用 061 的共用述詞，與真跑同一份實作。
    //
    // `fires IS NULL` 單獨計數：那代表 condition 判斷不出來（op 打錯、缺 value），
    // 而真跑時它是「永遠不會響而沒有人知道」。
    let outcome: DryRunOutcome = sqlx::query_as(
        "WITH scoped AS (
           SELECT p.id FROM fms.telemetry_points p
            WHERE fms.alarm_rule_covers_point($1::uuid, $2::text, p.id, p.point_code::text)
         ), sample AS (
           SELECT r.observed_at, r.value_num,
                  fms.telemetry_rule_fires($3::jsonb, r.value_num) AS fires
             FROM fms.telemetry_readings r
            WHERE r.telemetry_point_id IN (SELECT id FROM scoped)
              AND r.observed_at >= $4 AND r.observed_at < $5
            LIMIT $6
         )
         SELECT count(*)::bigint AS scanned,
                count(*) FILTER (WHERE fires)::bigint AS matched,
                count(*) FILTER (WHERE fires IS NULL)::bigint AS unknown,
                min(observed_at) FILTER (WHERE fires) AS first_at,
                max(observed_at) FILTER (WHERE fires) AS last_at,
                -- 越界時最極端的值。門檻該調到哪裡，靠這個數字判斷。
                --
                -- `FILTER` 必須緊接在聚合函式之後，cast 要包在外面：
                -- `min(x)::float8 FILTER (...)` 是語法錯誤。
                CASE WHEN $3::jsonb->>'op' IN ('<','<=')
                     THEN (min(value_num) FILTER (WHERE fires))::float8
                     ELSE (max(value_num) FILTER (WHERE fires))::float8 END AS peak
           FROM sample",
    )
    .bind(rule_point_id)
    .bind(rule_point_code.as_deref())
    .bind(&condition)
    .bind(from)
    .bind(to)
    .bind(MAX_DRY_RUN_READINGS + 1)
    .fetch_one(tx.conn())
    .await?;
    tx.commit().await?;

    let truncated = outcome.scanned > MAX_DRY_RUN_READINGS;
    Ok(Json(serde_json::json!({
        "rule": { "id": id, "code": code, "name": name, "rule_type": rule_type },
        "evaluable": true,
        "condition_used": condition,
        "readings_scanned": outcome.scanned.min(MAX_DRY_RUN_READINGS),
        "would_fire": outcome.matched,
        // 判斷不出來的筆數。> 0 代表這個 condition 設定有問題 ——
        // 真跑時它會進 `bad_rule_codes` 而永遠不觸發。
        "not_evaluable_readings": outcome.unknown,
        "first_breach_at": outcome.first_at,
        "last_breach_at": outcome.last_at,
        "peak_value": outcome.peak,
        "meta": {
            "from": from,
            "to": to,
            "limit": MAX_DRY_RUN_READINGS,
            // 看得見的截斷：掃到上限就停，而結果只反映那一部分。
            "truncated": truncated,
        },
    })))
}

/// 驗 condition。THRESHOLD 以外的型別不驗內容 —— 057 還不評估它們，
/// 而替一個沒有實作的型別發明 schema 就是猜。
fn validate_condition(rule_type: &str, condition: &serde_json::Value) -> Result<(), Problem> {
    if !condition.is_object() {
        return Err(Problem::validation("condition 必須是一個物件"));
    }
    if rule_type != "THRESHOLD" {
        return Ok(());
    }
    let op = condition
        .get("op")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Problem::validation("THRESHOLD 的 condition 要有 op"))?;
    if !OPS.contains(&op) {
        // 這個訊息是這一輪最重要的一個 422：061 的
        // `telemetry_rule_fires` 對不認得的 op 回 NULL，而真跑時那條規則
        // 會進 `bad_rule_codes` —— 也就是設了卻永遠不會響。在建立時就擋掉。
        return Err(Problem::validation(format!(
            "op「{op}」不認得，必須是 {} 其中之一 —— \
             不認得的 op 會讓這條規則永遠不觸發，而且不會有錯誤訊息",
            OPS.join(" ")
        )));
    }
    if condition.get("value").and_then(|v| v.as_f64()).is_none() {
        return Err(Problem::validation(
            "THRESHOLD 的 condition 要有數值的 value —— \
             缺 value 的規則永遠不會觸發",
        ));
    }
    Ok(())
}

fn required<'a>(v: &'a Option<String>, field: &str) -> Result<&'a str, Problem> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation(format!("{field} 為必填")))
}

fn translate(err: sqlx::Error) -> Problem {
    let constraint = match &err {
        sqlx::Error::Database(db) => db.constraint().map(str::to_string),
        _ => None,
    };
    match constraint.as_deref() {
        Some("uq_alarm_rules_code") => {
            Problem::new(fms_shared::ProblemCode::Conflict).with_detail("這個 code 已被使用")
        }
        Some("ck_alarm_rule_scope") => Problem::validation(
            "要有 telemetry_point_id 或 point_code —— \
             一條不知道自己管哪些點位的規則永遠不會響",
        ),
        Some("alarm_rules_facility_id_fkey") => {
            Problem::not_found("找不到這個場域（或它不在你的範圍內）")
        }
        Some("alarm_rules_telemetry_point_id_fkey") => Problem::not_found("找不到這個計量點"),
        Some("alarm_rules_asset_category_id_fkey") => Problem::not_found("找不到這個設備分類"),
        Some("alarm_rules_wo_team_id_fkey") => Problem::not_found("找不到這個團隊"),
        Some("alarm_rules_wo_sla_policy_id_fkey") => Problem::not_found("找不到這個 SLA 政策"),
        _ => Problem::from(err),
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_condition, OPS};
    use serde_json::json;

    #[test]
    fn threshold_conditions_that_would_never_fire_are_rejected() {
        assert!(validate_condition("THRESHOLD", &json!({"op": ">", "value": 28})).is_ok());
        // op 打錯 → 061 回 NULL → 真跑進 bad_rule_codes → 永遠不響。
        assert!(validate_condition("THRESHOLD", &json!({"op": "=>", "value": 28})).is_err());
        assert!(validate_condition("THRESHOLD", &json!({"op": ">"})).is_err());
        assert!(validate_condition("THRESHOLD", &json!({"op": ">", "value": "28"})).is_err());
        assert!(validate_condition("THRESHOLD", &json!([])).is_err());
        // 未實作的型別不驗內容 —— 替沒有實作的東西發明 schema 就是猜。
        assert!(validate_condition("DEVICE_OFFLINE", &json!({"anything": 1})).is_ok());
    }

    /// `OPS` 與 061 的 `telemetry_rule_fires` 必須認得同一組運算子。
    /// 這裡只釘住清單本身；兩邊真的一致由 `alarm_rules_slice.rs` 對資料庫驗。
    #[test]
    fn ops_list_is_the_six_from_migration_061() {
        assert_eq!(OPS.len(), 6);
        for op in [">", ">=", "<", "<=", "=", "!="] {
            assert!(OPS.contains(&op), "{op} 不在清單裡");
        }
    }
}
