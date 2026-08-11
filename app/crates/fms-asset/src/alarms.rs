//! 告警（`/alarms`）。
//!
//! # 在這三支端點之前，告警只有機器看得到
//!
//! `fms.alarms` 從 006 就存在，`raise_alarm()` 會建它、去重、並自動開工單
//! （010 的 T4 驗過整條鏈）。但**沒有任何端點讀得到告警** ——
//! 值班的人不知道現在有什麼在響。
//!
//! # `unlinked_only` 是契約裡最有意思的一個參數
//!
//! 「只回傳尚未關聯工單的告警（用於稽核 IoT 與工單的串接缺口）」。
//! 那正是這個系統最容易靜默失效的地方：規則沒設定自動建單、或歷史資料
//! 沒串到 —— 告警響了、沒有人被指派、而沒有任何錯誤訊息。
//! 這個過濾條件把那個缺口變成一個查得到的清單。
//!
//! # 重複開單的防護在資料庫裡
//!
//! `POST /alarms/{id}/work-order` 呼叫 `fms.create_work_order_from_alarm()`
//! （migration 056）。**判定必須是原子的**：在 handler 裡「先讀 work_order_id、
//! 是 NULL 才建」在並發下會建出兩張工單，而其中一張沒有人知道它存在。
//! 056 用條件式 `UPDATE ... WHERE work_order_id IS NULL RETURNING` 解決，
//! 而那與 006 的 `raise_alarm` 是同一條述詞 —— 因此自動建單與人工補建
//! 互相之間也是安全的。

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, concurrency, page, require_permission, Caller, Cursor, PageMeta,
    Problem, SortSpec,
};

use crate::handlers::AssetState;

const ENDPOINT: &str = "POST /alarms/{alarmId}/work-order";

