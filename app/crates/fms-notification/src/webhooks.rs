//! 客戶端 webhook 訂閱（`/webhooks`）。
//!
//! # 契約只有 GET 與 POST
//!
//! 沒有 PATCH，也沒有 DELETE。一個關不掉的 webhook 是兩個問題：它會永遠敲一個
//! 死掉的端點，而且它是一條關不掉的**資料外送通道**。
//!
//! 因此 `POST` 對既有的 url 是**更新**（072 的 `(tenant_id, url)` 唯一鍵）——
//! 帶 `is_active: false` 就能停用。這讓「關掉」在契約宣告的兩個動作之內做得到，
//! 不需要偷偷加一支契約沒有的端點。
//!
//! # 簽章金鑰只回傳一次
//!
//! 金鑰由伺服器產生，**只在建立時出現在回應裡**。之後任何端點都拿不到它 ——
//! 不是因為程式碼記得不要選它，而是因為 072 對 `fms_app` 做了欄位級
//! `REVOKE SELECT (signing_secret)`。一次 `SELECT *` 也讀不出來。
//!
//! 更新既有訂閱時不重新產生金鑰（客戶端那一側已經存了）。要換金鑰目前沒有
//! 端點 —— 而 072 連 `UPDATE (signing_secret)` 的權限都沒給 `fms_app`，
//! 所以那不是「忘了做」，是「做不到」。
//!
//! # 事件型別白名單
//!
//! 訂閱一個系統不會發出的事件是靜默失敗：訂閱建得起來、清單裡看起來正常、
//! 而永遠不會收到東西。因此 `POST` 拒絕不在
//! [`fms_worker::webhook::SUBSCRIBABLE_EVENT_TYPES`] 裡的型別，
//! 而 `GET` 把清單回傳出去。打錯字因此在訂閱時就是 422。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, page, require_tenant_scoped_permission, Caller, Cursor,
    FieldError, PageMeta, Problem, SortSpec,
};

#[derive(Clone)]
pub struct WebhookState {
    pub pool: PgPool,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct SubscriptionDto {
    pub id: Uuid,
    pub url: String,
    pub event_types: Vec<String>,
    pub description: Option<String>,
    pub is_active: bool,
    /// 連續失敗數。達門檻會自動停用（`disabled_reason` 會寫明）。
    pub consecutive_failures: i32,
    pub disabled_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 為什麼被停用。**分得出「客戶自己關的」與「系統因為連續失敗關的」** ——
    /// 前者是 `None`，後者有值。
    pub disabled_reason: Option<String>,
    pub last_success_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_failure_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// **刻意不含 `signing_secret`。** 072 的欄位級 REVOKE 讓 `fms_app` 連讀都讀不到，
/// 因此這裡不是「記得不要選它」，而是選了會拿到權限錯誤。
const COLUMNS: &str = "id, url, event_types, description, is_active,
                       consecutive_failures, disabled_at, disabled_reason,
                       last_success_at, last_failure_at, last_error,
                       created_at, updated_at";

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub limit: Option<i64>,
    pub cursor: Option<String>,
    /// 預設連停用的也回：一筆被系統自動停用的訂閱正是最需要被看到的那一筆。
    pub active_only: Option<bool>,
}

/// `GET /webhooks`
///
/// 需要 `tenant:update`（TENANT 範圍）。
///
/// **讀取用寫入權限是刻意的**：訂閱清單裡有客戶的內部端點網址，那是他們的
/// 基礎架構資訊。看得到工單的人不需要、也不該看得到「資料被送到哪裡去」。
///
/// # 回應帶整合方需要的三件事
///
/// `meta.subscribable_event_types`、`meta.signature_scheme`、
/// `meta.delivery_semantics`。少了它們，整合方會（依序）訂到一個不存在的事件、
/// 只驗 body 的簽章（於是可被重放）、以及假設投遞是恰好一次。
pub async fn list(
    State(state): State<WebhookState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "tenant:update").await?;

