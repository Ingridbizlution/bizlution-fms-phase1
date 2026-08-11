//! 報表匯出的產檔（`report_export.requested` 的 handler）。
//!
//! # 這個檔案唯一真正困難的地方，與 audit_export 完全相同
//!
//! relay 跑在**平台情境**下 —— 它必須跨租戶取用 `event_outbox`。
//! 六支報表函式都是 `SECURITY INVOKER`，收斂靠的是**呼叫者的情境**：
//! `is_platform_context()` 為真時，每一張底層表的 `tenant_isolation` 與
//! `facility_scope` 第一個 OR 分支就成立，產出的檔案會是整個資料庫。
//!
//! 因此 [`write_file`] **一定**先把情境切成 `requested_by` 的，兩步：
//!
//! 1. `fms.set_context(tenant, requested_by, false)` —— 第三個參數 `false`
//!    同時把 `app.is_platform` 寫成 `'off'`（001／013 的實作），所以
//!    **不需要**再單獨呼叫一次 `set_config('app.is_platform','off')`。
//!    量過：把那一行拿掉，`d_` 仍然通過 —— 因為它本來就沒有作用。
//!    （`audit_export.rs` 有那一行，而它的檔頭把三步都寫成必要的。那一行
//!    無害，但「必要」的說法不成立。留在那裡沒改，是因為改別的功能的
//!    程式碼不屬於這個變更；記在這裡讓下一個讀的人知道。）
//! 2. `set_config('app.facility_ids', <他能存取的場域>, true)`
//!
//! **第 2 步是真的不能省的那一步。** `current_facility_ids()` 是 NULL 時
//! `facility_in_scope()` 一律放行 —— 那會讓場域收斂**看起來**有做，
//! 實際沒做，而且沒有任何錯誤。`report_export_slice.rs` 的 `d_` 專門盯這個：
//! 場域受限的發起者匯出的檔案裡不能出現別的場域的列。
//! （拿掉第 2 步，`d_` 立刻失敗 —— 量過。）
//!
//! # 表頭的順序取自資料庫
//!
//! `pg_proc.proargnames` 配上 `proargmodes = 't'`（TABLE 欄位）就是
//! `RETURNS TABLE` 的宣告順序。手抄一份會在某次改欄位之後**安靜地錯位**
//! —— CSV 除了表頭沒有任何校驗，錯位的檔案看起來完全正常，而它會被拿去
//! 對帳。所以這裡不留任何手寫的欄位清單。
//!
//! # 幂等
//!
//! relay 保證至少一次投遞。`produce` 只處理 `PENDING` 與 `RUNNING`；
//! `COMPLETED` 直接回成功而不重做。物件鍵用 export id，所以重做會覆寫同一個
//! 物件而不是留下垃圾 —— 與 audit_export 同一個做法。

use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

/// 與發送端共用的事件型別。
///
/// 刻意不從 `fms-report` import：那會讓 worker 依賴一個 HTTP 層的 crate。
/// 兩邊各自宣告、由 `report_export_slice.rs` 的一格斷言它們相等 ——
/// 「宣告了但沒有人比對」正是這個專案反覆出現的缺陷類型。
pub const EVENT_TYPE: &str = "report_export.requested";