const SEVERITIES: [&str; 5] = ["INFO", "WARNING", "MINOR", "MAJOR", "CRITICAL"];
const STATUSES: [&str; 5] = ["ACTIVE", "ACKNOWLEDGED", "SUPPRESSED", "CLEARED", "CLOSED"];

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AlarmDto {
    pub id: Uuid,
    pub alarm_no: String,
    pub facility_id: Uuid,
    pub rule_code: Option<String>,
    pub severity: String,
    pub status: String,
    pub message: String,
    /// `numeric` 以 `float8` 讀出 —— 這個 codebase 刻意不引入 decimal crate
    /// （`repo.rs` 的計量寫入也是 `$n::float8::numeric`）。告警的門檻與觸發值
    /// 是感測讀數，f64 的精度足夠；金額類的欄位才需要 decimal。
    pub trigger_value: Option<f64>,
    pub threshold_value: Option<f64>,
    pub occurrence_count: i32,
    pub asset_id: Option<Uuid>,
    pub asset_code: Option<String>,
    pub asset_name: Option<String>,
    pub spatial_node_id: Option<Uuid>,
    pub location_name: Option<String>,
    pub work_order_id: Option<Uuid>,
    pub work_order_no: Option<String>,
    pub first_seen_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    pub acknowledged_at: Option<chrono::DateTime<chrono::Utc>>,
    pub acknowledged_by: Option<Uuid>,
    /// 抑制到什麼時候。`status = SUPPRESSED` 時必有值（071 的
    /// `ck_alarms_suppression_bounded`）。
    ///
    /// 列表也回傳它：一個 SUPPRESSED 的告警在值班畫面上必須看得出「到幾點為止」，
    /// 否則「這則為什麼沒響」沒有答案。
    pub suppressed_until: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub facility_id: Option<Uuid>,
    pub severity: Option<String>,
    /// 逗號分隔（契約的例子是 `ACTIVE,ACKNOWLEDGED`）—— 值班畫面要的是
    /// 「還沒結束的」，而那是兩個狀態的聯集。
    pub status: Option<String>,
    pub asset_id: Option<Uuid>,
    pub unlinked_only: Option<bool>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AcknowledgeBody {
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct WorkOrderFromAlarm {
    pub work_order_type: Option<String>,
    pub priority: Option<String>,
    pub team_id: Option<Uuid>,
    pub assignee_id: Option<Uuid>,
    pub title: Option<String>,
}

const COLUMNS: &str = "a.id, a.alarm_no::text AS alarm_no, a.facility_id,
                       r.code::text AS rule_code, a.severity, a.status,
                       a.message::text AS message,
                       a.trigger_value::float8 AS trigger_value,
                       a.threshold_value::float8 AS threshold_value,
                       a.occurrence_count, a.asset_id,
                       ast.asset_code::text AS asset_code, ast.name::text AS asset_name,
                       a.spatial_node_id, sn.name::text AS location_name,
                       a.work_order_id, wo.wo_no::text AS work_order_no,
                       a.first_seen_at, a.last_seen_at, a.acknowledged_at, a.acknowledged_by,
                       a.suppressed_until";

const FROM: &str = "FROM fms.alarms a
                    LEFT JOIN fms.alarm_rules r  ON r.id = a.alarm_rule_id
                    LEFT JOIN fms.assets ast     ON ast.id = a.asset_id
                    LEFT JOIN fms.spatial_nodes sn ON sn.id = a.spatial_node_id
                    LEFT JOIN fms.work_orders wo ON wo.id = a.work_order_id";

/// `GET /alarms`
pub async fn list(
    State(state): State<AssetState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let statuses: Option<Vec<String>> = match q.status.as_deref() {
        Some(s) => {
            let v: Vec<String> = s
                .split(',')
                .map(|x| x.trim().to_uppercase())
                .filter(|x| !x.is_empty())
                .collect();
            for s in &v {
                if !STATUSES.contains(&s.as_str()) {
                    return Err(Problem::validation(format!(
                        "status 必須是 {} 其中之一（可逗號分隔）",
                        STATUSES.join("／")
                    )));
                }
            }
            Some(v)
        }
        None => None,
    };
    if let Some(sev) = q.severity.as_deref() {
        if !SEVERITIES.contains(&sev.to_uppercase().as_str()) {
            return Err(Problem::validation(format!(
                "severity 必須是 {} 其中之一",
                SEVERITIES.join("／")
            )));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "alarm:read", q.facility_id, None).await?;

    let limit = clamp_limit(q.limit);
    // 最新的在前：值班要看的是「剛剛響了什麼」。
    let sort = SortSpec {
        column: "last_seen_at".to_string(),
        desc: true,
    };
    let (ckey, cid) = match q.cursor.as_deref() {
        Some(raw) => {
            let c = Cursor::decode(raw, &sort.column)?;
            (Some(c.as_timestamp()?), Some(c.uuid_id()?))
        }
        None => (None, None),
    };

    let rows: Vec<AlarmDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} {FROM}
          WHERE ($1::uuid IS NULL OR a.facility_id = $1::uuid)
            AND ($2::text IS NULL OR a.severity = upper($2::text))
            AND ($3::text[] IS NULL OR a.status = ANY($3::text[]))
            AND ($4::uuid IS NULL OR a.asset_id = $4::uuid)
            AND (NOT $5::bool OR a.work_order_id IS NULL)
            AND ($6::timestamptz IS NULL
                 OR (a.last_seen_at, a.id) < ($6::timestamptz, $7::uuid))
          ORDER BY a.last_seen_at DESC, a.id DESC
          LIMIT $8"
    ))
    .bind(q.facility_id)
    .bind(q.severity.as_deref())
    .bind(statuses.as_deref())
    .bind(q.asset_id)
    .bind(q.unlinked_only.unwrap_or(false))
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| {
        (r.last_seen_at.to_rfc3339(), r.id)
    });
    Ok(Json(serde_json::json!({
        "data": paged.data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
    })))
}

