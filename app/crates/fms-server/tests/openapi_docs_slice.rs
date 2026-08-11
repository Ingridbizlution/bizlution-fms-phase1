//! `GET /api/v1/openapi.yaml` 與 `GET /docs`，外加兩個契約結構守衛。
//!
//! # 這裡**不**重做的事
//!
//! `endpoints_doc.rs` 已經用 `serde_yaml` 解析契約並比對「路徑 × 方法」的
//! 集合（見該檔 `contract_ops()`）。因此 **YAML 語法錯誤已經有守衛**：
//! 解析失敗會讓那三格一起紅。這裡只補它沒做的兩件事。
//!
//! # 補的兩件事，以及它們各自的症狀
//!
//! 1. **`operationId` 存在且不重複。** 重複的 operationId 會讓產生出來的
//!    client SDK 有兩個同名函式 —— 而依產生器不同，症狀從編譯錯誤到
//!    「其中一支靜靜地覆蓋另一支」都有。
//! 2. **所有 `$ref` 解得開。** 指向不存在的 schema 目前不會被任何測試抓到。
//!    它在瀏覽器裡的症狀是**某一段 schema 顯示不出來**，不是報錯 ——
//!    所以沒有人會發現。
//!
//! # 下界不是裝飾
//!
//! 「全部解得開」在一份空文件上也成立；「沒有重複」在零個 operation 上
//! 也成立。走訪器一旦壞掉（例如日後改用別的 YAML 型別），最糟的結果是
//! 「一個節點都沒走到、測試照樣通過」。因此每一格都帶一個數量下界。
//!
//! 下界取自實測（2026-08-02，PR #16 之後：66 paths／93 operations／329 refs），
//! 往下留了餘裕。**規格書 §1.3 寫的「168 列端點、下界取 100」是錯的** ——
//! 168 是 `ENDPOINTS.md` 的表格列數（含尚未進契約的規劃項），不是
//! `openapi.yaml` 的 path 數。用 100 當下界會讓這一格從第一天就是紅的。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use http_body_util::BodyExt;
use std::collections::BTreeMap;

/// 以 `CARGO_MANIFEST_DIR` 組出絕對路徑：測試的工作目錄是 crate 根。
/// 與 `contract_conformance.rs`／`endpoints_doc.rs` 用的是同一份檔案。
const CONTRACT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../api/openapi.yaml");

const METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

fn load_contract() -> serde_yaml::Value {
    let raw = std::fs::read_to_string(CONTRACT).expect("讀不到 openapi.yaml");
    serde_yaml::from_str(&raw).expect("openapi.yaml 不是合法 YAML")
}

fn paths_of(doc: &serde_yaml::Value) -> &serde_yaml::Mapping {
    doc["paths"].as_mapping().expect("契約缺少 paths")
}

// ---------------------------------------------------------------------------
// 契約結構守衛（不需要資料庫）
// ---------------------------------------------------------------------------

#[test]
fn every_operation_has_a_unique_operation_id() {
    let doc = load_contract();
    let paths = paths_of(&doc);

    let mut missing = Vec::new();
    // operationId -> 用到它的 `METHOD /path`。用 BTreeMap 讓失敗訊息穩定排序。
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (path, item) in paths {
        let path = path.as_str().expect("路徑應為字串");
        for method in METHODS {
            let Some(op) = item.get(method) else { continue };
            let label = format!("{} {path}", method.to_uppercase());
            match op.get("operationId").and_then(|v| v.as_str()) {
                Some(id) => seen.entry(id.to_string()).or_default().push(label),
                None => missing.push(label),
            }
        }
    }

    assert!(
        missing.is_empty(),
        "契約有 {} 支 operation 沒有 operationId：\n{}\n\
         前端由這份契約產生 client，沒有 operationId 的 operation 會拿到\
         一個由路徑硬湊出來的函式名，而那個名字會隨路徑改動而變。",
        missing.len(),
        missing.join("\n  ")
    );

    let dupes: Vec<String> = seen
        .iter()
        .filter(|(_, uses)| uses.len() > 1)
        .map(|(id, uses)| format!("  {id} 用在 {} 處：{}", uses.len(), uses.join("、")))
        .collect();
    assert!(
        dupes.is_empty(),
        "契約有重複的 operationId：\n{}\n\
         產生出來的 client SDK 會有兩個同名函式 —— 依產生器不同，\
         症狀從編譯錯誤到「其中一支靜靜地覆蓋另一支」都有。",
        dupes.join("\n")
    );

    // 走訪器壞掉時最糟的結果是「什麼都沒走到、測試通過」。
    assert!(
        seen.len() >= 85,
        "只走訪到 {} 個 operationId（撰寫時實測 93，契約只會長不會縮）—— \
         契約被截斷了，或這個走訪器與契約的結構脫節了。",
        seen.len()
    );
}

