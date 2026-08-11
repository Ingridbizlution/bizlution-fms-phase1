//! 資產的資料存取。
//!
//! 刻意的分工：
//!   * `category_code` ↔ `category_id` 的換算由這一層負責，
//!     因為那是儲存細節；契約對外只有 code。
//!   * `subtree_of_node` 用 ltree 的 `<@`，讓資料庫做子樹判定，
//!     不在應用層遞迴查詢。運算子寫成 `OPERATOR(public.<@)`：
//!     擴充安裝在 public 而物件在 fms（見 migration 001 的註解）。
//!   * `version` 由 `trg_bump_version` 維護，不在應用層加一。

use uuid::Uuid;

use fms_shared::{Problem, TenantTx};

pub struct AssetRow {
    pub id: Uuid,
    pub facility_id: Uuid,
    pub spatial_node_id: Option<Uuid>,
    pub spatial_node_path: Option<String>,
    pub asset_code: String,
    pub name: String,
    pub serial_no: Option<String>,
    pub category_code: String,
    pub asset_model_id: Option<Uuid>,
    pub parent_asset_id: Option<Uuid>,
    pub criticality: String,
    pub status: String,
    pub install_date: Option<chrono::NaiveDate>,
    pub warranty_end_date: Option<chrono::NaiveDate>,
    pub health_score: Option<f64>,
    pub last_telemetry_at: Option<chrono::DateTime<chrono::Utc>>,
    pub open_work_order_count: i64,
    pub active_alarm_count: i64,
    pub specifications: serde_json::Value,
    pub attributes: serde_json::Value,
    pub version: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl AssetRow {
    /// 該列在指定排序欄位下的游標鍵。必須與查詢的 ORDER BY 一致，
    /// 否則翻頁會從錯誤的位置繼續。
    pub fn cursor_key(&self, sort_column: &str) -> (String, Uuid) {
        let key = match sort_column {
            "asset_code" => self.asset_code.clone(),
            "name" => self.name.clone(),
            // created_at 與預設值：以 RFC3339 承載
            _ => self.created_at.to_rfc3339(),
        };
        (key, self.id)
    }
}

// SELECT 主體在 list 與 get 中重複書寫：`query_as!` 的第一個參數必須是
// 字串字面值，抽成常數或巨集會讓編譯期驗證失效。

/// 依條件列出資產。RLS 已限定租戶，因此無需 tenant_id 條件。
///
/// # 動態排序如何與 `query_as!` 共存
///
/// `query_as!` 要求 SQL 是字串字面值，因此不能拼接 ORDER BY。做法是把
/// 白名單內的每個「欄位 × 方向」組合寫成 ORDER BY 中的一個 CASE 運算式：
/// 未被選中的分支對**所有列**都回 NULL，因此全部同值、不影響排序，
/// 由下一個運算式決定；被選中的那一支才產生實際值。
///
/// keyset 的比較子同理用 CASE 分派，並依方向在 `<` 與 `>` 之間切換。
/// 破平鍵 `a.id` 的方向必須跟隨主排序方向，否則翻頁會漏列或重複。
///
/// 代價是這段 SQL 隨可排序欄位數線性變長。這是 sqlx 編譯期驗證與動態查詢
/// 之間的真實張力；選擇留住驗證（ADR-09 的主要理由），付出冗長。
#[allow(clippy::too_many_arguments)]
pub async fn list(
    tx: &mut TenantTx,
    facility_id: Option<Uuid>,
    spatial_node_id: Option<Uuid>,
    subtree_of_node: Option<Uuid>,
    category_code: Option<&str>,
    status: Option<&str>,
    criticality: Option<&str>,
    has_open_work_order: Option<bool>,
    q: Option<&str>,
    cursor: Option<&fms_shared::Cursor>,
    sort: &fms_shared::SortSpec,
    limit: i64,
) -> Result<Vec<AssetRow>, Problem> {
    // 依排序欄位把游標鍵解讀成對應型別；未使用的那個傳 NULL。
    let (cursor_id, cursor_ts, cursor_text) = match cursor {
        None => (None, None, None),
        Some(c) if c.sort_column == "created_at" => {
            (Some(c.uuid_id()?), Some(c.as_timestamp()?), None)
        }
        Some(c) => (Some(c.uuid_id()?), None, Some(c.key.clone())),
    };

    sqlx::query_as!(
        AssetRow,
        r#"
        SELECT a.id,
               a.facility_id,
               a.spatial_node_id,
               sn.node_path::text AS "spatial_node_path",
               a.asset_code::text AS "asset_code!",
               a.name::text AS "name!",
               a.serial_no::text AS "serial_no",
               ac.code::text AS "category_code!",
               a.asset_model_id,
               a.parent_asset_id,
               a.criticality,
               a.status,
               a.install_date,
               a.warranty_end_date,
               a.health_score::float8 AS "health_score",
               a.last_telemetry_at,
               (SELECT count(*) FROM fms.work_orders w
                 WHERE w.asset_id = a.id
                   AND w.status NOT IN ('CLOSED','CANCELLED','REJECTED'))
                 AS "open_work_order_count!",
               (SELECT count(*) FROM fms.alarms al
                 WHERE al.asset_id = a.id AND al.status = 'ACTIVE')
                 AS "active_alarm_count!",
               a.specifications AS "specifications!",
               a.attributes AS "attributes!",
               a.version,
               a.created_at,
               a.updated_at
        FROM fms.assets a
        JOIN fms.asset_categories ac ON ac.id = a.category_id
        LEFT JOIN fms.spatial_nodes sn ON sn.id = a.spatial_node_id
        WHERE a.deleted_at IS NULL
          AND ($1::uuid IS NULL OR a.facility_id = $1)
          AND ($2::uuid IS NULL OR a.spatial_node_id = $2)
          AND ($3::uuid IS NULL OR sn.node_path OPERATOR(public.<@)
                 (SELECT n2.node_path FROM fms.spatial_nodes n2 WHERE n2.id = $3))
          AND ($4::text IS NULL OR ac.code = $4)
          AND ($5::text IS NULL OR a.status = $5)
          AND ($6::text IS NULL OR a.criticality = $6)
          AND ($7::bool IS NULL OR $7 = EXISTS (
                SELECT 1 FROM fms.work_orders w
                 WHERE w.asset_id = a.id
                   AND w.status NOT IN ('CLOSED','CANCELLED','REJECTED')))
          AND ($8::text IS NULL
               OR a.name ILIKE '%' || $8 || '%'
               OR a.asset_code ILIKE '%' || $8 || '%')
          AND ($9::uuid IS NULL OR CASE
                WHEN $12::text = 'created_at' AND $13::bool
                  THEN (a.created_at, a.id) < ($10::timestamptz, $9::uuid)
                WHEN $12::text = 'created_at' AND NOT $13::bool
                  THEN (a.created_at, a.id) > ($10::timestamptz, $9::uuid)
                WHEN $12::text = 'asset_code' AND $13::bool
                  THEN (a.asset_code::text, a.id) < ($11::text, $9::uuid)
                WHEN $12::text = 'asset_code' AND NOT $13::bool
                  THEN (a.asset_code::text, a.id) > ($11::text, $9::uuid)
                WHEN $12::text = 'name' AND $13::bool
                  THEN (a.name::text, a.id) < ($11::text, $9::uuid)
                WHEN $12::text = 'name' AND NOT $13::bool
                  THEN (a.name::text, a.id) > ($11::text, $9::uuid)
              END)
        ORDER BY
          (CASE WHEN $12::text = 'created_at' AND $13::bool THEN a.created_at END) DESC,
          (CASE WHEN $12::text = 'created_at' AND NOT $13::bool THEN a.created_at END) ASC,
          (CASE WHEN $12::text = 'asset_code' AND $13::bool THEN a.asset_code::text END) DESC,
          (CASE WHEN $12::text = 'asset_code' AND NOT $13::bool THEN a.asset_code::text END) ASC,
          (CASE WHEN $12::text = 'name' AND $13::bool THEN a.name::text END) DESC,
          (CASE WHEN $12::text = 'name' AND NOT $13::bool THEN a.name::text END) ASC,
          (CASE WHEN $13::bool THEN a.id END) DESC,
          (CASE WHEN NOT $13::bool THEN a.id END) ASC
        LIMIT $14
        "#,
        facility_id,
        spatial_node_id,
        subtree_of_node,
        category_code,
        status,
        criticality,
        has_open_work_order,
        q,
        cursor_id,
        cursor_ts,
        cursor_text,
        sort.column,
        sort.desc,
        limit + 1
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 依 id 集合或父設備取資產。
///
/// 刻意保持 private 並只由 [`get`]、[`children`]、[`relations`] 的呼叫端使用：
/// 兩個條件都傳 None 會回傳全表，不該是外部能構造的呼叫。
///
/// 合成這一支的理由是避免第三、第四份相同的 SELECT 主體。契約的
/// `AssetDetail.children` 與 `relations[].asset` 都要完整的 `Asset`，
/// 若各寫一份查詢，`query_as!` 只會驗證每份自身合法，**不會**發現四份之間
/// 已經漂移。合成後只剩 `list`（動態排序）與這一支兩份。
async fn fetch(
    tx: &mut TenantTx,
    ids: Option<&[Uuid]>,
    parent_of: Option<Uuid>,
) -> Result<Vec<AssetRow>, Problem> {
    sqlx::query_as!(
        AssetRow,
        r#"
        SELECT a.id,
               a.facility_id,
               a.spatial_node_id,
               sn.node_path::text AS "spatial_node_path",
               a.asset_code::text AS "asset_code!",
               a.name::text AS "name!",
               a.serial_no::text AS "serial_no",
               ac.code::text AS "category_code!",
               a.asset_model_id,
               a.parent_asset_id,
               a.criticality,
               a.status,
               a.install_date,
               a.warranty_end_date,
               a.health_score::float8 AS "health_score",
               a.last_telemetry_at,
               (SELECT count(*) FROM fms.work_orders w
                 WHERE w.asset_id = a.id
                   AND w.status NOT IN ('CLOSED','CANCELLED','REJECTED'))
                 AS "open_work_order_count!",
               (SELECT count(*) FROM fms.alarms al
                 WHERE al.asset_id = a.id AND al.status = 'ACTIVE')
                 AS "active_alarm_count!",
               a.specifications AS "specifications!",
               a.attributes AS "attributes!",
               a.version,
               a.created_at,
               a.updated_at
        FROM fms.assets a
        JOIN fms.asset_categories ac ON ac.id = a.category_id
        LEFT JOIN fms.spatial_nodes sn ON sn.id = a.spatial_node_id
        WHERE a.deleted_at IS NULL
          AND ($1::uuid[] IS NULL OR a.id = ANY($1))
          AND ($2::uuid IS NULL OR a.parent_asset_id = $2)
        ORDER BY a.asset_code
        "#,
        ids,
        parent_of
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

pub async fn get(tx: &mut TenantTx, id: Uuid) -> Result<Option<AssetRow>, Problem> {
    Ok(fetch(tx, Some(&[id]), None).await?.pop())
}

/// 直屬子設備（`include=children`）。只取一層 ——
/// 契約的 `children` 是 `Asset` 陣列而非樹，遞迴展開該由客戶端逐層要求。
pub async fn children(tx: &mut TenantTx, id: Uuid) -> Result<Vec<AssetRow>, Problem> {
    fetch(tx, None, Some(id)).await
}

/// 一條依賴邊在「某個設備視角」下的樣子。
pub struct RelationEdge {
    pub relation_type: String,
    pub impact_level: String,
    /// `upstream`：對方是供應側，我依賴它。`downstream`：我是供應側，對方受我影響。
    pub direction: String,
    pub other_asset_id: Uuid,
}

/// 邊的方向如何解讀成 upstream／downstream。
///
/// `asset_relations` 只存 `(from, to, relation_type)`，箭頭的語意由型別決定，
/// **沒有**一體適用的規則：
///
/// | relation_type | 供應側（upstream） |
/// |---|---|
/// | `DEPENDS_ON` | `to`（from 依賴 to） |
/// | `FEEDS` / `CONTROLS` / `BACKUP_OF` | `from` |
/// | `MONITORS` / `CONNECTED_TO` | `from`（見下） |
///
/// 也就是只有 `DEPENDS_ON` 會反轉。這與 009 種下的示範資料一致：
/// `(AHU, DEPENDS_ON, UPS)`，而規格對本端點的說明是「UPS 停機會影響哪些設備」
/// —— 從 UPS 看 AHU 必須是 downstream。
///
/// `MONITORS` 與 `CONNECTED_TO` 其實沒有供應方向（監控不構成依賴、
/// 連接是對稱的），但契約的 `direction` 列舉只有 `upstream`／`downstream`，
/// 沒有 `peer`。這裡採用資料庫中儲存的方向 —— 至少是決定性的、
/// 可從 `relation_type` 反推的，而不是憑空發明一個列舉值。
pub async fn relations(tx: &mut TenantTx, id: Uuid) -> Result<Vec<RelationEdge>, Problem> {
    sqlx::query_as!(
        RelationEdge,
        r#"
        WITH e AS (
          SELECT r.relation_type,
                 r.impact_level,
                 CASE WHEN r.relation_type = 'DEPENDS_ON'
                      THEN r.to_asset_id ELSE r.from_asset_id END AS supplier_id,
                 CASE WHEN r.relation_type = 'DEPENDS_ON'
                      THEN r.from_asset_id ELSE r.to_asset_id END AS dependent_id
          FROM fms.asset_relations r
          WHERE r.from_asset_id = $1 OR r.to_asset_id = $1
        )
        SELECT e.relation_type AS "relation_type!",
               e.impact_level  AS "impact_level!",
               CASE WHEN e.supplier_id = $1 THEN 'downstream' ELSE 'upstream' END
                 AS "direction!",
               CASE WHEN e.supplier_id = $1 THEN e.dependent_id ELSE e.supplier_id END
                 AS "other_asset_id!"
        FROM e
        "#,
        id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 依 id 集合取資產，供 `relations` 展開對方設備用。
pub async fn by_ids(tx: &mut TenantTx, ids: &[Uuid]) -> Result<Vec<AssetRow>, Problem> {
    fetch(tx, Some(ids), None).await
}

/// `AssetDetail.meters` 的一列。
pub struct MeterRow {
    pub meter_code: String,
    pub name: String,
    pub unit: String,
    pub last_value: Option<f64>,
    pub last_read_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// 讀表（`include=meters`）。只回啟用中的表；停用的表留著是為了歷史讀值，
/// 不該出現在設備詳情裡。
pub async fn meters(tx: &mut TenantTx, id: Uuid) -> Result<Vec<MeterRow>, Problem> {
    sqlx::query_as!(
        MeterRow,
        r#"
        SELECT m.meter_code::text AS "meter_code!",
               m.name::text       AS "name!",
               m.unit::text       AS "unit!",
               m.last_value::float8 AS "last_value",
               m.last_read_at
        FROM fms.asset_meters m
        WHERE m.asset_id = $1 AND m.is_active
        ORDER BY m.meter_code
        "#,
        id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// `AssetDetail.maintenance_plans` 的一列。
pub struct PlanRow {
    pub id: Uuid,
    pub facility_id: Uuid,
    pub code: String,
    pub name: String,
    pub template_id: Uuid,
    pub template_name: String,
    pub target_type: String,
    pub target_id: Uuid,
    pub target_label: Option<String>,
    pub trigger_type: String,
    pub rrule: Option<String>,
    pub meter_code: Option<String>,
    pub meter_threshold: Option<f64>,
    pub generate_lead_days: i16,
    pub priority: String,
    pub assigned_team_id: Option<Uuid>,
    pub next_due_at: Option<chrono::DateTime<chrono::Utc>>,
    pub is_active: bool,
}

/// 適用於此設備的保養計畫（`include=maintenance_plans`）。
///
/// `ck_plan_target` 保證計畫恰好瞄準三者之一：單一設備、空間子樹、或分類。
/// 三種都要比對 —— 只比 `asset_id` 會漏掉「整個 4 樓的空調」這種計畫，
/// 而那正是契約把 `target.type` 做成列舉的原因。空間子樹用 ltree 的 `<@`，
/// 與 `list` 的 `subtree_of_node` 同一個手法。
pub async fn maintenance_plans(tx: &mut TenantTx, id: Uuid) -> Result<Vec<PlanRow>, Problem> {
    sqlx::query_as!(
        PlanRow,
        r#"
        SELECT p.id            AS "id!",
               p.facility_id   AS "facility_id!",
               p.code::text    AS "code!",
               p.name::text    AS "name!",
               p.template_id   AS "template_id!",
               t.name::text    AS "template_name!",
               CASE WHEN p.asset_id IS NOT NULL        THEN 'ASSET'
                    WHEN p.spatial_node_id IS NOT NULL THEN 'SPATIAL_NODE'
                    ELSE 'CATEGORY' END               AS "target_type!",
               coalesce(p.asset_id, p.spatial_node_id, p.category_id) AS "target_id!",
               CASE WHEN p.asset_id IS NOT NULL
                      THEN (SELECT a2.name::text FROM fms.assets a2 WHERE a2.id = p.asset_id)
                    WHEN p.spatial_node_id IS NOT NULL
                      THEN (SELECT n2.name::text FROM fms.spatial_nodes n2
                             WHERE n2.id = p.spatial_node_id)
                    ELSE (SELECT c2.name::text FROM fms.asset_categories c2
                           WHERE c2.id = p.category_id) END AS "target_label",
               p.trigger_type  AS "trigger_type!",
               p.rrule,
               p.meter_code::text AS "meter_code",
               p.meter_threshold::float8 AS "meter_threshold",
               p.generate_lead_days AS "generate_lead_days!",
               p.priority      AS "priority!",
               p.assigned_team_id,
               p.next_due_at,
               p.is_active     AS "is_active!"
        FROM fms.maintenance_plans p
        JOIN fms.maintenance_templates t ON t.id = p.template_id
        JOIN fms.assets a ON a.id = $1 AND a.deleted_at IS NULL
        LEFT JOIN fms.spatial_nodes sn ON sn.id = a.spatial_node_id
        WHERE p.is_active
          AND ( p.asset_id = a.id
                OR ( p.spatial_node_id IS NOT NULL
                     AND sn.node_path OPERATOR(public.<@)
                           (SELECT n.node_path FROM fms.spatial_nodes n
                             WHERE n.id = p.spatial_node_id) )
                OR p.category_id = a.category_id )
        ORDER BY p.code
        "#,
        id
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 依賴圖的一個節點。契約的 `DependencyGraph.nodes` 是精簡物件，
/// 不是完整的 `Asset` —— 影響分析要的是一張圖，不是幾十個欄位。
pub struct GraphNode {
    pub id: Uuid,
    pub asset_code: String,
    pub name: String,
    pub category_code: String,
    pub status: String,
    pub criticality: String,
}

/// 依賴圖的一條邊。`from`／`to` 保持**資料庫中儲存的方向**，
/// 與 `relation_type` 一起看才有意義（`(AHU, DEPENDS_ON, UPS)`）。
/// 不改寫成「供應側 → 被影響側」，否則客戶端照字面讀
/// 「from DEPENDS_ON to」會得到反的結論。
pub struct GraphEdge {
    pub from_asset_id: Uuid,
    pub to_asset_id: Uuid,
    pub relation_type: String,
    pub impact_level: String,
}

/// 在指定深度內走訪依賴圖，回傳可達的節點。
///
/// # 為什麼由資料庫遞迴
///
/// 應用層逐層查詢會是 depth 次 round-trip，且每層的節點數不可預期。
/// `WITH RECURSIVE` 一次做完，而 RLS 仍然生效 —— 遞迴 CTE 讀的是
/// `fms.asset_relations`，政策照樣套用，因此不可能走到別的租戶。
///
/// 終止有兩道保障：`UNION`（去重）讓環不會無限展開，
/// `depth < $2` 讓深度有硬上界（契約上限 5）。
pub async fn graph_nodes(
    tx: &mut TenantTx,
    id: Uuid,
    depth: i32,
    direction: &str,
) -> Result<Vec<GraphNode>, Problem> {
    sqlx::query_as!(
        GraphNode,
        r#"
        WITH RECURSIVE e AS (
          SELECT CASE WHEN r.relation_type = 'DEPENDS_ON'
                      THEN r.to_asset_id ELSE r.from_asset_id END AS supplier_id,
                 CASE WHEN r.relation_type = 'DEPENDS_ON'
                      THEN r.from_asset_id ELSE r.to_asset_id END AS dependent_id
          FROM fms.asset_relations r
        ),
        adj AS (
          -- 往上游走：從被影響側走到供應側
          SELECT dependent_id AS src, supplier_id  AS dst, 'upstream'::text   AS dir FROM e
          UNION ALL
          -- 往下游走：從供應側走到被影響側
          SELECT supplier_id  AS src, dependent_id AS dst, 'downstream'::text AS dir FROM e
        ),
        walk AS (
          SELECT $1::uuid AS asset_id, 0 AS depth
          UNION
          SELECT adj.dst, w.depth + 1
          FROM walk w
          JOIN adj ON adj.src = w.asset_id
          WHERE w.depth < $2::int
            AND ($3::text = 'both' OR adj.dir = $3::text)
        )
        SELECT a.id            AS "id!",
               a.asset_code::text AS "asset_code!",
               a.name::text    AS "name!",
               ac.code::text   AS "category_code!",
               a.status        AS "status!",
               a.criticality   AS "criticality!"
        FROM (SELECT DISTINCT asset_id FROM walk) n
        JOIN fms.assets a ON a.id = n.asset_id AND a.deleted_at IS NULL
        JOIN fms.asset_categories ac ON ac.id = a.category_id
        ORDER BY a.asset_code
        "#,
        id,
        depth,
        direction
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 節點集合內部的邊。只回兩端都在集合裡的邊 ——
/// 回一條指向集合外節點的邊，客戶端就畫不出來。
pub async fn graph_edges(tx: &mut TenantTx, ids: &[Uuid]) -> Result<Vec<GraphEdge>, Problem> {
    sqlx::query_as!(
        GraphEdge,
        r#"
        SELECT r.from_asset_id AS "from_asset_id!",
               r.to_asset_id   AS "to_asset_id!",
               r.relation_type AS "relation_type!",
               r.impact_level  AS "impact_level!"
        FROM fms.asset_relations r
        WHERE r.from_asset_id = ANY($1) AND r.to_asset_id = ANY($1)
        ORDER BY r.created_at
        "#,
        ids
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 由 category_code 解析出 category_id。
///
/// `asset_categories` 是 catalog 表：007 給它兩條政策，讀取允許
/// `tenant_id IS NULL` 的平台預設列，因此租戶看得到 008 種下的 28 個分類。
pub async fn resolve_category(tx: &mut TenantTx, code: &str) -> Result<Option<Uuid>, Problem> {
    sqlx::query_scalar!("SELECT id FROM fms.asset_categories WHERE code = $1", code)
        .fetch_optional(tx.conn())
        .await
        .map_err(Problem::from)
}

pub struct NewAsset<'a> {
    pub facility_id: Uuid,
    pub category_id: Uuid,
    pub asset_code: &'a str,
    pub name: &'a str,
    pub spatial_node_id: Option<Uuid>,
    pub parent_asset_id: Option<Uuid>,
    pub asset_model_id: Option<Uuid>,
    pub serial_no: Option<&'a str>,
    pub criticality: Option<&'a str>,
    pub status: Option<&'a str>,
    pub install_date: Option<chrono::NaiveDate>,
    pub warranty_end_date: Option<chrono::NaiveDate>,
    pub custodian_user_id: Option<Uuid>,
    pub specifications: Option<&'a serde_json::Value>,
    pub attributes: Option<&'a serde_json::Value>,
}

/// 建立資產。
///
/// `criticality`／`status` 的 coalesce 預設值刻意與 `fms.assets` 的
/// column default 一致（`MEDIUM`／`OPERATIONAL`）。這是重複，但無法避免：
/// `coalesce` 需要字面值，而 SQL 沒有「NULL 時採用 column default」的寫法。
/// 值的合法性由 handler 先擋（見 `validate_enums`），因此不會出現
/// 傳入非法值卻得到 500 的情形。
pub async fn create(tx: &mut TenantTx, new: NewAsset<'_>) -> Result<Uuid, Problem> {
    let tenant_id = tx.context().tenant_id;
    sqlx::query_scalar!(
        r#"
        INSERT INTO fms.assets
          (tenant_id, facility_id, category_id, asset_code, name,
           spatial_node_id, parent_asset_id, asset_model_id, serial_no,
           criticality, status, install_date, warranty_end_date,
           custodian_user_id, specifications, attributes)
        VALUES
          ($1, $2, $3, $4, $5, $6, $7, $8, $9,
           coalesce($10, 'MEDIUM'), coalesce($11, 'OPERATIONAL'), $12, $13, $14,
           coalesce($15, '{}'::jsonb), coalesce($16, '{}'::jsonb))
        RETURNING id
        "#,
        tenant_id,
        new.facility_id,
        new.category_id,
        new.asset_code,
        new.name,
        new.spatial_node_id,
        new.parent_asset_id,
        new.asset_model_id,
        new.serial_no,
        new.criticality,
        new.status,
        new.install_date,
        new.warranty_end_date,
        new.custodian_user_id,
        new.specifications,
        new.attributes,
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 局部更新。`COALESCE` 讓未提供的欄位保持原值。
#[allow(clippy::too_many_arguments)]
pub async fn update(
    tx: &mut TenantTx,
    id: Uuid,
    category_id: Option<Uuid>,
    w: &crate::dto::AssetWrite,
) -> Result<(), Problem> {
    sqlx::query!(
        r#"
        UPDATE fms.assets SET
          facility_id       = coalesce($2, facility_id),
          category_id       = coalesce($3, category_id),
          asset_code        = coalesce($4, asset_code),
          name              = coalesce($5, name),
          spatial_node_id   = coalesce($6, spatial_node_id),
          parent_asset_id   = coalesce($7, parent_asset_id),
          asset_model_id    = coalesce($8, asset_model_id),
          serial_no         = coalesce($9, serial_no),
          criticality       = coalesce($10, criticality),
          status            = coalesce($11, status),
          install_date      = coalesce($12, install_date),
          warranty_end_date = coalesce($13, warranty_end_date),
          custodian_user_id = coalesce($14, custodian_user_id),
          specifications    = coalesce($15, specifications),
          attributes        = coalesce($16, attributes)
        WHERE id = $1 AND deleted_at IS NULL
        "#,
        id,
        w.facility_id,
        category_id,
        w.asset_code,
        w.name,
        w.spatial_node_id,
        w.parent_asset_id,
        w.asset_model_id,
        w.serial_no,
        w.criticality,
        w.status,
        w.install_date,
        w.warranty_end_date,
        w.custodian_user_id,
        w.specifications,
        w.attributes,
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// 阻止軟刪除的原因。契約的 DELETE 在被參照時回 409。
pub struct DeleteBlockers {
    pub children: i64,
    pub open_work_orders: i64,
}

/// 檢查是否可刪。子設備與未結工單都會阻止刪除 ——
/// 前者會讓子樹失去父節點，後者代表這台設備還有進行中的維護。
pub async fn delete_blockers(tx: &mut TenantTx, id: Uuid) -> Result<DeleteBlockers, Problem> {
    let row = sqlx::query!(
        r#"SELECT
             (SELECT count(*) FROM fms.assets c
               WHERE c.parent_asset_id = $1 AND c.deleted_at IS NULL)
               AS "children!",
             (SELECT count(*) FROM fms.work_orders w
               WHERE w.asset_id = $1
                 AND w.status NOT IN ('CLOSED','CANCELLED','REJECTED'))
               AS "open_work_orders!""#,
        id
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;

    Ok(DeleteBlockers {
        children: row.children,
        open_work_orders: row.open_work_orders,
    })
}

/// 軟刪除。schema 有 `deleted_at` 且唯一索引都是
/// `WHERE deleted_at IS NULL` 的部分索引，因此軟刪除後 asset_code 可重用。
pub async fn soft_delete(tx: &mut TenantTx, id: Uuid) -> Result<u64, Problem> {
    let done = sqlx::query!(
        "UPDATE fms.assets SET deleted_at = clock_timestamp()
         WHERE id = $1 AND deleted_at IS NULL",
        id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(done.rows_affected())
}

// =============================================================================
// WBS 4.8：設備型錄
// =============================================================================

/// `AssetModel` 的一列。
pub struct AssetModelRow {
    pub id: Uuid,
    pub is_platform: bool,
    pub category_code: String,
    pub manufacturer: String,
    pub model_no: String,
    pub name: String,
    pub specifications: serde_json::Value,
    pub supported_protocols: Vec<String>,
    pub expected_life_months: Option<i32>,
}

/// 列出設備型錄。
///
/// `scope` 是契約特有的過濾條件，對應 `tenant_id IS NULL`（平台共用）
/// 與 `IS NOT NULL`（租戶自建）。**不能**在應用層用 tenant_id 過濾平台列：
/// 007 給 catalog 表的政策允許讀 `tenant_id IS NULL`，
/// 而 `is_platform` 這個對外欄位就是由它推導出來的。
///
/// 排序固定 `manufacturer, model_no`：契約的這支端點沒有 `sort` 參數，
/// 而型錄是給人挑選的清單，字母序比時間序有用。游標仍記下欄位。
pub async fn list_models(
    tx: &mut TenantTx,
    category_code: Option<&str>,
    manufacturer: Option<&str>,
    scope: &str,
    cursor: Option<&fms_shared::Cursor>,
    limit: i64,
) -> Result<Vec<AssetModelRow>, Problem> {
    let (cursor_key, cursor_id) = match cursor {
        Some(c) => (Some(c.key.clone()), Some(c.uuid_id()?)),
        None => (None, None),
    };

    sqlx::query_as!(
        AssetModelRow,
        r#"
        SELECT m.id,
               (m.tenant_id IS NULL) AS "is_platform!",
               ac.code::text          AS "category_code!",
               m.manufacturer::text   AS "manufacturer!",
               m.model_no::text       AS "model_no!",
               m.name::text           AS "name!",
               m.specifications       AS "specifications!",
               m.supported_protocols  AS "supported_protocols!",
               m.expected_life_months
        FROM fms.asset_models m
        JOIN fms.asset_categories ac ON ac.id = m.category_id
        WHERE m.is_active
          AND ($1::text IS NULL OR ac.code = $1)
          AND ($2::text IS NULL OR m.manufacturer ILIKE '%' || $2 || '%')
          AND ($3::text = 'all'
               OR ($3 = 'platform' AND m.tenant_id IS NULL)
               OR ($3 = 'tenant'   AND m.tenant_id IS NOT NULL))
          AND ($4::text IS NULL
               OR (m.manufacturer::text, m.model_no::text, m.id) >
                  ($4::text, $5::text, $6::uuid))
        ORDER BY m.manufacturer, m.model_no, m.id
        LIMIT $7
        "#,
        category_code,
        manufacturer,
        scope,
        cursor_key,
        // 游標鍵是 `manufacturer|model_no` 兩段。第二段單獨傳，
        // 讓 SQL 端的列比較（row comparison）能正確排序。
        cursor
            .map(|c| c.key.split_once('\u{1f}').map(|(_, b)| b.to_string()))
            .flatten(),
        cursor_id,
        limit + 1
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

// =============================================================================
// WBS 4.9：計量讀數
// =============================================================================

/// 登錄讀數前需要知道的讀表狀態。
pub struct MeterState {
    pub id: Uuid,
    pub meter_code: String,
    pub reading_type: String,
    pub last_value: Option<f64>,
    pub last_read_at: Option<chrono::DateTime<chrono::Utc>>,
    pub rollover_at: Option<f64>,
}

/// 依設備與讀表代碼取讀表狀態。
///
/// `lower(meter_code)` 比對唯一索引 `uq_asset_meters (asset_id, lower(meter_code))`
/// 的定義 —— 契約的路徑參數是 `LAMP_HOURS`，但索引是不分大小寫的，
/// 用區分大小寫的比對會出現「找不到但其實存在」。
pub async fn meter_state(
    tx: &mut TenantTx,
    asset_id: Uuid,
    meter_code: &str,
) -> Result<Option<MeterState>, Problem> {
    sqlx::query_as!(
        MeterState,
        r#"
        SELECT m.id,
               m.meter_code::text AS "meter_code!",
               m.reading_type,
               m.last_value::float8   AS "last_value",
               m.last_read_at,
               m.rollover_at::float8  AS "rollover_at"
        FROM fms.asset_meters m
        WHERE m.asset_id = $1 AND lower(m.meter_code) = lower($2) AND m.is_active
        "#,
        asset_id,
        meter_code
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 寫入一筆讀數並（在時序允許時）推進讀表的當前值。
///
/// 回傳讀表在本次之後的 `last_value`。
///
/// # 為什麼歷史與當前值分開處理
///
/// 讀數表是歷史，遲到的讀數仍然要收；但 `last_value` 代表「現在」，
/// 被較舊的讀數覆寫會讓計量觸發的保養判斷退回去。因此 UPDATE 帶
/// `last_read_at <= $reading_at` 的條件 —— 這與 006 的 `ingest_telemetry`
/// 用的是同一個守則，兩條寫入路徑對「遲到」的處理必須一致。
pub async fn record_reading(
    tx: &mut TenantTx,
    meter: &MeterState,
    value: f64,
    new_last_value: f64,
    reading_at: chrono::DateTime<chrono::Utc>,
    source: &str,
) -> Result<f64, Problem> {
    let tenant_id = tx.context().tenant_id;
    let user_id = tx.context().user_id;

    sqlx::query!(
        r#"
        INSERT INTO fms.asset_meter_readings
          (tenant_id, asset_meter_id, reading_at, value, source, recorded_by)
        VALUES ($1, $2, $3, $4::float8::numeric, $5, $6)
        "#,
        tenant_id,
        meter.id,
        reading_at,
        value,
        source,
        user_id
    )
    .execute(tx.conn())
    .await
    .map_err(Problem::from)?;

    let updated: Option<f64> = sqlx::query_scalar!(
        r#"
        UPDATE fms.asset_meters
           SET last_value = $2::float8::numeric, last_read_at = $3
         WHERE id = $1
           AND (last_read_at IS NULL OR last_read_at <= $3)
        RETURNING last_value::float8 AS "last_value!"
        "#,
        meter.id,
        new_last_value,
        reading_at
    )
    .fetch_optional(tx.conn())
    .await
    .map_err(Problem::from)?;

    // UPDATE 未命中代表這是一筆遲到的讀數：歷史已寫入，當前值保持原樣。
    Ok(updated.unwrap_or(meter.last_value.unwrap_or(value)))
}

/// 找出因為這筆讀數而到達門檻的計量型保養計畫。
///
/// # 觸發規則
///
/// 依讀表型別分兩種，因為「門檻」對兩者的意思不同：
///
///   * **累計型**（`CUMULATIVE`／`DELTA`）：門檻是**週期**。
///     燈泡壽命 5000 小時的計畫在 5000、10000、15000… 各觸發一次，
///     因此判定式是 `floor(new / threshold) > floor(old / threshold)`。
///   * **瞬時型**（`GAUGE`）：門檻是**界線**。壓差超過設定值就該保養，
///     判定式是 `old < threshold <= new`（只在向上跨越時觸發，
///     否則在界線附近震盪會每筆讀數都觸發）。
///
/// 這個規則是本次定案的設計決策：schema 只給了 `meter_threshold` 與
/// `meter_tolerance_pct`，沒有任何函式或註解說明如何判定，
/// 而對累計型與瞬時型套同一條規則必然有一種是錯的。
///
/// 目標比對沿用 `maintenance_plans` 的三種瞄準模式（設備／空間子樹／分類），
/// 與 `include=maintenance_plans` 完全一致 —— 同一個問題有兩種答案更糟。
///
/// **本端點不產生工單**：契約的欄位叫 `triggered_maintenance_plan_ids`，
/// 不是 `created_work_order_ids`。產單是 PM 產生器的職責（尚未實作），
/// 因此這裡只回報「哪些計畫到期了」並發出 outbox 事件讓後續處理接手。
pub async fn plans_crossing_threshold(
    tx: &mut TenantTx,
    asset_id: Uuid,
    meter_code: &str,
    reading_type: &str,
    old_value: Option<f64>,
    new_value: f64,
) -> Result<Vec<Uuid>, Problem> {
    let accumulating = matches!(reading_type, "CUMULATIVE" | "DELTA");
    sqlx::query_scalar!(
        r#"
        SELECT p.id
        FROM fms.maintenance_plans p
        JOIN fms.assets a ON a.id = $1 AND a.deleted_at IS NULL
        LEFT JOIN fms.spatial_nodes sn ON sn.id = a.spatial_node_id
        WHERE p.is_active
          AND p.trigger_type IN ('METER', 'HYBRID')
          AND p.meter_threshold IS NOT NULL
          AND p.meter_threshold > 0
          AND lower(p.meter_code) = lower($2)
          AND ( p.asset_id = a.id
                OR ( p.spatial_node_id IS NOT NULL
                     AND sn.node_path OPERATOR(public.<@)
                           (SELECT n.node_path FROM fms.spatial_nodes n
                             WHERE n.id = p.spatial_node_id) )
                OR p.category_id = a.category_id )
          AND CASE
                WHEN $3::bool THEN
                  floor($5::float8::numeric / p.meter_threshold)
                    > floor(coalesce($4::float8, 0)::numeric / p.meter_threshold)
                ELSE
                  coalesce($4::float8, 0)::numeric < p.meter_threshold
                    AND $5::float8::numeric >= p.meter_threshold
              END
        ORDER BY p.code
        "#,
        asset_id,
        meter_code,
        accumulating,
        old_value,
        new_value
    )
    .fetch_all(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 發出「計量門檻到達」事件，供 PM 產生器接手。
///
/// 與讀數寫入同一個交易，因此不會出現「讀數存了但事件遺失」——
/// 這正是 001 的 transactional outbox 存在的理由。
pub async fn emit_threshold_event(
    tx: &mut TenantTx,
    asset_id: Uuid,
    meter_code: &str,
    value: f64,
    reading_at: chrono::DateTime<chrono::Utc>,
    plan_ids: &[Uuid],
) -> Result<(), Problem> {
    let tenant_id = tx.context().tenant_id;
    // `reading_at` 是**冪等的關鍵**，不只是資訊性欄位：消費端用它當
    // `maintenance_occurrences.scheduled_for`。若消費端改用處理時的
    // 時鐘，同一筆事件在不同秒重放就會產生第二個占位、第二張工單 ——
    // outbox 是 at-least-once，那不是理論風險。
    let payload = serde_json::json!({
        "asset_id": asset_id,
        "meter_code": meter_code,
        "value": value,
        "reading_at": reading_at,
        "maintenance_plan_ids": plan_ids,
    });
    sqlx::query_scalar!(
        r#"SELECT fms.emit_event($1, 'maintenance.meter_threshold_reached',
                                 'ASSET', $2, $3) AS "id!""#,
        tenant_id,
        asset_id,
        payload
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)?;
    Ok(())
}

/// 讀數推進規則。委派給 `fms.next_meter_value()`（migration 030）。
///
/// 這裡刻意不做任何判斷 —— 規則只有一份，在資料庫（ADR-09 紀律 2）。
/// 先前應用層有一份正確實作、`ingest_telemetry` 有一份錯誤實作，
/// 於是同一支讀表在人工登錄與 IoT 上報下會推進出不同的 `last_value`。
///
/// 函式對負增量與累計倒退會拋 `23514` 並在訊息裡帶 `METER_VALUE_INVALID`；
/// `Problem::from(sqlx::Error)` 依那個標記轉成 422，而不是 500。
pub async fn next_meter_value(
    tx: &mut TenantTx,
    asset_meter_id: Uuid,
    value: f64,
) -> Result<f64, Problem> {
    sqlx::query_scalar!(
        r#"SELECT fms.next_meter_value($1, $2::float8::numeric)::float8 AS "next!""#,
        asset_meter_id,
        value
    )
    .fetch_one(tx.conn())
    .await
    .map_err(Problem::from)
}

/// 鎖住那一列，供樂觀鎖的前置讀取使用。
///
/// **必須在讀出 `version` 之前呼叫。** 少了它，兩個並發的 PATCH 會讀到同一個
/// 版本、都通過 `check_version`、都寫入 —— 見該函式的說明。
///
/// 不做 not-found 判斷：呼叫端緊接著的 `get()` 會處理。列不存在時
/// `FOR UPDATE` 什麼也不鎖，那是正確的行為。
///
/// 刻意用不帶 JOIN 的最小查詢：`get()` 帶 JOIN，而對它加 `FOR UPDATE` 會連
/// `users` 那些列一起鎖住 —— 過度加鎖，而且會製造與其他路徑的死鎖機會。
pub async fn lock(tx: &mut TenantTx, id: Uuid) -> Result<(), Problem> {
    sqlx::query("SELECT 1 FROM fms.assets WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(tx.conn())
        .await
        .map_err(Problem::from)?;
    Ok(())
}