/// `POST /alarms/{alarmId}/acknowledge`
///
/// 只有 `ACTIVE` 能被確認。已經 `CLEARED`／`CLOSED` 的告警再確認一次沒有意義，
/// 而回 200 會讓操作者以為自己做了什麼 —— 那是這個專案反覆出現的缺陷類型。
///
/// **重複確認是 no-op 而不是錯誤**：已經 `ACKNOWLEDGED` 就原樣回傳，
/// 保留第一次確認的人與時間。第二個人按下按鈕不該把責任歸屬改掉。
pub async fn acknowledge(
    State(state): State<AssetState>,
    caller: Caller,
    Path(alarm_id): Path<Uuid>,
    Json(body): Json<AcknowledgeBody>,
) -> Result<Json<AlarmDto>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let current: Option<(Uuid, String)> =
        sqlx::query_as("SELECT facility_id, status FROM fms.alarms WHERE id = $1")
            .bind(alarm_id)
            .fetch_optional(tx.conn())
            .await?;
    let (facility_id, status) = current.ok_or_else(|| Problem::not_found("找不到這個告警"))?;

    require_permission(&mut tx, "alarm:acknowledge", Some(facility_id), None).await?;

    match status.as_str() {
        "ACTIVE" | "ACKNOWLEDGED" => {}
        other => {
            return Err(Problem::validation(format!(
                "只有 ACTIVE 的告警可以確認 —— 這一則是 {other}"
            )))
        }
    }

    // `WHERE status = 'ACTIVE'` 讓重複確認不覆寫第一次的人與時間。
    // 用條件式 UPDATE 而不是先讀後寫：兩個人同時按下時，
    // 責任歸屬要落在先到的那一個，而不是後寫的那一個。
    sqlx::query(
        "UPDATE fms.alarms
            SET status = 'ACKNOWLEDGED',
                acknowledged_at = clock_timestamp(),
                acknowledged_by = $2,
                context = context || jsonb_build_object('acknowledge_note', $3::text),
                updated_at = clock_timestamp()
          WHERE id = $1 AND status = 'ACTIVE'",
    )
    .bind(alarm_id)
    .bind(caller.user_id)
    .bind(body.note.as_deref())
    .execute(tx.conn())
    .await?;

    let row: AlarmDto = sqlx::query_as(&format!("SELECT {COLUMNS} {FROM} WHERE a.id = $1"))
        .bind(alarm_id)
        .fetch_one(tx.conn())
        .await?;
    tx.commit().await?;

    Ok(Json(row))
}

/// `POST /alarms/{alarmId}/suppress` 的請求。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuppressBody {
    /// 抑制多久。上限由 `tenants.settings.alarm_max_suppress_minutes` 決定
    /// （管理者可設，5–10080，未設定時 1440）。
    ///
    /// **用時長而不是絕對時刻**：操作者想的是「維修這四個小時先別響」，
    /// 而一個絕對時刻要求呼叫端處理時區，那是一個沒有必要的出錯機會。
    pub duration_minutes: i64,
    /// 為什麼要抑制。**必填** —— 下一個看到這則告警為什麼沒響的人需要答案，
    /// 而「誰在什麼時候按了抑制」不足以回答那個問題。
    pub reason: String,
}