    let limit = clamp_limit(q.limit);
    let sort = SortSpec {
        column: "url".to_string(),
        desc: false,
    };
    let (ckey, cid) = match q.cursor.as_deref() {
        Some(raw) => {
            let c = Cursor::decode(raw, &sort.column)?;
            (Some(c.key.clone()), Some(c.uuid_id()?))
        }
        None => (None, None),
    };

    let rows: Vec<SubscriptionDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM fms.webhook_subscriptions
          WHERE (NOT $1::bool OR is_active)
            AND ($2::text IS NULL OR (url, id) > ($2::text, $3::uuid))
          ORDER BY url, id
          LIMIT $4"
    ))
    .bind(q.active_only.unwrap_or(false))
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1)
    .fetch_all(tx.conn())
    .await?;

    // 被系統自動停用的訂閱數。**不回報這個，一份看起來正常的清單會讓人
    // 以為事件都送出去了** —— 而那些訂閱早就停了。
    let auto_disabled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fms.webhook_subscriptions WHERE disabled_reason IS NOT NULL",
    )
    .fetch_one(tx.conn())
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.url.clone(), r.id));
    Ok(Json(serde_json::json!({
        "data": paged.data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
        "meta": {
            "subscribable_event_types": fms_worker::webhook::SUBSCRIBABLE_EVENT_TYPES,
            // 整合方需要知道簽章是**對什麼**算的。只驗 body 的實作會接受重放。
            "signature_scheme": {
                "header": "X-FMS-Signature",
                "format": "sha256=<hex>",
                "algorithm": "HMAC-SHA256",
                "signed_payload": "<X-FMS-Timestamp>.<raw request body>",
                "why_timestamp": "時間戳在簽章裡，否則被錄下的請求可以原封不動重放到永遠。\
                                  接收端應拒絕時間戳過舊的請求。",
            },
            "delivery_semantics": "at-least-once —— 投遞成功但狀態回寫前崩潰會重送。\
                                   接收端必須自行幂等（可用 payload 的 aggregate_id \
                                   加 X-FMS-Event 去重）。",
            "auto_disabled_count": auto_disabled,
            "auto_disable_note": "連續失敗達門檻會自動停用並寫入 disabled_reason；\
                                  重新啟用請以同一個 url 呼叫 POST 並帶 is_active: true",
        },
    })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsertSubscription {
    /// 必須是 https（072 的 CHECK 與應用層的 SSRF 閘門都擋）。
    pub url: String,
    pub event_types: Vec<String>,
    pub description: Option<String>,
    /// 省略時：新建為 `true`，更新既有時**不動**。
    pub is_active: Option<bool>,
}

/// `POST /webhooks`
///
/// 需要 `tenant:update`。對既有的 url 是**更新**（見模組說明）。
///
/// # 新建時回應帶 `signing_secret`，更新時不帶
///
/// 金鑰只會出現這一次。更新既有訂閱時不重新產生 —— 客戶端那一側已經存了同一份，
/// 換掉會讓所有簽章驗證在他們部署新金鑰之前失敗。
///
/// # 網址在存進去之前就過一次 SSRF 閘門
///
/// 072 的 CHECK 只擋非 https。指向 `169.254.169.254` 的 https 網址會通過那個
/// CHECK —— 因此這裡先跑一次 `resolve_and_check`。
///
/// 投遞時**還會再跑一次**（`fms_worker::webhook`），因為 DNS 記錄可以在建立
/// 之後被改掉。這裡這一次的價值是**讓錯誤在建立時就出現**，而不是變成一筆
/// 永遠投遞失敗的訂閱。
pub async fn upsert(
    State(state): State<WebhookState>,
    caller: Caller,
    Json(body): Json<UpsertSubscription>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let url = body.url.trim().to_string();

    // **授權先於一切驗證。**
    //
    // 這一段的順序是被測試逼出來的（`e_`）：第一版把網址與事件型別的驗證放在
    // 授權之前，於是一個沒有 `tenant:update` 的使用者可以
    //
    //   * 送一個內部主機名，從回應是「解析失敗」還是「解析到私有位址」判斷
    //     那台機器存不存在 —— 一支 DNS 與內網探測器；
    //   * 送一個亂打的事件型別，從 422 的訊息拿到完整的可訂閱事件清單。
    //
    // 兩者都不嚴重，但它們是同一個錯誤：**回應的內容洩漏了呼叫者無權取得的
    // 資訊**。security review 第 1 項的同一條紀律。
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_tenant_scoped_permission(&mut tx, "tenant:update").await?;

    if url.is_empty() {
        return Err(Problem::validation("url 為必填"));
    }
    if body.event_types.is_empty() {
        return Err(Problem::validation(
            "event_types 至少要有一個 —— 空的訂閱永遠不會觸發，而它在清單裡看起來是正常的",
        ));
    }

    // 事件型別白名單。訂一個系統不會發出的事件是靜默失敗，因此在這裡就 422。
    let unknown: Vec<&String> = body
        .event_types
        .iter()
        .filter(|t| !fms_worker::webhook::SUBSCRIBABLE_EVENT_TYPES.contains(&t.as_str()))
        .collect();
    if !unknown.is_empty() {
        return Err(Problem::validation(format!(
            "不認識的事件型別：{}。可訂閱的是：{}",
            unknown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("、"),
            fms_worker::webhook::SUBSCRIBABLE_EVENT_TYPES.join("、")
        ))
        .with_errors(vec![FieldError {
            pointer: "/event_types".to_string(),
            code: "UNKNOWN_EVENT_TYPE".to_string(),
            message: "訂閱一個系統不會發出的事件永遠不會收到東西，而那是靜默的".to_string(),
        }]));
    }

    // SSRF 閘門。**在寫入之前** —— 一筆指向內網的訂閱不該存在於資料庫裡，
    // 即使投遞時還會再檢查一次。
    //
    // # 解析失敗**不是**拒絕的理由
    //
    // 客戶常常在端點還沒上線時就先來註冊，而 DNS 也可能只是暫時查不到。
    // 那兩種情況都不代表這個網址有問題，而送出時的檢查才是權威的
    // （`fms_worker::webhook` 每次投遞都重跑一次）。
    //
    // 在這裡因為 DNS 查不到就回 422，會讓一個合法的整合流程卡住，
    // 而使用者完全不知道要等什麼。因此：靜態問題（非 https、帶憑證、
    // 網址解析不了）與**解析出私有位址**要拒絕；解析不到位址只回報成警告。
    let outbound = fms_shared::OutboundSettings::default();
    let dns_warning = match fms_shared::safe_http::resolve_and_check(&url, &outbound).await {
        Ok(_) => None,
        Err(
            rejection @ (fms_shared::OutboundRejected::DnsFailed(_)
            | fms_shared::OutboundRejected::NoAddress),
        ) => Some(rejection.to_string()),
        Err(rejection) => {
            return Err(
                Problem::validation(format!("url 不可用：{rejection}")).with_errors(vec![
                    FieldError {
                        pointer: "/url".to_string(),
                        code: "TARGET_NOT_PERMITTED".to_string(),
                        message: rejection.to_string(),
                    },
                ]),
            )
        }
    };

    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM fms.webhook_subscriptions WHERE url = $1")
            .bind(&url)
            .fetch_optional(tx.conn())
            .await?;

    let (status_code, secret) = match existing {
        Some(id) => {
            // 更新。**不動 signing_secret** —— 072 連 UPDATE 權限都沒給。
            //
            // 重新啟用時要清掉 `disabled_at`／`disabled_reason`，否則一筆
            // 手動重新啟用的訂閱會永遠帶著「因連續失敗自動停用」的說明。
            sqlx::query(
                "UPDATE fms.webhook_subscriptions
                    SET event_types = $2,
                        description = coalesce($3, description),
                        is_active = coalesce($4, is_active),
                        disabled_at = CASE WHEN coalesce($4, is_active)
                                           THEN NULL ELSE disabled_at END,
                        disabled_reason = CASE WHEN coalesce($4, is_active)
                                               THEN NULL ELSE disabled_reason END,
                        updated_at = clock_timestamp()
                  WHERE id = $1",
            )
            .bind(id)
            .bind(&body.event_types)
            .bind(body.description.as_deref())
            .bind(body.is_active)
            .execute(tx.conn())
            .await?;
            (StatusCode::OK, None)
        }
        None => {
            let generated = generate_secret();
            sqlx::query(
                "INSERT INTO fms.webhook_subscriptions
                        (tenant_id, url, event_types, signing_secret, description,
                         is_active, created_by)
                 VALUES (fms.current_tenant_id(), $1, $2, $3, $4,
                         coalesce($5, true), $6)",
            )
            .bind(&url)
            .bind(&body.event_types)
            .bind(&generated)
            .bind(body.description.as_deref())
            .bind(body.is_active)
            .bind(caller.user_id)
            .execute(tx.conn())
            .await?;
            (StatusCode::CREATED, Some(generated))
        }
    };

    // 兩個語句，不是資料修改型 CTE：後者的 INSERT 版會回零筆、UPDATE 版會回
    // 更新前的值（PostgreSQL 手冊 7.8.2）。
    let row: SubscriptionDto = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM fms.webhook_subscriptions WHERE url = $1"
    ))
    .bind(&url)
    .fetch_one(tx.conn())
    .await?;
    tx.commit().await?;

    Ok((
        status_code,
        Json(serde_json::json!({
            "data": row,
            // 只在新建時有值。**這是它唯一出現的機會** —— 072 讓 API 之後
            // 連讀都讀不到（欄位級 REVOKE SELECT）。
            "signing_secret": secret,
            "meta": {
                "secret_shown_once": secret.is_some(),
                "secret_note": if secret.is_some() {
                    "請立即保存 —— 這是它唯一一次出現在回應裡。\
                     API 之後讀不到它（資料庫層的欄位級權限，不是程式碼的自律）。\
                     目前沒有換金鑰的端點。"
                } else {
                    "更新既有訂閱不重新產生金鑰 —— 客戶端那一側已經存了同一份，\
                     換掉會讓簽章驗證在對方部署新金鑰之前全部失敗。"
                },
                "signature_scheme": "HMAC-SHA256(secret, \"<X-FMS-Timestamp>.<body>\")，\
                                     以 X-FMS-Signature: sha256=<hex> 送出",
                // 解析不到位址不是拒絕的理由（見 upsert 的說明），但要說出來 ——
                // 否則一筆永遠投遞失敗的訂閱看起來是正常的。
                "dns_warning": dns_warning,
                "url_checked_at_creation_and_at_every_delivery":
                    "建立時檢查過位址，投遞時**每次**再檢查一次 —— \
                     DNS 記錄可以在建立之後被改成指向內網",
            },
        })),
    ))
}

/// 產生 256 bit 的簽章金鑰（十六進位字串）。
///
/// 用兩個 UUIDv4 拼起來而不是引入一個 RNG crate：`uuid` 的 v4 走 `getrandom`
/// （作業系統的 CSPRNG），兩個各 122 bit 的隨機位元合起來遠超過 HMAC-SHA256
/// 需要的熵。**這不是「用 UUID 當密碼」那種誤用** —— 誤用是把可預測的
/// v1／v7 或單一 UUID 的字串形式當金鑰；這裡取的是 v4 的隨機位元本身。
fn generate_secret() -> String {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    a.as_bytes()
        .iter()
        .chain(b.as_bytes().iter())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
