//! RFC 9457 Problem Details，欄位對齊 `api/openapi.yaml` 的 `Problem` schema。
//!
//! 契約方向不可反轉（ADR-09 實作紀律 1）：本型別是「為了符合手寫契約」而存在，
//! 不是用來產生契約。任何欄位變動都應先改 `openapi.yaml`。

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// `Problem.errors[]` 的單一項目：指向請求 body 中出錯的位置。
#[derive(Debug, Clone, Serialize)]
pub struct FieldError {
    /// JSON Pointer，例如 `/payload/headcount`
    pub pointer: String,
    /// 穩定的機器可讀碼，例如 `MAXIMUM`
    pub code: String,
    pub message: String,
}

/// 穩定的機器可讀錯誤碼。供前端 i18n 對照，因此新增可以、改名不行。
///
/// 刻意不叫 `ErrorKind`：它同時決定 HTTP status、`type` URI 與 `title`，
/// 是契約的一部分，不只是內部分類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemCode {
    /// 請求本身不合法（例如缺少必要標頭）。與 `ValidationError`(422) 區分：
    /// 後者是「body 內容不符規則」，前者是「請求連被處理的前提都不成立」。
    /// 規格書 §4.3 明訂缺少 `X-Tenant-ID` 回 400。
    BadRequest,
    ValidationError,
    Unauthenticated,
    PermissionDenied,
    NotFound,
    Conflict,
    ReservationConflict,
    QuotaExceeded,
    StaleVersion,
    /// 缺少 `If-Match`。規格書 §4.3：寫入類請求缺少樂觀鎖標頭回 428。
    PreconditionRequired,
    /// 工單狀態機拒絕了這個動作。與 `Conflict` 分開，因為前端要據此
    /// 重新拉 `available-actions` 而不是單純重試 —— 同一個請求再送一次
    /// 永遠會得到同樣的結果。
    WorkOrderIllegalTransition,
    IdempotencyKeyReused,
    IdempotencyInProgress,
    TenantMismatch,
    TooManyRequests,
    /// 這條路徑**刻意還沒有實作**，而且原因是結構性的（缺少某個外部前提），
    /// 不是「還沒排到」。
    ///
    /// 與 `Internal` 分開的理由：500 是「我們壞了」，而 501 是「這件事在這個
    /// 部署裡做不到」—— 前者該去看伺服器日誌，後者該去看 `detail` 裡列的前提。
    /// 目前的使用者是 `/auth/sso/*`（缺密鑰解析器與可對接的 IdP）。
    ///
    /// **不用它來包裝「回一個假的成功」**：一支核發身分卻沒有驗證任何東西的
    /// callback 比 501 危險得多。
    NotImplemented,
    Internal,
}

impl ProblemCode {
    /// `Problem.code`
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadRequest => "BAD_REQUEST",
            Self::ValidationError => "VALIDATION_ERROR",
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::NotFound => "NOT_FOUND",
            Self::Conflict => "CONFLICT",
            Self::ReservationConflict => "RESERVATION_CONFLICT",
            Self::QuotaExceeded => "QUOTA_EXCEEDED",
            Self::StaleVersion => "STALE_VERSION",
            Self::PreconditionRequired => "PRECONDITION_REQUIRED",
            Self::WorkOrderIllegalTransition => "WORK_ORDER_ILLEGAL_TRANSITION",
            Self::IdempotencyKeyReused => "IDEMPOTENCY_KEY_REUSED",
            Self::IdempotencyInProgress => "IDEMPOTENCY_IN_PROGRESS",
            Self::TenantMismatch => "TENANT_MISMATCH",
            Self::TooManyRequests => "TOO_MANY_REQUESTS",
            Self::NotImplemented => "NOT_IMPLEMENTED",
            Self::Internal => "INTERNAL_ERROR",
        }
    }

    /// `Problem.status`
    pub fn status(self) -> StatusCode {
        match self {
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::ValidationError => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::PermissionDenied | Self::TenantMismatch => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict
            | Self::ReservationConflict
            | Self::QuotaExceeded
            | Self::WorkOrderIllegalTransition
            | Self::IdempotencyInProgress => StatusCode::CONFLICT,
            Self::StaleVersion => StatusCode::PRECONDITION_FAILED,
            Self::PreconditionRequired => StatusCode::PRECONDITION_REQUIRED,
            Self::IdempotencyKeyReused => StatusCode::UNPROCESSABLE_ENTITY,
            Self::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
            Self::NotImplemented => StatusCode::NOT_IMPLEMENTED,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// `Problem.title`（人可讀，穩定）
    pub fn title(self) -> &'static str {
        match self {
            Self::BadRequest => "Bad Request",
            Self::ValidationError => "Unprocessable Entity",
            Self::Unauthenticated => "Unauthorized",
            Self::PermissionDenied => "Forbidden",
            Self::TenantMismatch => "Tenant Mismatch",
            Self::NotFound => "Not Found",
            Self::Conflict => "Conflict",
            Self::ReservationConflict => "Reservation Conflict",
            Self::QuotaExceeded => "Quota Exceeded",
            Self::WorkOrderIllegalTransition => "Illegal state transition",
            Self::StaleVersion => "Precondition Failed",
            Self::PreconditionRequired => "Precondition Required",
            Self::IdempotencyKeyReused => "Idempotency Key Reused",
            Self::IdempotencyInProgress => "Request In Progress",
            Self::TooManyRequests => "Too Many Requests",
            Self::NotImplemented => "Not Implemented",
            Self::Internal => "Internal Server Error",
        }
    }

    /// `Problem.type`，形式為 `<base>/problems/<kebab-code>`
    fn type_uri(self) -> String {
        format!(
            "{}/problems/{}",
            problem_base_uri(),
            self.as_str().to_lowercase().replace('_', "-")
        )
    }
}

