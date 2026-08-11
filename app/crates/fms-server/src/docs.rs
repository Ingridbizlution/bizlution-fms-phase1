//! 把 `api/openapi.yaml` 服務出來，外加一個互動式瀏覽器。
//!
//! # 為什麼是 `include_str!` 而不是執行時讀檔
//!
//! ADR-09 紀律 1 是「契約是權威」。服務出去的規格必須與 `api/openapi.yaml`
//! **是同一份位元組**，不是它的副本 —— 這個 repo 已經有多次「同一條規則兩份
//! 手抄本，其中一份漏了東西」的實例（見 `sql/053` 的檔頭）。
//!
//! 編進 binary 另外解決兩件事：
//!   * 不依賴執行時的工作目錄。容器裡的 CWD 與開發機不同，而「讀檔版」的
//!     失敗方式是第一個請求才 500，那時候已經上線了。
//!   * 契約改了但忘記重編會被 cargo 自己抓到 —— `include_str!` 會把檔案
//!     登記成編譯相依。
//!
//! # 授權決定：`/docs` 與 `/api/v1/openapi.yaml` **刻意不需認證**
//!
//! 兩個選項都合理，這裡選不認證，理由：
//!
//!   1. 規格本身不含任何租戶資料，只有形狀。
//!   2. 這是地端產品，網路邊界已經是一層防護。
//!   3. **技術上必須如此**：瀏覽器頁面要先把規格抓下來才畫得出「Authorize」
//!      按鈕。冷啟動的頁面沒有 token 可帶，所以規格若要認證，這個瀏覽器就
//!      根本開不起來 —— 除非另外做一套只給 `/docs` 用的登入，
//!      而那是為了守一份不含資料的檔案去蓋一套新的認證路徑。
//!
//! 反面的理由是真的、且被接受了：規格洩漏了完整的攻擊面清單（端點、參數、
//! 權限碼）。接受它，是因為**我們不把「端點清單不為人知」當成一項控制** ——
//! 每一支端點自己都有認證與權限檢查（見 `require_auth` 與 026 的
//! `min_scope_level`）。若哪天靠隱藏清單才安全，那是那支端點的缺陷。
//!
//! # 這兩支是基礎設施端點，不進契約表格
//!
//! 它們在 router 裡，但**不在** `IMPLEMENTED_OPERATIONS`、
//! 不在 `api/openapi.yaml`、也不在 `api/ENDPOINTS.md`。
//!
//! 這不是新發明的例外：`/api/v1/health` 從一開始就是這樣處理的
//! （見 `lib.rs` 的 `health()`）。`IMPLEMENTED_OPERATIONS` 是「契約覆蓋率」
//! 的輸入 —— 把營運端點放進去會讓那個數字說謊。
//!
//! 之所以能這樣做而不會讓守衛失效：`endpoints_doc` 的
//! `implemented_column_matches_the_router` **不列舉 axum 的路由表**，
//! 它比對的是 `IMPLEMENTED_OPERATIONS` 這份常數清單與 `ENDPOINTS.md`。
//! 因此不加進清單就不會有任何測試要求 `ENDPOINTS.md` 補列。
//! （動這兩份東西之前請先重讀那個測試，這段話是它現在的行為，不是保證。）

use axum::http::header::{HeaderValue, CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};

/// 權威契約，編譯期嵌入。路徑相對於本檔（`app/crates/fms-server/src/`）。
pub const OPENAPI_YAML: &str = include_str!("../../../../api/openapi.yaml");

/// 瀏覽器的 JS 與 CSS。未經修改的 upstream 檔案 ——
/// 來源、版本與雜湊見 `api/vendor/swagger-ui/README.md`。
const SWAGGER_UI_JS: &str = include_str!("../../../../api/vendor/swagger-ui/swagger-ui-bundle.js");
const SWAGGER_UI_CSS: &str = include_str!("../../../../api/vendor/swagger-ui/swagger-ui.css");

/// 瀏覽器頁面。
///
/// **不得引用任何外部來源。** 客戶環境可能沒有外網，而那時的症狀是
/// 「頁面開得起來、內容不出現」—— 看起來像伺服器壞了。
/// `the_docs_page_makes_no_external_requests` 守著這一點。
const INDEX_HTML: &str = r#"<!doctype html>
<html lang="zh-Hant">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Facility Management System Platform API</title>
<link rel="stylesheet" href="/docs/swagger-ui.css">
<style>body { margin: 0; }</style>
</head>
<body>
<div id="swagger-ui"></div>
<script src="/docs/swagger-ui-bundle.js"></script>
<script>
window.ui = SwaggerUIBundle({
  url: '/api/v1/openapi.yaml',
  dom_id: '#swagger-ui',
  deepLinking: true,
  persistAuthorization: true,

  // 契約的 servers 指向 production／staging 的公網位址（見 openapi.yaml
  // 的 servers 區塊）。要試打的永遠是**現在這台**伺服器：
  //   * 從開發機按 Execute 卻打到 production 是會造成損害的意外
  //   * 離網環境根本連不到那些網址，症狀是每個請求都 fail
  // 因此把每個請求改寫回本頁的 origin。/docs 與 API 同源，所以也不會有 CORS。
  requestInterceptor: function (req) {
    var target = new URL(req.url, window.location.href);
    req.url = window.location.origin + target.pathname + target.search;
    return req;
  },

  // Swagger UI 預設會在頁尾放一張
  // `https://validator.swagger.io/validator?url=...` 的徽章圖。
  // 那是這個頁面唯一會發出的外部請求，而且會把契約網址送給第三方。
  // 地端部署沒有外網，關掉。
  validatorUrl: null
});
</script>
</body>
</html>
"#;

/// 靜態回應。`content_type` 用 `&'static str` 是因為呼叫端全是常數 ——
/// `from_static` 因此不會 panic。
fn static_response(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (CONTENT_TYPE, HeaderValue::from_static(content_type)),
            // 契約與瀏覽器資產都會隨版本改變，而開發時改了契約卻看到舊的
            // 是很難察覺的假象。不快取。
            (CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        body,
    )
        .into_response()
}

/// `GET /api/v1/openapi.yaml` —— 權威契約本身。
///
/// media type 依 RFC 9512 用 `application/yaml`。
pub async fn openapi_yaml() -> Response {
    static_response(OPENAPI_YAML, "application/yaml; charset=utf-8")
}

/// `GET /docs` —— 互動式 API 瀏覽器。
pub async fn index() -> Response {
    static_response(INDEX_HTML, "text/html; charset=utf-8")
}

/// `GET /docs/swagger-ui-bundle.js`
pub async fn swagger_ui_js() -> Response {
    static_response(SWAGGER_UI_JS, "text/javascript; charset=utf-8")
}

/// `GET /docs/swagger-ui.css`
pub async fn swagger_ui_css() -> Response {
    static_response(SWAGGER_UI_CSS, "text/css; charset=utf-8")
}
