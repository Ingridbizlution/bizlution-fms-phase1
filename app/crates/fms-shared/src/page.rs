//! cursor 分頁與排序，對齊 `openapi.yaml` 的 `PagedEnvelope`／`PageMeta`／
//! `Cursor`／`Sort` 參數。
//!
//! # 為什麼排序與游標必須一起設計
//!
//! 採 keyset（而非 OFFSET）分頁：OFFSET 在深頁要掃過並丟棄前面所有列，
//! 且併發插入時會漏列或重複列。但 keyset 的代價是**游標必須編碼排序鍵** ——
//! 換一個排序欄位就換一組游標語意。因此游標裡一併記下排序欄位，
//! 並在解碼時比對：客戶端若改了 `sort` 卻沿用舊 `cursor`，
//! 會得到 400 而不是一頁語意錯亂的資料。
//!
//! # 只支援單欄排序，且這是完整而非半套的決定
//!
//! 契約的 `sort` 寫的是「逗號分隔」。本層只接受單一欄位，多欄一律 422。
//! 理由是多欄 keyset 需要游標承載 N 個鍵、SQL 比較子展開成 N 層字典序，
//! 複雜度與實際需求不成比例 —— UI 上的排序幾乎都是「點一個欄位標頭」。
//! 明確拒絕並說明原因，比默默只用第一個欄位誠實。

use base64::Engine;
use serde::Serialize;

use crate::problem::Problem;

/// `PageMeta`
#[derive(Debug, Serialize)]
pub struct PageMeta {
    pub next_cursor: Option<String>,
    pub limit: i64,
    /// 大表上為估算值。本階段一律回 `null`：契約允許，
    /// 且精確 count 需全表掃描，不該是每次列表都付的成本。
    pub total_estimate: Option<i64>,
}

/// `PagedEnvelope`
#[derive(Debug, Serialize)]
pub struct Paged<T> {
    pub data: Vec<T>,
    pub page: PageMeta,
}

/// `limit` 的界線，對齊契約：min 1、max 200、default 50。
pub fn clamp_limit(requested: Option<i64>) -> i64 {
    requested.unwrap_or(50).clamp(1, 200)
}

/// 解析後的排序指示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortSpec {
    /// 已對照白名單，因此可安全用於 SQL 的 CASE 分支比對。
    pub column: String,
    pub desc: bool,
}

impl SortSpec {
    /// 解析 `sort` 參數（`-` 前綴為降冪）。
    ///
    /// `allowed` 是該端點支援的排序欄位白名單；未列入者回 422 並列出可用值 ——
    /// 靜默改用預設排序會讓客戶端以為排序生效。
    pub fn parse(
        raw: Option<&str>,
        allowed: &[&str],
        default_column: &str,
        default_desc: bool,
    ) -> Result<Self, Problem> {
        let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(Self {
                column: default_column.to_string(),
                desc: default_desc,
            });
        };

        if raw.contains(',') {
            return Err(Problem::validation(
                "multi-column `sort` is not supported; specify a single field. \
                 Keyset pagination requires the cursor to encode the sort key, and \
                 multi-column keys would expand the comparison into N-level \
                 lexicographic order for no realistic UI benefit.",
            ));
        }

        let (column, desc) = match raw.strip_prefix('-') {
            Some(rest) => (rest.trim(), true),
            None => (raw, false),
        };

        if !allowed.contains(&column) {
            return Err(Problem::validation(format!(
                "cannot sort by `{column}`; sortable fields: {allowed:?}"
            )));
        }

        Ok(Self {
            column: column.to_string(),
            desc,
        })
    }
}

/// 分頁游標。
///
/// 以 `(排序欄位, 排序鍵, id)` 組成：
///   * 記下**排序欄位**才能在解碼時偵測「換了排序卻沿用舊游標」
///   * 排序鍵以字串承載，讓同一個型別能服務時間戳與文字欄位；
///     呼叫端依排序欄位決定要把它解讀成哪種型別
///   * `id` 是最終破平鍵，缺了它同值列會跳號或重複
#[derive(Debug, Clone)]
pub struct Cursor {
    pub sort_column: String,
    pub key: String,
    /// 破平鍵，以**字串**承載 —— 理由與 `key` 完全相同：
    /// 同一個型別要能服務 uuid 主鍵，也要能服務 `audit_log` 的 bigint 主鍵
    /// （那張表的 PK 是 `(occurred_at, id)`，id 是 bigint）。
    ///
    /// 用 [`Cursor::uuid_id`] 或 [`Cursor::bigint_id`] 取回型別化的值，
    /// 兩者都在格式錯誤時回 400 —— 型別檢查沒有消失，只是移到取用的地方。
    ///
    /// 這個欄位不可能含 `|`：[`Cursor::encode`] 把它放在中間，
    /// 而 `decode` 用 `splitn(3, '|')`，多出來的 `|` 全歸給最後的 `key`。
    pub id: String,
}