#[test]
fn every_ref_resolves() {
    let doc = load_contract();

    // 收集全文件的 `$ref`，連同它出現的位置 —— 失敗訊息要說得出「哪裡壞了」，
    // 只報 ref 字串的話，同一個 schema 被引用 20 次時無從下手。
    let mut refs: Vec<(String, String)> = Vec::new();
    collect_refs(&doc, "#".to_string(), &mut refs);

    let mut broken = Vec::new();
    for (target, site) in &refs {
        if resolve_pointer(&doc, target).is_none() {
            broken.push(format!("  {target}\n    被引用於 {site}"));
        }
    }
    assert!(
        broken.is_empty(),
        "契約有 {} 個解不開的 $ref：\n{}\n\
         這種錯誤在瀏覽器裡的症狀是**那一段 schema 顯示不出來**（不是報錯），\
         所以沒有守衛就不會有人發現。",
        broken.len(),
        broken.join("\n")
    );

    assert!(
        refs.len() >= 300,
        "只找到 {} 個 $ref（撰寫時實測 329）—— \
         走訪器沒有走遍整份文件，「全部解得開」因此不代表任何事。",
        refs.len()
    );
}

/// 遞迴收集 `$ref`。`site` 是目前節點的 JSON pointer，用於失敗訊息。
fn collect_refs(node: &serde_yaml::Value, site: String, out: &mut Vec<(String, String)>) {
    match node {
        serde_yaml::Value::Mapping(map) => {
            for (key, value) in map {
                let key = key.as_str().unwrap_or("<non-string-key>");
                match (key, value.as_str()) {
                    ("$ref", Some(target)) => out.push((target.to_string(), site.clone())),
                    _ => collect_refs(value, format!("{site}/{key}"), out),
                }
            }
        }
        serde_yaml::Value::Sequence(items) => {
            for (i, item) in items.iter().enumerate() {
                collect_refs(item, format!("{site}/{i}"), out);
            }
        }
        _ => {}
    }
}

