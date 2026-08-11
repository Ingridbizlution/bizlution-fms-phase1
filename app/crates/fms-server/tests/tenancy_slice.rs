//! 組織、場域、空間節點（Tenancy／Spatial）。
//!
//! 本切片的意義是**能不能只用 API 開通一個租戶**：在它之前，
//! `facility_id` 與 `spatial_node_id` 只能來自種子資料。
//!
//! 重點驗證 003／001 兩個 `ltree` 觸發器，它們先前從未被執行過：
//!   * `node_path` 與 `depth` 由 `parent_id + code` 推導
//!   * **搬移子樹時整棵子樹的路徑與深度都重算**（最容易寫錯的一類 SQL）
//!   * 搬移後掛在下面的設備的 `subtree_of_node` 查詢仍然正確

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

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

/// 唯一入口：兩個場景依序執行（同檔案的測試會平行跑，而 setup 的清理
/// 會刪掉對方建立的組織與場域）。
#[tokio::test]
async fn tenancy_slice_end_to_end() {
    let ctx = TestContext::setup().await;
    let ids = onboard_a_tenant_through_the_api(&ctx).await;
    ltree_triggers_maintain_paths_and_subtree_moves(&ctx, &ids).await;
    ctx.teardown().await;
}

struct Created {
    facility_id: String,
    floor: String,
    room: String,
    other_floor: String,
}

