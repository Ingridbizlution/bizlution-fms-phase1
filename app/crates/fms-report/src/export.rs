//! 報表匯出（`POST /reports/{reportCode}:export`、`GET /reports/exports/{id}`）。
//!
//! # 為什麼是兩支端點
//!
//! 契約只列了 `POST`。**只有那一支的話，發起者拿不回檔案** —— 非同步作業
//! 必須有一個可輪詢的資源，否則 202 之後就沒有下文。稽核匯出（054／#20）
//! 遇過同一件事，這裡直接照那個結論做。
//!
//! # 佇列複用 event_outbox，狀態放 report_exports（066）
//!
//! 與稽核匯出同一個分工：outbox 觸發（同一個交易寫入，所以「作業建立了但
//! 沒有人去做」不可能發生），`report_exports` 記狀態與結果。
//!
//! # 報表目錄是程式碼的事實，不是管理者定義的條件
//!
//! 每一支報表是一個獨立的 SQL 函式，有自己的參數與欄位。所以 [`REPORTS`]
//! 是一份程式碼裡的清單，而不是一張資料表 —— 表裡有一列但沒有對應的函式，
//! 只會產出一個永遠 FAILED 的作業。
//!
//! 但一份手寫的清單會與實際掛上的路由分歧，而分歧的兩個方向都很難察覺：
//!
//!   * 清單有、路由沒有 → 可匯出一份讀不到的報表
//!   * 路由有、清單沒有 → 那支報表匯不出來，而沒有任何錯誤訊息
//!
//! `report_export_slice.rs` 的 `a_` 拿 `IMPLEMENTED_OPERATIONS` 逐一比對，
//! 兩個方向都檢查。
//!
//! # 欄位順序取自資料庫，不是手寫的
//!
//! 產檔要有表頭，而表頭的順序就是函式 `RETURNS TABLE` 的順序。手抄一份會
//! 在某次改欄位之後安靜地錯位 —— CSV 沒有欄位名稱以外的校驗，錯位的檔案
//! 看起來完全正常。所以 worker 從 `pg_proc.proargnames` 讀順序（見
//! `fms_worker::report_export`），這裡只記下**參數**。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use fms_shared::{begin_tenant_tx, require_permission, Caller, FieldError, Problem, Storage};

/// outbox 的事件型別。發送端與接收端共用這個常數不可能寫成不同的字串
/// ——`report_export_slice.rs` 另有一格斷言 worker 那邊的值與這裡相等。
pub const EVENT_TYPE: &str = "report_export.requested";

/// 一個參數要用什麼型別綁進 SQL。
///
/// 型別必須明講：`params` 是 jsonb，而 `$1` 綁 text 進一個 `date` 參數
/// 會在**產檔的時候**才炸 —— 那時發起者已經拿到 202 了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    Date,
    Uuid,
    Int,
    Text,
}

impl ParamKind {
    pub fn cast(self) -> &'static str {
        match self {
            ParamKind::Date => "date",
            ParamKind::Uuid => "uuid",
            ParamKind::Int => "int",
            ParamKind::Text => "text",
        }
    }
}

/// 一支可匯出的報表。
pub struct ReportSpec {
    /// 路徑裡的 `{reportCode}`，同時是 `GET /reports/<code>` 的那個 code。
    pub code: &'static str,
    /// `fms.` 底下的函式名。
    pub function: &'static str,
    /// 除了 `p_from`／`p_to` 之外的具名參數：
    /// `(請求體的鍵, 函式的參數名, 型別, 省略時的預設值)`。
    ///
    /// 順序不重要（用具名記號呼叫），但名稱必須與函式的參數名一致 ——
    /// `a_` 那一格會逐一驗。
    ///
    /// # 為什麼要有預設值，而不是讓函式自己的 DEFAULT 生效
    ///
    /// `report_sla_compliance` 與 `report_pm_compliance` 的 `p_group_by`
    /// **沒有** DEFAULT —— 少帶它連查詢都跑不起來。而那兩支的 `GET` 端點
    /// 各自在應用層預設成 `facility`／`strict`。
    ///
    /// 那個預設必須在**建立作業時**就寫進 `params`，不是產檔時才補：
    /// `params` 是這份檔案唯一的紀錄，它必須完整決定檔案的內容。
    /// 少了 `group_by` 的那一列，事後沒有人能回答「這份檔案是按什麼分組的」。
    /// （第一版沒有這個欄位，症狀是那兩支報表匯出時 FAILED，
    ///  而訊息是 `function ... does not exist` —— 看起來像函式不見了。）
    pub extra_params: &'static [(&'static str, &'static str, ParamKind, Option<&'static str>)],
}

