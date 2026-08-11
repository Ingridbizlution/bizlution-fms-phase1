//! 封鎖時段（`/resource-blackouts`）。
//!
//! # 這張表**有**讀者，缺的只是寫入端點
//!
//! `fms.resource_blackouts` 從 005 就存在，而它真的在擋預約：011 的
//! `check_resource_availability()` 有一段
//! `WHERE (b.bookable_resource_id = v_res.id OR (b.bookable_resource_id IS NULL
//! AND b.facility_id = v_res.facility_id)) AND b.blackout_range && v_range`，
//! 命中就回衝突。可用性查詢（`repo.rs` 的 `BusyRow`）也把它當成忙碌時段回傳。
//!
//! 所以這一刀不是「讓一張死表活起來」，而是補上**唯一缺的那一半**：
//! 在此之前只有手寫 SQL 建得出封鎖時段。
//!
//! # 順手修掉一個潛伏的缺陷
//!
//! 可用性查詢的封鎖時段那一段是
//! `FROM fms.resource_blackouts bl JOIN fms.bookable_resources b ON b.id =
//! bl.bookable_resource_id` —— **內連接**。因此
//! `bookable_resource_id IS NULL`（全場域封鎖）的列在可用性視圖裡**看不到**，
//! 而 011 的衝突檢查**會**擋。
//!
//! 症狀：日曆顯示可預約，使用者選了，送出得到一個衝突錯誤，而錯誤指向一個
//! 他在畫面上看不到的封鎖時段。
//!
//! 這個缺陷之前是潛伏的（沒有端點建得出全場域封鎖），而 `POST` 正是會產生
//! 那種列的東西 —— 所以修它屬於這一刀的範圍，不是順便重構。
//! 修法是為全場域封鎖加一段 UNION，把它展開到查詢要求的每一個資源上。
//!
//! # GET 用 `reservation:read` 而不是契約寫的 `blackout:write`
//!
//! **刻意偏離。** 封鎖時段的視窗與 `reason` 已經從可用性查詢洩漏出去了
//! （`BusyRow` 的 `kind` 是 `BLACKOUT`／`MAINTENANCE`，`reason` 原樣帶出）。
//! 把清單擋在寫入權限後面不會保護任何東西，只會讓看得到日曆的人問
//! 「為什麼這格是灰的」而得不到答案。
//!
//! POST 仍然是 `blackout:write`。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, page, permission_codes, require_permission, Caller, Cursor,
    FieldError, PageMeta, Problem, ProblemCode, SortSpec,
};

use crate::handlers::ReservationState;

