//! 資產切片端到端測試（WBS 4.6）。
//!
//! 除了 CRUD，特別驗證幾件資產特有的事：
//!   * `category_code` ↔ `category_id` 的換算（契約對外是 code，儲存是 id）
//!   * `spatial_node_path` 由 ltree 帶出
//!   * `subtree_of_node` 用 ltree `<@` 做子樹查詢（不是只比對單一節點）
//!   * 軟刪除，且被子設備或未結工單參照時回 409
//!   * `fields` 稀疏欄位集合
//!   * `sort` 單欄升／降冪、白名單外回 422、多欄回 422
//!   * 游標記下排序欄位：換了 sort 卻沿用舊 cursor 回 400
//!   * `include` 關聯展開（children／relations／meters／maintenance_plans），
//!     以及「契約列出但未實作」與「拼錯」兩種 422 的區別（WBS 4.7）
//!   * 依賴圖：方向由 `relation_type` 決定、`direction` 真的過濾走訪方向、
//!     `depth` 超界回 422（WBS 4.7）

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

/// 009 示範資料
const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
/// A 棟（B1／4F 的父節點），用來驗證 subtree 查詢
const NODE_BUILDING_A: &str = "10000000-0000-4000-8000-000000000001";
/// B1 機房 —— 009 的 UPS／AHU 掛在這裡
const NODE_B1: &str = "10000000-0000-4000-8000-000000000003";