/// 可匯出的報表。**與 `GET /reports/*` 對得上**，見模組檔頭。
///
/// `facility-dashboard` 刻意不在裡面：它回的是一個彙總物件而不是列，
/// 沒有表頭可寫。那不是遺漏，`a_` 那一格把它列為已知的例外並說明理由。
pub const REPORTS: &[ReportSpec] = &[
    ReportSpec {
        code: "sla-compliance",
        function: "report_sla_compliance",
        extra_params: &[
            ("group_by", "p_group_by", ParamKind::Text, Some("facility")),
            (
                "strictness",
                "p_strictness",
                ParamKind::Text,
                Some("strict"),
            ),
        ],
    },
    ReportSpec {
        code: "pm-compliance",
        function: "report_pm_compliance",
        extra_params: &[
            ("group_by", "p_group_by", ParamKind::Text, Some("facility")),
            ("grace_days", "p_grace_override", ParamKind::Int, None),
        ],
    },
    ReportSpec {
        code: "group-rollup",
        function: "report_group_rollup",
        extra_params: &[("subtree_of", "p_subtree_of", ParamKind::Uuid, None)],
    },
    ReportSpec {
        code: "asset-reliability",
        function: "report_asset_reliability",
        extra_params: &[
            ("facility_id", "p_facility_id", ParamKind::Uuid, None),
            ("limit", "p_limit", ParamKind::Int, None),
        ],
    },
    ReportSpec {
        code: "space-utilization",
        function: "report_space_utilization",
        extra_params: &[("facility_id", "p_facility_id", ParamKind::Uuid, None)],
    },
    ReportSpec {
        code: "service-volume",
        function: "report_service_volume",
        extra_params: &[(
            "group_by",
            "p_group_by",
            ParamKind::Text,
            Some("service_item"),
        )],
    },
];

pub fn find_report(code: &str) -> Option<&'static ReportSpec> {
    REPORTS.iter().find(|r| r.code == code)
}

#[derive(Clone)]
pub struct ReportExportState {
    pub pool: PgPool,
    pub storage: Storage,
}

/// 請求體：`from`／`to` 必填，其餘依報表而定，原樣存進 `params`。
///
/// `from`／`to` 必填是刻意的。稽核匯出允許「不帶條件＝匯出全部」並用
/// `meta.warning` 說出來，但報表函式沒有那個語意：`p_from` 是 NOT NULL 的
/// 參數，少了它連查詢都跑不起來。與其讓作業在產檔時失敗（那時發起者已經
/// 拿到 202），在這裡就擋掉。
#[derive(Debug, Deserialize)]
pub struct ExportRequest {
    pub from: chrono::NaiveDate,
    pub to: chrono::NaiveDate,
    #[serde(default)]
    pub format: Option<String>,
    /// 那支報表自己的參數。未知的鍵會被拒絕 —— 見 `create`。
    #[serde(flatten)]
    pub params: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, sqlx::FromRow)]
struct ExportRow {
    id: Uuid,
    report_code: String,
    format: String,
    status: String,
    params: serde_json::Value,
    row_count: Option<i64>,
    object_key: Option<String>,
    error: Option<String>,
    requested_by: Uuid,
    created_at: chrono::DateTime<chrono::Utc>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize)]
