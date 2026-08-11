//! 工單滿意度評價（`POST /work-orders/{workOrderId}/satisfaction`）。
//!
//! # 這一支補的是一條斷了三段的鏈
//!
//! 004 就有 `work_orders.satisfaction_score`（CHECK 1–5）與
//! `satisfaction_comment`，`WorkOrderDto` 已經在回傳它們，而 008 的狀態機
//! **在資料裡宣告了觸發點**（`IN_PROGRESS → COMPLETED` 與 SERVICE 的
//! `COMPLETED → CLOSED` 都帶 `"request_satisfaction": true`）。
//!
//! 缺的是中間兩段：`apply_side_effects` 從來不執行那個宣告，而且沒有任何
//! 寫入者。所以那兩欄從 004 到現在一直是 NULL，而每次結案都宣告了一件
//! 不會發生的事。
//!
//! # 授權不是權限碼，是「你是不是申請人」
//!
//! ENDPOINTS.md 寫的是「申請人本人」，而那不能用權限碼表達 —— 任何有
//! `work_order:read` 的人都看得到這張工單。用權限碼會讓管理員能替客戶打分，
//! 而那個數字接著會出現在對客戶的報告裡。
//!
//! 所以條件是 `created_by = fms.current_user_id()`，並且**寫在 SQL 的
//! WHERE 裡**而不是先查再比：先查再比會在兩個語句之間留下一個窗，
//! 而且會需要把「查不到」與「不是你的」分開處理兩次。
//!
//! # 四種失敗要分得開
//!
//! | 情況 | 回應 | 為什麼不是別的 |
//! |---|---|---|
//! | 工單不存在／不在你的範圍 | 404 | 403 會洩漏「這張工單存在」 |
//! | 存在但你不是申請人 | 403 | 404 會讓申請人以為自己的工單不見了 |
//! | 還沒完成 | 409 | 這是狀態衝突，不是輸入錯誤 |
//! | 過了可修改期限 | 409 | 同上，而且 detail 要說出期限是幾天 |
//!
//! 分不開的話，一個申請人在期限外重評會看到「找不到工單」，然後去問管理員
//! 為什麼工單消失了。

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, read_scope, Caller, FieldError, Problem};

use crate::handlers::WorkOrderState;

/// 缺 `satisfaction_editable_days` 設定時的預設天數。
///
/// **與 067 的 `fms.request_satisfaction()` 必須是同一個數字** —— 那裡的邀請
/// 信會寫「{{editable_days}} 天內可修改」。兩邊不一致的話，信裡承諾的期限與
/// 實際擋人的期限不同，而使用者只會看到信。`d_` 那一格拿 SQL 的預設值比對。
pub const DEFAULT_EDITABLE_DAYS: i32 = 14;

#[derive(Debug, Deserialize)]
pub struct SatisfactionRequest {
    pub score: i16,
    #[serde(default)]
    pub comment: Option<String>,
}

