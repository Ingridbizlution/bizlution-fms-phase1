//! 附件端點（WBS S5）：`POST/GET /attachments`、
//! `GET/DELETE /attachments/{attachmentId}`。
//!
//! 這四支是**本次新增的契約面**（原契約沒有任何附件端點），
//! 因此形狀刻意保守：沿用既有的 `Attachment` schema，
//! 不發明新的回應結構。

use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, object_key, require_permission, Caller, Problem, Storage, TenantTx,
};

use crate::dto::*;
use crate::repo;

#[derive(Clone)]
pub struct AttachmentState {
    pub pool: PgPool,
    pub storage: Storage,
}

/// `attachments.entity_type` 支援的值。
///
/// 白名單而非自由字串：`entity_id` 是多型的、**沒有外鍵**，
/// 唯一能防止「掛到不存在的東西上」的機制就是這份清單加上
/// `repo::entity_facility` 的存在性檢查。
const ENTITY_TYPES: &[&str] = &["WORK_ORDER", "ASSET", "SPATIAL_NODE", "RESERVATION"];

/// 各實體型別上傳附件所需的權限。
///
/// 用「修改該實體」的權限而不是新造一個 `attachment:write`：
/// 上傳附件實質上就是修改那個實體，而權限目錄裡沒有附件專屬的權限碼
/// —— 新增權限碼要動 008 的種子與所有角色，代價遠大於收益。
fn write_permission(entity_type: &str) -> &'static str {
    match entity_type {
        "WORK_ORDER" => "work_order:update",
        "ASSET" => "asset:write",
        "SPATIAL_NODE" => "spatial_node:write",
        _ => "reservation:update",
    }
}

fn read_permission(entity_type: &str) -> &'static str {
    match entity_type {
        "WORK_ORDER" => "work_order:read",
        "ASSET" => "asset:read",
        "SPATIAL_NODE" => "spatial_node:read",
        _ => "reservation:read",
    }
}

/// 單檔上限。
///
/// 有上限才有預期行為：沒有上限時一個 500MB 的上傳會把整個請求讀進記憶體
/// （目前是直接上傳，位元組確實會經過應用層），在容器裡就是 OOM。
/// 25MB 對照片與說明書足夠；BIM 模型應改走預簽 PUT（見 `storage` 模組）。
const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

/// 把資料列轉成契約形狀，並即時預簽下載網址。
async fn to_dto(storage: &Storage, row: repo::AttachmentRow) -> Result<AttachmentDto, Problem> {
    let download_url = storage
        .presign_get(&row.storage_key, &row.file_name)
        .await?;
    Ok(AttachmentDto {
        id: row.id,
        purpose: row.purpose,
        file_name: row.file_name,
        mime_type: row.mime_type,
        size_bytes: row.size_bytes,
        download_url,
        created_at: row.created_at,
    })
}

/// 供其他模組嵌入 `attachments`（工單詳情的 `include=attachments`）。
pub async fn for_entity(
    tx: &mut TenantTx,
    storage: &Storage,
    entity_type: &str,
    entity_id: Uuid,
) -> Result<Vec<AttachmentDto>, Problem> {
    let rows = repo::for_entity(tx, entity_type, entity_id, None).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(to_dto(storage, row).await?);
    }
    Ok(out)
}

