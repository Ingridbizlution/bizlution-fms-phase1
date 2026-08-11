//! 預約的資料存取。
//!
//! 幾個刻意的分工：
//!   * 可用性判定呼叫 `fms.check_resource_availability()` —— 營業時間、封鎖時段、
//!     固定座位、hold、時長規則共 9 種 conflict_type 都在那支函式裡，
//!     應用層不重新實作。
//!   * 但**最終權威仍是排他約束**（ADR-04）。函式回報可用只是為了給出好的錯誤
//!     訊息；真正防止雙重預訂的是 INSERT 時的 `excl_reservations_no_overlap`。
//!     T11 已證實 100 路併發下恰好一筆成功。
//!   * `reservation_no` 由 `fms.next_document_no()` 產生，不在應用層編號。

use uuid::Uuid;

use fms_shared::{Cursor, Problem, TenantTx};

/// 契約未提供 `sort`，排序固定於 `start_at` 降冪。
pub const RESERVATION_SORT_COLUMN: &str = "start_at";

/// 查詢用的列。`resource_name` 來自 bookable_resources 的 display_name。
pub struct ReservationRow {
    pub id: Uuid,
    pub reservation_no: String,
    pub facility_id: Uuid,
    pub resource_id: Uuid,
    pub resource_name: String,
    pub resource_type: String,
    pub title: Option<String>,
    pub purpose: Option<String>,
    pub party_size: i32,
    pub start_at: chrono::DateTime<chrono::Utc>,
    pub end_at: chrono::DateTime<chrono::Utc>,
    pub status: String,
    pub organizer_id: Uuid,
    /// 代訂時的實際使用者。擁有權判定要算它 —— 別人幫你訂的會議室是你的，
    /// 只看 `organizer_id` 會讓被代訂的人看不到自己的預約。
    /// 刻意不進 DTO：契約的 `Reservation` 沒有這個欄位。
    pub on_behalf_of_id: Option<Uuid>,
    pub organizer_display_name: String,
    /// 011 的 `is_private`。**讀取路徑必須讀它** —— 標題、備註與主辦人姓名
    /// 對非本人／非 `reservation:view_private` 持有者要遮罩。
    /// 這一欄在 011 建立時就有，但直到現在才有讀者。
    pub is_private: bool,
    pub approval_required: bool,
    pub requires_check_in: bool,
    pub checked_in_at: Option<chrono::DateTime<chrono::Utc>>,
    pub auto_release_at: Option<chrono::DateTime<chrono::Utc>>,
    pub recurrence_group_id: Option<Uuid>,
    pub created_via: String,
    pub version: i32,
}

impl ReservationRow {
    /// 該列的游標鍵。與查詢的 ORDER BY（`start_at DESC, id DESC`）一致。
    ///
    /// 契約的 `GET /reservations` 沒有 `sort` 參數，因此排序固定，
    /// 這裡忽略傳入的欄位名。
    pub fn cursor_key(&self, _sort_column: &str) -> (String, Uuid) {
        (self.start_at.to_rfc3339(), self.id)
    }
}

// 注意：以下兩支查詢的 SELECT 主體刻意重複書寫。
// `query_as!` 的第一個參數必須是字串字面值，不接受 `concat!` 或字串相加 ——
// 這是換取編譯期驗證必須付的代價。抽成常數或巨集會讓驗證失效。