pub struct ExportDto {
    pub id: Uuid,
    pub report_code: String,
    pub format: String,
    pub status: String,
    pub params: serde_json::Value,
    pub row_count: Option<i64>,
    pub download_url: Option<String>,
    pub error: Option<String>,
    pub requested_by: Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

const FORMATS: [&str; 2] = ["csv", "xlsx"];

/// `POST /reports/{reportCode}:export`
///
/// # 為什麼未知的參數鍵要回 422 而不是忽略
///
/// 忽略一個打錯的 `facilty_id` 會產出一份**範圍比預期大**的報表，而它看起來
/// 完全正常 —— 檔案有資料、狀態是 COMPLETED、列數是一個合理的數字。
/// 那份檔案接著會被拿去談合約。
pub async fn create(
    State(state): State<ReportExportState>,
    caller: Caller,
    uri: axum::http::Uri,
    Json(req): Json<ExportRequest>,
) -> Result<(StatusCode, Json<ExportDto>), Problem> {
    // 報表代碼從路徑取。axum 0.8 不接受 `{reportCode}:export`（一個路段裡
    // 不能同時有參數與字面值），所以 router 是從 [`REPORTS`] 逐一展開的
    // 靜態路徑 —— 也就是說**路由表就是白名單**，走到這裡的代碼一定在清單裡。
    // 下面的 422 因此是「router 掛了一條沒有 spec 的路徑」才會走到，
    // 留著是為了讓那種不一致有訊息而不是 panic。
    let report_code = uri
        .path()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .trim_end_matches(":export")
        .to_string();

    let spec = find_report(&report_code).ok_or_else(|| {
        Problem::validation(format!(
            "`{report_code}` 不是可匯出的報表；可用的是 {}",
            REPORTS
                .iter()
                .map(|r| r.code)
                .collect::<Vec<_>>()
                .join("／")
        ))
        .with_errors(vec![FieldError {
            pointer: "/reportCode".to_string(),
            code: "ENUM".to_string(),
            message: format!("未知的報表代碼 `{report_code}`"),
        }])
    })?;

    if req.from > req.to {
        return Err(
            Problem::validation("`from` 不得晚於 `to`").with_errors(vec![FieldError {
                pointer: "/from".to_string(),
                code: "RANGE".to_string(),
                message: format!("from={} 晚於 to={}", req.from, req.to),
            }]),
        );
    }

    let format = req.format.clone().unwrap_or_else(|| "csv".to_string());
    if !FORMATS.contains(&format.as_str()) {
        return Err(
            Problem::validation("`format` 必須是 csv 或 xlsx").with_errors(vec![FieldError {
                pointer: "/format".to_string(),
                code: "ENUM".to_string(),
                message: format!("`{format}` 不是支援的格式"),
            }]),
        );
    }

    // 未知的鍵一律拒絕，見函式檔頭。
    for key in req.params.keys() {
        if !spec.extra_params.iter().any(|(k, _, _, _)| k == key) {
            let allowed = spec
                .extra_params
                .iter()
                .map(|(k, _, _, _)| *k)
                .collect::<Vec<_>>()
                .join("／");
            let detail = if allowed.is_empty() {
                format!("`{}` 除了 from／to 之外不接受其他參數", spec.code)
            } else {
                format!("`{}` 只接受 from／to／{allowed}", spec.code)
            };
            return Err(Problem::validation(detail).with_errors(vec![FieldError {
                pointer: format!("/{key}"),
                code: "UNKNOWN".to_string(),
                message: format!(
                    "`{key}` 不是這支報表的參數 —— 忽略它會產出一份範圍比預期大的檔案"
                ),
            }]));
        }
    }

    let mut params = req.params.clone();
    params.insert("from".to_string(), serde_json::json!(req.from));
    params.insert("to".to_string(), serde_json::json!(req.to));
    // 省略的參數補上預設值，見 `ReportSpec::extra_params` 的檔頭 ——
    // `params` 必須完整決定檔案的內容。
    for (key, _, _, default) in spec.extra_params {
        if let Some(d) = default {
            params
                .entry(key.to_string())
                .or_insert_with(|| serde_json::json!(d));
        }
    }
    let params = serde_json::Value::Object(params);

    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    // `report:export` 是 FACILITY 範圍的權限，但這裡不指定場域 ——
    // 報表本來就是跨場域彙總，而**內容**的收斂由 RLS 在產檔時完成
    // （報表函式是 SECURITY INVOKER，worker 以 requested_by 的情境查）。
    require_permission(&mut tx, "report:export", None, None).await?;

    let row: ExportRow = sqlx::query_as(
        "INSERT INTO fms.report_exports
           (tenant_id, requested_by, report_code, format, params)
         VALUES (fms.current_tenant_id(), $1, $2, $3, $4)
         RETURNING id, report_code, format, status, params, row_count, object_key,
                   error, requested_by, created_at, completed_at",
    )
    .bind(caller.user_id)
    .bind(spec.code)
    .bind(&format)
    .bind(&params)
    .fetch_one(tx.conn())
    .await?;

    // 同一個交易裡寫 outbox：「作業建立了但沒有人去做」因此不可能發生。
    sqlx::query(
        "INSERT INTO fms.event_outbox
           (tenant_id, event_type, aggregate_type, aggregate_id, payload)
         VALUES (fms.current_tenant_id(), $1, 'REPORT_EXPORT', $2,
                 jsonb_build_object('export_id', $2::text))",
    )
    .bind(EVENT_TYPE)
    .bind(row.id)
    .execute(tx.conn())
    .await?;
    tx.commit().await?;

    Ok((StatusCode::ACCEPTED, Json(dto(row, None))))
}

/// `GET /reports/exports/{id}`
pub async fn get(
    State(state): State<ReportExportState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<ExportDto>, Problem> {
    let mut tx = begin_tenant_tx(&state.pool, caller.into()).await?;
    require_permission(&mut tx, "report:export", None, None).await?;

    let row: Option<ExportRow> = sqlx::query_as(
        "SELECT id, report_code, format, status, params, row_count, object_key,
                error, requested_by, created_at, completed_at
           FROM fms.report_exports WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(tx.conn())
    .await?;
    tx.commit().await?;

    let row = row.ok_or_else(|| Problem::not_found("找不到這個匯出作業"))?;

    // 066 的 CHECK 保證 COMPLETED 一定有 object_key，所以這個 match 不是
    // 防禦性判斷而是型別上的必要。
    let url = match (row.status.as_str(), row.object_key.as_deref()) {
        ("COMPLETED", Some(key)) => Some(
            state
                .storage
                .presign_get(
                    key,
                    &format!("{}-{}.{}", row.report_code, row.id, row.format),
                )
                .await?,
        ),
        _ => None,
    };

    Ok(Json(dto(row, url)))
}

fn dto(row: ExportRow, download_url: Option<String>) -> ExportDto {
    ExportDto {
        id: row.id,
        report_code: row.report_code,
        format: row.format,
        status: row.status,
        params: row.params,
        row_count: row.row_count,
        download_url,
        error: row.error,
        requested_by: row.requested_by,
        created_at: row.created_at,
        completed_at: row.completed_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每一支報表的 code 必須是 066 的 `ck_report_exports_code` 認的形狀
    /// —— 否則 INSERT 會在權限檢查**之後**才失敗，而失敗訊息是一個
    /// 資料庫層的約束違反。
    #[test]
    fn report_codes_match_the_check_constraint() {
        for r in REPORTS {
            assert!(
                r.code.len() >= 3 && r.code.len() <= 40,
                "{} 的長度不合 ck_report_exports_code",
                r.code
            );
            assert!(
                r.code.starts_with(|c: char| c.is_ascii_lowercase()),
                "{} 必須以小寫字母開頭",
                r.code
            );
            assert!(
                r.code
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{} 只能有小寫字母、數字與連字號",
                r.code
            );
        }
    }

    /// code 不重複。重複的話 `find_report` 會安靜地只找到第一個。
    #[test]
    fn report_codes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for r in REPORTS {
            assert!(seen.insert(r.code), "{} 重複了", r.code);
        }
    }
}
