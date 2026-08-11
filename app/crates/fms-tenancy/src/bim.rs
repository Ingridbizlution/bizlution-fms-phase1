//! BIM 模型與直傳上傳（`/facilities/{id}/bim-models`、`/uploads/presign`）。
//!
//! # 解析是非同步的 —— 這支 API 只負責登記
//!
//! 註冊之後模型的 `status` 從 `UPLOADED` 開始。獨立常駐服務
//! `services/bim-worker` 每 30 秒輪詢一次 `status = 'UPLOADED'` 的模型，
//! 用 IfcOpenShell 解析出樓層／空間／設備並寫回 `spatial_nodes`／
//! `assets`，過程中把 `status` 依序改成 `PARSING` → `PARSED`（或
//! `PARSE_FAILED`）。這支 API **不會同步等待解析完成** ——
//! 呼叫端要輪詢 `GET .../bim-models/{id}` 直到 `status` 變成終態
//! （`PARSED` 或 `PARSE_FAILED`）。
//!
//! 而 **`unresolved_elements` 是空陣列有兩種完全不同的意思**：
//!
//!   * 「解析完了，全部都對應好了」
//!   * 「還沒解析完」
//!
//! 它們長得一模一樣。所以兩支讀取端點的回應都帶 `meta.parsing`，
//! 直說是哪一種。
//!
//! **刻意不用 outbox 事件觸發解析** —— worker 直接輪詢
//! `bim_models.status`，不透過 outbox（見
//! `services/bim-worker/README.md`）。
//!
//! # 為什麼 BIM 走直傳，而附件不走
//!
//! `attachments::create` 是把 bytes 收進 API 再寫儲存體 —— 對照片與 PDF
//! 那樣做沒問題。但**一個 IFC 動輒數百 MB**，塞過 API 伺服器會佔住連線
//! 好幾分鐘、吃記憶體，而逾時對客戶端只是「連線中斷」。
//!
//! 契約因此設計成「先取預簽網址直傳、再註冊」。而 `POST /uploads/presign`
//! 原本**只存在於 ENDPOINTS.md**，openapi 裡沒有 —— 又一個「指向不存在的
//! 地方」（BIM 註冊端點的說明就指著它）。這一輪把它補進契約並實作。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use fms_shared::{
    begin_tenant_tx, clamp_limit, object_key, page, require_permission, Caller, Cursor, FieldError,
    PageMeta, Problem, ProblemCode, SortSpec, Storage, TenantTx,
};

use crate::handlers::TenancyState;

const SOURCE_FORMATS: [&str; 6] = ["IFC", "RVT", "NWD", "DWG", "GLTF", "OTHER"];

/// `bim-worker` 目前只有 IfcOpenShell 這一種解析器；其他格式
/// `bim-worker::main._SUPPORTED_SOURCE_FORMATS` 會標成 `PARSE_FAILED`，
/// 但那是**非同步**的——上傳、直傳、註冊都成功之後，還要等 worker
/// 下一輪輪詢（最多 30 秒）才會看到失敗原因。這裡在註冊時就先擋下，
/// 讓使用者當下就知道，不必等一輪非同步往返才發現選錯格式。
const SUPPORTED_FORMATS: [&str; 1] = ["IFC"];

/// 直傳上傳的檔案大小上限。IFC 動輒數百 MB（見模組檔頭）——這裡留足餘裕，
/// 只擋明顯選錯檔案（例如誤選了幾 GB 的影片）的情況。
const MAX_UPLOAD_BYTES: i64 = 1024 * 1024 * 1024;

