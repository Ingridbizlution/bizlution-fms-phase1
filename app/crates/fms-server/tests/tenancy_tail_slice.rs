//! 組織與空間節點的單筆讀寫刪。
//!
//! # `b_` 是這一組存在的理由
//!
//! 001／003 的 re-path 觸發器只擋「把自己設成自己的父節點」，而**把一個節點搬到
//! 它自己的後代底下不會報錯，只會讓兩者互為祖先**。量出來的：
//!
//! ```text
//! CYC_A | CYC_A.CYC_B.CYC_A       | parent = CYC_B
//! CYC_B | CYC_A.CYC_B.CYC_A.CYC_B | parent = CYC_A
//! ```
//!
//! 而 ltree 的 `<@` 是整個系統做子樹彙總的方式 —— `report_group_rollup` 就是
//! 用它算集團彙總的。損毀之後那些數字永遠是錯的，而症狀不是錯誤而是**數字不對**。
//!
//! migration 069 把守衛補在觸發器裡（不是應用層 —— re-path 本身就是觸發器做的，
//! 任何寫入者都會經過它）。`b_` 從 HTTP 這一層確認它真的擋得住，並且**合法的
//! 搬移仍然帶著子樹一起走** —— 少了後半，一個「什麼都擋」的守衛也會通過。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn req(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