/// `POST /alarms/{alarmId}/suppress`
///
/// 需要 **`alarm:suppress`**（071 新增），不是契約原本寫的 `alarm:acknowledge`。
///
/// 理由：`alarm:acknowledge` 的持有者包含 TECHNICIAN 與 SERVICE_STAFF。
/// 「確認」只是留下「有人看到了」的紀錄；「抑制」讓監控在一段時間內不再發報。
/// 現場人員該能做前者，不該能做後者。`alarm:suppress` 授予
/// `alarm_rule:write` 的持有者（MAINTENANCE_SUPERVISOR／TENANT_ADMIN／
/// PLATFORM_ADMIN）—— 能改門檻的人已經可以讓告警安靜，抑制沒有給出新的能力，
/// 只是給了一個有期限、留痕跡的做法。
///
/// # 抑制真正做了什麼
///
/// 071 之前，把狀態設成 `SUPPRESSED` 會讓事情**變糟**：`raise_alarm()` 找既有
/// 告警的條件不含 SUPPRESSED，於是下一次觸發會新增一筆告警加一封通知。
/// 完整說明在 071 檔頭。071 之後：
///
///   * 期限內：`occurrence_count` 繼續累加，但**不發 `alarm.raised`、
///     不自動建單**。抑制的是通知，不是事實 —— 解除後要能看出這段時間響了幾次。
///   * 期限過後：`raise_alarm()` 把它放回 `ACTIVE` 並恢復正常發報。
///
/// # 它不做的事（都寫在回應的 meta 裡）
///
/// 不會排除故障、不會取消已經建好的工單、不會停用規則。
pub async fn suppress(
    State(state): State<AssetState>,
    caller: Caller,
    Path(alarm_id): Path<Uuid>,
    Json(body): Json<SuppressBody>,
) -> Result<Json<serde_json::Value>, Problem> {
    if body.reason.trim().is_empty() {
        return Err(Problem::validation(
            "reason 為必填 —— 下一個發現這則告警沒響的人需要知道為什麼",
        ));
    }
    if body.duration_minutes < 1 {
        return Err(Problem::validation("duration_minutes 必須至少 1 分鐘"));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let current: Option<(Uuid, String)> =
        sqlx::query_as("SELECT facility_id, status FROM fms.alarms WHERE id = $1")
            .bind(alarm_id)
            .fetch_optional(tx.conn())
            .await?;
    let (facility_id, status) = current.ok_or_else(|| Problem::not_found("找不到這個告警"))?;

    require_permission(&mut tx, "alarm:suppress", Some(facility_id), None).await?;

    // 上限是租戶政策（071 把它加進 tenants.settings 的已知鍵）。
    // 預設 1440（一天）是「租戶沒設定時的回退」，不是上限也不是建議值 ——
    // 真正的上下界（5–10080）由形狀約束守著。
    let max_minutes: i32 =
        sqlx::query_scalar("SELECT fms.tenant_setting_int('alarm_max_suppress_minutes', 1440)")
            .fetch_one(tx.conn())
            .await?;
    if body.duration_minutes > max_minutes as i64 {
        return Err(
            Problem::validation("抑制時長超過租戶的上限").with_errors(vec![
                fms_shared::FieldError {
                    pointer: "/duration_minutes".to_string(),
                    code: "MAXIMUM".to_string(),
                    message: format!(
                        "這個租戶最多允許 {max_minutes} 分鐘（tenants.settings.alarm_max_suppress_minutes）"
                    ),
                },
            ]),
        );
    }

    // 已經結束的告警沒有東西可抑制。回 200 會讓操作者以為自己做了什麼。
    let extending = match status.as_str() {
        "ACTIVE" | "ACKNOWLEDGED" => false,
        // 重新抑制是延長期限，不是錯誤：維修拖長了是正常的。
        "SUPPRESSED" => true,
        other => {
            return Err(Problem::validation(format!(
                "只有 ACTIVE／ACKNOWLEDGED／SUPPRESSED 的告警可以抑制 —— 這一則是 {other}"
            )))
        }
    };

    // `suppressed_until` 從資料庫的時鐘算，不從應用層 —— 兩者若有時差，
    // `raise_alarm()`（在資料庫裡比對）看到的期限就不是回應裡說的那一個。
    //
    // 延長時取「現在 + 時長」而不是「原期限 + 時長」：操作者按下「再抑制四小時」
    // 想要的是從現在起四小時，而不是把一個可能早就過期的期限往後推。
    // **兩個語句，不是一個資料修改型 CTE。**
    // `WITH u AS (UPDATE … RETURNING id) SELECT … JOIN u` 回傳的是**更新前**的
    // snapshot（PostgreSQL 手冊 7.8.2），症狀是回應裡的 status 還是 ACTIVE、
    // suppressed_until 還是舊值 —— 也就是「按了抑制但畫面沒變」。
    // COLUMNS 需要四個 JOIN，投影不進 RETURNING，因此照 `acknowledge` 的做法。
    sqlx::query(
        "UPDATE fms.alarms
            SET status = 'SUPPRESSED',
                suppressed_until = clock_timestamp() + ($2::bigint * interval '1 minute'),
                context = context || jsonb_build_object(
                  'suppress_reason', $3::text,
                  'suppressed_by', $4::text,
                  'suppressed_at', clock_timestamp()),
                updated_at = clock_timestamp()
          WHERE id = $1",
    )
    .bind(alarm_id)
    .bind(body.duration_minutes)
    .bind(body.reason.trim())
    .bind(caller.user_id.to_string())
    .execute(tx.conn())
    .await?;

    let row: AlarmDto = sqlx::query_as(&format!("SELECT {COLUMNS} {FROM} WHERE a.id = $1"))
        .bind(alarm_id)
        .fetch_one(tx.conn())
        .await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "data": row,
        "meta": {
            "extended_existing_suppression": extending,
            // 抑制**不做**的事。這幾行是為了讓按下按鈕的人不會以為問題解決了。
            "does_not": [
                "不會排除故障 —— 條件仍然成立，occurrence_count 會繼續累加",
                "不會取消已經由這則告警建立的工單",
                "不會停用規則 —— 期限到了就恢復發報",
            ],
            "max_minutes_allowed": max_minutes,
            "policy_source": "tenants.settings.alarm_max_suppress_minutes（未設定時 1440）",
        },
    })))
}

/// `POST /alarms:reconcile-work-orders` 的請求。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileBody {
    /// **必填**，見 [`reconcile_work_orders`] 的說明。
    pub facility_id: Uuid,
    /// 這一輪最多補幾張。預設 50。
    pub limit: Option<i64>,
}