/// 上傳用的狀態：`TenancyState` 沒有 storage，而直傳需要它。
#[derive(Clone)]
pub struct UploadState {
    pub storage: Storage,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct BimModelDto {
    pub id: Uuid,
    pub facility_id: Uuid,
    pub name: String,
    pub source_format: String,
    pub version_label: Option<String>,
    pub discipline: Option<String>,
    pub status: String,
    pub element_count: i32,
    pub mapped_node_count: i32,
    pub mapped_asset_count: i32,
    pub viewer_urn: Option<String>,
    pub parsed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub uploaded_by: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BimModelCreate {
    pub name: Option<String>,
    pub source_format: Option<String>,
    pub storage_key: Option<String>,
    pub version_label: Option<String>,
    pub discipline: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PresignRequest {
    pub file_name: Option<String>,
    pub content_type: Option<String>,
    pub content_length: Option<i64>,
    #[allow(dead_code)]
    pub purpose: Option<String>,
}

const COLUMNS: &str = "id, facility_id, name::text AS name, source_format,
                       version_label::text AS version_label,
                       discipline::text AS discipline, status,
                       element_count, mapped_node_count, mapped_asset_count,
                       viewer_urn, parsed_at, uploaded_by, created_at";

/// 解析狀態的說明。**空的 `unresolved_elements` 有兩種意思**，這個字串
/// 讓呼叫端分得出來。
pub(crate) fn parsing_note(status: &str) -> &'static str {
    match status {
        "PARSED" => "已解析 —— unresolved_elements 反映真實的對應缺口",
        "PARSING" => "解析中 —— unresolved_elements 尚未完整",
        "PARSE_FAILED" => "解析失敗 —— 見 parse_report",
        // UPLOADED 是排隊中：獨立的 bim-worker 每 30 秒輪詢一次，
        // 沒有推播通道 —— 呼叫端要輪詢 status 直到變成終態。
        _ => {
            "已登記，排隊等待解析（獨立服務每 30 秒輪詢一次，沒有推播，\
              請輪詢本端點的 status 直到變成 PARSED 或 PARSE_FAILED）。\
              空的 unresolved_elements 代表「還沒解析完」，\
              不代表「全部都對應好了」"
        }
    }
}

/// `POST /uploads/presign`
pub async fn presign_upload(
    State(state): State<UploadState>,
    caller: Caller,
    Json(body): Json<PresignRequest>,
) -> Result<Json<serde_json::Value>, Problem> {
    let file_name = body
        .file_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation("file_name 為必填"))?;
    let content_type = body
        .content_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation("content_type 為必填 —— 它是簽章的一部分"))?;
    let content_length = body.content_length.filter(|&n| n > 0).ok_or_else(|| {
        Problem::validation("content_length 為必填且必須大於 0 —— 用檔案實際大小")
    })?;
    if content_length > MAX_UPLOAD_BYTES {
        return Err(Problem::validation(format!(
            "檔案大小 {content_length} bytes 超過上限 {MAX_UPLOAD_BYTES} bytes（{} MiB）",
            MAX_UPLOAD_BYTES / 1024 / 1024
        )));
    }

    // 只要求已登入（契約如此）。**不做 bim_model:write 檢查**是刻意的：
    // 預簽網址本身不洩漏任何資料，也不會讓物件出現在任何清單裡 ——
    // 真正的守衛是註冊端點（`POST .../bim-models` 要 bim_model:write）。
    // 一個上傳了但沒註冊的物件是孤兒，不是資料洩漏。
    //
    // 物件鍵含租戶 id（`object_key` 的慣例），因此跨租戶猜鍵猜不到。
    let key = object_key(caller.tenant_id, "bim-model", Uuid::new_v4(), file_name);
    let url = state.storage.presign_put(&key, content_type).await?;

    Ok(Json(serde_json::json!({
        "upload_url": url,
        "storage_key": key,
        // 客戶端**必須**用這個值當 Content-Type，否則儲存體會拒絕
        // （它被簽進網址裡）。回傳它比只寫在文件裡可靠。
        "content_type": content_type,
        "expires_in_seconds": 900,
    })))
}

/// `GET /facilities/{facilityId}/bim-models`
pub async fn list(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(facility_id): Path<Uuid>,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "bim_model:read", Some(facility_id), None).await?;

    let limit = clamp_limit(q.limit);
    // 最新的在前：模型是有版本的，而看的人要的通常是最新那一版。
    let sort = SortSpec {
        column: "created_at".to_string(),
        desc: true,
    };
    let (ckey, cid) = match q.cursor.as_deref() {
        Some(raw) => {
            let c = Cursor::decode(raw, &sort.column)?;
            (Some(c.as_timestamp()?), Some(c.uuid_id()?))
        }
        None => (None, None),
    };

    let rows: Vec<BimModelDto> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM fms.bim_models
          WHERE facility_id = $1
            AND ($2::timestamptz IS NULL
                 OR (created_at, id) < ($2::timestamptz, $3::uuid))
          ORDER BY created_at DESC, id DESC
          LIMIT $4"
    ))
    .bind(facility_id)
    .bind(ckey)
    .bind(cid)
    .bind(limit + 1)
    .fetch_all(tx.conn())
    .await?;
    tx.commit().await?;

    let paged = page::build(rows, limit, &sort, |r, _| (r.created_at.to_rfc3339(), r.id));
    // 每一列各自帶解析說明：同一個場域可能同時有已解析與未解析的模型。
    let data: Vec<serde_json::Value> = paged
        .data
        .iter()
        .map(|m| {
            let mut v = serde_json::to_value(m).unwrap_or_default();
            v["parsing"] = serde_json::Value::String(parsing_note(&m.status).to_string());
            v
        })
        .collect();

    Ok(Json(serde_json::json!({
        "data": data,
        "page": PageMeta {
            next_cursor: paged.page.next_cursor,
            limit: paged.page.limit,
            total_estimate: None,
        },
    })))
}

