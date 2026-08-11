//! 附件的資料存取。

use uuid::Uuid;

use fms_shared::{Problem, TenantTx};

pub struct AttachmentRow {
    pub id: Uuid,
    pub entity_type: String,
    pub entity_id: Uuid,
    pub purpose: String,
    pub file_name: String,
    pub mime_type: Option<String>,
    pub size_bytes: Option<i64>,
    pub storage_bucket: String,
    pub storage_key: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub struct NewAttachment<'a> {
    pub entity_type: &'a str,
    pub entity_id: Uuid,
    pub purpose: &'a str,
    pub file_name: &'a str,
    pub mime_type: Option<&'a str>,
    pub size_bytes: i64,
    pub checksum_sha256: &'a str,
    pub storage_bucket: &'a str,
    pub storage_key: &'a str,
}

/// 寫入附件資料列。
///
/// **呼叫順序很重要**：物件必須先上傳成功才寫這一列。反過來的話，
/// 上傳失敗會留下一筆指向不存在物件的紀錄，而 `download_url` 會預簽成功
/// （預簽不檢查物件存在），使用者要到點下去才發現 404。
pub async fn create(tx: &mut TenantTx, new: NewAttachment<'_>) -> Result<Uuid, Problem> {
    let tenant_id = tx.context().tenant_id;
    let user_id = tx.context().user_id;
    sqlx::query_scalar!(
        r#"
        INSERT INTO fms.attachments
          (tenant_id, entity_type, entity_id, purpose, file_name, mime_type,
           size_bytes, checksum_sha256, storage_bucket, storage_key, uploaded_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        RETURNING id
        "#,
        tenant_id,
        new.entity_type,
        new.entity_id,
        new.purpose,
        new.file_name,
        new.mime_type,
        new.size_bytes,
        new.checksum_sha256,
        new.storage_bucket,
        new.storage_key,
        user_id
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)
}

pub async fn get(tx: &mut TenantTx, id: Uuid) -> Result<Option<AttachmentRow>, Problem> {
    sqlx::query_as!(
        AttachmentRow,
        r#"SELECT id, entity_type::text AS "entity_type!", entity_id,
                  purpose::text AS "purpose!", file_name::text AS "file_name!",
                  mime_type::text AS "mime_type", size_bytes,
                  storage_bucket::text AS "storage_bucket!", storage_key,
                  created_at
             FROM fms.attachments
            WHERE id = $1 AND deleted_at IS NULL"#,
        id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 某個實體的附件。
pub async fn for_entity(
    tx: &mut TenantTx,
    entity_type: &str,
    entity_id: Uuid,
    purpose: Option<&str>,
) -> Result<Vec<AttachmentRow>, Problem> {
    sqlx::query_as!(
        AttachmentRow,
        r#"SELECT id, entity_type::text AS "entity_type!", entity_id,
                  purpose::text AS "purpose!", file_name::text AS "file_name!",
                  mime_type::text AS "mime_type", size_bytes,
                  storage_bucket::text AS "storage_bucket!", storage_key,
                  created_at
             FROM fms.attachments
            WHERE entity_type = $1 AND entity_id = $2 AND deleted_at IS NULL
              AND ($3::text IS NULL OR purpose = $3)
            ORDER BY created_at"#,
        entity_type,
        entity_id,
        purpose
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 軟刪除資料列。物件本身由 handler 另外刪除（見 `storage::delete` 的說明）。
pub async fn soft_delete(tx: &mut TenantTx, id: Uuid) -> Result<u64, Problem> {
    let done = sqlx::query!(
        "UPDATE fms.attachments SET deleted_at = clock_timestamp()
          WHERE id = $1 AND deleted_at IS NULL",
        id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(done.rows_affected())
}

/// 附件掛載的實體是否存在、屬於哪個場域。
///
/// 這一步不能省：`attachments.entity_id` 沒有外鍵（它是多型的，
/// 指向五種不同的表），因此**沒有任何資料庫約束能防止上傳到不存在的實體**。
/// 權限也要靠它 —— 要檢查 `work_order:update` 就得先知道場域。
pub async fn entity_facility(
    tx: &mut TenantTx,
    entity_type: &str,
    entity_id: Uuid,
) -> Result<Option<Uuid>, Problem> {
    let facility: Option<Uuid> = match entity_type {
        "WORK_ORDER" => sqlx::query_scalar!(
            "SELECT facility_id FROM fms.work_orders WHERE id = $1 AND deleted_at IS NULL",
            entity_id
        )
        .fetch_optional(tx.conn())
        .await
        .map_err(Problem::from)?,
        "ASSET" => sqlx::query_scalar!(
            "SELECT facility_id FROM fms.assets WHERE id = $1 AND deleted_at IS NULL",
            entity_id
        )
        .fetch_optional(tx.conn())
        .await
        .map_err(Problem::from)?,
        "SPATIAL_NODE" => sqlx::query_scalar!(
            "SELECT facility_id FROM fms.spatial_nodes WHERE id = $1 AND deleted_at IS NULL",
            entity_id
        )
        .fetch_optional(tx.conn())
        .await
        .map_err(Problem::from)?,
        "RESERVATION" => sqlx::query_scalar!(
            "SELECT facility_id FROM fms.reservations WHERE id = $1",
            entity_id
        )
        .fetch_optional(tx.conn())
        .await
        .map_err(Problem::from)?,
        _ => None,
    };
    Ok(facility)
}