/// `POST /work-orders/{workOrderId}/satisfaction`
///
/// 權限是 `work_order:read` **或** `work_order:read_own`，加上申請人本人。
///
/// 用 `read_scope` 而不是 `require_permission("work_order:read")`：
/// `REQUESTER` 只有 `read_own`（`scope.rs` 的檔頭記了這件事 —— 那三個角色
/// 若只檢查完整的 read，連自己報修的工單都看不到）。第一版就是那樣寫的，
/// 症狀是**唯一有資格評分的人拿到 403**。
pub async fn submit(
    State(state): State<WorkOrderState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(req): Json<SatisfactionRequest>,
) -> Result<Json<serde_json::Value>, Problem> {
    // 分數在應用層先擋，而不是讓 004 的 CHECK 擋：資料庫的約束違反會變成
    // 500 或一個看不懂的 detail，而這是一個很容易打錯的輸入。
    if !(1..=5).contains(&req.score) {
        return Err(
            Problem::validation("`score` 必須是 1 到 5").with_errors(vec![FieldError {
                pointer: "/score".to_string(),
                code: "RANGE".to_string(),
                message: format!("{} 不在 1–5 之間", req.score),
            }]),
        );
    }
    if let Some(c) = &req.comment {
        if c.chars().count() > 2000 {
            return Err(Problem::validation("`comment` 最多 2000 字"));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 兩者都沒有才 403。有 read_own 但這一列不是他的，由下面的 `is_owner`
    // 處理成 403（不是 404）—— 見本檔頭的表。
    read_scope(&mut tx, "work_order:read", "work_order:read_own", None).await?;

    // 期限與**它是不是租戶設的**一起讀。只讀值的話，租戶剛好設成 14 天與
    // 完全沒設會回傳一樣的 meta，而那兩件事對讀者的意義不同（前者是政策，
    // 後者是我們的預設值，隨版本可能改）。
    let setting = sqlx::query!(
        r#"SELECT (t.settings ? 'satisfaction_editable_days') AS "from_tenant!",
                  coalesce((t.settings ->> 'satisfaction_editable_days')::int, $1)
                    AS "days!"
             FROM fms.tenants t WHERE t.id = fms.current_tenant_id()"#,
        DEFAULT_EDITABLE_DAYS
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;
    let days = setting.days;

    // 條件式 UPDATE：授權、狀態、期限都在 WHERE 裡，所以沒有先查再寫的窗。
    // `is_owner`／`is_done`／`within_window` 一起回傳，好讓下面分得出
    // 403／409 是哪一種 —— 只回 0 列的話這三種會變成同一個錯誤。
    let row = sqlx::query!(
        r#"WITH target AS (
             SELECT w.id,
                    w.created_by = fms.current_user_id() AS is_owner,
                    w.status IN ('COMPLETED','CLOSED') AS is_done,
                    w.satisfaction_score IS NULL AS is_first,
                    -- 期限從完成時刻起算。`completed_at` 為 NULL 時（狀態是
                    -- COMPLETED 但 044 清過完成時刻）退回 updated_at ——
                    -- 那比直接放行安全，也比拒絕合理。
                    coalesce(w.completed_at, w.updated_at)
                      > clock_timestamp() - make_interval(days => $3) AS within_window
               FROM fms.work_orders w
              WHERE w.id = $1 AND w.deleted_at IS NULL
           ), updated AS (
             UPDATE fms.work_orders w
                SET satisfaction_score = $2, satisfaction_comment = $4
               FROM target t
              WHERE w.id = t.id AND t.is_owner AND t.is_done
                AND (t.is_first OR t.within_window)
             RETURNING w.id
           )
           SELECT t.is_owner, t.is_done, t.is_first, t.within_window,
                  EXISTS (SELECT 1 FROM updated) AS "updated!"
             FROM target t"#,
        id,
        req.score,
        days,
        req.comment.as_deref(),
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?;

    // 沒有這一列 → 不存在，或 RLS 讓它看不見。兩者都回 404：
    // 分開會洩漏「這張工單存在但不屬於你的場域」。
    // 以下幾個提早返回都不 commit —— `tx` 被丟棄時 sqlx 自動 rollback，
    // 而 `TenantTx` 沒有 `rollback()`（`commit()` 才會消耗它）。
    let Some(row) = row else {
        return Err(Problem::not_found("找不到這張工單"));
    };

    if !row.is_owner.unwrap_or(false) {
        return Err(Problem::permission_denied(
            "只有工單的申請人可以評價 —— 代替他評分會讓這個數字失去意義",
        ));
    }
    if !row.is_done.unwrap_or(false) {
        return Err(Problem::new(fms_shared::ProblemCode::Conflict)
            .with_detail("工單還沒完成，現在還不能評價"));
    }
    if !row.updated {
        // 走到這裡只剩一種原因：已經評過，而且過了可修改期限。
        return Err(
            Problem::new(fms_shared::ProblemCode::Conflict).with_detail(format!(
                "已經評價過，而可修改的期限是完成後 {days} 天（由租戶設定 \
             satisfaction_editable_days 決定），現在已經過了"
            )),
        );
    }

    let is_first = row.is_first.unwrap_or(true);
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "data": {
            "work_order_id": id,
            "score": req.score,
            "comment": req.comment,
        },
        "meta": {
            // 第一次評 vs 修改。前端要據此決定顯示「感謝評分」還是「已更新」。
            "was_first_submission": is_first,
            "editable_days": days,
            // 期限是從哪裡來的 —— 與其他端點的 `*_source` meta 同一條規則：
            // 有前提的數字要說出前提。`platform_default` 代表這個數字會隨
            // 版本改，`tenant_setting` 代表它是這個客戶談定的。
            "editable_days_source": if setting.from_tenant {
                "tenant_setting"
            } else {
                "platform_default"
            },
        },
    })))
}