/// `POST /attachments`（multipart）
///
/// 欄位：`entity_type`、`entity_id`、`purpose`（選填）、`file`。
///
/// # 寫入順序
///
/// 先上傳物件、再寫資料列，且資料列的交易在上傳成功後才提交。反過來的話，
/// 上傳失敗會留下指向不存在物件的紀錄，而預簽**不檢查物件是否存在** ——
/// 使用者要到點下載才會看到 404。
///
/// 反向的失敗（物件上傳成功但交易回滾）會留下孤立物件，那是可接受的一側：
/// 孤立物件只佔空間，可由生命週期規則清掃；孤立資料列則是壞資料。
pub async fn create(
    State(state): State<AttachmentState>,
    caller: Caller,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<AttachmentDto>), Problem> {
    let mut entity_type: Option<String> = None;
    let mut entity_id: Option<Uuid> = None;
    let mut purpose: Option<String> = None;
    let mut file_name: Option<String> = None;
    let mut mime_type: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| Problem::validation(format!("malformed multipart body: {e}")))?
    {
        match field.name().unwrap_or_default() {
            "entity_type" => {
                entity_type = Some(
                    field
                        .text()
                        .await
                        .map_err(field_error)?
                        .trim()
                        .to_uppercase(),
                )
            }
            "entity_id" => {
                let raw = field.text().await.map_err(field_error)?;
                entity_id = Some(
                    Uuid::parse_str(raw.trim())
                        .map_err(|_| Problem::validation("entity_id is not a valid uuid"))?,
                );
            }
            "purpose" => {
                purpose = Some(
                    field
                        .text()
                        .await
                        .map_err(field_error)?
                        .trim()
                        .to_uppercase(),
                )
            }
            "file" => {
                file_name = field.file_name().map(str::to_owned);
                mime_type = field.content_type().map(str::to_owned);
                let data = field.bytes().await.map_err(field_error)?;
                if data.len() > MAX_UPLOAD_BYTES {
                    return Err(Problem::validation(format!(
                        "file exceeds the {MAX_UPLOAD_BYTES} byte limit"
                    )));
                }
                bytes = Some(data.to_vec());
            }
            // 未知欄位忽略：multipart 表單常被前端框架加上額外欄位，
            // 為此拒絕整個請求沒有好處。與 `fields`／`include` 不同 ——
            // 那兩者的未知值會改變回應內容，這裡不會。
            _ => {}
        }
    }

    let entity_type = entity_type.ok_or_else(|| Problem::validation("entity_type is required"))?;
    let entity_id = entity_id.ok_or_else(|| Problem::validation("entity_id is required"))?;
    let bytes = bytes.ok_or_else(|| Problem::validation("a `file` part is required"))?;
    if bytes.is_empty() {
        return Err(Problem::validation("the uploaded file is empty"));
    }
    let file_name = file_name
        .filter(|f| !f.trim().is_empty())
        .ok_or_else(|| Problem::validation("the `file` part must carry a filename"))?;
    if !ENTITY_TYPES.contains(&entity_type.as_str()) {
        return Err(Problem::validation(format!(
            "invalid entity_type `{entity_type}`; allowed: {ENTITY_TYPES:?}"
        )));
    }
    let purpose = purpose.unwrap_or_else(|| "GENERAL".to_string());

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // 存在性檢查不能省：entity_id 是多型的，沒有外鍵保護。
    let facility_id = repo::entity_facility(&mut tx, &entity_type, entity_id)
        .await?
        .ok_or_else(|| Problem::not_found(format!("{entity_type} {entity_id} does not exist")))?;
    require_permission(
        &mut tx,
        write_permission(&entity_type),
        Some(facility_id),
        None,
    )
    .await?;

    let tenant_id = caller.tenant_id;
    let key = object_key(tenant_id, &entity_type, entity_id, &file_name);
    let size_bytes = bytes.len() as i64;
    // checksum 在上傳前算：之後就沒有原始位元組了，
    // 而 `attachments.checksum_sha256` 是為了偵測儲存端損毀。
    let checksum = hex(Sha256::digest(&bytes));

    state.storage.put(&key, bytes, mime_type.as_deref()).await?;

    let id = repo::create(
        &mut tx,
        repo::NewAttachment {
            entity_type: &entity_type,
            entity_id,
            purpose: &purpose,
            file_name: &file_name,
            mime_type: mime_type.as_deref(),
            size_bytes,
            checksum_sha256: &checksum,
            storage_bucket: state.storage.bucket(),
            storage_key: &key,
        },
    )
    .await?;

    let row = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::internal(std::io::Error::other("attachment vanished")))?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(to_dto(&state.storage, row).await?),
    ))
}