/// `Problem.type` 的前綴。以 `FMS_PROBLEM_BASE_URI` 覆寫，
/// 預設值對齊 openapi.yaml 的 servers。
fn problem_base_uri() -> String {
    std::env::var("FMS_PROBLEM_BASE_URI")
        .unwrap_or_else(|_| "https://api.fms.bizlution.com".to_string())
}

/// 應用層錯誤。`IntoResponse` 產生的 body 逐欄位符合 `Problem` schema，
/// 且 Content-Type 為 `application/problem+json`（非一般 `application/json`）。
#[derive(Debug)]
pub struct Problem {
    pub code: ProblemCode,
    pub detail: Option<String>,
    pub errors: Vec<FieldError>,
    /// 僅 `Internal` 使用：記進 log，不回傳給呼叫端。
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
    /// 僅 `TooManyRequests` 使用：`Retry-After` 標頭的秒數。
    ///
    /// 契約把這個標頭列在 `TooManyRequests` 回應上，因此它是契約的一部分
    /// 而不是額外好意 —— 少了它，客戶端只能靠猜測決定何時重試。
    retry_after: Option<u64>,
}

impl Problem {
    pub fn new(code: ProblemCode) -> Self {
        Self {
            code,
            detail: None,
            errors: Vec::new(),
            source: None,
            retry_after: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_errors(mut self, errors: Vec<FieldError>) -> Self {
        self.errors = errors;
        self
    }

    /// 包裝內部錯誤：`source` 只進 log，不外洩實作細節。
    pub fn internal<E: std::error::Error + Send + Sync + 'static>(err: E) -> Self {
        Self {
            code: ProblemCode::Internal,
            detail: None,
            errors: Vec::new(),
            source: Some(Box::new(err)),
            retry_after: None,
        }
    }

    /// 429，附上 `Retry-After`。
    ///
    /// 刻意不提供「沒有 `Retry-After` 的 429」建構子：契約的
    /// `TooManyRequests` 回應帶這個標頭，而一個不告訴客戶端何時能重試的
    /// 限流會讓對方以更短的間隔重試，等於自找更多流量。
    pub fn too_many_requests(retry_after_secs: u64, detail: impl Into<String>) -> Self {
        Self {
            retry_after: Some(retry_after_secs),
            ..Self::new(ProblemCode::TooManyRequests).with_detail(detail)
        }
    }

    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(ProblemCode::NotFound).with_detail(detail)
    }

    pub fn unauthenticated(detail: impl Into<String>) -> Self {
        Self::new(ProblemCode::Unauthenticated).with_detail(detail)
    }

    pub fn permission_denied(detail: impl Into<String>) -> Self {
        Self::new(ProblemCode::PermissionDenied).with_detail(detail)
    }

    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(ProblemCode::ValidationError).with_detail(detail)
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(ProblemCode::BadRequest).with_detail(detail)
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.detail {
            Some(d) => write!(f, "{}: {}", self.code.as_str(), d),
            None => write!(f, "{}", self.code.as_str()),
        }
    }
}