/// `POST /alarms:reconcile-work-orders`
///
/// 需要 `work_order:create`（FACILITY 範圍）。把「規則說要自動建單、但沒有工單」
/// 的告警補上工單 —— `GET /alarms?unlinked_only=true` 診斷出的那個缺口的修復動作。
///
/// # 為什麼 `facility_id` 是必填
///
/// `work_order:create` 是 **FACILITY 範圍**的權限。一個跨場域的掃描只有兩種
/// 收場：對沒有權限的場域靜默跳過（把部分成功回報成成功 —— 正是這個專案要
/// 避免的缺陷類型），或整批失敗（那讓有權限的部分也修不了）。
///
/// 因此修復是逐場域的，而診斷仍然是租戶級的
/// （`GET /alarms?unlinked_only=true` 不需要 facility_id）。
///
/// # 回應區分四種「沒有補」
///
/// 只回一個 `reconciled: N` 會讓「沒有缺口」與「有缺口但全部被跳過」長得一樣。
/// 因此四個計數各自分開，而且 `skipped_rule_does_not_auto_create`
/// **不是缺口** —— 那些告警的規則本來就沒要求自動建單，把它們算進缺口會讓
/// 這個數字永遠降不到 0，於是沒有人再看它。
pub async fn reconcile_work_orders(
    State(state): State<AssetState>,
    caller: Caller,
    Json(body): Json<ReconcileBody>,
) -> Result<Json<serde_json::Value>, Problem> {
    let limit = body.limit.unwrap_or(50).clamp(1, 500);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "work_order:create", Some(body.facility_id), None).await?;

    // 先把這個場域的缺口分類數出來。**在補之前數**，因為補完之後
    // `reconciled` 那一類就消失了，而回應要說得出「原本有多少」。
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT
           count(*) FILTER (
             WHERE a.status IN ('ACTIVE','ACKNOWLEDGED') AND r.auto_create_work_order)  AS gap,
           count(*) FILTER (WHERE a.status = 'SUPPRESSED')                              AS suppressed,
           count(*) FILTER (
             WHERE coalesce(r.auto_create_work_order, false) = false)                   AS not_auto
         FROM fms.alarms a
         LEFT JOIN fms.alarm_rules r ON r.id = a.alarm_rule_id
        WHERE a.facility_id = $1
          AND a.work_order_id IS NULL
          AND a.status IN ('ACTIVE','ACKNOWLEDGED','SUPPRESSED')",
    )
    .bind(body.facility_id)
    .fetch_one(tx.conn())
    .await?;

    // 只補「規則要求自動建單」的那些。最舊的先補 —— 積壓最久的缺口影響最大。
    //
    // 刻意**排除 SUPPRESSED**：`raise_alarm()` 在抑制期間也不自動建單
    // （071），這裡若補了，同一個條件會依「是誰觸發的」得到不同結果。
    let candidates: Vec<Uuid> = sqlx::query_scalar(
        "SELECT a.id
           FROM fms.alarms a
           JOIN fms.alarm_rules r ON r.id = a.alarm_rule_id
          WHERE a.facility_id = $1
            AND a.work_order_id IS NULL
            AND a.status IN ('ACTIVE','ACKNOWLEDGED')
            AND r.auto_create_work_order
          ORDER BY a.first_seen_at
          LIMIT $2",
    )
    .bind(body.facility_id)
    .bind(limit)
    .fetch_all(tx.conn())
    .await?;

    let mut reconciled = 0i64;
    let mut already_linked = 0i64;
    for alarm_id in &candidates {
        // 056 的判定是條件式 `UPDATE ... WHERE work_order_id IS NULL RETURNING`，
        // 因此與 `raise_alarm()` 的自動建單互斥。回 NULL 代表在我們讀出候選
        // 之後有人（或規則引擎）已經補上了 —— 那是成功的競態，不是錯誤。
        let wo_id: Option<Uuid> = sqlx::query_scalar("SELECT fms.create_work_order_from_alarm($1)")
            .bind(alarm_id)
            .fetch_one(tx.conn())
            .await
            .map_err(translate)?;
        match wo_id {
            Some(_) => reconciled += 1,
            None => already_linked += 1,
        }
    }
    tx.commit().await?;

    let remaining = (counts.0 - candidates.len() as i64).max(0);
    Ok(Json(serde_json::json!({
        "facility_id": body.facility_id,
        "reconciled": reconciled,
        // 讀出候選之後被別人補上的。是競態，不是失敗。
        "already_linked": already_linked,
        // 這一輪的上限之外還剩幾筆缺口。**不回報這個數字，一次呼叫看起來就像
        // 全部補完了** —— 而下一次巡檢才會發現還有 300 筆。
        "remaining_gap": remaining,
        "gap_before": counts.0,
        "meta": {
            "limit_applied": limit,
            // 以下兩類**不是缺口**，列出來是為了讓「為什麼還有未關聯的告警」
            // 有答案 —— 否則 unlinked_only 的清單與這裡的數字對不上。
            "skipped_suppressed": counts.1,
            "skipped_suppressed_reason":
                "抑制期間 raise_alarm 也不自動建單（071）；這裡若補了，\
                 同一個條件會依「是誰觸發的」得到不同結果",
            "skipped_rule_does_not_auto_create": counts.2,
            "skipped_rule_reason":
                "這些告警的規則沒有開 auto_create_work_order —— 它們不是缺口。\
                 要為它們建單請用 POST /alarms/{id}/work-order",
            "diagnosis_endpoint": "GET /alarms?unlinked_only=true（租戶級，不需要 facility_id）",
        },
    })))
}

