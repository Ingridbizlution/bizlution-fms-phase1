//! 形狀對齊 `openapi.yaml` 的 `Attachment`。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `Attachment`
#[derive(Debug, Serialize)]
pub struct AttachmentDto {
    pub id: Uuid,
    pub purpose: String,
    pub file_name: String,
    pub mime_type: Option<String>,
    pub size_bytes: Option<i64>,
    /// 短期有效的預簽網址。每次讀取重新產生 ——
    /// 存下來會過期，而延長有效期是錯的解法（見 `storage` 模組）。
    pub download_url: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// `GET /attachments` 的查詢參數：附件一律以「掛在哪個實體上」查詢。
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub entity_type: Option<String>,
    pub entity_id: Option<Uuid>,
    pub purpose: Option<String>,
}
