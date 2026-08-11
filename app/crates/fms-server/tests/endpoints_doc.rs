//! `api/ENDPOINTS.md` 的兩個欄位必須與來源一致。
//!
//! 那份表有 150+ 列、全靠手維護，而它是決定「接下來做哪支端點」的地圖。
//! 盤點時實測到三種漂移：
//!   * 3 列的狀態填錯（已進契約卻標「待補」）
//!   * 5 支**已實作**的 operation 整列不存在（4 支 attachments 與 occupancy）
//!   * 「狀態」單一欄位把「有沒有進契約」與「有沒有實作」混為一談，
//!     因此連「實作了嗎」都表達不出來
//!
//! 前兩種是漂移，第三種是結構問題。結構已改成兩欄，漂移由本檔擋住。
//!
//! 這是同一個模式的第三次應用：`contract_conformance` 讓實作對齊契約，
//! 026 讓 `min_scope_level` 從宣告變成執行，這裡讓文件從宣稱變成可檢查。

use std::collections::BTreeSet;

const DOC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../api/ENDPOINTS.md");
const CONTRACT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../api/openapi.yaml");

const YES: &str = "✔";
const NO: &str = "—";

/// 把 `{reservationId}` 與 `{id}` 視為同一個位置。
///
/// 文件用短名（`/reservations/{id}`）、契約用具名參數
/// （`/reservations/{reservationId}`）。兩邊指的是同一支端點，
/// 而要求文件逐字複製契約的參數名只會讓表更難讀。
fn norm(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut depth = 0usize;
    for c in path.trim().chars() {
        match c {
            '{' => {
                depth += 1;
                if depth == 1 {
                    out.push_str("{}");
                }
            }
            '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// 契約裡宣告的 `(method, 正規化路徑)`。
fn contract_ops() -> BTreeSet<(String, String)> {
    let raw = std::fs::read_to_string(CONTRACT).expect("讀不到 openapi.yaml");
    let doc: serde_yaml::Value = serde_yaml::from_str(&raw).expect("openapi.yaml 不是合法 YAML");
    let mut out = BTreeSet::new();
    for (path, item) in doc["paths"].as_mapping().expect("契約缺少 paths") {
        let path = path.as_str().expect("路徑應為字串");
        for method in ["get", "post", "put", "patch", "delete"] {
            if item.get(method).is_some() {
                out.insert((method.to_string(), norm(path)));
            }
        }
    }
    out
}

fn implemented_ops() -> BTreeSet<(String, String)> {
    fms_server::IMPLEMENTED_OPERATIONS
        .iter()
        .map(|(m, p)| (m.to_string(), norm(p)))
        .collect()
}

/// 表格的一列。`methods` 與 `paths` 都可能是多值
/// （`GET/POST | /resource-blackouts`、`/scim/v2/Users, /scim/v2/Groups`）。
struct Row {
    lineno: usize,
    methods: Vec<String>,
    paths: Vec<String>,
    in_contract: bool,
    implemented: bool,
}

impl Row {
    /// 這一列涵蓋的全部 `(method, 路徑)` 組合。
    fn keys(&self) -> BTreeSet<(String, String)> {
        self.methods
            .iter()
            .flat_map(|m| self.paths.iter().map(move |p| (m.clone(), norm(p))))
            .collect()
    }

    fn label(&self) -> String {
        format!(
            "L{} {} {}",
            self.lineno,
            self.methods.join("/").to_uppercase(),
            self.paths.join(", ")
        )
    }
}

fn parse_rows() -> Vec<Row> {
    let raw = std::fs::read_to_string(DOC).expect("讀不到 ENDPOINTS.md");
    let mut rows = Vec::new();

    for (idx, line) in raw.lines().enumerate() {
        if !line.starts_with('|') {
            continue;
        }
        // 說明欄可能含轉義的 `\|`（例如 `view=flat\|tree`）。先換成一個
        // 不會出現在文件裡的字元再切，否則那一列會被切成 7 格而被忽略 ——
        // 「被忽略」正是最危險的失敗方式：漏檢查不會有任何症狀。
        let safe = line.replace("\\|", "\u{0}");
        let cells: Vec<String> = safe
            .trim()
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().replace('\u{0}', "|"))
            .collect();
        if cells.len() != 6 {
            continue;
        }
        let (methods, paths, contract, implemented) = (&cells[0], &cells[1], &cells[4], &cells[5]);
        if methods == "Method" || methods.chars().all(|c| c == '-') {
            continue;
        }
        // 只認兩個標記；其他內容代表這一列不是端點列。
        if ![YES, NO].contains(&contract.as_str()) || ![YES, NO].contains(&implemented.as_str()) {
            continue;
        }
        rows.push(Row {
            lineno: idx + 1,
            methods: methods
                .split('/')
                .map(|m| m.trim().to_lowercase())
                .collect(),
            paths: paths
                .split(',')
                .map(|p| p.trim().trim_matches('`').to_string())
                .collect(),
            in_contract: contract == YES,
            implemented: implemented == YES,
        });
    }

    // 解析器一旦壞掉（例如日後表格改格式），最糟的結果是「一列都沒解析到、
    // 測試照樣通過」。下界不是裝飾。
    assert!(
        rows.len() >= 140,
        "只解析到 {} 列端點，遠少於預期 —— 解析器與表格格式脫節了",
        rows.len()
    );
    rows
}

/// 同一個 `(method, 路徑)` 不該出現在兩列。
///
/// 這一項是實測需要的。`befb993` 用 perl 改這份文件時把兩列表格接到了
/// 檔案開頭 —— 標題變成
/// `| GET | /facilities/.../service-items | … |# FMS Platform API — …`，
/// 而**那個狀態一路存活到現在**：前兩個測試把列收進 `BTreeSet`，
/// 重複的列不改變集合，因此兩者都通過。
///
/// 那正是最危險的失敗方式：文件壞了、測試說沒事。
#[test]
fn no_operation_is_listed_twice() {
    let mut seen: std::collections::BTreeMap<(String, String), Vec<String>> = Default::default();
    for row in parse_rows() {
        for key in row.keys() {
            seen.entry(key).or_default().push(row.label());
        }
    }
    let dupes: Vec<String> = seen
        .iter()
        .filter(|(_, rows)| rows.len() > 1)
        .map(|((m, p), rows)| {
            format!(
                "  {} {} 出現在 {} 列：{}",
                m.to_uppercase(),
                p,
                rows.len(),
                rows.join("、")
            )
        })
        .collect();
    assert!(
        dupes.is_empty(),
        "ENDPOINTS.md 有重複的端點列：\n{}\n\
         重複通常不是有意的 —— 它是文字處理事故的殘留（見本測試的說明）。",
        dupes.join("\n")
    );
}

/// 檔案必須以標題開頭。
///
/// 上面那次事故留下的是**黏在標題前的兩列**，因此「沒有重複」還不夠 ——
/// 一列被接到標題上時，它同時毀掉標題而且可能不重複。
#[test]
fn the_document_starts_with_its_title() {
    let raw = std::fs::read_to_string(DOC).expect("讀不到 ENDPOINTS.md");
    assert!(
        raw.starts_with("# FMS Platform API"),
        "ENDPOINTS.md 應以標題開頭，實際開頭是：{:?}",
        raw.chars().take(60).collect::<String>()
    );
}

#[test]
fn contract_column_matches_openapi() {
    let contract = contract_ops();
    let mut wrong = Vec::new();
    for row in parse_rows() {
        let declared = row.keys().iter().any(|k| contract.contains(k));
        if declared != row.in_contract {
            wrong.push(format!(
                "  {} — 表格標「契約={}」，openapi.yaml 實際{}",
                row.label(),
                if row.in_contract { YES } else { NO },
                if declared { "有" } else { "沒有" }
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "ENDPOINTS.md 的「契約」欄與 openapi.yaml 不符：\n{}\n\
         契約是權威（ADR-09 紀律 1）：先確認 openapi.yaml 對，再改表格。",
        wrong.join("\n")
    );
}

#[test]
fn implemented_column_matches_the_router() {
    let impl_ops = implemented_ops();
    let mut wrong = Vec::new();
    for row in parse_rows() {
        let built = row.keys().iter().any(|k| impl_ops.contains(k));
        if built != row.implemented {
            wrong.push(format!(
                "  {} — 表格標「實作={}」，IMPLEMENTED_OPERATIONS 實際{}",
                row.label(),
                if row.implemented { YES } else { NO },
                if built { "有" } else { "沒有" }
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "ENDPOINTS.md 的「實作」欄與 router 不符：\n{}\n\
         新增或移除端點時要一併更新這張表 —— 它是決定下一步做什麼的地圖。",
        wrong.join("\n")
    );
}

/// 表格必須是**完整**盤點：契約有的、實作有的，都要在表上找得到。
///
/// 這一項抓的是最容易發生也最沒有症狀的漂移：新增一支端點卻忘記加列。
/// 盤點當時就有 5 支已實作的 operation 整列不存在。
#[test]
fn every_contract_and_implemented_operation_appears_in_the_table() {
    let rows = parse_rows();
    let covered: BTreeSet<(String, String)> = rows.iter().flat_map(|r| r.keys()).collect();

    let mut missing: Vec<String> = contract_ops()
        .difference(&covered)
        .map(|(m, p)| format!("  契約有但表格沒有：{} {p}", m.to_uppercase()))
        .collect();
    missing.extend(
        implemented_ops()
            .difference(&covered)
            .map(|(m, p)| format!("  已實作但表格沒有：{} {p}", m.to_uppercase())),
    );

    assert!(
        missing.is_empty(),
        "ENDPOINTS.md 不再是完整盤點：\n{}",
        missing.join("\n")
    );
}