/// `POST /facilities/{facilityId}/bim-models`
///
/// **註冊已上傳的物件，排入解析。** 實際解析由獨立的 `bim-worker`
/// 服務非同步完成 —— 見模組檔頭。
pub async fn register(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(facility_id): Path<Uuid>,
    Json(body): Json<BimModelCreate>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Problem::validation("name 為必填"))?;
    let storage_key = body
        .storage_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Problem::validation(
                "storage_key 為必填 —— 先用 POST /uploads/presign 取得網址直傳，\
                 再拿回傳的 storage_key 來註冊",
            )
        })?;
    let format = body
        .source_format
        .as_deref()
        .map(|s| s.trim().to_uppercase())
        .unwrap_or_else(|| "IFC".to_string());
    if !SOURCE_FORMATS.contains(&format.as_str()) {
        return Err(Problem::validation(format!(
            "source_format 必須是 {} 其中之一",
            SOURCE_FORMATS.join("／")
        )));
    }
    if !SUPPORTED_FORMATS.contains(&format.as_str()) {
        return Err(Problem::validation(format!(
            "目前只支援 {} 格式匯入——{format} 還沒有解析器，請先轉出 {} 再上傳",
            SUPPORTED_FORMATS.join("／"),
            SUPPORTED_FORMATS.join("／"),
        )));
    }

    // 物件鍵必須屬於這個租戶。`object_key` 把租戶 id 放進前綴，因此這一個
    // 比對就足以擋掉「註冊別人上傳的物件」——
    // 少了它，一個猜到鍵的人可以把別的租戶的檔案掛進自己的場域。
    let prefix = format!("{}/", caller.tenant_id);
    if !storage_key.starts_with(&prefix) {
        return Err(Problem::validation(
            "storage_key 不屬於這個租戶 —— 它必須是 POST /uploads/presign 回傳的那一個",
        ));
    }

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "bim_model:write", Some(facility_id), None).await?;

    let row: BimModelDto = sqlx::query_as(&format!(
        "INSERT INTO fms.bim_models
           (tenant_id, facility_id, name, source_format, version_label, discipline,
            storage_bucket, storage_key, status, uploaded_by)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4, $5, $6, $7, 'UPLOADED', $8)
         RETURNING {COLUMNS}"
    ))
    .bind(facility_id)
    .bind(name)
    .bind(&format)
    .bind(body.version_label.as_deref())
    .bind(body.discipline.as_deref())
    .bind(state_bucket())
    .bind(storage_key)
    .bind(caller.user_id)
    .fetch_one(tx.conn())
    .await
    .map_err(translate)?;
    tx.commit().await?;

    let mut v = serde_json::to_value(&row)
        .map_err(|e| Problem::internal(std::io::Error::other(e.to_string())))?;
    v["parsing"] = serde_json::Value::String(parsing_note(&row.status).to_string());

    // 202：模型已登記，解析是非同步的（bim-worker 輪詢處理），
    // 不是這次請求就會完成。
    Ok((StatusCode::ACCEPTED, Json(v)))
}