/// 建一個組織（走 API，這樣路徑由觸發器算）。
async fn new_org(ctx: &TestContext, token: &str, code: &str, parent: Option<&str>) -> String {
    let mut body = json!({ "code": code, "name": code, "org_type": "DEPARTMENT" });
    if let Some(p) = parent {
        body["parent_id"] = json!(p);
    }
    let (status, created) = ctx
        .send(authed(
            json_request("POST", "/api/v1/organizations", body),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "建 {code} 失敗：{created}");
    created["id"].as_str().expect("id").to_string()
}

async fn org_path(ctx: &TestContext, token: &str, id: &str) -> String {
    let (status, body) = ctx
        .send(authed(get(&format!("/api/v1/organizations/{id}")), token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["org_path"].as_str().expect("org_path").to_string()
}

/// GET／PATCH 一輪，含改名（`code` 變動會重編路徑）。
#[tokio::test]
async fn a_get_and_patch_an_organization() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let a = new_org(ctx, &token, "TL_A", None).await;
    let b = new_org(ctx, &token, "TL_B", Some(&a)).await;

    assert_eq!(org_path(ctx, &token, &a).await, "TL_A");
    assert_eq!(org_path(ctx, &token, &b).await, "TL_A.TL_B");

    // 改 code → 自己與子樹的路徑都要重編。
    let (status, updated) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/organizations/{a}"),
                json!({ "code": "TL_A2", "name": "改名後" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["code"], "TL_A2");
    assert_eq!(updated["name"], "改名後");
    assert_eq!(updated["org_path"], "TL_A2");
    assert_eq!(
        org_path(ctx, &token, &b).await,
        "TL_A2.TL_B",
        "**子樹的路徑要跟著改**：001 的觸發器用一個 UPDATE 重編整棵子樹"
    );

    // 送 null 清空；不送不動。
    let (_, cleared) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/organizations/{a}"),
                json!({ "cost_center": null }),
            ),
            &token,
        ))
        .await;
    assert_eq!(cleared["cost_center"], Value::Null);
    assert_eq!(cleared["name"], "改名後", "沒送的欄位不該被重設");

    // 不存在 → 404。
    let (status, _) = ctx
        .send(authed(
            get("/api/v1/organizations/00000000-0000-4000-8000-000000000000"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// **搬到自己的後代底下要被擋，而合法的搬移要帶著子樹走。**
#[tokio::test]
async fn b_moving_a_node_under_its_own_descendant_is_rejected() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let a = new_org(ctx, &token, "CY_A", None).await;
    let b = new_org(ctx, &token, "CY_B", Some(&a)).await;
    let c = new_org(ctx, &token, "CY_C", Some(&b)).await;

    // (1) 搬到直接子節點底下 → 422，而且錯誤碼要說出是循環。
    let (status, p) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/organizations/{a}"),
                json!({ "parent_id": b }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "**把組織搬到它的子節點底下不能成功。** 在 migration 069 之前這不會報錯，\
         只會讓兩者互為祖先，而 <@ 從此回錯的答案：{p}"
    );
    assert_eq!(p["errors"][0]["code"], "TREE_CYCLE", "{p}");
    assert_eq!(p["errors"][0]["pointer"], "/parent_id", "{p}");

    // (2) 搬到**孫**節點底下 → 一樣要被擋。只比對直接子節點的實作會漏掉它。
    let (status, p) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/organizations/{a}"),
                json!({ "parent_id": c }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "孫節點也是後代：{p}"
    );

    // (3) 自己當自己的父節點 → 422（001 原本就擋的那一條）。
    let (status, p) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/organizations/{a}"),
                json!({ "parent_id": a }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{p}");

    // (4) 路徑一個都沒被改壞。
    assert_eq!(org_path(ctx, &token, &a).await, "CY_A");
    assert_eq!(org_path(ctx, &token, &b).await, "CY_A.CY_B");
    assert_eq!(org_path(ctx, &token, &c).await, "CY_A.CY_B.CY_C");

    // (5) **合法的搬移仍然要成功，而且子樹要跟著走。**
    //     少了這一格，一個「什麼都擋」的守衛也會通過前面四格。
    let (status, moved) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/organizations/{b}"),
                json!({ "parent_id": null }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "升為根節點是合法的：{moved}");
    assert_eq!(moved["org_path"], "CY_B");
    assert_eq!(
        org_path(ctx, &token, &c).await,
        "CY_B.CY_C",
        "**孫節點要跟著搬** —— 守衛不該把合法的重編也擋掉"
    );

    // (6) 不存在的父節點 → 422（而不是 500）。
    let (status, p) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/organizations/{c}"),
                json!({ "parent_id": "00000000-0000-4000-8000-000000000000" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{p}");

    ctx.teardown().await;
}

/// 觸發器算出來的欄位不可指定。
#[tokio::test]
async fn c_derived_fields_cannot_be_set_by_hand() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let a = new_org(ctx, &token, "DV_A", None).await;

    for f in ["org_path", "depth", "id", "tenant_id"] {
        let (status, p) = ctx
            .send(authed(
                json_request(
                    "PATCH",
                    &format!("/api/v1/organizations/{a}"),
                    json!({ f: "X" }),
                ),
                &token,
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{f}` 不該可指定 —— 手動給值會讓它與樹的實際結構不一致：{p}"
        );
        assert_eq!(p["errors"][0]["code"], "DERIVED", "{p}");
    }

    // 未知欄位（打錯字）→ 與 DERIVED 不同的錯誤碼。
    let (status, p) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/organizations/{a}"),
                json!({ "org_typ": "TEAM" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{p}");
    assert_eq!(p["errors"][0]["code"], "UNKNOWN_FIELD", "{p}");

    // 空的 PATCH。
    let (status, _) = ctx
        .send(authed(
            json_request("PATCH", &format!("/api/v1/organizations/{a}"), json!({})),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    ctx.teardown().await;
}

/// **刪除的阻擋物各自回報數字，而軟刪除不毀掉別的東西。**
#[tokio::test]
async fn d_deleting_reports_each_blocker_separately() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let a = new_org(ctx, &token, "DEL_A", None).await;
    let b = new_org(ctx, &token, "DEL_B", Some(&a)).await;

    // 有子組織 → 409，而且數字要在。
    let (status, p) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/organizations/{a}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{p}");
    let errs = p["errors"].as_array().expect("errors");
    let child = errs
        .iter()
        .find(|e| e["code"] == "HAS_CHILDREN")
        .expect("該有 HAS_CHILDREN");
    assert_eq!(
        child["message"], "1",
        "**每個阻擋物要各自回報數字** —— 子組織與設施要做的處理完全不同，\
         只回一個沒有內容的 409 會讓呼叫者一個一個猜：{p}"
    );

    // 子組織刪掉之後就可以刪父組織了。
    let (status, deleted_b) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/organizations/{b}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted_b}");
    assert_eq!(deleted_b["data"]["deleted"], true);
    assert_eq!(deleted_b["meta"]["soft_delete"], true);

    let (status, deleted_a) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/organizations/{a}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted_a}");

    // 軟刪除之後讀不到（既有的 get／list 都帶 deleted_at IS NULL）。
    let (status, _) = ctx
        .send(authed(get(&format!("/api/v1/organizations/{a}")), &token))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "軟刪除該讓那一列消失");

    // 再刪一次 → 404。
    let (status, _) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/organizations/{a}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // **有設施的組織刪不掉。** 示範資料的 HQ 組織底下有設施。
    let mut tx = ctx.owner_tx().await;
    let with_facility: uuid::Uuid =
        sqlx::query_scalar("SELECT org_id FROM fms.facilities WHERE id = $1::uuid")
            .bind(FACILITY_HQ)
            .fetch_one(&mut *tx)
            .await
            .expect("查 HQ 的組織");
    tx.commit().await.expect("commit");

    let (status, p) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/organizations/{with_facility}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{p}");
    assert!(
        p["errors"]
            .as_array()
            .expect("errors")
            .iter()
            .any(|e| e["code"] == "HAS_FACILITIES" && e["message"] != "0"),
        "設施數要回報出來：{p}"
    );

    ctx.teardown().await;
}

/// 空間節點：讀寫刪一輪，含型別碼驗證與 `facility_id` 不可變更。
#[tokio::test]
async fn e_spatial_node_round_trip() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 建兩層。
    let (status, root) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/facilities/{FACILITY_HQ}/spatial-nodes"),
                json!({ "code": "TN_BLD", "name": "測試大樓",
                        "node_type_code": "BUILDING" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{root}");
    let root_id = root["id"].as_str().expect("id").to_string();

    let (status, child) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/facilities/{FACILITY_HQ}/spatial-nodes"),
                json!({ "code": "TN_FL1", "name": "1F",
                        "node_type_code": "FLOOR", "parent_id": root_id }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{child}");
    let child_id = child["id"].as_str().expect("id").to_string();
    assert_eq!(child["node_path"], "TN_BLD.TN_FL1");
    assert_eq!(child["depth"], 1);

    // GET 單筆。
    let (status, got) = ctx
        .send(authed(
            get(&format!("/api/v1/spatial-nodes/{child_id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["code"], "TN_FL1");

    // PATCH：改 code → 路徑與 depth 都要重算。
    let (status, patched) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/spatial-nodes/{root_id}"),
                json!({ "code": "TN_BLD2", "capacity": 120 }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["node_path"], "TN_BLD2");
    assert_eq!(patched["capacity"], 120);

    let (_, child_after) = ctx
        .send(authed(
            get(&format!("/api/v1/spatial-nodes/{child_id}")),
            &token,
        ))
        .await;
    assert_eq!(
        child_after["node_path"], "TN_BLD2.TN_FL1",
        "子節點的路徑要跟著改：{child_after}"
    );

    // 循環守衛在這棵樹上也有效。
    let (status, p) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/spatial-nodes/{root_id}"),
                json!({ "parent_id": child_id }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "**spatial_nodes 的循環也要擋** —— 只改 organizations 會留下一個\
         「組織不會壞但空間會壞」的系統：{p}"
    );
    assert_eq!(p["errors"][0]["code"], "TREE_CYCLE", "{p}");

    // `facility_id` 不可變更。
    let (status, p) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/spatial-nodes/{child_id}"),
                json!({ "facility_id": "cccccccc-0000-4000-8000-000000000002" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{p}");
    assert_eq!(p["errors"][0]["code"], "DERIVED", "{p}");

    // 型別碼要驗（那一欄沒有外鍵）。
    let (status, p) = ctx
        .send(authed(
            json_request(
                "PATCH",
                &format!("/api/v1/spatial-nodes/{child_id}"),
                json!({ "node_type_code": "NOT_A_TYPE" }),
            ),
            &token,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "node_type_code 沒有外鍵，所以打錯字必須在應用層擋：{p}"
    );

    // DELETE：有子節點 → 409，數字要在。
    let (status, p) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/spatial-nodes/{root_id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{p}");
    assert!(
        p["errors"]
            .as_array()
            .expect("errors")
            .iter()
            .any(|e| e["code"] == "HAS_CHILDREN" && e["message"] == "1"),
        "{p}"
    );

    // 子節點刪掉 → 父節點可刪，而 meta 說出還有什麼指著它。
    let (status, d1) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/spatial-nodes/{child_id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{d1}");
    let (status, d2) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/spatial-nodes/{root_id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{d2}");
    assert_eq!(d2["meta"]["soft_delete"], true);
    assert!(
        d2["meta"]["assets_still_referencing"].as_i64().is_some(),
        "**軟刪除的重點就是它們沒有被連帶毀掉**，所以要說出還有什麼指著它：{}",
        d2["meta"]
    );

    ctx.teardown().await;
}

/// 有未結工單的節點刪不掉，而那是與子節點不同的阻擋物。
#[tokio::test]
async fn f_an_open_work_order_blocks_deleting_its_node() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (_, node) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/facilities/{FACILITY_HQ}/spatial-nodes"),
                json!({ "code": "WO_NODE", "name": "有工單的房間",
                        "node_type_code": "ROOM" }),
            ),
            &token,
        ))
        .await;
    let node_id = node["id"].as_str().expect("id").to_string();

    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            "INSERT INTO fms.work_orders
               (tenant_id, facility_id, wo_no, work_order_type, source, title,
                status, priority, spatial_node_id)
             VALUES ($1::uuid, $2::uuid,
                     'WO-ND-' || substr(md5(random()::text), 1, 8),
                     'CORRECTIVE', 'MANUAL', '房間維修', 'IN_PROGRESS', 'MEDIUM',
                     $3::uuid)",
        )
        .bind(TENANT_ID)
        .bind(FACILITY_HQ)
        .bind(&node_id)
        .execute(&mut *tx)
        .await
        .expect("建工單");
        tx.commit().await.expect("commit");
    }

    let (status, p) = ctx
        .send(authed(
            req("DELETE", &format!("/api/v1/spatial-nodes/{node_id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{p}");
    assert!(
        p["errors"]
            .as_array()
            .expect("errors")
            .iter()
            .any(|e| e["code"] == "HAS_OPEN_WORK_ORDERS" && e["message"] == "1"),
        "未結工單是與子節點**不同**的阻擋物，要各自回報：{p}"
    );
    // 而子節點數是 0 —— 三個數字要分得開。
    assert!(
        p["errors"]
            .as_array()
            .expect("errors")
            .iter()
            .any(|e| e["code"] == "HAS_CHILDREN" && e["message"] == "0"),
        "{p}"
    );

    ctx.teardown().await;
}

/// 權限。
#[tokio::test]
async fn g_permissions_are_enforced() {
    let ctx = &TestContext::setup().await;
    let requester = ctx.login_as(USERNAME_REQUESTER).await;
    let admin = ctx.login().await;
    let a = new_org(ctx, &admin, "PERM_A", None).await;

    for r in [
        json_request(
            "PATCH",
            &format!("/api/v1/organizations/{a}"),
            json!({ "name": "x" }),
        ),
        req("DELETE", &format!("/api/v1/organizations/{a}")),
    ] {
        let (status, _) = ctx.send(authed(r, &requester)).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "REQUESTER 沒有 organization:write"
        );
    }

    ctx.teardown().await;
}