/// `POST /alarms/{alarmId}/work-order`
///
/// 冪等登記在最前面、回放在授權之後 —— 與 `fms-reservation::create` 同一個
/// 順序，理由也相同（security review 第 1 項：回放走在授權前面等於不檢查授權）。
pub async fn create_work_order(
    State(state): State<AssetState>,
    caller: Caller,
    Path(alarm_id): Path<Uuid>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let raw = body
        .map(|Json(v)| v)
        .unwrap_or_else(|| serde_json::json!({}));
    let req: WorkOrderFromAlarm = serde_json::from_value(raw.clone())
        .map_err(|e| Problem::validation(format!("invalid request body: {e}")))?;

    let idem_key = concurrency::key_from(&headers)?;
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    let pending = match idem_key.as_deref() {
        Some(key) => concurrency::begin(&mut tx, key, ENDPOINT, &raw)
            .await?
            .pending(),
        None => None,
    };

    // 先解析告警所屬場域，才能用正確的範圍檢查權限。
    // 回放也走這一段：回放與首次執行要走同一道門。
    let facility_id: Option<Uuid> =
        sqlx::query_scalar("SELECT facility_id FROM fms.alarms WHERE id = $1")
            .bind(alarm_id)
            .fetch_optional(tx.conn())
            .await?;
    let facility_id = facility_id.ok_or_else(|| Problem::not_found("找不到這個告警"))?;

    let auth = require_permission(&mut tx, "work_order:create", Some(facility_id), None).await?;

    if let Some(replay) = pending {
        let (code, body) = replay.release(auth);
        return Ok((code, Json(body)));
    }

    let wo_id: Option<Uuid> =
        sqlx::query_scalar("SELECT fms.create_work_order_from_alarm($1, $2, $3, $4, $5, $6)")
            .bind(alarm_id)
            .bind(req.work_order_type.as_deref())
            .bind(req.priority.as_deref())
            .bind(req.team_id)
            .bind(req.assignee_id)
            .bind(req.title.as_deref())
            .fetch_one(tx.conn())
            .await
            .map_err(translate)?;

    // 056 回 NULL 只有一個可能：告警已經有工單（不存在的情況上面已經擋掉）。
    let wo_id = wo_id.ok_or_else(|| {
        Problem::new(fms_shared::ProblemCode::Conflict).with_detail(
            "這個告警已經關聯了工單 —— 規則可能已經自動建過。\
             用 GET /alarms?unlinked_only=true 找真正還沒串接的告警",
        )
    })?;

    let wo: serde_json::Value = sqlx::query_scalar(
        "SELECT to_jsonb(w) - 'search_vector' FROM fms.work_orders w WHERE w.id = $1",
    )
    .bind(wo_id)
    .fetch_one(tx.conn())
    .await?;

    if let Some(key) = idem_key.as_deref() {
        concurrency::complete(&mut tx, key, ENDPOINT, 201, &wo).await?;
    }
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(wo)))
}

fn translate(err: sqlx::Error) -> Problem {
    // 056 在輸掉併發時拋 40001（serialization_failure）並回滾自己建的工單。
    // 對呼叫端而言那與「已經有工單」是同一件事：別人先綁上了。
    if let sqlx::Error::Database(db) = &err {
        if db.code().as_deref() == Some("40001") {
            return Problem::new(fms_shared::ProblemCode::Conflict)
                .with_detail("這個告警在同一瞬間被別的請求關聯了工單，請重新查詢");
        }
    }
    Problem::from(err)
}