/// 列表查詢。RLS 已限定租戶，因此不需寫 tenant_id 條件。
///
/// 排序 `start_at DESC, id DESC` 並多取一列以判斷是否還有下一頁。
/// 過濾條件一律以「參數為 NULL 時不生效」的形式表達，避免動態組 SQL
/// （那會使 `query_as!` 的編譯期驗證失效）。
#[allow(clippy::too_many_arguments)]
pub async fn list(
    tx: &mut TenantTx,
    facility_id: Option<Uuid>,
    resource_id: Option<Uuid>,
    organizer_id: Option<Uuid>,
    status: Option<&str>,
    from: Option<chrono::DateTime<chrono::Utc>>,
    to: Option<chrono::DateTime<chrono::Utc>>,
    cursor: Option<&Cursor>,
    limit: i64,
) -> Result<Vec<ReservationRow>, Problem> {
    let (cursor_start, cursor_id) = match cursor {
        Some(c) => (Some(c.as_timestamp()?), Some(c.uuid_id()?)),
        None => (None, None),
    };

    sqlx::query_as!(
        ReservationRow,
        r#"        SELECT r.id,
               r.reservation_no::text AS "reservation_no!",
               r.facility_id,
               r.resource_id,
               coalesce(br.display_name::text, '(unknown)') AS "resource_name!",
               r.resource_type,
               r.title::text AS "title",
               r.purpose::text AS "purpose",
               r.party_size,
               r.start_at,
               r.end_at,
               r.status,
               r.organizer_id,
               r.on_behalf_of_id,
               coalesce(u.display_name::text, '(unknown)') AS "organizer_display_name!",
               r.is_private,
               r.approval_required,
               r.requires_check_in,
               r.checked_in_at,
               r.auto_release_at,
               r.recurrence_group_id,
               r.created_via,
               r.version
        FROM fms.reservations r
        LEFT JOIN fms.bookable_resources br ON br.id = r.bookable_resource_id
        LEFT JOIN fms.users u ON u.id = r.organizer_id
        WHERE ($1::uuid IS NULL OR r.facility_id = $1)
          AND ($2::uuid IS NULL OR r.resource_id = $2)
          AND ($3::uuid IS NULL OR r.organizer_id = $3)
          AND ($4::text IS NULL OR r.status = $4)
          AND ($5::timestamptz IS NULL OR r.end_at >= $5)
          AND ($6::timestamptz IS NULL OR r.start_at <= $6)
          AND ($7::timestamptz IS NULL
               OR (r.start_at, r.id) < ($7::timestamptz, $8::uuid))
        ORDER BY r.start_at DESC, r.id DESC
        LIMIT $9
        "#,
        facility_id,
        resource_id,
        organizer_id,
        status,
        from,
        to,
        cursor_start,
        cursor_id,
        limit + 1
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

pub async fn get(tx: &mut TenantTx, id: Uuid) -> Result<Option<ReservationRow>, Problem> {
    sqlx::query_as!(
        ReservationRow,
        r#"        SELECT r.id,
               r.reservation_no::text AS "reservation_no!",
               r.facility_id,
               r.resource_id,
               coalesce(br.display_name::text, '(unknown)') AS "resource_name!",
               r.resource_type,
               r.title::text AS "title",
               r.purpose::text AS "purpose",
               r.party_size,
               r.start_at,
               r.end_at,
               r.status,
               r.organizer_id,
               r.on_behalf_of_id,
               coalesce(u.display_name::text, '(unknown)') AS "organizer_display_name!",
               r.is_private,
               r.approval_required,
               r.requires_check_in,
               r.checked_in_at,
               r.auto_release_at,
               r.recurrence_group_id,
               r.created_via,
               r.version
        FROM fms.reservations r
        LEFT JOIN fms.bookable_resources br ON br.id = r.bookable_resource_id
        LEFT JOIN fms.users u ON u.id = r.organizer_id
        WHERE r.id = $1"#,
        id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

/// `fms.check_resource_availability()` 的回傳。
pub struct Availability {
    pub is_available: bool,
    pub conflict_type: Option<String>,
    pub detail: Option<String>,
}

/// 可用性預檢。用途是給出可讀的錯誤，不是防止重複 —— 後者由約束保證。
pub async fn check_availability(
    tx: &mut TenantTx,
    resource_id: Uuid,
    start_at: chrono::DateTime<chrono::Utc>,
    end_at: chrono::DateTime<chrono::Utc>,
    user_id: Uuid,
) -> Result<Availability, Problem> {
    let row = sqlx::query_as!(
        Availability,
        r#"SELECT is_available AS "is_available!",
                  conflict_type,
                  detail
           FROM fms.check_resource_availability($1, $2, $3, NULL, $4)"#,
        resource_id,
        start_at,
        end_at,
        user_id
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(row)
}

/// 消耗一個短期佔位。回傳 `false` 表示這個佔位不可用。
///
/// # 為什麼是單一個 UPDATE 而不是先查再改
///
/// 兩個併發請求可能拿著同一個 token。`SELECT` 檢查完再 `UPDATE` 之間有空窗，
/// 兩者都會通過檢查、都會建立預約 —— 而佔位的用意正是防止這件事。
/// 把全部條件寫進 `WHERE` 之後，這是一次 compare-and-set：
/// PostgreSQL 的列鎖讓兩者序列化，後到的看到 `status` 已是 `CONSUMED`，
/// 影響 0 列，於是被拒。
///
/// # 回 `false` 的四種情況刻意不可分辨
///
/// token 不存在／已被消耗／已過期／屬於別人／範圍不涵蓋請求的時段 ——
/// 全部回同一個結果。理由與登入失敗相同：能分辨就等於一個預言機，
/// 可以用來探測「這個 token 是否存在」。
///
/// `tenant_id` 不寫進 `WHERE`：本函式在已設情境的交易內執行，RLS 已保證
/// 只看得到本租戶的列（與 `find_auth_user_by_username` 同一個理由）。
///
/// # 範圍用「涵蓋」而不是「相等」
///
/// `hold_range @> 請求範圍`：佔位 09:00–10:00 可以用來建立 09:15–09:45。
/// 這是安全的 —— 涵蓋檢查不可能給出比當初佔下的更多，而在那個窗內
/// 其他人本來就被排他約束擋著。要求完全相等會讓「確認時微調時間」
/// 必須重新佔位，而那不是安全上的必要。
///
/// 佔位一旦消耗就整個消耗，不會切成剩餘區段：它是一個三分鐘的鎖，
/// 不是一段可分割的庫存。
pub async fn consume_hold(
    tx: &mut TenantTx,
    hold_token: &str,
    bookable_resource_id: Uuid,
    start_at: chrono::DateTime<chrono::Utc>,
    end_at: chrono::DateTime<chrono::Utc>,
) -> Result<bool, Problem> {
    let user_id = tx.context().user_id;
    let done = sqlx::query!(
        r#"
        UPDATE fms.reservation_holds
           SET status = 'CONSUMED'
         WHERE hold_token = $1
           AND user_id = $2
           AND bookable_resource_id = $3
           AND status = 'ACTIVE'
           AND expires_at > clock_timestamp()
           AND hold_range @> tstzrange($4, $5, '[)')
        "#,
        hold_token,
        user_id,
        bookable_resource_id,
        start_at,
        end_at
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;

    Ok(done.rows_affected() == 1)
}

/// 場域時區（IANA 名稱）。週期展開必須在當地時區進行 ——
/// 「每週三上午 10 點」是當地時間的敘述，在 UTC 展開會讓跨夏令時的
/// 場域整批位移一小時（理由詳見 `fms_shared::schedule` 的模組註解）。
pub async fn facility_timezone(
    tx: &mut TenantTx,
    facility_id: Uuid,
) -> Result<Option<String>, Problem> {
    sqlx::query_scalar!(
        r#"SELECT timezone::text AS "timezone!" FROM fms.facilities WHERE id = $1"#,
        facility_id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

/// `service_items` 上與「能不能附加到這筆預約」相關的全部欄位。
///
/// 一次取回全部要求的服務項目（`= ANY`），不是每項查一次：一筆預約帶五個
/// 服務就是五次往返，而它們用的是同一張表的同一組欄位。
pub struct AttachableService {
    pub id: Uuid,
    pub name: String,
    pub facility_id: Option<Uuid>,
    pub is_attachable_to_reservation: bool,
    pub lead_time_minutes: i32,
    pub default_duration_minutes: i32,
    pub relative_offset_minutes: i32,
    pub requires_approval: bool,
    pub chargeable: bool,
    pub unit_price: Option<f64>,
    pub max_quantity: Option<i32>,
    pub form_schema: serde_json::Value,
}

pub async fn load_service_items(
    tx: &mut TenantTx,
    ids: &[Uuid],
) -> Result<Vec<AttachableService>, Problem> {
    sqlx::query_as!(
        AttachableService,
        r#"SELECT id,
                  name::text AS "name!",
                  facility_id,
                  is_attachable_to_reservation,
                  lead_time_minutes,
                  default_duration_minutes,
                  relative_offset_minutes,
                  requires_approval,
                  chargeable,
                  unit_price::float8 AS "unit_price",
                  max_quantity,
                  form_schema AS "form_schema!"
           FROM fms.service_items
           WHERE id = ANY($1) AND deleted_at IS NULL"#,
        ids
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 一筆待寫入的附加服務。班表與費用都已由呼叫端依 `service_items` 的宣告算好
/// —— repo 不做業務判斷，它只負責忠實寫入。
pub struct NewReservationService {
    pub service_item_id: Uuid,
    pub quantity: f64,
    pub payload: serde_json::Value,
    pub notes: Option<String>,
    pub service_start_at: chrono::DateTime<chrono::Utc>,
    pub service_end_at: chrono::DateTime<chrono::Utc>,
    pub status: &'static str,
    pub estimated_cost: Option<f64>,
}

/// 寫入附加服務。與預約建立在同一個交易內，因此不會出現
/// 「預約建立了但服務沒登記」。
pub async fn insert_services(
    tx: &mut TenantTx,
    reservation_id: Uuid,
    items: &[NewReservationService],
) -> Result<(), Problem> {
    let tenant_id = tx.context().tenant_id;
    for s in items {
        sqlx::query!(
            r#"
            INSERT INTO fms.reservation_services
              (tenant_id, reservation_id, service_item_id, quantity, payload, notes,
               service_start_at, service_end_at, status, estimated_cost)
            VALUES ($1, $2, $3, $4::float8::numeric, $5, $6, $7, $8, $9,
                    $10::float8::numeric)
            "#,
            tenant_id,
            reservation_id,
            s.service_item_id,
            s.quantity,
            s.payload,
            s.notes,
            s.service_start_at,
            s.service_end_at,
            s.status,
            s.estimated_cost
        )
        .execute(tx.conn())
        .await
        .map_err(Problem::from)?;
    }
    Ok(())
}

/// 讀回一筆預約的附加服務，供 `ReservationDetail.services` 使用。
pub struct ReservationServiceRow {
    pub id: Uuid,
    pub service_item_id: Uuid,
    pub service_name: String,
    pub quantity: Option<f64>,
    pub payload: serde_json::Value,
    pub service_start_at: Option<chrono::DateTime<chrono::Utc>>,
    pub status: String,
}

pub async fn load_services(
    tx: &mut TenantTx,
    reservation_id: Uuid,
) -> Result<Vec<ReservationServiceRow>, Problem> {
    sqlx::query_as!(
        ReservationServiceRow,
        r#"SELECT rs.id,
                  rs.service_item_id,
                  si.name::text AS "service_name!",
                  rs.quantity::float8 AS "quantity",
                  rs.payload AS "payload!",
                  rs.service_start_at,
                  rs.status
           FROM fms.reservation_services rs
           JOIN fms.service_items si ON si.id = rs.service_item_id
           WHERE rs.reservation_id = $1
           ORDER BY rs.service_start_at, si.name"#,
        reservation_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 寫入與會者。與服務同一個模式：跟預約建立在同一個交易內。
///
/// 身分（`user_id` 或 `external_email` 至少一個）在 handlers 端已經驗證過，
/// 這裡不重複檢查——DB 的 `ck_participant_identity` 仍然是最後一道防線。
pub async fn insert_participants(
    tx: &mut TenantTx,
    reservation_id: Uuid,
    items: &[crate::dto::ParticipantRequest],
) -> Result<(), Problem> {
    let tenant_id = tx.context().tenant_id;
    for p in items {
        sqlx::query!(
            r#"
            INSERT INTO fms.reservation_participants
              (tenant_id, reservation_id, user_id, external_email, role)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (reservation_id, coalesce(user_id::text, external_email)) DO NOTHING
            "#,
            tenant_id,
            reservation_id,
            p.user_id,
            p.external_email,
            p.role,
        )
        .execute(tx.conn())
        .await
        .map_err(Problem::from)?;
    }
    Ok(())
}

/// 讀回一筆預約的與會者，供 `ReservationDetail.participants` 使用。
/// `display_name` 來自 `fms.users`——`external_email` 型的與會者沒有對應
/// 使用者列，因此是 LEFT JOIN。
pub struct ParticipantRow {
    pub user_id: Option<Uuid>,
    pub display_name: Option<String>,
    pub external_email: Option<String>,
    pub role: String,
    pub response: String,
}

pub async fn load_participants(
    tx: &mut TenantTx,
    reservation_id: Uuid,
) -> Result<Vec<ParticipantRow>, Problem> {
    sqlx::query_as!(
        ParticipantRow,
        r#"SELECT rp.user_id,
                  u.display_name AS "display_name?",
                  rp.external_email,
                  rp.role AS "role!",
                  rp.response AS "response!"
           FROM fms.reservation_participants rp
           LEFT JOIN fms.users u ON u.id = rp.user_id
           WHERE rp.reservation_id = $1
           ORDER BY rp.invited_at"#,
        reservation_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 建立所需的、由 bookable_resources 決定的欄位。
pub struct ResourceBooking {
    pub bookable_resource_id: Uuid,
    pub facility_id: Uuid,
    pub resource_type: String,
    pub requires_approval: bool,
    pub requires_check_in: bool,
    /// 未報到多久後視為 no-show 並釋放時段。`None` 表示該資源不做此判定。
    pub auto_release_minutes: Option<i32>,
    /// 可提前多少天預約。週期展開用它當視窗上界 —— 上界必須來自資源的
    /// 宣告，不是應用層憑空定一個數字。
    pub advance_booking_days: i32,
}

/// 由 `resource_id`（spatial_node 或 asset）反查其 bookable_resources 設定。
pub async fn find_bookable(
    tx: &mut TenantTx,
    resource_id: Uuid,
) -> Result<Option<ResourceBooking>, Problem> {
    sqlx::query_as!(
        ResourceBooking,
        r#"SELECT id AS bookable_resource_id,
                  facility_id,
                  resource_type,
                  requires_approval,
                  requires_check_in,
                  auto_release_minutes,
                  advance_booking_days
           FROM fms.bookable_resources
           WHERE (spatial_node_id = $1 OR asset_id = $1)
             AND is_bookable = true"#,
        resource_id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 建立預約。
///
/// `status` 依 `requires_approval` 決定：需審批時為 `PENDING_APPROVAL`，
/// 且該狀態同樣落在排他約束的 WHERE 內 —— 待審批期間仍佔用時段，
/// 這是正確行為（見 Offision 對照矩陣「預約審批佇列」一項）。
/// 要寫入的一個時段。
///
/// 單筆預約就是一個 `Occurrence`；週期預約是 RRULE 展開後的每一個。
/// 把時段從 `CreateReservation` 抽出來，是因為週期展開後每一筆的
/// `start_at`／`end_at` 都不同，而請求裡只有第一筆的。
pub struct Occurrence<'a> {
    pub start_at: chrono::DateTime<chrono::Utc>,
    pub end_at: chrono::DateTime<chrono::Utc>,
    /// 週期系列的識別；`None` 表示單筆預約。
    ///
    /// `rule` 寫進**每一列**而不只是第一列：任何一筆都該能自己回答
    /// 「我屬於哪個系列、規則是什麼」，否則查詢單筆時得先找到系列的頭。
    pub recurrence: Option<(Uuid, &'a str)>,
}

pub async fn create(
    tx: &mut TenantTx,
    booking: &ResourceBooking,
    resource_id: Uuid,
    organizer_id: Uuid,
    req: &crate::dto::CreateReservation,
    occurrence: &Occurrence<'_>,
) -> Result<Uuid, Problem> {
    let tenant_id = tx.context().tenant_id;
    let status = if booking.requires_approval {
        "PENDING_APPROVAL"
    } else {
        "CONFIRMED"
    };

    let id = sqlx::query_scalar!(
        r#"
        INSERT INTO fms.reservations
          (tenant_id, facility_id, bookable_resource_id, reservation_no,
           resource_type, resource_id, organizer_id, title, purpose, party_size,
           start_at, end_at, status, approval_required, requires_check_in, created_via,
           -- 已消耗的佔位留在這一列上：事後要回答「這筆預約是不是走兩階段來的」
           -- 只能靠它（reservation_holds 的列會隨保留期限被清掉）。
           hold_token,
           recurrence_group_id, recurrence_rule,
           auto_release_at, is_private)
        VALUES
          ($1, $2, $3, fms.next_document_no($1, 'RESERVATION', 'RSV'),
           $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, 'API', $16, $17, $18,
           -- no-show 的判定時點。只在「需要報到」且資源設了釋放分鐘數時才有值。
           -- 先前這一欄從未被填入，等於 no-show 機制整條是斷的：
           -- 背景作業沒有東西可掃，`NO_SHOW` 狀態永遠用不到。
           CASE WHEN $14 AND $15::int IS NOT NULL
                -- $10 是 start_at（$11 是 end_at）：釋放時點從**開始時間**起算，
                -- 「開始後 15 分鐘還沒到就算 no-show」。用 end_at 起算會變成
                -- 會議結束後才判定，那時釋放時段已經沒有意義。
                THEN $10::timestamptz + ($15::int * interval '1 minute') END,
           -- 011 的私人旗標。未帶等於 false —— `coalesce` 而不是讓 NOT NULL
           -- 擋下請求：契約把它標成選用，而預設不私人是正確的預設。
           coalesce($19, false))
        RETURNING id
        "#,
        tenant_id,
        booking.facility_id,
        booking.bookable_resource_id,
        booking.resource_type,
        resource_id,
        organizer_id,
        req.title,
        req.purpose,
        req.party_size,
        occurrence.start_at,
        occurrence.end_at,
        status,
        booking.requires_approval,
        booking.requires_check_in,
        booking.auto_release_minutes,
        req.hold_token,
        occurrence.recurrence.map(|(group, _)| group),
        occurrence.recurrence.map(|(_, rule)| rule),
        req.is_private,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;

    Ok(id)
}

/// 局部更新。`version` 由 `trg_reservations_version` 自動遞增，
/// 因此這裡不手動加一 —— 手動加會與觸發器相衝。
pub async fn update(
    tx: &mut TenantTx,
    id: Uuid,
    req: &crate::dto::UpdateReservation,
) -> Result<(), Problem> {
    sqlx::query!(
        r#"UPDATE fms.reservations
           SET title      = coalesce($2, title),
               purpose    = coalesce($3, purpose),
               party_size = coalesce($4, party_size),
               start_at   = coalesce($5, start_at),
               end_at     = coalesce($6, end_at),
               is_private = coalesce($7, is_private)
           WHERE id = $1"#,
        id,
        req.title,
        req.purpose,
        req.party_size,
        req.start_at,
        req.end_at,
        req.is_private,
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// `apply_scope=THIS_AND_FOLLOWING／ALL` 涉及的場域清單與「是不是全部都是
/// 呼叫端自己的」——同 `tail::cancel_series` 的判斷，一個系列理論上跨場域，
/// 權限要對每個場域都檢查。`from_start_at` 為 `None` 時涵蓋整個系列（ALL），
/// 有值時只涵蓋該時刻之後的場次（THIS_AND_FOLLOWING）。
pub struct SeriesScope {
    pub facilities: Vec<Uuid>,
    pub all_mine: bool,
}

pub async fn series_scope(
    tx: &mut TenantTx,
    group_id: Uuid,
    from_start_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<SeriesScope, Problem> {
    let facilities = sqlx::query_scalar!(
        r#"SELECT DISTINCT facility_id FROM fms.reservations
            WHERE recurrence_group_id = $1
              AND ($2::timestamptz IS NULL OR start_at >= $2)"#,
        group_id,
        from_start_at,
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)?;

    // 呼叫端已經持有 group_id（來自它自己那筆已讀到的預約），
    // 因此這個集合必定至少含那一筆，bool_and 不會遇到空集合。
    let all_mine: bool = sqlx::query_scalar!(
        r#"SELECT bool_and(organizer_id = fms.current_user_id()) AS "all_mine!"
             FROM fms.reservations
            WHERE recurrence_group_id = $1
              AND ($2::timestamptz IS NULL OR start_at >= $2)"#,
        group_id,
        from_start_at,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;

    Ok(SeriesScope {
        facilities,
        all_mine,
    })
}

/// 批次更新整個（或系列中「這筆及以後」的）場次。只動
/// `title`／`purpose`／`party_size`／`is_private`——`start_at`／`end_at`
/// 由 handlers 的 `update` 擋在呼叫這支之前（時段沒有整系列一起改的語意）。
///
/// **這裡不做逐列版本比對**：這是刻意的批次操作，跟 `update()` 的單列
/// 樂觀鎖是不同的一致性等級——呼叫端對「自己正在看的那一筆」（`id`）仍然
/// 有版本檢查（見 handlers 的 `update`），但系列裡其他場次沒有個別的
/// `If-Match` 可比對。
pub async fn update_series(
    tx: &mut TenantTx,
    group_id: Uuid,
    from_start_at: Option<chrono::DateTime<chrono::Utc>>,
    req: &crate::dto::UpdateReservation,
) -> Result<u64, Problem> {
    let result = sqlx::query!(
        r#"UPDATE fms.reservations
              SET title      = coalesce($3, title),
                  purpose    = coalesce($4, purpose),
                  party_size = coalesce($5, party_size),
                  is_private = coalesce($6, is_private)
            WHERE recurrence_group_id = $1
              AND ($2::timestamptz IS NULL OR start_at >= $2)"#,
        group_id,
        from_start_at,
        req.title,
        req.purpose,
        req.party_size,
        req.is_private,
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(result.rows_affected())
}

// =============================================================================
// S3：可預約資源與可用性
// =============================================================================

/// `bookable_resources` 的一列，外加對外要用的顯示欄位。
pub struct ResourceRow {
    pub id: Uuid,
    /// 契約對外的 `resource_id` 是**被預約的東西**（空間節點或設備），
    /// 不是 `bookable_resources.id` —— 既有的 `POST /reservations`
    /// 收的也是這個。兩者混用會讓客戶端拿到一個 POST 不接受的 id。
    pub resource_id: Uuid,
    pub resource_type: String,
    pub display_name: String,
    pub capacity: i32,
    pub is_bookable: bool,
    pub requires_approval: bool,
    pub min_duration_minutes: i32,
    pub max_duration_minutes: i32,
    pub slot_granularity_minutes: i32,
    pub advance_booking_days: i32,
    pub opening_hours: serde_json::Value,
    pub facility_id: Uuid,
}

/// 列出可預約資源。
///
/// `resource_id` 用 `coalesce(spatial_node_id, asset_id)`：schema 允許兩者
/// 之一，而 `ck_bookable_target` 保證恰好一個非空。
pub async fn list_resources(
    tx: &mut TenantTx,
    facility_id: Option<Uuid>,
    min_capacity: Option<i32>,
    bookable_only: bool,
    limit: i64,
) -> Result<Vec<ResourceRow>, Problem> {
    sqlx::query_as!(
        ResourceRow,
        r#"
        SELECT b.id,
               coalesce(b.spatial_node_id, b.asset_id) AS "resource_id!",
               b.resource_type,
               b.display_name::text AS "display_name!",
               b.capacity, b.is_bookable, b.requires_approval,
               b.min_duration_minutes, b.max_duration_minutes,
               b.slot_granularity_minutes, b.advance_booking_days,
               b.opening_hours AS "opening_hours!", b.facility_id
        FROM fms.bookable_resources b
        WHERE ($1::uuid IS NULL OR b.facility_id = $1)
          AND ($2::int IS NULL OR b.capacity >= $2)
          AND (NOT $3::bool OR b.is_bookable)
        ORDER BY b.display_name, b.id
        LIMIT $4
        "#,
        facility_id,
        min_capacity,
        bookable_only,
        limit
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 忙碌區塊的一列。`kind` 對齊契約的列舉。
pub struct BusyRow {
    pub resource_id: Uuid,
    pub start_at: chrono::DateTime<chrono::Utc>,
    pub end_at: chrono::DateTime<chrono::Utc>,
    pub kind: String,
    pub reason: Option<String>,
    /// 三者只有 `RESERVATION`／`BUFFER` 那個分支有意義——HOLD／BLACKOUT／
    /// MAINTENANCE 沒有主辦人，固定回 `false`／`NULL`，因此永遠不會被遮罩
    /// （見 handlers.rs 的 `PrivateView::masks_raw`）。
    pub is_private: bool,
    pub organizer_id: Option<Uuid>,
    pub on_behalf_of_id: Option<Uuid>,
}

/// 指定資源集合在時間範圍內的忙碌區塊。
///
/// # 為什麼這裡不呼叫 `check_resource_availability`
///
/// 那支函式回答的是「**這一個**時段可不可以訂」，包含規則判定
/// （最短／最長時長、預約窗、每人上限、配額）。可用性端點問的是不同問題：
/// 「哪些區間已經被佔用」。忙碌區塊是**資料**（預約、佔位、封鎖），
/// 不是規則判斷，因此直接查來源表不算複製判定邏輯。
///
/// 規則本身以 `rules` 欄位原樣回傳給客戶端，而**權威判定仍然只在
/// `POST /reservations` 發生** —— `free_slots` 是繪圖用的建議，不是保留。
///
/// 三種來源分別標記 `kind`，讓前端能區分「別人訂走了」與「本來就不開放」。
/// 緩衝時間併入預約區塊：`buffer_start_at`／`buffer_end_at` 由 005 的
/// 觸發器算好，忽略它會讓前端顯示的可用時段比實際寬。
pub async fn busy_blocks(
    tx: &mut TenantTx,
    facility_id: Uuid,
    resource_ids: &[Uuid],
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<BusyRow>, Problem> {
    sqlx::query_as!(
        BusyRow,
        r#"
        SELECT r.resource_id AS "resource_id!",
               coalesce(r.buffer_start_at, r.start_at) AS "start_at!",
               coalesce(r.buffer_end_at, r.end_at)     AS "end_at!",
               CASE WHEN r.buffer_start_at IS NOT NULL OR r.buffer_end_at IS NOT NULL
                    THEN 'BUFFER' ELSE 'RESERVATION' END AS "kind!",
               r.title::text AS "reason",
               r.is_private AS "is_private!",
               r.organizer_id AS "organizer_id",
               r.on_behalf_of_id AS "on_behalf_of_id"
        FROM fms.reservations r
        WHERE r.facility_id = $1
          AND r.resource_id = ANY($2)
          AND r.status NOT IN ('CANCELLED','REJECTED','NO_SHOW','COMPLETED')
          AND tstzrange(coalesce(r.buffer_start_at, r.start_at),
                        coalesce(r.buffer_end_at, r.end_at), '[)')
              && tstzrange($3, $4, '[)')

        UNION ALL

        -- 尚未過期的佔位。`reservation_holds` 以 `bookable_resource_id` 為鍵
        -- （不像 reservations 有反正規化的 resource_id），因此要 JOIN 換回來。
        SELECT coalesce(hb.spatial_node_id, hb.asset_id) AS "resource_id!",
               h.start_at AS "start_at!", h.end_at AS "end_at!",
               'HOLD'::text AS "kind!",
               NULL::text AS "reason",
               false AS "is_private!",
               NULL::uuid AS "organizer_id",
               NULL::uuid AS "on_behalf_of_id"
        FROM fms.reservation_holds h
        JOIN fms.bookable_resources hb ON hb.id = h.bookable_resource_id
        WHERE h.facility_id = $1
          AND coalesce(hb.spatial_node_id, hb.asset_id) = ANY($2)
          AND h.expires_at > clock_timestamp()
          AND h.status = 'ACTIVE'
          AND tstzrange(h.start_at, h.end_at, '[)') && tstzrange($3, $4, '[)')

        UNION ALL

        -- 封鎖時段（維護、活動、清潔）—— 指定了資源的那些。
        SELECT coalesce(b.spatial_node_id, b.asset_id) AS "resource_id!",
               bl.start_at AS "start_at!", bl.end_at AS "end_at!",
               CASE WHEN bl.work_order_id IS NOT NULL
                    THEN 'MAINTENANCE' ELSE 'BLACKOUT' END AS "kind!",
               bl.reason::text AS "reason",
               false AS "is_private!",
               NULL::uuid AS "organizer_id",
               NULL::uuid AS "on_behalf_of_id"
        FROM fms.resource_blackouts bl
        JOIN fms.bookable_resources b ON b.id = bl.bookable_resource_id
        WHERE bl.facility_id = $1
          AND coalesce(b.spatial_node_id, b.asset_id) = ANY($2)
          AND tstzrange(bl.start_at, bl.end_at, '[)') && tstzrange($3, $4, '[)')

        UNION ALL

        -- **全場域封鎖**（`bookable_resource_id IS NULL`）。
        --
        -- 上面那一段是 `JOIN fms.bookable_resources`（內連接），因此
        -- `bookable_resource_id` 為 NULL 的列會被丟掉 —— 而 011 的
        -- `check_resource_availability()` **會**擋這種列
        -- （`OR (b.bookable_resource_id IS NULL AND b.facility_id = ...)`）。
        --
        -- 少了這一段，症狀是：日曆顯示可預約 → 使用者選了 → 送出得到衝突，
        -- 而衝突指向一個他在畫面上看不到的封鎖時段。
        --
        -- 這個缺口原本是潛伏的（在 `POST /resource-blackouts` 之前，沒有端點
        -- 建得出全場域封鎖），因此補上寫入端點的同一刀必須修它。
        --
        -- `CROSS JOIN unnest($2)` 把一段全場域封鎖展開到查詢要求的每一個資源上
        -- —— 它擋的就是全部。
        SELECT rid AS "resource_id!",
               bl.start_at AS "start_at!", bl.end_at AS "end_at!",
               CASE WHEN bl.work_order_id IS NOT NULL
                    THEN 'MAINTENANCE' ELSE 'BLACKOUT' END AS "kind!",
               bl.reason::text AS "reason",
               false AS "is_private!",
               NULL::uuid AS "organizer_id",
               NULL::uuid AS "on_behalf_of_id"
        FROM fms.resource_blackouts bl
        CROSS JOIN unnest($2::uuid[]) AS rid
        WHERE bl.facility_id = $1
          AND bl.bookable_resource_id IS NULL
          AND tstzrange(bl.start_at, bl.end_at, '[)') && tstzrange($3, $4, '[)')

        ORDER BY 1, 2
        "#,
        facility_id,
        resource_ids,
        from,
        to
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

// =============================================================================
// S4：兩階段佔位、報到、取消
// =============================================================================

/// 建立短期佔位。
///
/// # 原子性同樣來自排他約束，不需要 advisory lock
///
/// `reservation_holds` 有 `excl_reservation_holds_overlap`
/// （`EXCLUDE USING gist (bookable_resource_id, hold_range) WHERE status='ACTIVE'`），
/// 與 `reservations` 是同一個機制。兩個使用者同時對同一資源同一時段佔位時，
/// 由資料庫擇一成功、另一個拿到 `23P01`，應用層只要忠實轉譯成 409。
///
/// 在應用層再加 advisory lock 會是第二套機制：它序列化的是**應用層的**
/// 臨界區，而真正的權威在資料庫；兩者一旦對「同一資源」的定義不一致
/// （例如 combinable 房間的影子資源），就會出現「鎖到了卻還是衝突」。
///
/// **佔位也必須避開既有預約**：排他約束只管 hold 與 hold 之間，
/// 不會看 `reservations`。因此先呼叫 `check_resource_availability`
/// —— 那是規則與衝突判定的權威來源。
pub async fn create_hold(
    tx: &mut TenantTx,
    bookable_resource_id: Uuid,
    facility_id: Uuid,
    start_at: chrono::DateTime<chrono::Utc>,
    end_at: chrono::DateTime<chrono::Utc>,
    ttl_seconds: i32,
) -> Result<(String, chrono::DateTime<chrono::Utc>), Problem> {
    let ctx = tx.context();
    let row = sqlx::query!(
        r#"
        INSERT INTO fms.reservation_holds
          (tenant_id, facility_id, bookable_resource_id, user_id,
           start_at, end_at, hold_token, status, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6,
                encode(gen_random_bytes(24), 'hex'), 'ACTIVE',
                clock_timestamp() + ($7::int * interval '1 second'))
        RETURNING hold_token::text AS "hold_token!", expires_at AS "expires_at!"
        "#,
        ctx.tenant_id,
        facility_id,
        bookable_resource_id,
        ctx.user_id,
        start_at,
        end_at,
        ttl_seconds
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok((row.hold_token, row.expires_at))
}

/// 報到。
///
/// 只在狀態允許時推進：`CONFIRMED` 才能報到。`PENDING_APPROVAL` 尚未核准、
/// 終態則已結束 —— 兩者都不該能報到，而回 409 讓客戶端知道是狀態問題
/// 而非請求格式問題。
///
/// 回傳受影響列數：0 表示狀態不允許（呼叫端已確認資料列存在）。
pub async fn check_in(tx: &mut TenantTx, id: Uuid, method: &str) -> Result<u64, Problem> {
    let done = sqlx::query!(
        r#"
        UPDATE fms.reservations
           SET status = 'CHECKED_IN',
               checked_in_at = clock_timestamp(),
               check_in_method = $2
         WHERE id = $1 AND status = 'CONFIRMED'
        "#,
        id,
        method
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(done.rows_affected())
}

/// 取消。
///
/// # 關於「取消窗」
///
/// schema **沒有** cancellation window 的設定欄位
/// （`bookable_resources` 只有 `min_notice_minutes`＝最少提前預約時間，
/// 與 `auto_release_minutes`＝未報到釋放時間，兩者都不是取消政策）。
/// 因此這裡不做時間窗判定 —— 憑空定一個「開始前 N 分鐘不得取消」
/// 會是發明政策，而那需要業務決定並且需要一個欄位來存。
/// 已列為已知缺口。
///
/// 實際擋住的是**狀態**：終態不可再取消。已開始的預約仍可取消
/// （現實中會議提早結束就是這樣），由 `cancelled_at` 與原因留下軌跡。
pub async fn cancel(tx: &mut TenantTx, id: Uuid, reason: Option<&str>) -> Result<u64, Problem> {
    let user_id = tx.context().user_id;
    let done = sqlx::query!(
        r#"
        UPDATE fms.reservations
           SET status = 'CANCELLED',
               cancelled_at = clock_timestamp(),
               cancelled_by = $2,
               cancellation_reason = $3
         WHERE id = $1
           AND status NOT IN ('CANCELLED','REJECTED','COMPLETED','NO_SHOW','EXPIRED')
        "#,
        id,
        user_id,
        reason
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(done.rows_affected())
}

/// 把一筆預約標記為屬於某個週期群組。
///
/// 展開後的每一筆共用 `recurrence_group_id`，第一筆的 id 即群組 id ——
/// 不另外造一張 series 表：schema 刻意用「群組 id + 規則字串」表示系列，
/// 而 `is_recurrence_exception` 讓單筆修改不必脫離群組。
pub async fn tag_recurrence(
    tx: &mut TenantTx,
    ids: &[Uuid],
    group_id: Uuid,
    rule: &str,
) -> Result<(), Problem> {
    sqlx::query!(
        r#"UPDATE fms.reservations
              SET recurrence_group_id = $2, recurrence_rule = $3
            WHERE id = ANY($1)"#,
        ids,
        group_id,
        rule
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

// =============================================================================
// S5：核准／駁回、即時佔用、no-show
// =============================================================================

/// 核准待審預約。
///
/// 只有 `PENDING_APPROVAL` 能被核准；回傳 0 表示狀態不允許。
/// 用條件式 UPDATE 而非先查再寫：兩個審核者同時按下核准時，
/// 由資料庫決定誰的那一次生效，第二次影響 0 列。
pub async fn approve(tx: &mut TenantTx, id: Uuid) -> Result<u64, Problem> {
    let user_id = tx.context().user_id;
    let done = sqlx::query!(
        r#"UPDATE fms.reservations
              SET status = 'CONFIRMED',
                  approved_by = $2,
                  approved_at = clock_timestamp()
            WHERE id = $1 AND status = 'PENDING_APPROVAL'"#,
        id,
        user_id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(done.rows_affected())
}

/// 駁回待審預約。原因必填 —— 被駁回的人需要知道為什麼，
/// 而 `rejection_reason` 是 schema 已經準備好的欄位。
pub async fn reject(tx: &mut TenantTx, id: Uuid, reason: &str) -> Result<u64, Problem> {
    let user_id = tx.context().user_id;
    let done = sqlx::query!(
        r#"UPDATE fms.reservations
              SET status = 'REJECTED',
                  approved_by = $2,
                  approved_at = clock_timestamp(),
                  rejection_reason = $3
            WHERE id = $1 AND status = 'PENDING_APPROVAL'"#,
        id,
        user_id,
        reason
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(done.rows_affected())
}

/// 即時佔用地圖的一列。
pub struct OccupancyRow {
    pub resource_id: Uuid,
    pub display_name: String,
    pub resource_type: String,
    pub capacity: i32,
    /// `FREE` / `OCCUPIED` / `RESERVED` / `HELD`。
    pub state: String,
    pub reservation_id: Option<Uuid>,
    pub title: Option<String>,
    pub organizer_name: Option<String>,
    /// 佔用地圖同樣要遮罩：這是牆面板與樓層圖的資料來源，
    /// 而它先前把私人會議的標題與主辦人姓名顯示給所有持有
    /// `reservation:read` 的人。
    ///
    /// `Option` 而非 `bool`：它來自 LEFT JOIN，資源空閒時沒有預約列。
    /// `None` 與 `Some(false)` 對遮罩判定是同一件事（沒有東西要遮），
    /// 但把「沒有預約」壓成 `false` 會讓這一欄說不出那個差別。
    pub is_private: Option<bool>,
    pub start_at: Option<chrono::DateTime<chrono::Utc>>,
    pub end_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// 設施內每個可預約資源**此刻**的佔用狀態。
///
/// # 為什麼 `OCCUPIED` 與 `RESERVED` 要分開
///
/// 「有人訂了而且已報到」與「有人訂了但還沒出現」對現場管理是不同的事：
/// 後者可能是即將發生的 no-show，是可以介入的（打電話、釋放給臨時需求）。
/// 把兩者混成一個「忙碌」會讓佔用地圖失去它最有用的資訊。
///
/// 每個資源只回**一列**：`DISTINCT ON` 取當下最相關的那一筆。
/// 排序讓已報到優先於未報到、預約優先於佔位 —— 同一時刻理論上不會有兩筆，
/// 但緩衝時間與佔位可能重疊，此時顯示「更確定的那個」。
pub async fn occupancy(tx: &mut TenantTx, facility_id: Uuid) -> Result<Vec<OccupancyRow>, Problem> {
    sqlx::query_as!(
        OccupancyRow,
        r#"
        SELECT DISTINCT ON (b.id)
               coalesce(b.spatial_node_id, b.asset_id) AS "resource_id!",
               b.display_name::text AS "display_name!",
               b.resource_type    AS "resource_type!",
               b.capacity         AS "capacity!",
               CASE
                 WHEN r.id IS NULL AND h.id IS NULL THEN 'FREE'
                 WHEN r.status = 'CHECKED_IN'       THEN 'OCCUPIED'
                 WHEN r.id IS NOT NULL              THEN 'RESERVED'
                 ELSE 'HELD'
               END AS "state!",
               -- 這六個都來自 LEFT JOIN：沒有匹配列時是 NULL，但 sqlx 會沿用
               -- 來源欄位的 NOT NULL 推斷（`reservations.id` 是主鍵、
               -- `users.display_name` 是 NOT NULL），因此必須用 `?` 明確標成可空。
               -- 不標的話編譯照樣通過，執行期才會 UnexpectedNullError。
               r.id               AS "reservation_id?",
               r.title::text      AS "title?",
               u.display_name::text AS "organizer_name?",
               r.is_private       AS "is_private?",
               coalesce(r.start_at, h.start_at) AS "start_at?",
               coalesce(r.end_at, h.end_at)     AS "end_at?"
        FROM fms.bookable_resources b
        LEFT JOIN fms.reservations r
               ON r.bookable_resource_id = b.id
              AND r.status IN ('CONFIRMED','CHECKED_IN')
              AND clock_timestamp() >= r.start_at
              AND clock_timestamp() <  r.end_at
        LEFT JOIN fms.users u ON u.id = r.organizer_id
        LEFT JOIN fms.reservation_holds h
               ON h.bookable_resource_id = b.id
              AND h.status = 'ACTIVE'
              AND h.expires_at > clock_timestamp()
              AND clock_timestamp() >= h.start_at
              AND clock_timestamp() <  h.end_at
        WHERE b.facility_id = $1 AND b.is_bookable
        -- 已報到 > 已預約 > 佔位 > 空閒：同一刻若有多筆，顯示最確定的那個
        ORDER BY b.id,
                 (r.status = 'CHECKED_IN') DESC NULLS LAST,
                 (r.id IS NOT NULL) DESC,
                 b.display_name
        "#,
        facility_id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 逾期未報到的預約（no-show 掃描的輸入）。
///
/// 條件三者皆須成立：需要報到、已過 `auto_release_at`、且**確實沒報到**。
/// 最後一項用 `checked_in_at IS NULL` 而非狀態判斷 —— 狀態可能因其他流程
/// 變動，而「有沒有出現」只有那個時間戳說了算。
pub async fn overdue_no_shows(tx: &mut TenantTx, batch: i64) -> Result<Vec<Uuid>, Problem> {
    sqlx::query_scalar!(
        r#"
        SELECT r.id
        FROM fms.reservations r
        WHERE r.status = 'CONFIRMED'
          AND r.requires_check_in
          AND r.checked_in_at IS NULL
          AND r.auto_release_at IS NOT NULL
          AND r.auto_release_at <= clock_timestamp()
        ORDER BY r.auto_release_at
        LIMIT $1
        "#,
        batch
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 把一筆逾期未報到的預約標為 `NO_SHOW`。
///
/// 條件重述一次而不是信任呼叫端傳來的 id：掃描與標記之間有時間差，
/// 使用者可能剛好在這段時間內報到。條件寫在 UPDATE 裡，
/// 那個競態就由資料庫解決（影響 0 列＝他及時報到了）。
pub async fn mark_no_show(tx: &mut TenantTx, id: Uuid) -> Result<u64, Problem> {
    let done = sqlx::query!(
        r#"UPDATE fms.reservations
              SET status = 'NO_SHOW'
            WHERE id = $1
              AND status = 'CONFIRMED'
              AND requires_check_in
              AND checked_in_at IS NULL
              AND auto_release_at IS NOT NULL
              AND auto_release_at <= clock_timestamp()"#,
        id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(done.rows_affected())
}

/// no-show 掃描需要的租戶清單（跨租戶掃描用）。
pub async fn tenant_of(tx: &mut TenantTx, id: Uuid) -> Result<Option<Uuid>, Problem> {
    sqlx::query_scalar!("SELECT tenant_id FROM fms.reservations WHERE id = $1", id)
        .fetch_optional(tx.conn())
        .await
        .map_err(Problem::from)
}

/// 鎖住那一列，供樂觀鎖的前置讀取使用。
///
/// **必須在讀出 `version` 之前呼叫。** 少了它，兩個並發的 PATCH 會讀到同一個
/// 版本、都通過 `check_version`、都寫入 —— 見該函式的說明。
///
/// 不做 not-found 判斷：呼叫端緊接著的 `get()` 會處理。列不存在時
/// `FOR UPDATE` 什麼也不鎖，那是正確的行為。
///
/// 刻意用不帶 JOIN 的最小查詢：`get()` 帶 JOIN，而對它加 `FOR UPDATE` 會連
/// `users` 那些列一起鎖住 —— 過度加鎖，而且會製造與其他路徑的死鎖機會。
pub async fn lock(tx: &mut TenantTx, id: Uuid) -> Result<(), Problem> {
    sqlx::query("SELECT 1 FROM fms.reservations WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(tx.conn())
        .await
        .map_err(Problem::from)?;
    Ok(())
}