fn field_error(e: axum::extract::multipart::MultipartError) -> Problem {
    Problem::validation(format!("could not read multipart field: {e}"))
}

fn hex(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .fold(String::with_capacity(64), |mut acc, b| {
            use std::fmt::Write;
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

/// `GET /attachments`：列出某個實體的附件。
pub async fn list(
    State(state): State<AttachmentState>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let entity_type = q
        .entity_type
        .map(|t| t.to_uppercase())
        .ok_or_else(|| Problem::validation("entity_type is required"))?;
    let entity_id = q
        .entity_id
        .ok_or_else(|| Problem::validation("entity_id is required"))?;
    if !ENTITY_TYPES.contains(&entity_type.as_str()) {
        return Err(Problem::validation(format!(
            "invalid entity_type `{entity_type}`; allowed: {ENTITY_TYPES:?}"
        )));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let facility_id = repo::entity_facility(&mut tx, &entity_type, entity_id)
        .await?
        .ok_or_else(|| Problem::not_found(format!("{entity_type} {entity_id} does not exist")))?;
    require_permission(
        &mut tx,
        read_permission(&entity_type),
        Some(facility_id),
        None,
    )
    .await?;

    let rows = repo::for_entity(&mut tx, &entity_type, entity_id, q.purpose.as_deref()).await?;
    tx.commit().await?;

    let mut data = Vec::with_capacity(rows.len());
    for row in rows {
        data.push(to_dto(&state.storage, row).await?);
    }
    // 附件是某個實體的子資源、數量有界，因此不分頁 ——
    // 硬塞一個 cursor 只會讓客戶端多寫一圈迴圈。
    Ok(Json(serde_json::json!({ "data": data })))
}

/// `GET /attachments/{attachmentId}`
///
/// 存在的理由是**重新取得預簽網址**：網址短期有效，過期後客戶端需要
/// 一個便宜的方式再要一個，而不是重新拉整份工單詳情。
pub async fn get(
    State(state): State<AttachmentState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<AttachmentDto>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let row = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("attachment not found"))?;
    let facility_id = repo::entity_facility(&mut tx, &row.entity_type, row.entity_id)
        .await?
        .ok_or_else(|| Problem::not_found("attachment not found"))?;
    require_permission(
        &mut tx,
        read_permission(&row.entity_type),
        Some(facility_id),
        None,
    )
    .await?;
    tx.commit().await?;

    Ok(Json(to_dto(&state.storage, row).await?))
}

/// `DELETE /attachments/{attachmentId}`
///
/// 資料列軟刪除（稽核需要知道曾經有這個檔案、誰上傳的），
/// 物件真的刪掉。順序是先提交交易再刪物件：反過來的話交易回滾
/// 會留下一筆資料列指向已刪除的物件，那正是最難診斷的狀態。
///
/// 代價是「交易成功、刪物件失敗」會留下孤立物件。同 create 的取捨：
/// 孤立物件可清掃，壞資料不行。
pub async fn delete(
    State(state): State<AttachmentState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let row = repo::get(&mut tx, id)
        .await?
        .ok_or_else(|| Problem::not_found("attachment not found"))?;
    let facility_id = repo::entity_facility(&mut tx, &row.entity_type, row.entity_id)
        .await?
        .ok_or_else(|| Problem::not_found("attachment not found"))?;
    require_permission(
        &mut tx,
        write_permission(&row.entity_type),
        Some(facility_id),
        None,
    )
    .await?;

    repo::soft_delete(&mut tx, id).await?;
    tx.commit().await?;

    if let Err(e) = state.storage.delete(&row.storage_key).await {
        // 資料列已經軟刪除，功能上已經完成。物件殘留只是空間問題，
        // 因此記錄而不讓整個請求失敗 —— 回 500 會讓客戶端以為刪除沒成功。
        tracing::error!(error = %e, key = %row.storage_key, "附件物件刪除失敗，資料列已軟刪除");
    }
    Ok(StatusCode::NO_CONTENT)
}