// 009 的依賴圖示範資料：`(AHU, DEPENDS_ON, UPS)`，即 AHU 依賴 UPS。
const SEED_UPS: &str = "20000000-0000-4000-8000-000000000001";
const SEED_UPS_CODE: &str = "HQ-UPS-B1-01";
const SEED_AHU: &str = "20000000-0000-4000-8000-000000000002";
const SEED_AHU_CODE: &str = "HQ-AHU-4F-01";
/// 1 廳投影機 —— 009 唯一有讀表（LAMP_HOURS）與 METER 觸發保養計畫的設備
const SEED_PROJECTOR: &str = "20000000-0000-4000-8000-000000000003";

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get_request(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

#[tokio::test]
async fn asset_slice_end_to_end() {
    let ctx = TestContext::setup().await;
    let token = ctx.login().await;
    let code = format!("TEST-AHU-{}", &uuid::Uuid::new_v4().to_string()[..8]);

    // ---- 建立：category_code 需被解析成 category_id ----
    let (status, created) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/assets",
                json!({
                    "facility_id": FACILITY_HQ,
                    "spatial_node_id": NODE_B1,
                    "category_code": "AHU",
                    "asset_code": code,
                    "name": "測試空調箱",
                    "criticality": "HIGH",
                    "specifications": { "cmh": 12000 }
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {created}");
    assert_eq!(created["category_code"], "AHU", "category_code 應被帶回");
    assert_eq!(created["criticality"], "HIGH");
    assert_eq!(created["version"], 1);
    assert_eq!(created["open_work_order_count"], 0);
    assert_eq!(created["active_alarm_count"], 0);
    assert!(
        created["spatial_node_path"].as_str().is_some(),
        "spatial_node_path 應由 ltree 帶出: {created}"
    );
    let asset_id = created["id"].as_str().unwrap().to_string();

    // ---- 未知 category_code → 422，而不是 500 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/assets",
                json!({
                    "facility_id": FACILITY_HQ,
                    "category_code": "NO_SUCH_CATEGORY",
                    "asset_code": "X",
                    "name": "X"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "VALIDATION_ERROR");

    // ---- 缺 required 欄位 → 422 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/assets",
                json!({ "facility_id": FACILITY_HQ, "category_code": "AHU" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- subtree 查詢：以 A 棟為根應找得到掛在 B1 的設備 ----
    let (status, subtree) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/assets?subtree_of_node={NODE_BUILDING_A}&limit=200"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{subtree}");
    let found = subtree["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["id"] == asset_id);
    assert!(
        found,
        "subtree_of_node 應以 ltree <@ 涵蓋子節點，未找到掛在 B1 的設備"
    );

    // ---- 精確節點查詢也要找得到 ----
    let (_, exact) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/assets?spatial_node_id={NODE_B1}&limit=200"
            )),
            &token,
        ))
        .await;
    assert!(exact["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["id"] == asset_id));

    // ---- category_code 過濾 ----
    let (_, by_cat) = ctx
        .send(authed(
            get_request("/api/v1/assets?category_code=AHU&limit=200"),
            &token,
        ))
        .await;
    assert!(by_cat["data"]
        .as_array()
        .unwrap()
        .iter()
        .all(|a| a["category_code"] == "AHU"));

    // ---- q 模糊搜尋 ----
    let (_, by_q) = ctx
        .send(authed(
            get_request(&format!("/api/v1/assets?q={}", &code[..9])),
            &token,
        ))
        .await;
    assert!(
        !by_q["data"].as_array().unwrap().is_empty(),
        "q 應能以 asset_code 前綴找到設備"
    );

    // ---- fields 稀疏欄位集合：只回請求的欄位（id 一律保留）----
    let (status, sparse) = ctx
        .send(authed(
            get_request("/api/v1/assets?fields=asset_code,name&limit=1"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{sparse}");
    let first = &sparse["data"][0];
    let keys: Vec<&String> = first.as_object().unwrap().keys().collect();
    assert_eq!(
        keys.len(),
        3,
        "應只剩 id + asset_code + name，實際：{keys:?}"
    );
    assert!(first["id"].is_string(), "id 應一律保留");
    assert!(first["asset_code"].is_string());
    assert!(
        first["criticality"].is_null(),
        "未請求的欄位不該出現：{first}"
    );

    // ---- fields 帶未知欄位 → 422，而不是靜默忽略 ----
    let (status, body) = ctx
        .send(authed(
            get_request("/api/v1/assets?fields=id,no_such_field"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- sort：升冪應真的改變順序，且與降冪相反 ----
    let (status, asc) = ctx
        .send(authed(
            get_request("/api/v1/assets?sort=asset_code&limit=200"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{asc}");
    let asc_codes: Vec<&str> = asc["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["asset_code"].as_str().unwrap())
        .collect();
    let mut expected = asc_codes.clone();
    expected.sort_unstable();
    assert_eq!(asc_codes, expected, "sort=asset_code 應為升冪");

    let (_, desc) = ctx
        .send(authed(
            get_request("/api/v1/assets?sort=-asset_code&limit=200"),
            &token,
        ))
        .await;
    let desc_codes: Vec<&str> = desc["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["asset_code"].as_str().unwrap())
        .collect();
    let mut expected_desc = desc_codes.clone();
    expected_desc.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(desc_codes, expected_desc, "sort=-asset_code 應為降冪");
    assert_ne!(
        asc_codes.first(),
        desc_codes.first(),
        "升冪與降冪的第一筆不該相同（排序未生效）"
    );

    // ---- sort 排序時翻頁不得重複（keyset 游標必須跟著排序鍵）----
    let (_, p1) = ctx
        .send(authed(
            get_request("/api/v1/assets?sort=asset_code&limit=1"),
            &token,
        ))
        .await;
    let cur = p1["page"]["next_cursor"]
        .as_str()
        .expect("應有 next_cursor")
        .to_string();
    let (_, p2) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/assets?sort=asset_code&limit=1&cursor={cur}"
            )),
            &token,
        ))
        .await;
    let a1 = p1["data"][0]["asset_code"].as_str().unwrap();
    let a2 = p2["data"][0]["asset_code"].as_str().unwrap();
    assert_ne!(a1, a2, "第二頁重複了第一頁的列");
    assert!(a2 > a1, "升冪翻頁應繼續往後：{a1} → {a2}");

    // ---- 換了 sort 卻沿用舊 cursor → 400，而不是語意錯亂的一頁 ----
    let (status, body) = ctx
        .send(authed(
            get_request(&format!("/api/v1/assets?sort=name&cursor={cur}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("cursor"),
        "應說明游標與排序不符：{body}"
    );

    // ---- 白名單外的排序欄位 → 422 ----
    let (status, body) = ctx
        .send(authed(
            get_request("/api/v1/assets?sort=health_score"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 多欄排序 → 422（明確拒絕，不默默只用第一欄）----
    let (status, body) = ctx
        .send(authed(
            get_request("/api/v1/assets?sort=name,-created_at"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- GET 單筆回 ETag ----
    let (status, etag, one) = ctx
        .send_with_headers(authed(
            get_request(&format!("/api/v1/assets/{asset_id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    let etag = etag.expect("GET 單筆應回 ETag");
    assert_eq!(etag, "\"1\"");

    // ---- PATCH 缺 If-Match → 428 ----
    let (status, _) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/assets/{asset_id}"),
                json!({ "name": "改名" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);

    // ---- PATCH 過期 If-Match → 412 ----
    let (status, _) = ctx
        .send(authed_if_match(
            json_request(
                "PATCH",
                &format!("/api/v1/assets/{asset_id}"),
                json!({ "name": "改名" }),
            ),
            &token,
            "999",
        ))
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);

    // ---- PATCH 正確 → version 遞增，且可換 category ----
    let (status, patched) = ctx
        .send(authed_if_match(
            json_request(
                "PATCH",
                &format!("/api/v1/assets/{asset_id}"),
                json!({ "name": "改名後的空調箱", "category_code": "FCU" }),
            ),
            &token,
            &etag,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["name"], "改名後的空調箱");
    assert_eq!(patched["category_code"], "FCU", "category 應可更換");
    assert_eq!(
        patched["version"], 2,
        "version 應由 trg_bump_version 自動遞增"
    );

    // ---- 有子設備時 DELETE → 409 ----
    let child_code = format!("TEST-CHILD-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let (status, child) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/assets",
                json!({
                    "facility_id": FACILITY_HQ,
                    "category_code": "FCU",
                    "asset_code": child_code,
                    "name": "子設備",
                    "parent_asset_id": asset_id
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{child}");
    let child_id = child["id"].as_str().unwrap().to_string();

    // ================= WBS 4.7：include 關聯展開與依賴圖 =================

    // ---- 未要求 include 時，關聯欄位不該出現（而非出現為空陣列）----
    let (status, plain) = ctx
        .send(authed(
            get_request(&format!("/api/v1/assets/{asset_id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{plain}");
    let plain_obj = plain.as_object().unwrap();
    assert!(
        !plain_obj.contains_key("children") && !plain_obj.contains_key("relations"),
        "未要求 include 時不該出現關聯欄位：{plain}"
    );

    // ---- include=children：一層直屬子設備，且是完整的 Asset ----
    let (status, detail) = ctx
        .send(authed(
            get_request(&format!("/api/v1/assets/{asset_id}?include=children")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let kids = detail["children"].as_array().expect("應有 children");
    assert_eq!(kids.len(), 1, "應有一個子設備：{detail}");
    assert_eq!(kids[0]["id"], child_id);
    assert_eq!(
        kids[0]["asset_code"], child_code,
        "children 的元素應是完整的 Asset，不是只有 id"
    );

    // ---- include=relations：方向由 relation_type 決定，不是由儲存的 from/to ----
    // 009 種下 (AHU, DEPENDS_ON, UPS)：從 UPS 看，AHU 是下游（受我影響）。
    let (status, ups) = ctx
        .send(authed(
            get_request(&format!("/api/v1/assets/{SEED_UPS}?include=relations")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{ups}");
    let rels = ups["relations"].as_array().expect("應有 relations");
    assert_eq!(rels.len(), 1, "UPS 應有一條依賴邊：{ups}");
    assert_eq!(rels[0]["relation_type"], "DEPENDS_ON");
    assert_eq!(rels[0]["impact_level"], "CRITICAL");
    assert_eq!(
        rels[0]["direction"], "downstream",
        "AHU DEPENDS_ON UPS：從 UPS 看 AHU 應是下游（UPS 停機會影響 AHU）"
    );
    assert_eq!(rels[0]["asset"]["asset_code"], SEED_AHU_CODE);

    // 反向：從 AHU 看 UPS 是上游
    let (_, ahu) = ctx
        .send(authed(
            get_request(&format!("/api/v1/assets/{SEED_AHU}?include=relations")),
            &token,
        ))
        .await;
    assert_eq!(
        ahu["relations"][0]["direction"], "upstream",
        "從 AHU 看 UPS 應是上游：{ahu}"
    );
    assert_eq!(ahu["relations"][0]["asset"]["asset_code"], SEED_UPS_CODE);

    // ---- include=meters ----
    let (status, prj) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/assets/{SEED_PROJECTOR}?include=meters,maintenance_plans"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{prj}");
    let meters = prj["meters"].as_array().expect("應有 meters");
    assert_eq!(meters.len(), 1, "投影機應有一個讀表：{prj}");
    assert_eq!(meters[0]["meter_code"], "LAMP_HOURS");
    assert_eq!(meters[0]["unit"], "h");
    assert_eq!(
        meters[0]["last_value"].as_f64(),
        Some(4820.0),
        "numeric 應轉為 JSON number：{prj}"
    );

    // ---- include=maintenance_plans：target 多型與 template_name ----
    let plans = prj["maintenance_plans"]
        .as_array()
        .expect("應有 maintenance_plans");
    let lamp = plans
        .iter()
        .find(|p| p["code"] == "PM_PRJ_LAMP_H1")
        .unwrap_or_else(|| panic!("應找到光源更換計畫：{prj}"));
    assert_eq!(lamp["trigger_type"], "METER");
    assert_eq!(lamp["meter_code"], "LAMP_HOURS");
    assert_eq!(lamp["meter_threshold"].as_f64(), Some(5000.0));
    assert_eq!(lamp["target"]["type"], "ASSET");
    assert_eq!(lamp["target"]["id"], SEED_PROJECTOR);
    assert!(
        lamp["target"]["label"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "target.label 應解析出被瞄準物件的名稱：{lamp}"
    );
    assert!(
        lamp["template_name"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "template_name 應由 maintenance_templates 帶出：{lamp}"
    );

    // ---- include=open_work_orders：隨工單模組（S4）上線，不再是 422 ----
    // 這台設備是本測試新建的，因此陣列應為空 —— 而空陣列在這裡是**正確的斷言**
    // （「查過了，沒有未結工單」），與先前「功能還沒做」的語意不同。
    let (status, with_wo) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/assets/{asset_id}?include=open_work_orders"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{with_wo}");
    assert_eq!(
        with_wo["open_work_orders"],
        serde_json::json!([]),
        "新建的設備沒有未結工單：{with_wo}"
    );
    assert_eq!(
        with_wo["open_work_order_count"], 0,
        "count 與陣列長度必須一致（兩者共用同一個「未結」定義）：{with_wo}"
    );

    // ---- 未知 include 值 → 422（拼錯與「真的沒有子設備」必須可分辨）----
    let (status, body) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/assets/{asset_id}?include=children,childrn"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 依賴圖：預設 both／depth=2 ----
    let (status, graph) = ctx
        .send(authed(
            get_request(&format!("/api/v1/assets/{SEED_UPS}/dependency-graph")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{graph}");
    let codes: Vec<&str> = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["asset_code"].as_str().unwrap())
        .collect();
    assert_eq!(codes.len(), 2, "UPS 與 AHU 都應在圖裡：{graph}");
    assert!(codes.contains(&SEED_UPS_CODE) && codes.contains(&SEED_AHU_CODE));
    let edges = graph["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "只有一條邊：{graph}");
    assert_eq!(
        edges[0]["from"], SEED_AHU,
        "edges 應保持資料庫儲存的方向（AHU DEPENDS_ON UPS），否則客戶端照字面讀會反：{graph}"
    );
    assert_eq!(edges[0]["to"], SEED_UPS);
    assert_eq!(edges[0]["relation_type"], "DEPENDS_ON");

    // ---- direction=upstream：UPS 沒有上游，只剩自己 ----
    let (_, up_only) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/assets/{SEED_UPS}/dependency-graph?direction=upstream"
            )),
            &token,
        ))
        .await;
    assert_eq!(
        up_only["nodes"].as_array().unwrap().len(),
        1,
        "UPS 沒有上游，direction 應真的過濾了走訪方向：{up_only}"
    );
    assert!(
        up_only["edges"].as_array().unwrap().is_empty(),
        "節點只有自己時不該有邊：{up_only}"
    );

    // ---- 反向：AHU 往上游應走到 UPS ----
    let (_, ahu_up) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/assets/{SEED_AHU}/dependency-graph?direction=upstream"
            )),
            &token,
        ))
        .await;
    assert!(
        ahu_up["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["asset_code"] == SEED_UPS_CODE),
        "AHU 的上游應包含 UPS：{ahu_up}"
    );

    // ---- depth／direction 超界 → 422（不默默夾住，上界是為了保護資料庫）----
    for uri in [
        format!("/api/v1/assets/{SEED_UPS}/dependency-graph?depth=0"),
        format!("/api/v1/assets/{SEED_UPS}/dependency-graph?depth=6"),
        format!("/api/v1/assets/{SEED_UPS}/dependency-graph?direction=sideways"),
    ] {
        let (status, body) = ctx.send(authed(get_request(&uri), &token)).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{uri} 應回 422：{body}"
        );
    }

    // ---- 不存在的設備 → 404（而非空圖）----
    let (status, _) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/assets/{}/dependency-graph",
                uuid::Uuid::new_v4()
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // ================= 回到刪除流程 =================

    let (status, body) = ctx
        .send(authed(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/assets/{asset_id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "有子設備時應回 409: {body}");
    assert_eq!(body["code"], "CONFLICT");

    // ---- 先刪子設備，再刪父設備 → 204 ----
    for id in [&child_id, &asset_id] {
        let (status, body) = ctx
            .send(authed(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/assets/{id}"))
                    .body(Body::empty())
                    .unwrap(),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "delete {id} failed: {body}");
    }

    // ---- 軟刪除後查不到（deleted_at IS NULL 過濾生效）----
    let (status, _) = ctx
        .send(authed(
            get_request(&format!("/api/v1/assets/{asset_id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "軟刪除後應查不到");

    ctx.teardown().await;
}