impl Cursor {
    /// 編碼為不透明字串。用 base64 而非直接暴露欄位值：
    /// 游標是實作細節，客戶端不該解析或自行構造。
    ///
    /// 欄位順序刻意是 `欄位|id|排序鍵`，把**排序鍵放最後** ——
    /// 文字型排序鍵可能含 `|`，放最後就不需要轉義，解碼也只要切兩刀。
    pub fn encode(&self) -> String {
        let raw = format!("{}|{}|{}", self.sort_column, self.id, self.key);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
    }

    /// 解碼並確認與當前排序一致。
    ///
    /// 格式錯誤或排序不符都回 400 —— 兩者都是客戶端可修正的輸入問題，
    /// 不是伺服器故障。
    pub fn decode(raw: &str, expected_column: &str) -> Result<Self, Problem> {
        let bad = || Problem::bad_request("cursor is not a valid pagination cursor");

        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(raw)
            .map_err(|_| bad())?;
        let text = String::from_utf8(bytes).map_err(|_| bad())?;

        let mut parts = text.splitn(3, '|');
        let sort_column = parts.next().ok_or_else(bad)?.to_string();
        let id = parts.next().ok_or_else(bad)?.to_string();
        let key = parts.next().ok_or_else(bad)?.to_string();
        if id.is_empty() {
            return Err(bad());
        }

        if sort_column != expected_column {
            return Err(Problem::bad_request(format!(
                "cursor was issued for sort `{sort_column}` but the request sorts by \
                 `{expected_column}`; restart pagination without a cursor"
            )));
        }

        Ok(Self {
            sort_column,
            key,
            id,
        })
    }

    /// 把破平鍵解讀為 uuid。絕大多數表的主鍵是 uuid，走這一支。
    pub fn uuid_id(&self) -> Result<uuid::Uuid, Problem> {
        self.id
            .parse()
            .map_err(|_| Problem::bad_request("cursor is not a valid pagination cursor"))
    }

    /// 把破平鍵解讀為 bigint。目前只有 `audit_log`（PK 是 `(occurred_at, id)`）。
    pub fn bigint_id(&self) -> Result<i64, Problem> {
        self.id
            .parse()
            .map_err(|_| Problem::bad_request("cursor is not a valid pagination cursor"))
    }

    /// 把排序鍵解讀為時間戳，供時間欄位的比較使用。
    pub fn as_timestamp(&self) -> Result<chrono::DateTime<chrono::Utc>, Problem> {
        chrono::DateTime::parse_from_rfc3339(&self.key)
            .map(|t| t.with_timezone(&chrono::Utc))
            .map_err(|_| Problem::bad_request("cursor key is not a valid timestamp"))
    }
}

/// 由「多取一列」的結果組出回應。
///
/// 呼叫端應查 `limit + 1` 列：多出來的那列只用來判斷是否還有下一頁，
/// 不回傳給客戶端。這樣就不必額外執行一次 COUNT。
///
/// `cursor_of` 需回傳該列在**當前排序欄位**下的排序鍵字串與 id。
/// 破平鍵對 `D` 泛型（只要求 `Display`），因此既有的 10 個呼叫點
/// （破平鍵都是 `Uuid`）一個字都不用改，而 `audit_log` 可以直接傳 `i64`。
/// 刻意不把它們改成 `String`：那些表的主鍵確實是 uuid，改了不會換到任何東西。
pub fn build<T, F, D>(mut rows: Vec<T>, limit: i64, sort: &SortSpec, cursor_of: F) -> Paged<T>
where
    F: Fn(&T, &str) -> (String, D),
    D: std::fmt::Display,
{
    let has_more = rows.len() as i64 > limit;
    if has_more {
        rows.truncate(limit as usize);
    }

    let next_cursor = if has_more {
        rows.last().map(|r| {
            let (key, id) = cursor_of(r, &sort.column);
            Cursor {
                sort_column: sort.column.clone(),
                key,
                id: id.to_string(),
            }
            .encode()
        })
    } else {
        None
    };

    Paged {
        data: rows,
        page: PageMeta {
            next_cursor,
            limit,
            total_estimate: None,
        },
    }
}