/// 報表代碼 → 函式名與參數。**必須與 `fms_report::export::REPORTS` 一致**，
/// 由 `report_export_slice.rs` 的 `a_` 逐一比對。
///
/// 這裡重複一份而不是共用，理由與 `EVENT_TYPE` 相同（crate 方向）；
/// 而「重複但沒有人比對」才是問題，所以那一格不能省。
/// `(請求體的鍵, 函式的參數名, SQL 轉型)`。
type ExtraParam = (&'static str, &'static str, &'static str);
/// `(報表代碼, 函式名, 額外參數)`。
type Spec = (&'static str, &'static str, &'static [ExtraParam]);

const SPECS: &[Spec] = &[
    (
        "sla-compliance",
        "report_sla_compliance",
        &[
            ("group_by", "p_group_by", "text"),
            ("strictness", "p_strictness", "text"),
        ],
    ),
    (
        "pm-compliance",
        "report_pm_compliance",
        &[
            ("group_by", "p_group_by", "text"),
            ("grace_days", "p_grace_override", "int"),
        ],
    ),
    (
        "group-rollup",
        "report_group_rollup",
        &[("subtree_of", "p_subtree_of", "uuid")],
    ),
    (
        "asset-reliability",
        "report_asset_reliability",
        &[
            ("facility_id", "p_facility_id", "uuid"),
            ("limit", "p_limit", "int"),
        ],
    ),
    (
        "space-utilization",
        "report_space_utilization",
        &[("facility_id", "p_facility_id", "uuid")],
    ),
    (
        "service-volume",
        "report_service_volume",
        &[("group_by", "p_group_by", "text")],
    ),
];

pub struct ReportExportHandler {
    pool: sqlx::PgPool,
    storage: fms_shared::Storage,
}

impl ReportExportHandler {
    pub fn new(pool: sqlx::PgPool, storage: fms_shared::Storage) -> Self {
        Self { pool, storage }
    }

    /// 產出一份匯出。回傳寫入的列數。
    ///
    /// **作業表的三次存取都自己開平台情境交易。** `begin_platform_tx` 把
    /// `app.is_platform` 設在交易層級，而 relay 的那個交易在另一條連線上。
    /// audit_export 記過這個失效：UPDATE 影響 0 列、`fetch_optional` 回 None、
    /// 於是走「已完成」那條路回 Ok(0)，作業永遠停在 PENDING 而 relay
    /// 認為它成功了。
    pub async fn produce(&self, export_id: Uuid) -> Result<i64, String> {
        let mut tx = crate::begin_platform_tx(&self.pool)
            .await
            .map_err(|e| format!("開啟平台情境交易失敗：{e}"))?;
        let job = sqlx::query(
            "UPDATE fms.report_exports
                SET status = 'RUNNING', started_at = coalesce(started_at, clock_timestamp())
              WHERE id = $1 AND status IN ('PENDING','RUNNING')
              RETURNING tenant_id, requested_by, report_code, format, params",
        )
        .bind(export_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("讀取匯出作業失敗：{e}"))?;

        let Some(job) = job else {
            // 已經 COMPLETED（重放）或作業不見了 —— 兩種都不該讓 relay 重試。
            let _ = tx.rollback().await;
            return Ok(0);
        };
        let tenant_id: Uuid = job.get("tenant_id");
        let requested_by: Uuid = job.get("requested_by");
        let report_code: String = job.get("report_code");
        let format: String = job.get("format");
        let params: serde_json::Value = job.get("params");
        // 先 commit「已認領」再產檔：產檔可能很久，把作業表鎖在長交易裡
        // 會擋住輪詢的讀取。
        tx.commit()
            .await
            .map_err(|e| format!("提交 RUNNING 狀態失敗：{e}"))?;

        match self
            .write_file(
                export_id,
                tenant_id,
                requested_by,
                &report_code,
                &format,
                &params,
            )
            .await
        {
            Ok(n) => Ok(n),
            Err(e) => {
                // 失敗要落地，否則客戶端輪詢到的永遠是 RUNNING ——
                // 「還在跑」與「早就死了」看起來一樣。
                if let Ok(mut tx) = crate::begin_platform_tx(&self.pool).await {
                    let _ = sqlx::query(
                        "UPDATE fms.report_exports
                            SET status = 'FAILED', error = $2, completed_at = clock_timestamp()
                          WHERE id = $1",
                    )
                    .bind(export_id)
                    .bind(&e)
                    .execute(&mut *tx)
                    .await;
                    let _ = tx.commit().await;
                }
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_file(
        &self,
        export_id: Uuid,
        tenant_id: Uuid,
        requested_by: Uuid,
        report_code: &str,
        format: &str,
        params: &serde_json::Value,
    ) -> Result<i64, String> {
        let (_, function, extras) = SPECS
            .iter()
            .find(|(code, _, _)| *code == report_code)
            .ok_or_else(|| {
                format!("`{report_code}` 不是可匯出的報表 —— worker 與 API 的清單分歧了")
            })?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("開啟交易失敗：{e}"))?;
        inject_requester_context(&mut tx, tenant_id, requested_by).await?;

        // --- 表頭：順序取自資料庫，見模組檔頭 -----------------------------
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT p.proargnames[i]
               FROM pg_proc p, generate_subscripts(p.proargnames, 1) i
              WHERE p.proname = $1 AND p.pronamespace = 'fms'::regnamespace
                AND p.proargmodes[i] = 't'
              ORDER BY i",
        )
        .bind(function)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| format!("讀取 {function} 的欄位順序失敗：{e}"))?;
        if columns.is_empty() {
            return Err(format!(
                "fms.{function} 沒有 TABLE 欄位 —— 函式不存在或簽章變了"
            ));
        }

        // --- 查詢：具名記號呼叫，所以參數順序不會錯 -----------------------
        // 只帶 `params` 裡真的有的鍵，其餘走函式的預設值。
        let mut binds: Vec<(&str, &str, serde_json::Value)> = Vec::new();
        for (key, arg, cast) in extras.iter() {
            if let Some(v) = params.get(key) {
                if !v.is_null() {
                    binds.push((arg, cast, v.clone()));
                }
            }
        }
        let mut call = format!("fms.{function}(p_from => $1::date, p_to => $2::date");
        for (i, (arg, cast, _)) in binds.iter().enumerate() {
            call.push_str(&format!(", {arg} => ${}::{cast}", i + 3));
        }
        call.push(')');
        let sql = format!("SELECT to_jsonb(t) AS row FROM {call} t");

        let from = json_str(params.get("from")).ok_or("params 少了 from")?;
        let to = json_str(params.get("to")).ok_or("params 少了 to")?;
        let mut q = sqlx::query(&sql).bind(from).bind(to);
        for (_, cast, v) in &binds {
            // 一律綁 text 再讓 SQL 轉型：`params` 是 jsonb，數字與字串在那裡
            // 都是 JSON 值，而 `$n::int` 收 text 完全合法。省掉一層 match
            // 也就省掉「漏掉一種型別」的機會。
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let _ = cast;
            q = q.bind(s);
        }
        let rows = q
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| format!("執行 {report_code} 失敗：{e}"))?;

        // 唯讀查詢，rollback 即可 —— 不留任何狀態。
        let _ = tx.rollback().await;

        let cells: Vec<Vec<String>> = rows
            .iter()
            .map(|r| {
                let obj: serde_json::Value = r.get("row");
                columns
                    .iter()
                    .map(|c| render(obj.get(c.as_str())))
                    .collect()
            })
            .collect();

        let (bytes, content_type) = match format {
            "xlsx" => (
                xlsx(report_code, &columns, &cells)?,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            _ => (csv(&columns, &cells), "text/csv; charset=utf-8"),
        };

        // 物件鍵用 export id：重放會覆寫同一個物件，不會留下垃圾。
        let key = format!("report-exports/{tenant_id}/{export_id}.{format}");
        self.storage
            .put(&key, bytes, Some(content_type))
            .await
            .map_err(|e| format!("上傳匯出檔失敗：{e}"))?;

        let n = cells.len() as i64;
        let mut done = crate::begin_platform_tx(&self.pool)
            .await
            .map_err(|e| format!("開啟平台情境交易失敗：{e}"))?;
        sqlx::query(
            "UPDATE fms.report_exports
                SET status = 'COMPLETED', object_key = $2, row_count = $3,
                    error = NULL, completed_at = clock_timestamp()
              WHERE id = $1",
        )
        .bind(export_id)
        .bind(&key)
        .bind(n)
        .execute(&mut *done)
        .await
        .map_err(|e| format!("回寫匯出結果失敗：{e}"))?;
        done.commit()
            .await
            .map_err(|e| format!("提交匯出結果失敗：{e}"))?;

        Ok(n)
    }
}

/// 把交易的情境切成發起者的。兩步，見模組檔頭 —— 其中場域那一步是
/// 唯一真的不能少的。
async fn inject_requester_context(
    tx: &mut Transaction<'static, Postgres>,
    tenant_id: Uuid,
    requested_by: Uuid,
) -> Result<(), String> {
    // 第三個參數 `false` 就是「關掉平台旁路」——`set_context` 自己會把
    // `app.is_platform` 寫成 `'off'`。
    sqlx::query("SELECT fms.set_context($1, $2, false)")
        .bind(tenant_id)
        .bind(requested_by)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("注入發起者情境失敗：{e}"))?;
    let facilities: Vec<Uuid> =
        sqlx::query_scalar("SELECT facility_id FROM fms.user_accessible_facilities($1)")
            .bind(requested_by)
            .fetch_all(&mut **tx)
            .await
            .map_err(|e| format!("取得可存取場域失敗：{e}"))?;
    // 與 `begin_tenant_tx` 的 `set_facility_scope` 同一份邏輯：空清單寫成
    // 全零 uuid 哨兵，而不是空字串 —— 空字串會讓 `current_facility_ids()`
    // 變成 NULL，而那等於「不限制」。
    let ids = if facilities.is_empty() {
        "00000000-0000-0000-0000-000000000000".to_string()
    } else {
        facilities
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    sqlx::query("SELECT set_config('app.facility_ids', $1, true)")
        .bind(&ids)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("設定場域範圍失敗：{e}"))?;
    Ok(())
}

fn json_str(v: Option<&serde_json::Value>) -> Option<String> {
    match v {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
        None => None,
    }
}

/// jsonb 值 → 儲存格文字。
///
/// **`null` render 成空字串而不是 `"null"`。** 報表的 null 是有意義的
/// （「分母為 0，算不出來」），而 `"null"` 這個字串在試算表裡會變成一個
/// 看起來像資料的值 —— 空白至少讀得出「這裡沒有數字」。
fn render(v: Option<&serde_json::Value>) -> String {
    match v {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        // 巢狀的（`skip_reasons` 是 jsonb）原樣輸出 JSON —— 攤平成多欄
        // 需要知道有哪些鍵，而那組鍵是資料而不是結構。
        Some(other) => other.to_string(),
    }
}

fn csv(columns: &[String], cells: &[Vec<String>]) -> Vec<u8> {
    let mut out = String::with_capacity(columns.len() * 16 + cells.len() * 160);
    out.push_str(
        &columns
            .iter()
            .map(|c| quote(c))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push('\n');
    for row in cells {
        out.push_str(&row.iter().map(|c| quote(c)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    out.into_bytes()
}

/// RFC 4180：一律加引號。與 audit_export 的 `quote` 同一個判斷 ——
/// 判斷「這一個需不需要」比一律加更容易寫錯，而多餘的引號對所有 CSV
/// 讀取器都合法。
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// xlsx。契約寫的是「匯出 xlsx/csv」，兩種都做 ——
/// 只做一種是把契約悄悄縮小。
fn xlsx(report_code: &str, columns: &[String], cells: &[Vec<String>]) -> Result<Vec<u8>, String> {
    use rust_xlsxwriter::{Format, Workbook};

    let mut book = Workbook::new();
    let sheet = book.add_worksheet();
    // 工作表名稱不能含 `[]:*?/\` 也不能超過 31 字 —— 報表代碼是
    // `^[a-z][a-z0-9-]{2,39}$`，連字號合法但長度要截。
    let name: String = report_code.chars().take(31).collect();
    sheet
        .set_name(&name)
        .map_err(|e| format!("設定工作表名稱失敗：{e}"))?;

    let bold = Format::new().set_bold();
    for (c, col) in columns.iter().enumerate() {
        sheet
            .write_string_with_format(0, c as u16, col, &bold)
            .map_err(|e| format!("寫入表頭失敗：{e}"))?;
    }
    for (r, row) in cells.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            // **數字寫成數字**：寫成字串的話試算表無法加總，
            // 而這份檔案的用途正是拿去加總。空字串保持空白（見 `render`）。
            match cell.parse::<f64>() {
                Ok(n) if !cell.is_empty() => sheet
                    .write_number((r + 1) as u32, c as u16, n)
                    .map_err(|e| format!("寫入數字失敗：{e}"))?,
                _ => sheet
                    .write_string((r + 1) as u32, c as u16, cell)
                    .map_err(|e| format!("寫入儲存格失敗：{e}"))?,
            };
        }
    }
    book.save_to_buffer()
        .map_err(|e| format!("產生 xlsx 失敗：{e}"))
}

impl crate::EventHandler for ReportExportHandler {
    fn handles(&self, event_type: &str) -> bool {
        event_type == EVENT_TYPE
    }

    async fn handle(&self, event: &crate::OutboxEvent) -> Result<(), String> {
        let id = event
            .payload
            .get("export_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| format!("事件 {} 的 payload 沒有可用的 export_id", event.id))?;

        let n = self.produce(id).await?;
        tracing::info!(event_id = event.id, export_id = %id, rows = n, "報表匯出完成");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `null` 是空白而不是 `"null"`。見 `render` 的檔頭。
    #[test]
    fn null_renders_blank_not_the_word_null() {
        assert_eq!(render(None), "");
        assert_eq!(render(Some(&serde_json::Value::Null)), "");
        assert_eq!(render(Some(&serde_json::json!(0))), "0");
    }

    /// CSV 的引號成對。
    #[test]
    fn quotes_are_doubled() {
        assert_eq!(quote("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote("a,b"), "\"a,b\"");
    }

    /// xlsx 產得出來，而且是一個 zip（`PK\x03\x04`）。
    #[test]
    fn xlsx_is_a_zip_container() {
        let cols = vec!["group_key".to_string(), "requests".to_string()];
        let cells = vec![vec!["a".to_string(), "3".to_string()]];
        let bytes = xlsx("service-volume", &cols, &cells).expect("xlsx");
        assert_eq!(&bytes[0..4], b"PK\x03\x04", "xlsx 應該是 zip 容器");
    }
}