/// 依 RFC 6901 解析文件內部指標。
///
/// 只接受 `#/…`：契約現在全部是內部參照，而外部參照（另一個檔案／網址）
/// 會讓「契約是一份檔案」不再成立，也會讓瀏覽器需要外網。
/// 因此外部參照在這裡一律視為解不開，讓它必須是一個**有意識的**決定。
fn resolve_pointer<'a>(
    doc: &'a serde_yaml::Value,
    reference: &str,
) -> Option<&'a serde_yaml::Value> {
    let pointer = reference.strip_prefix("#/")?;
    let mut cur = doc;
    for token in pointer.split('/') {
        // RFC 6901 的轉義：`~1` 是 `/`、`~0` 是 `~`。契約的路徑鍵含 `/`，
        // 所以 `#/paths/~1work-orders` 這種寫法是合法的。
        let token = token.replace("~1", "/").replace("~0", "~");
        cur = match cur {
            serde_yaml::Value::Mapping(map) => map.get(serde_yaml::Value::String(token))?,
            serde_yaml::Value::Sequence(items) => items.get(token.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

// ---------------------------------------------------------------------------
// 服務端點（需要資料庫：router 由 `build_router` 建出，帶著真正的 state）
// ---------------------------------------------------------------------------

/// 取回 `(狀態碼, content-type, body 原文)`。
///
/// 不能用 `ctx.send()`：那個會把 body 當 JSON 解析，而這裡要的是 YAML 與
/// HTML 的**原始位元組** —— 位元組相同正是這一格要守的東西。
async fn get_text(ctx: &TestContext, uri: &str) -> (StatusCode, String, String) {
    let res = ctx
        .send_raw(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await;
    let status = res.status();
    let content_type = res
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("body 應為 UTF-8");
    (status, content_type, body)
}

/// 服務出去的規格必須與磁碟上那份**是同一份**，不是它的副本。
#[tokio::test]
async fn the_served_contract_is_the_file_on_disk() {
    let ctx = TestContext::setup().await;

    let (status, content_type, body) = get_text(&ctx, "/api/v1/openapi.yaml").await;
    assert_eq!(status, StatusCode::OK, "GET /api/v1/openapi.yaml 應回 200");
    assert!(
        content_type.starts_with("application/yaml"),
        "content-type 應為 application/yaml（RFC 9512），實際是 {content_type:?} —— \
         型別錯了瀏覽器會拿它當純文字，規格畫不出來。"
    );

    let on_disk = std::fs::read_to_string(CONTRACT).expect("讀不到 openapi.yaml");
    assert_eq!(
        body, on_disk,
        "服務出去的規格與 api/openapi.yaml 不是同一份位元組。\n\
         ADR-09 紀律 1 是「契約是權威」：`docs.rs` 必須用 include_str! 直接嵌入\
         那個檔案，不能複製內容、也不能在回應前加工。"
    );

    // 位元組相同已經涵蓋內容，這一段守的是另一件事：**服務出去的是 YAML**。
    // 若哪天有人在中間加了壓縮或編碼，位元組比對會先失敗；但若是換成
    // 「等價但不是 YAML」的東西（例如加了 BOM），只有解析得到才會發現。
    let served: serde_yaml::Value =
        serde_yaml::from_str(&body).expect("服務出去的內容不是合法 YAML");
    assert!(
        paths_of(&served).len() >= 60,
        "服務出去的規格只有 {} 個 path（撰寫時實測 66）—— 規格被截斷了。",
        paths_of(&served).len()
    );

    ctx.teardown().await;
}

/// 瀏覽器頁面打得開，而且**離網環境也打得開**。
#[tokio::test]
async fn the_docs_page_mounts_the_browser_without_external_requests() {
    let ctx = TestContext::setup().await;

    let (status, content_type, html) = get_text(&ctx, "/docs").await;
    assert_eq!(status, StatusCode::OK, "GET /docs 應回 200");
    assert!(
        content_type.starts_with("text/html"),
        "content-type 應為 text/html，實際是 {content_type:?}"
    );

    // 掛載點。Swagger UI 把整個介面畫進這個節點，少了它頁面是空白的。
    assert!(
        html.contains(r#"id="swagger-ui""#),
        "/docs 的 body 缺少瀏覽器的掛載點 `id=\"swagger-ui\"`"
    );
    assert!(
        html.contains("SwaggerUIBundle("),
        "/docs 沒有初始化 Swagger UI —— 掛載點在但沒有人畫進去，頁面會是空白的"
    );
    assert!(
        html.contains("url: '/api/v1/openapi.yaml'"),
        "/docs 應讀 /api/v1/openapi.yaml —— 指向別處就等於在展示另一份契約"
    );

    // 唯一會發出外部請求的 Swagger UI 內建功能。
    assert!(
        html.contains("validatorUrl: null"),
        "/docs 沒有關掉 validator 徽章。Swagger UI 預設會載入 \
         https://validator.swagger.io/validator?url=… —— 那在離網環境是壞的，\
         而且會把契約網址送給第三方。"
    );

    // 每一個子資源都必須是同源相對路徑。`//host/x` 是 scheme-relative 的
    // 外部網址，長得很像相對路徑 —— 這是這個檢查最容易漏掉的形狀。
    let external: Vec<String> = subresource_urls(&html)
        .into_iter()
        .filter(|u| !(u.starts_with('/') && !u.starts_with("//")))
        .collect();
    assert!(
        external.is_empty(),
        "/docs 引用了非同源的資源：{external:?}\n\
         這是地端產品，客戶環境可能沒有外網。症狀會是「頁面開得起來、\
         內容不出現」—— 看起來像伺服器壞了。資產要 vendored\
         （見 api/vendor/swagger-ui/README.md）。"
    );

    // 兩個 vendored 資產真的服務得出來。掛載點在、初始化在，但 JS 404 的話
    // 頁面一樣是空白的 —— 而上面每一項都會通過。
    let (status, content_type, js) = get_text(&ctx, "/docs/swagger-ui-bundle.js").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "GET /docs/swagger-ui-bundle.js 應回 200"
    );
    assert!(
        content_type.starts_with("text/javascript"),
        "swagger-ui-bundle.js 的 content-type 是 {content_type:?}；\
         型別不對瀏覽器會拒絕執行它"
    );
    assert!(
        js.contains("SwaggerUIBundle"),
        "服務出去的 JS 沒有定義 SwaggerUIBundle —— vendored 的檔案不對"
    );

    let (status, content_type, css) = get_text(&ctx, "/docs/swagger-ui.css").await;
    assert_eq!(status, StatusCode::OK, "GET /docs/swagger-ui.css 應回 200");
    assert!(
        content_type.starts_with("text/css"),
        "swagger-ui.css 的 content-type 是 {content_type:?}"
    );
    // CSS 是升級 vendored 資產時最容易把外部相依帶回來的地方
    // （webfont 的 @import、CDN 上的圖）。上游 5.32.11 的圖全是 data: URI。
    assert!(
        !css.contains("url(http") && !css.contains("@import"),
        "swagger-ui.css 引用了外部資源（url(http… 或 @import）—— \
         升級 vendored 資產時把 CDN 相依帶回來了，離網環境會壞"
    );

    ctx.teardown().await;
}

/// 取出 HTML 裡所有 `src=` 與 `href=` 的值。
///
/// 刻意不引 HTML 解析器：要檢查的是我們自己寫的、形狀固定的一頁，
/// 為它加一個依賴不划算。**但也因此它只認雙引號** —— 頁面若改成單引號，
/// 這個檢查會靜靜地什麼都找不到，所以下面有一個數量下界。
fn subresource_urls(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for attr in ["src=\"", "href=\""] {
        let mut rest = html;
        while let Some(i) = rest.find(attr) {
            rest = &rest[i + attr.len()..];
            match rest.find('"') {
                Some(end) => {
                    out.push(rest[..end].to_string());
                    rest = &rest[end..];
                }
                None => break,
            }
        }
    }
    // 頁面現在有 2 個：CSS 與 JS。找不到就是這個擷取器脫節了 ——
    // 而「什麼都沒找到」會讓上面的外部資源檢查變成一句空話。
    assert!(
        out.len() >= 2,
        "只從 /docs 擷取到 {} 個子資源網址（預期至少 2：CSS 與 JS）—— \
         這個擷取器與頁面的寫法脫節了，外部資源檢查已經形同虛設。",
        out.len()
    );
    out
}