impl std::error::Error for Problem {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|e| e as _)
    }
}

/// 序列化用的形狀；`request_id` 由 middleware 於回應時填入。
#[derive(Serialize)]
struct ProblemBody {
    #[serde(rename = "type")]
    type_uri: String,
    title: &'static str,
    status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<FieldError>,
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        if let Some(err) = &self.source {
            tracing::error!(error = %err, code = self.code.as_str(), "internal error");
        }

        let status = self.code.status();
        let body = ProblemBody {
            type_uri: self.code.type_uri(),
            title: self.code.title(),
            status: status.as_u16(),
            detail: self.detail,
            code: self.code.as_str(),
            // request_id 由 ProblemRequestId middleware 補上（見 fms-shared::middleware）
            request_id: None,
            errors: self.errors,
        };

        let mut response = (status, axum::Json(body)).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/problem+json"),
        );
        if let Some(secs) = self.retry_after {
            // 秒數由 u64 格式化而來，必定是合法的標頭值。
            if let Ok(v) = header::HeaderValue::from_str(&secs.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        response
    }
}

/// Postgres 錯誤映射。把資料庫既有的約束與 `RAISE` 訊息翻譯成契約定義的 code。
///
/// 這一層是刻意存在的：`sql/` 已經用 SQLSTATE 與 HINT 表達語意
/// （例如 `consume_quota` 以 `23514` + `HINT=QUOTA_EXCEEDED` 表示配額用盡），
/// 應用層的責任是忠實轉譯，而不是重新實作判斷。
impl From<sqlx::Error> for Problem {
    fn from(err: sqlx::Error) -> Self {
        if matches!(err, sqlx::Error::RowNotFound) {
            return Problem::not_found("resource not found");
        }

        if let sqlx::Error::Database(db) = &err {
            // 23514 在這個 schema 裡有三種來源，必須靠訊息內容區分：
            //   * consume_quota 的 HINT=QUOTA_EXCEEDED → 409 QUOTA_EXCEEDED
            //   * transition_work_order／trg_enforce_wo_transition 的
            //     RAISE → 409 WORK_ORDER_ILLEGAL_TRANSITION
            //   * 其他真正的 CHECK 約束違反 → 走到最後變成 500，而那是對的
            //     （代表應用層漏擋了某個值，是我們的 bug 不是客戶端的）
            //
            // 用訊息內容分派並不優雅，但這是 schema 已經表達語意的方式，
            // 應用層的責任是忠實轉譯而非另建一套判斷（見上方註解）。
            if db.code().as_deref() == Some("23514") {
                let msg = db.message();
                if msg.contains("METER_VALUE_INVALID") {
                    // 030 的讀數推進規則。訊息本身已經說明是負增量還是累計倒退，
                    // 且含可行動的建議（設 rollover_at），因此原樣轉譯。
                    return Problem::validation(msg.to_string());
                }
                if msg.contains("QUOTA_EXCEEDED") {
                    return Problem::new(ProblemCode::QuotaExceeded)
                        .with_detail("quota exhausted for this period");
                }
                if msg.contains("is not allowed from status")
                    || msg.contains("illegal work order transition")
                {
                    return Problem::new(ProblemCode::WorkOrderIllegalTransition)
                        .with_detail(msg.to_string());
                }
            }

            match db.code().as_deref() {
                // exclusion_violation —— 預約時段重疊
                Some("23P01") => {
                    return Problem::new(ProblemCode::ReservationConflict)
                        .with_detail("the requested time range is no longer available")
                }
                // deadlock_detected —— 高併發搶訂時 PostgreSQL 擇一犧牲。
                // T11 證實 100 路競爭下落敗者會在 23P01 與 40P01 間隨機分佈，
                // 語意同樣是「時段被搶走」，絕不可回 500（ADR-09 實作紀律 5）。
                Some("40P01") => {
                    return Problem::new(ProblemCode::ReservationConflict)
                        .with_detail("the requested time range is no longer available")
                }
                Some("23505") => {
                    return Problem::new(ProblemCode::Conflict)
                        .with_detail("a conflicting record already exists")
                }
                // 42501 insufficient_privilege —— 含 set_context 的
                // PLATFORM_CONTEXT_DENIED 與 freeze_tenant_id 觸發器
                Some("42501") => {
                    return Problem::new(ProblemCode::PermissionDenied)
                        .with_detail("operation not permitted in this context")
                }
                _ => {}
            }
        }

        Problem::internal(err)
    }
}