/// `GET /bim-models/{bimModelId}/unresolved-elements`
pub async fn unresolved_elements(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(model_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;

    // 先取模型的場域，才能用正確的範圍檢查權限 —— 與 alarms 那一輪同一個
    // 順序，理由也相同（傳 None 會讓只有租戶級角色通得過）。
    let found: Option<(Uuid, String, serde_json::Value)> = sqlx::query_as(
        "SELECT facility_id, status, unresolved_elements FROM fms.bim_models WHERE id = $1",
    )
    .bind(model_id)
    .fetch_optional(tx.conn())
    .await?;
    let (facility_id, status, elements) =
        found.ok_or_else(|| Problem::not_found("找不到這個 BIM 模型"))?;

    require_permission(&mut tx, "bim_model:read", Some(facility_id), None).await?;
    tx.commit().await?;

    Ok(Json(serde_json::json!({
        "data": elements,
        "meta": {
            "status": status,
            // **空陣列有兩種意思，這裡說出是哪一種。**
            "parsing": parsing_note(&status),
        },
    })))
}

/// 找模型所屬的場域，同時判斷模型是否存在——刪除／重置都要先知道場域才能
/// 檢查權限，而 404 與權限不足的錢包（回應碼）不該混在一起。
async fn find_facility_and_status(
    tx: &mut TenantTx,
    model_id: Uuid,
) -> Result<(Uuid, String), Problem> {
    sqlx::query!(
        "SELECT facility_id, status FROM fms.bim_models WHERE id = $1",
        model_id
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?
    .map(|r| (r.facility_id, r.status))
    .ok_or_else(|| Problem::not_found("找不到這個 BIM 模型"))
}

/// 這個模型匯入的資料還被什麼卡住——跟 `tail::delete_node`／
/// `fms-asset::delete` 同一個判斷，只是彙總到整個模型範圍：模型匯入的是一
/// 整棵子樹，逐節點檢查「有沒有子節點」在這裡沒有意義（子節點也會一起清
/// 掉），真正該擋的是**這次操作管不到、但會被波及**的東西——未結的工單、
/// 啟用中的可預約資源、或模型範圍外的節點還掛在它底下（使用者手動加的）。
struct ImportBlockers {
    extra_children: i64,
    open_work_orders: i64,
    bookable: i64,
}

impl ImportBlockers {
    fn is_clear(&self) -> bool {
        self.extra_children == 0 && self.open_work_orders == 0 && self.bookable == 0
    }
}

async fn check_import_blockers(
    tx: &mut TenantTx,
    model_id: Uuid,
) -> Result<ImportBlockers, Problem> {
    let row = sqlx::query!(
        r#"WITH imported_nodes AS (
             SELECT id FROM fms.spatial_nodes
              WHERE bim_model_id = $1 AND deleted_at IS NULL AND node_type_code != 'BUILDING'
           ), imported_assets AS (
             SELECT id FROM fms.assets WHERE bim_model_id = $1 AND deleted_at IS NULL
           )
           SELECT
             (SELECT count(*) FROM fms.spatial_nodes c
               WHERE c.parent_id IN (SELECT id FROM imported_nodes) AND c.deleted_at IS NULL
                 AND c.id NOT IN (SELECT id FROM imported_nodes))::bigint AS "extra_children!",
             (SELECT count(*) FROM fms.work_orders w
               LEFT JOIN fms.work_order_statuses st ON st.code = w.status
              WHERE (w.spatial_node_id IN (SELECT id FROM imported_nodes)
                     OR w.asset_id IN (SELECT id FROM imported_assets))
                AND w.deleted_at IS NULL AND st.is_terminal IS NOT TRUE)::bigint
               AS "open_work_orders!",
             (SELECT count(*) FROM fms.bookable_resources br
               WHERE br.spatial_node_id IN (SELECT id FROM imported_nodes)
                 AND br.is_bookable)::bigint AS "bookable!""#,
        model_id
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;

    Ok(ImportBlockers {
        extra_children: row.extra_children,
        open_work_orders: row.open_work_orders,
        bookable: row.bookable,
    })
}

fn blocked_response(b: &ImportBlockers) -> Problem {
    Problem::new(ProblemCode::Conflict)
        .with_detail(format!(
            "這個模型匯入的空間底下還有 {} 個手動加的子節點、{} 張未結工單、\
             {} 個啟用中的可預約資源。先處理掉這些才能刪除／重置模型",
            b.extra_children, b.open_work_orders, b.bookable
        ))
        .with_errors(vec![
            FieldError {
                pointer: "/extra_children".to_string(),
                code: "HAS_EXTRA_CHILDREN".to_string(),
                message: b.extra_children.to_string(),
            },
            FieldError {
                pointer: "/open_work_orders".to_string(),
                code: "HAS_OPEN_WORK_ORDERS".to_string(),
                message: b.open_work_orders.to_string(),
            },
            FieldError {
                pointer: "/bookable_resources".to_string(),
                code: "HAS_BOOKABLE_RESOURCE".to_string(),
                message: b.bookable.to_string(),
            },
        ])
}

/// 軟刪除這個模型匯入的所有節點／資產。**不動 BUILDING 節點**——那是場域
/// 共用的根節點，不是這個模型獨有（見 bim-worker::ingest 的
/// `_find_or_create_building_root`：同一場域的多個模型會共用同一個根），
/// 刪它會連帶波及其他模型或手動建立的空間樹。
async fn clear_imported_data(tx: &mut TenantTx, model_id: Uuid) -> Result<(), Problem> {
    sqlx::query!(
        "UPDATE fms.assets SET deleted_at = clock_timestamp()
          WHERE bim_model_id = $1 AND deleted_at IS NULL",
        model_id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;

    sqlx::query!(
        "UPDATE fms.spatial_nodes SET deleted_at = clock_timestamp(), is_active = false,
                updated_at = clock_timestamp()
          WHERE bim_model_id = $1 AND deleted_at IS NULL AND node_type_code != 'BUILDING'",
        model_id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;

    Ok(())
}

/// `DELETE /bim-models/{bimModelId}`
///
/// 連同這個模型匯入的樓層／空間／設備一起清掉（軟刪除），再把模型紀錄本身
/// 硬刪——模型紀錄沒有 `deleted_at`（見 sql/003），本來就不走軟刪除慣例。
pub async fn delete(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(model_id): Path<Uuid>,
) -> Result<StatusCode, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let (facility_id, status) = find_facility_and_status(&mut tx, model_id).await?;
    require_permission(&mut tx, "bim_model:write", Some(facility_id), None).await?;

    if status == "PARSING" {
        return Err(Problem::new(ProblemCode::Conflict)
            .with_detail("這個模型正在解析中，請等解析完成（PARSED／PARSE_FAILED）再刪除"));
    }

    let blockers = check_import_blockers(&mut tx, model_id).await?;
    if !blockers.is_clear() {
        return Err(blocked_response(&blockers));
    }

    clear_imported_data(&mut tx, model_id).await?;
    sqlx::query!("DELETE FROM fms.bim_models WHERE id = $1", model_id)
        .execute(tx.conn())
        .await
        .map_err(Problem::from)?;
    tx.commit().await?;

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /bim-models/{bimModelId}/reset`
///
/// 清掉這個模型上次匯入的樓層／空間／設備，把狀態改回 `UPLOADED`——
/// `bim-worker` 每 30 秒輪詢一次 `status='UPLOADED'`，之後會自動重新解析同
/// 一個已經上傳好的檔案（`storage_key` 沒有變，物件還在儲存體裡）。用在
/// 「解析器邏輯修好了、想重新跑一次而不必重新上傳檔案」的情境。
pub async fn reset(
    State(state): State<TenancyState>,
    caller: Caller,
    Path(model_id): Path<Uuid>,
) -> Result<(StatusCode, Json<serde_json::Value>), Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    let (facility_id, status) = find_facility_and_status(&mut tx, model_id).await?;
    require_permission(&mut tx, "bim_model:write", Some(facility_id), None).await?;

    if status == "PARSING" {
        return Err(Problem::new(ProblemCode::Conflict)
            .with_detail("這個模型正在解析中，請等解析完成（PARSED／PARSE_FAILED）再重置"));
    }

    let blockers = check_import_blockers(&mut tx, model_id).await?;
    if !blockers.is_clear() {
        return Err(blocked_response(&blockers));
    }

    clear_imported_data(&mut tx, model_id).await?;

    let row: BimModelDto = sqlx::query_as(&format!(
        "UPDATE fms.bim_models SET
           status = 'UPLOADED', element_count = 0, mapped_node_count = 0,
           mapped_asset_count = 0, unresolved_elements = '[]'::jsonb,
           parse_report = '{{}}'::jsonb, parsed_at = NULL, updated_at = clock_timestamp()
         WHERE id = $1
         RETURNING {COLUMNS}"
    ))
    .bind(model_id)
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;
    tx.commit().await?;

    let mut v = serde_json::to_value(&row)
        .map_err(|e| Problem::internal(std::io::Error::other(e.to_string())))?;
    v["parsing"] = serde_json::Value::String(parsing_note(&row.status).to_string());

    Ok((StatusCode::ACCEPTED, Json(v)))
}

/// 註冊時寫入的 bucket 名。從環境變數取 —— 與 `Storage` 用的是同一個值，
/// 而這個 handler 的 state 沒有 storage（它只寫資料列）。
///
/// 刻意不預設成空字串：`storage_bucket` 是 NOT NULL，而一個空的 bucket 名
/// 會讓日後的下載找不到檔案，且錯誤訊息只說「物件不存在」。
fn state_bucket() -> String {
    std::env::var("S3_BUCKET_ATTACHMENTS").unwrap_or_else(|_| "fms".to_string())
}

fn translate(err: sqlx::Error) -> Problem {
    match &err {
        sqlx::Error::Database(db) => match db.constraint() {
            Some(c) if c.contains("source_format") => Problem::validation(format!(
                "source_format 必須是 {}",
                SOURCE_FORMATS.join("／")
            )),
            Some(c) if c.contains("facility") => {
                Problem::not_found("找不到這個場域（或它不在你的範圍內）")
            }
            _ => Problem::from(err),
        },
        _ => Problem::from(err),
    }
}