async fn onboard_a_tenant_through_the_api(ctx: &TestContext) -> Created {
    let token = ctx.login().await;
    let suffix = uuid::Uuid::new_v4().to_string()[..8].to_string();

    // ---- 組織：建一個根組織 ----
    let (status, root) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/organizations",
                json!({
                    "code": format!("TNTEST_{suffix}"),
                    "name": "測試集團",
                    "org_type": "GROUP"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{root}");
    assert_eq!(
        root["org_path"],
        format!("TNTEST_{suffix}"),
        "根組織的 org_path 應等於 code（由 trg_organization_path 推導）：{root}"
    );
    assert_eq!(root["depth"], 0, "根的深度是 0：{root}");
    assert_eq!(root["facility_count"], 0);
    let root_id = root["id"].as_str().unwrap().to_string();

    // ---- 子組織：org_path 應是「父.子」----
    let (status, child) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/organizations",
                json!({
                    "parent_id": root_id,
                    "code": format!("TNPROP_{suffix}"),
                    "name": "物業部",
                    "org_type": "DEPARTMENT",
                    "cost_center": "CC-100"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{child}");
    assert_eq!(
        child["org_path"],
        format!("TNTEST_{suffix}.TNPROP_{suffix}"),
        "子組織的路徑應由觸發器接在父路徑後：{child}"
    );
    assert_eq!(child["depth"], 1);
    let child_org = child["id"].as_str().unwrap().to_string();

    // ---- ltree 標籤限制：帶連字號的 code 會被觸發器換成底線，
    //      造成兩個不同 code 撞同一個路徑，因此應在應用層先擋 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/organizations",
                json!({ "code": "BAD-CODE", "name": "x", "org_type": "TEAM" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "ltree 標籤只允許字母數字與底線，應在應用層擋下：{body}"
    );

    // ---- 不存在的父組織 → 422 而非 500 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/organizations",
                json!({
                    "parent_id": uuid::Uuid::new_v4(),
                    "code": "ORPHAN", "name": "x", "org_type": "TEAM"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 列表：依 org_path 排序，父必然在子之前 ----
    let (status, orgs) = ctx
        .send(authed(
            get_request("/api/v1/organizations?limit=200"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{orgs}");
    let paths: Vec<&str> = orgs["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["org_path"].as_str().unwrap())
        .collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(
        paths, sorted,
        "依 org_path 排序讓前端能線性掃過就組出樹：{orgs}"
    );

    // ---- 場域 ----
    let (status, facility) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/facilities",
                json!({
                    "org_id": child_org,
                    "code": format!("TNFAC_{suffix}"),
                    "name": "測試園區",
                    "facility_type": "OFFICE",
                    "city": "台北",
                    "country_code": "TW",
                    "timezone": "Asia/Taipei",
                    "gross_area_sqm": 12000.5
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{facility}");
    assert_eq!(facility["timezone"], "Asia/Taipei");
    assert_eq!(facility["gross_area_sqm"].as_f64(), Some(12000.5));
    assert!(
        facility["version"].as_i64().is_some(),
        "契約宣告了 version，即使 schema 沒有該欄位也要有值：{facility}"
    );
    let facility_id = facility["id"].as_str().unwrap().to_string();

    // 組織的 facility_count 應跟著變
    let (_, orgs) = ctx
        .send(authed(
            get_request("/api/v1/organizations?limit=200"),
            &token,
        ))
        .await;
    let c = orgs["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == child_org)
        .unwrap();
    assert_eq!(c["facility_count"], 1, "{orgs}");

    // ---- country_code 長度 → 422（CHAR(2) 不該變成 500）----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/facilities",
                json!({
                    "org_id": child_org, "code": "X1", "name": "x",
                    "country_code": "TWN"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 不存在的組織 → 422 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/facilities",
                json!({ "org_id": uuid::Uuid::new_v4(), "code": "X2", "name": "x" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- PATCH ----
    let (status, patched) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/facilities/{facility_id}"),
                json!({ "name": "改名後的園區", "city": "新竹" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["name"], "改名後的園區");
    assert_eq!(patched["city"], "新竹");
    assert_eq!(
        patched["code"], facility["code"],
        "未提供的欄位應保持原值：{patched}"
    );

    // ---- 單筆讀取與 404 ----
    let (status, one) = ctx
        .send(authed(
            get_request(&format!("/api/v1/facilities/{facility_id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    let (status, _) = ctx
        .send(authed(
            get_request(&format!("/api/v1/facilities/{}", uuid::Uuid::new_v4())),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // ---- 空間樹：BLDG → FL → ROOM ----
    let mk = |parent: Option<&str>, code: &str, name: &str, ntype: &str, floor: Option<i32>| {
        let mut b = json!({ "node_type_code": ntype, "code": code, "name": name });
        if let Some(p) = parent {
            b["parent_id"] = json!(p);
        }
        if let Some(f) = floor {
            b["floor_level"] = json!(f);
        }
        b
    };

    let (status, building) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/facilities/{facility_id}/spatial-nodes"),
                mk(None, "TB", "測試大樓", "BUILDING", None),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{building}");
    assert_eq!(building["node_path"], "TB", "根節點路徑等於 code");
    assert_eq!(building["depth"], 0);
    let building_id = building["id"].as_str().unwrap().to_string();

    let (status, floor) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/facilities/{facility_id}/spatial-nodes"),
                mk(Some(&building_id), "FL01", "1 樓", "FLOOR", Some(1)),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{floor}");
    assert_eq!(floor["node_path"], "TB.FL01");
    assert_eq!(floor["depth"], 1);
    let floor_id = floor["id"].as_str().unwrap().to_string();

    let (status, room) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/facilities/{facility_id}/spatial-nodes"),
                mk(
                    Some(&floor_id),
                    "R101",
                    "101 會議室",
                    "MEETING_ROOM",
                    Some(1),
                ),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{room}");
    assert_eq!(
        room["node_path"], "TB.FL01.R101",
        "三層路徑應由觸發器逐層接起：{room}"
    );
    assert_eq!(room["depth"], 2);
    let room_id = room["id"].as_str().unwrap().to_string();

    // 第二個樓層，供搬移測試使用
    let (status, other) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/facilities/{facility_id}/spatial-nodes"),
                mk(Some(&building_id), "FL02", "2 樓", "FLOOR", Some(2)),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{other}");
    let other_floor_id = other["id"].as_str().unwrap().to_string();

    // ---- 未知的 node_type_code → 422 ----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/facilities/{facility_id}/spatial-nodes"),
                mk(None, "ZZ", "x", "NO_SUCH_TYPE", None),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // ---- 跨場域的父節點 → 422（觸發器不檢查這一點）----
    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/facilities/cccccccc-0000-4000-8000-000000000001/spatial-nodes",
                mk(Some(&floor_id), "XFAC", "x", "MEETING_ROOM", None),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "跨場域父子關係會讓 (facility_id, node_path) 唯一索引失去意義：{body}"
    );

    // ---- view=tree：巢狀結構 ----
    let (status, tree) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/facilities/{facility_id}/spatial-nodes?view=tree&limit=200"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{tree}");
    let roots = tree["data"].as_array().unwrap();
    assert_eq!(roots.len(), 1, "只有一個根（大樓）：{tree}");
    assert_eq!(roots[0]["code"], "TB");
    let floors = roots[0]["children"].as_array().expect("大樓應有子節點");
    assert_eq!(floors.len(), 2, "兩個樓層：{tree}");
    let f1 = floors.iter().find(|f| f["code"] == "FL01").unwrap();
    assert_eq!(
        f1["children"].as_array().unwrap().len(),
        1,
        "1 樓下有一間會議室：{tree}"
    );

    // ---- view=flat（預設）：扁平且附 node_path ----
    let (_, flat) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/facilities/{facility_id}/spatial-nodes?limit=200"
            )),
            &token,
        ))
        .await;
    assert!(
        flat["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n.get("children").is_none()),
        "flat 視圖不該有 children 欄位：{flat}"
    );

    // ---- bookable_only 與 floor_level 過濾 ----
    let (_, by_floor) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/facilities/{facility_id}/spatial-nodes?floor_level=1&limit=200"
            )),
            &token,
        ))
        .await;
    assert!(
        by_floor["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["floor_level"] == 1),
        "{by_floor}"
    );

    // ---- 不合法的 view → 422 ----
    let (status, body) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/facilities/{facility_id}/spatial-nodes?view=graph"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    Created {
        facility_id,
        floor: floor_id,
        room: room_id,
        other_floor: other_floor_id,
    }
}

/// 子樹搬移：003 的 `trg_spatial_node_path` 唯一有意義的驗證方式。
///
/// 契約沒有搬移端點，因此經 repo 的 `move_node` 呼叫 —— 被驗證的對象是
/// 資料庫觸發器，而那是它的正式入口。
async fn ltree_triggers_maintain_paths_and_subtree_moves(ctx: &TestContext, ids: &Created) {
    let token = ctx.login().await;
    let facility = uuid::Uuid::parse_str(&ids.facility_id).unwrap();
    let floor = uuid::Uuid::parse_str(&ids.floor).unwrap();
    let other_floor = uuid::Uuid::parse_str(&ids.other_floor).unwrap();

    // 在會議室下掛一台設備，用來驗證搬移後 subtree 查詢仍然正確
    let asset_code = format!("TEST-TN-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let (status, asset) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/assets",
                json!({
                    "facility_id": ids.facility_id,
                    "spatial_node_id": ids.room,
                    "category_code": "FCU",
                    "asset_code": asset_code,
                    "name": "搬移測試用風機盤管"
                }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{asset}");
    assert_eq!(
        asset["spatial_node_path"], "TB.FL01.R101",
        "設備的 spatial_node_path 應反映當前樹狀位置：{asset}"
    );
    let asset_id = asset["id"].as_str().unwrap().to_string();

    // ---- 搬移：把 1 樓整層改掛到 2 樓底下 ----
    // （不合理的建築結構，但正是要測「中間節點帶著子樹搬走」）
    let mut tx = ctx.tenant_tx_mut().await;
    let moved = fms_tenancy::repo::move_node(&mut tx, floor, Some(other_floor))
        .await
        .expect("move node");
    assert_eq!(moved, 1, "應更新一列（觸發器負責其餘子樹）");
    tx.commit().await.expect("commit move");

    // ---- 整棵子樹的路徑與深度都要重算 ----
    let (status, flat) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/facilities/{facility}/spatial-nodes?limit=200"
            )),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{flat}");
    let nodes = flat["data"].as_array().unwrap();
    let find = |code: &str| {
        nodes
            .iter()
            .find(|n| n["code"] == code)
            .unwrap_or_else(|| panic!("找不到 {code}：{flat}"))
    };

    assert_eq!(
        find("FL01")["node_path"],
        "TB.FL02.FL01",
        "被搬移的節點路徑要重算：{flat}"
    );
    assert_eq!(find("FL01")["depth"], 2, "深度也要重算：{flat}");
    assert_eq!(
        find("R101")["node_path"],
        "TB.FL02.FL01.R101",
        "**子節點的路徑必須跟著重算** —— 這是 trg_spatial_node_path 的 \
         subpath() 那一段，也是最容易寫錯的地方：{flat}"
    );
    assert_eq!(find("R101")["depth"], 3, "子節點深度也要重算：{flat}");
    assert_eq!(find("FL02")["node_path"], "TB.FL02", "未受影響的節點不該變");

    // ---- 設備的 spatial_node_path 與 subtree 查詢都要跟著正確 ----
    let (_, asset_after) = ctx
        .send(authed(
            get_request(&format!("/api/v1/assets/{asset_id}")),
            &token,
        ))
        .await;
    assert_eq!(
        asset_after["spatial_node_path"], "TB.FL02.FL01.R101",
        "設備讀到的路徑是 JOIN 出來的，搬移後應自動反映：{asset_after}"
    );

    // 以 2 樓為根的 subtree 查詢現在應該找得到那台設備（搬移前不會）
    let (_, subtree) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/assets?subtree_of_node={other_floor}&limit=200"
            )),
            &token,
        ))
        .await;
    assert!(
        subtree["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == asset_id),
        "搬移後 2 樓的子樹應涵蓋那台設備 —— ltree 的 <@ 查詢依賴重算後的路徑：{subtree}"
    );

    // ---- 自己當自己的父節點 → 觸發器擋下（23514 → 409）----
    let mut tx = ctx.tenant_tx_mut().await;
    let err = fms_tenancy::repo::move_node(&mut tx, floor, Some(floor)).await;
    assert!(err.is_err(), "節點不能是自己的父節點，觸發器應拋錯");
    drop(tx);

    // ---- tree 視圖在搬移後仍然正確（三層變四層）----
    let (_, tree) = ctx
        .send(authed(
            get_request(&format!(
                "/api/v1/facilities/{facility}/spatial-nodes?view=tree&limit=200"
            )),
            &token,
        ))
        .await;
    let root = &tree["data"][0];
    assert_eq!(root["code"], "TB");
    let fl02 = root["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["code"] == "FL02")
        .unwrap_or_else(|| panic!("{tree}"));
    let fl01 = fl02["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["code"] == "FL01")
        .unwrap_or_else(|| panic!("搬移後 FL01 應在 FL02 之下：{tree}"));
    assert_eq!(
        fl01["children"].as_array().unwrap()[0]["code"],
        "R101",
        "整棵子樹都要跟著搬：{tree}"
    );
}