const BLACKOUT_TYPES: [&str; 6] = [
    "MAINTENANCE",
    "HOLIDAY",
    "RENOVATION",
    "PRIVATE_EVENT",
    "EMERGENCY",
    "OTHER",
];

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct BlackoutDto {
    pub id: Uuid,
    pub facility_id: Uuid,
    /// `None` 代表**整個場域**被封鎖，不是「資料缺漏」。
    pub bookable_resource_id: Option<Uuid>,
    /// 資源的顯示名稱。全場域封鎖時是 `None`。
    pub resource_name: Option<String>,
    pub start_at: chrono::DateTime<chrono::Utc>,
    pub end_at: chrono::DateTime<chrono::Utc>,
    pub reason: String,
    pub blackout_type: String,
    pub work_order_id: Option<Uuid>,
    pub work_order_no: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

const COLUMNS: &str = "bl.id, bl.facility_id, bl.bookable_resource_id,
                       coalesce(sn.name, ast.name)::text AS resource_name,
                       bl.start_at, bl.end_at, bl.reason::text AS reason,
                       bl.blackout_type, bl.work_order_id,
                       wo.wo_no::text AS work_order_no,
                       bl.created_by, bl.created_at";

const FROM: &str = "FROM fms.resource_blackouts bl
                    LEFT JOIN fms.bookable_resources br ON br.id = bl.bookable_resource_id
                    LEFT JOIN fms.spatial_nodes sn ON sn.id = br.spatial_node_id
                    LEFT JOIN fms.assets ast ON ast.id = br.asset_id
                    LEFT JOIN fms.work_orders wo ON wo.id = bl.work_order_id";

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub facility_id: Option<Uuid>,
    pub bookable_resource_id: Option<Uuid>,
    /// 視窗起點。**未指定時預設為現在** —— 見 [`list`] 的說明。
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

/// `GET /resource-blackouts`
///
/// 需要 `reservation:read`（見模組說明：為什麼不是 `blackout:write`）。
///
/// # 預設只回「現在起」的封鎖時段
///
/// 「哪裡被封鎖了」是一個**向前看**的問題。不設預設起點的話，第一頁會被
/// 歷史資料佔滿，而使用者要的是「今天之後」。要查歷史就明確給 `from`。
///
/// 套用的視窗回在 `meta.window` 裡 —— 一個被預設值過濾掉的結果如果不說，
/// 看起來就像「沒有封鎖時段」。
pub async fn list(
    State(state): State<ReservationState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    if let (Some(from), Some(to)) = (q.from, q.to) {
        if to <= from {
            return Err(Problem::validation("to 必須晚於 from"));
        }
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "reservation:read", q.facility_id, None).await?;

    let from = q.from.unwrap_or_else(chrono::Utc::now);
    let limit = clamp_limit(q.limit);
    // 由近到遠：使用者要先看到「接下來」的封鎖。
    let sort = SortSpec {
        column: "start_at".to_string(),
        desc: false,
    };
    let (ckey, cid) = match q.cursor.as_deref() {
        Some(raw) => {
            let c = Cursor::decode(raw, &sort.column)?;
            (Some(c.as_timestamp()?), Some(c.uuid_id()?))
        }
        None => (None, None),
    };

    let rows: Vec<BlackoutDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} {FROM}
          WHERE ($1::uuid IS NULL OR bl.facility_id = $1::uuid)
            AND ($2::uuid IS NULL OR bl.bookable_resource_id = $2::uuid)
            -- 重疊而不是「起點在視窗內」：一段昨天開始、明天結束的封鎖
            -- **現在正在生效**，用起點過濾會把它濾掉。
            AND bl.end_at > $3::timestamptz
            AND ($4::timestamptz IS NULL OR bl.start_at < $4::timestamptz)
            AND ($5::timestamptz IS NULL
                 OR (bl.start_at, bl.id) > ($5::timestamptz, $6::uuid))
          ORDER BY bl.start_at, bl.id
          LIMIT $7"
    ))
    .bind(q.facility_id)
    .bind(q.bookable_resource_id)
    .bind(from)
    .bind(q.to)
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.start_at.to_rfc3339(), r.id));
    Ok(Json(serde_json::json!({
        "data": paged.data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
        "meta": {
            // 說出實際套用的視窗。少了它，被預設起點濾掉的結果看起來就像
            // 「沒有封鎖時段」。
            "window": { "from": from, "to": q.to },
            "window_default_applied": q.from.is_none(),
            "note": "bookable_resource_id 為 null 代表整個場域被封鎖，不是資料缺漏",
        },
    })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBlackout {
    pub facility_id: Uuid,
    /// 省略或 `null` 代表**整個場域**。
    pub bookable_resource_id: Option<Uuid>,
    pub start_at: chrono::DateTime<chrono::Utc>,
    pub end_at: chrono::DateTime<chrono::Utc>,
    /// 必填（005 的欄位就是 NOT NULL）。使用者在日曆上看到的就是這句話。
    pub reason: String,
    /// 預設 `MAINTENANCE`。
    pub blackout_type: Option<String>,
    pub work_order_id: Option<Uuid>,
    /// 視窗內已經有預約時，必須明確帶 `true` 才建得起來。
    ///
    /// 見 [`create`]：**封鎖時段不會取消既有預約**，所以默默建立等於讓那些人
    /// 在當天到一間關著的房間前面。
    #[serde(default)]
    pub acknowledge_conflicting_reservations: bool,
}

/// 視窗內會被影響到的既有預約。
#[derive(Debug, Serialize, sqlx::FromRow)]
struct ConflictingReservation {
    id: Uuid,
    reservation_no: String,
    title: Option<String>,
    start_at: chrono::DateTime<chrono::Utc>,
    end_at: chrono::DateTime<chrono::Utc>,
    status: String,
    /// 011 的私人旗標。這份清單會在 409 裡回給操作者，因此同樣要遮罩 ——
    /// 「能建封鎖時段」不等於「能看私人會議的標題」。
    is_private: bool,
    /// 005 的欄位名是 `organizer_id`（不是 `requested_by`）。操作者要通知的人
    /// 就是他 —— 少了這一欄，409 只說得出「有三筆衝突」而說不出要找誰。
    organizer_id: Option<Uuid>,
}

/// `POST /resource-blackouts`
///
/// 需要 `blackout:write`（FACILITY 範圍）。
///
/// # 既有預約不會被取消，因此不能默默建立
///
/// 011 的衝突檢查只擋**新的**預約。一段蓋在三筆已確認預約上的維修封鎖，
/// 不會讓那三筆消失 —— 那三個人當天會到一間關著的房間前面，而系統從頭到尾
/// 沒有說過任何話。
///
/// 所以：視窗內有既有預約時回 **409**，並把那些預約列出來（含 `organizer_id`，
/// 讓操作者知道要通知誰）。要照樣建立必須帶
/// `acknowledge_conflicting_reservations: true`，而回應仍然把清單帶回去。
///
/// **不自動取消**是刻意的：取消別人的預約需要 `reservation:cancel_any`，
/// 那是一個獨立的決定，不該由「建立封鎖時段」順手完成。
pub async fn create(
    State(state): State<ReservationState>,
    caller: Caller,
    Json(body): Json<CreateBlackout>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    if body.reason.trim().is_empty() {
        return Err(Problem::validation(
            "reason 為必填 —— 它是使用者在日曆上看到的說明",
        ));
    }
    if body.end_at <= body.start_at {
        return Err(Problem::validation("end_at 必須晚於 start_at"));
    }
    let blackout_type = body
        .blackout_type
        .as_deref()
        .unwrap_or("MAINTENANCE")
        .to_uppercase();
    if !BLACKOUT_TYPES.contains(&blackout_type.as_str()) {
        return Err(Problem::validation(format!(
            "blackout_type 必須是 {} 其中之一",
            BLACKOUT_TYPES.join("／")
        )));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "blackout:write", Some(body.facility_id), None).await?;

    // 資源必須屬於這個場域。少了這一格，一個跨場域的 id 會建出一筆
    // 「掛在 A 場域、擋著 B 場域資源」的封鎖 —— 而 011 的衝突檢查以
    // `bookable_resource_id` 為主鍵比對，所以它真的會擋，只是沒有人找得到原因。
    if let Some(resource_id) = body.bookable_resource_id {
        let ok: Option<bool> = sqlx::query_scalar(
            "SELECT true FROM fms.bookable_resources
              WHERE id = $1 AND facility_id = $2",
        )
        .bind(resource_id)
        .bind(body.facility_id)
        .fetch_optional(tx.conn())
        .await?;
        if ok.is_none() {
            return Err(Problem::validation(
                "bookable_resource_id 不存在，或不屬於這個 facility_id",
            ));
        }
    }
    // 工單同理：一筆指向別的場域的工單會讓「這段封鎖是為了哪張工單」變成假的。
    if let Some(wo) = body.work_order_id {
        let ok: Option<bool> = sqlx::query_scalar(
            "SELECT true FROM fms.work_orders
              WHERE id = $1 AND facility_id = $2 AND deleted_at IS NULL",
        )
        .bind(wo)
        .bind(body.facility_id)
        .fetch_optional(tx.conn())
        .await?;
        if ok.is_none() {
            return Err(Problem::validation(
                "work_order_id 不存在，或不屬於這個 facility_id",
            ));
        }
    }

    // 會被影響到的既有預約。
    //
    // `bookable_resource_id IS NULL`（全場域）時比對整個場域 —— 與 011 的
    // 衝突檢查同一條述詞，否則這裡說「沒有衝突」而預約端說有。
    //
    // 狀態集合抄自 005 的 `excl_reservations_no_overlap`：只有那幾個狀態
    // 真的佔著時段。已取消／已完成的預約不受封鎖影響。
    // 遮罩的判定條件：**由權限資料決定，不是由「他能建封鎖」推導**。
    //
    // 直覺是「facility_manager 建封鎖時段時當然該看得到要通知誰」。但那個
    // 「當然」就是把管理者可定義的條件寫死 —— 需要看的人把
    // `reservation:view_private` 加進他的角色即可，而那是 seed／角色設定的事。
    // 在這裡開一個「blackout 寫入者一律看得見」的例外，會讓
    // `reservation:view_private` 在這條路徑上再次變成裝飾。
    //
    // 沒有被遮的：`organizer_id`。409 的用途是「你要去通知這些人」，
    // 少了它整個回應就沒有可行動的資訊 —— 而 011 遮的是**姓名與標題**，
    // 不是「有人佔著這個時段」這件事。取捨寫在這裡。
    let may_view_private = permission_codes(&mut tx, Some(body.facility_id), None)
        .await?
        .contains("reservation:view_private");

    let mut conflicts: Vec<ConflictingReservation> = sqlx::query_as(
        "SELECT r.id, r.reservation_no::text AS reservation_no, r.title::text AS title,
                r.start_at, r.end_at, r.status, r.organizer_id, r.is_private
           FROM fms.reservations r
          WHERE r.facility_id = $1
            AND ($2::uuid IS NULL
                 OR r.resource_id = (SELECT coalesce(br.spatial_node_id, br.asset_id)
                                       FROM fms.bookable_resources br WHERE br.id = $2::uuid))
            AND r.status IN ('PENDING_APPROVAL','CONFIRMED','CHECKED_IN')
            AND tstzrange(r.start_at, r.end_at, '[)')
                && tstzrange($3::timestamptz, $4::timestamptz, '[)')
          ORDER BY r.start_at
          LIMIT 200",
    )
    .bind(body.facility_id)
    .bind(body.bookable_resource_id)
    .bind(body.start_at)
    .bind(body.end_at)
    .fetch_all(tx.conn())
    .await?;

    if !may_view_private {
        for c in conflicts.iter_mut().filter(|c| c.is_private) {
            c.title = None;
        }
    }

    if !conflicts.is_empty() && !body.acknowledge_conflicting_reservations {
        // **409 而不是靜默建立。** 封鎖時段不會取消這些預約，所以不讓操作者
        // 看到它們就等於讓那些人白跑一趟。
        let listed = serde_json::to_value(&conflicts)
            .map_err(|e| Problem::internal(std::io::Error::other(e.to_string())))?;
        return Err(Problem::new(ProblemCode::Conflict)
            .with_detail(format!(
                "這個視窗內有 {} 筆既有預約。封鎖時段**不會**取消它們 —— \
                 確認要照樣建立請帶 acknowledge_conflicting_reservations: true，\
                 並自行通知這些預約人",
                conflicts.len()
            ))
            .with_errors(vec![FieldError {
                pointer: "/start_at".to_string(),
                code: "RESERVATION_CONFLICT".to_string(),
                message: listed.to_string(),
            }]));
    }

    // **兩個語句，不是一個資料修改型 CTE。**
    // `WITH i AS (INSERT … RETURNING id) SELECT … JOIN i` 會回**零筆**：
    // WITH 的各個 sub-statement 看不到彼此對目標表的效果（PostgreSQL 手冊
    // 7.8.2），所以外層對 `resource_blackouts` 的 SELECT 讀不到剛插入的那一列。
    // （UPDATE 版的同一個陷阱是「回傳更新前的值」，INSERT 版是「什麼都回不到」。）
    let new_id: Uuid = sqlx::query_scalar(
        "INSERT INTO fms.resource_blackouts
                (tenant_id, facility_id, bookable_resource_id, start_at, end_at,
                 reason, blackout_type, work_order_id, created_by)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING id",
    )
    .bind(body.facility_id)
    .bind(body.bookable_resource_id)
    .bind(body.start_at)
    .bind(body.end_at)
    .bind(body.reason.trim())
    .bind(&blackout_type)
    .bind(body.work_order_id)
    .bind(caller.user_id)
    .fetch_one(tx.conn())
    .await?;

    let row: BlackoutDto = sqlx::query_as(&format!("SELECT {COLUMNS} {FROM} WHERE bl.id = $1"))
        .bind(new_id)
        .fetch_one(tx.conn())
        .await?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "data": row,
            "meta": {
                // 帶回清單，讓操作者有名單可以通知。
                "conflicting_reservations": conflicts,
                "conflicting_reservation_count": conflicts.len(),
                "does_not": [
                    "不會取消既有預約 —— 取消別人的預約需要 reservation:cancel_any，那是獨立的決定",
                    "不會通知既有預約人 —— 這一步目前沒有自動化",
                ],
                "affects": if body.bookable_resource_id.is_some() {
                    "指定的可預約資源"
                } else {
                    "整個場域的所有可預約資源"
                },
            },
        })),
    ))
}
