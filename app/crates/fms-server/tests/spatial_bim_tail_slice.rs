//! Spatial & BIM 補完六支。
//!
//! # `b_` 是這一組存在的理由
//!
//! 匯入檔的父節點用 `parent_code` 指定，而**子節點可以出現在父節點之前** ——
//! 匯入檔是從別的系統匯出的，順序不由呼叫者決定。`b_` 送一份倒序的檔案，
//! 斷言它整份成功；再送一份互相引用成環的，斷言它停在 `UNRESOLVED_PARENT`
//! 而不是寫出一棵壞掉的樹。
//!
//! # `e_` 盯的是三件事必須一起做
//!
//! `mappings` 是 `mapped_node_count`／`mapped_asset_count`／
//! `unresolved_elements` 的第一個寫入者。少了「從 unresolved 移除」，同一個
//! 元件會永遠留在待補正清單裡；少了「重算計數」，畫面上的「已對應 12 個」
//! 是假的。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
const FACILITY_CINEMA: &str = "cccccccc-0000-4000-8000-000000000002";

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

async fn import(ctx: &TestContext, token: &str, body: Value) -> (StatusCode, Value) {
    ctx.send(authed(
        json_request(
            "POST",
            &format!("/api/v1/facilities/{FACILITY_HQ}/spatial-nodes:bulk-import"),
            body,
        ),
        token,
    ))
    .await
}

/// 註冊一個 BIM 模型。
///
/// **`storage_key` 必須來自 `POST /uploads/presign`** —— 註冊端點只收它自己
/// 發出的那一個（`bim_slice.rs` 的 `c_` 驗過那條防線：偷別的租戶的 key 會被拒）。
/// 第一版直接塞一個 `"bim/hq/a.ifc"` 字串，得到 422 —— 那個 422 是對的。
///
/// 註冊回 `202` 而不是 `201`：解析是非同步的（`bim-worker` 輪詢處理），
/// 模型註冊當下停在 `UPLOADED`，所以「已接受但還沒處理完」比「已建立」誠實。
async fn register_model(ctx: &TestContext, token: &str, name: &str) -> String {
    let (status, pre) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/uploads/presign",
                json!({ "file_name": "tower.ifc",
                        "content_type": "application/octet-stream",
                        "content_length": 12345 }),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "presign 失敗：{pre}");
    let key = pre["storage_key"]
        .as_str()
        .expect("storage_key")
        .to_string();

    let (status, model) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/facilities/{FACILITY_HQ}/bim-models"),
                json!({ "name": name, "source_format": "IFC",
                        "storage_key": key, "discipline": "STRUCT" }),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "註冊模型失敗：{model}");
    model["id"].as_str().expect("id").to_string()
}

/// 型別目錄：平台預設都在，而 `allowed_child_codes` 只是建議。
#[tokio::test]
async fn a_node_types_are_the_only_source_of_valid_codes() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, body) = ctx
        .send(authed(get("/api/v1/spatial-node-types"), &token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data");
    assert!(rows.len() >= 10, "003 種了十幾個平台型別：{}", rows.len());

    let codes: Vec<&str> = rows.iter().filter_map(|r| r["code"].as_str()).collect();
    for c in ["BUILDING", "FLOOR", "ROOM"] {
        assert!(codes.contains(&c), "{c} 該在型別目錄裡：{codes:?}");
    }
    for r in rows {
        assert!(r["is_platform"].is_boolean(), "{r}");
        assert!(r["level_hint"].is_i64(), "{r}");
        assert!(r["allowed_child_codes"].is_array(), "{r}");
    }
    assert_eq!(
        body["meta"]["no_foreign_key_on_node_type_code"], true,
        "要說出這份清單是唯一的合法值來源：{}",
        body["meta"]
    );
    assert_eq!(
        body["meta"]["allowed_child_codes_is_advisory_only"], true,
        "**`allowed_child_codes` 沒有執行者** —— 後端不擋「ROOM 掛在 DESK 底下」，\
         那件事必須說出來，否則前端會以為後端保證了樹的形狀：{}",
        body["meta"]
    );

    ctx.teardown().await;
}

/// **匯入檔的順序不重要，而環會被擋住。**
#[tokio::test]
async fn b_children_may_precede_their_parents_but_cycles_cannot_resolve() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 倒序：孫 → 子 → 父。三趟才解得完。
    let rows = json!([
        { "code": "IMP_R301", "name": "301", "node_type_code": "ROOM",
          "parent_code": "IMP_FL3" },
        { "code": "IMP_FL3", "name": "3F", "node_type_code": "FLOOR",
          "parent_code": "IMP_BLDG", "floor_level": 3 },
        { "code": "IMP_BLDG", "name": "測試棟", "node_type_code": "BUILDING" }
    ]);

    // 先 dry-run：全部該是 WOULD_CREATE，而且什麼都沒寫進去。
    let (status, dry) = import(ctx, &token, json!({ "rows": rows })).await;
    assert_eq!(status, StatusCode::OK, "{dry}");
    assert_eq!(dry["meta"]["dry_run"], true, "**預設是 dry-run**");
    assert_eq!(dry["meta"]["created"], 3, "{}", dry["meta"]);
    assert_eq!(dry["meta"]["unresolved_parent"], 0, "{}", dry["meta"]);
    for o in dry["data"].as_array().expect("data") {
        assert_eq!(o["status"], "WOULD_CREATE", "{o}");
        // dry-run 也真的 INSERT 過，所以路徑算得出來 —— 那正是它能驗到
        // 唯一性與循環守衛的原因。
        assert!(o["node_path"].as_str().is_some(), "{o}");
    }
    // 真的沒寫進去。
    let mut tx = ctx.owner_tx().await;
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM fms.spatial_nodes WHERE code LIKE 'IMP\\_%'")
            .fetch_one(&mut *tx)
            .await
            .expect("查");
    tx.commit().await.expect("commit");
    assert_eq!(n, 0, "dry-run 不該留下任何列");

    // 真跑。
    let (status, real) = import(ctx, &token, json!({ "rows": rows, "dry_run": false })).await;
    assert_eq!(status, StatusCode::OK, "{real}");
    assert_eq!(
        real["meta"]["created"], 3,
        "**子節點出現在父節點之前也要成功** —— 匯入檔的順序不由呼叫者決定：{}",
        real["meta"]
    );
    let room = real["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|o| o["code"] == "IMP_R301")
        .expect("該有 IMP_R301");
    assert_eq!(
        room["node_path"], "IMP_BLDG.IMP_FL3.IMP_R301",
        "路徑要是三層（多趟解析真的解對了）：{room}"
    );

    // ---- 環：兩列互相引用 ----
    let (status, cyc) = import(
        ctx,
        &token,
        json!({ "rows": [
            { "code": "CY_X", "name": "X", "node_type_code": "ZONE", "parent_code": "CY_Y" },
            { "code": "CY_Y", "name": "Y", "node_type_code": "ZONE", "parent_code": "CY_X" }
        ], "dry_run": false }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cyc}");
    assert_eq!(
        cyc["meta"]["created"], 0,
        "**互相引用的兩列一個都建不出來** —— 停在「沒有進展」比寫出壞掉的樹好：{}",
        cyc["meta"]
    );
    assert_eq!(cyc["meta"]["unresolved_parent"], 2, "{}", cyc["meta"]);
    for o in cyc["data"].as_array().expect("data") {
        assert_eq!(o["status"], "UNRESOLVED_PARENT", "{o}");
        assert_eq!(o["error_code"], "UNRESOLVED_PARENT", "{o}");
        assert!(
            o["error"].as_str().is_some_and(|e| e.contains("環")),
            "原因要說出可能是環：{o}"
        );
    }

    // 父節點指向這個場域裡既有的節點 → 掛得上去。
    let (status, hang) = import(
        ctx,
        &token,
        json!({ "rows": [
            { "code": "IMP_R302", "name": "302", "node_type_code": "ROOM",
              "parent_code": "IMP_FL3" }
        ], "dry_run": false }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{hang}");
    assert_eq!(
        hang["meta"]["created"], 1,
        "**parent_code 也可以指向既有的節點** —— 匯入檔要掛得上既有的樹：{}",
        hang["meta"]
    );

    ctx.teardown().await;
}

/// 匯入的四種失敗各自回報，而檔案內部重複與資料庫既有重複是不同的錯誤。
#[tokio::test]
async fn c_import_failures_are_classified() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 檔案內部重複 → 422（整批擋，因為那是檔案本身的問題）。
    let (status, p) = import(
        ctx,
        &token,
        json!({ "rows": [
            { "code": "DUP", "name": "a", "node_type_code": "ROOM" },
            { "code": "dup", "name": "b", "node_type_code": "ROOM" }
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{p}");
    assert_eq!(
        p["errors"][0]["code"], "DUPLICATE_IN_FILE",
        "**檔案內部重複與「資料庫已經有這個 code」是不同的錯誤** —— \
         混在一起會讓使用者去查一個不存在的既有節點：{p}"
    );

    // 型別碼不存在 → 422，而且一次列出所有錯的（不是一個一個來）。
    let (status, p) = import(
        ctx,
        &token,
        json!({ "rows": [
            { "code": "BT_1", "name": "a", "node_type_code": "NOPE" },
            { "code": "BT_2", "name": "b", "node_type_code": "ALSO_NOPE" }
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{p}");
    assert_eq!(
        p["errors"].as_array().map(Vec::len),
        Some(2),
        "**一份檔案裡打錯的型別通常不只一個** —— 一次講完比讓他來 20 次好：{p}"
    );

    // 資料庫已經有的 code → 逐列 FAILED（不是整批擋），錯誤碼是 DUPLICATE_CODE。
    import(
        ctx,
        &token,
        json!({ "rows": [{ "code": "EXIST_1", "name": "a", "node_type_code": "ROOM" }],
                "dry_run": false }),
    )
    .await;
    let (status, again) = import(
        ctx,
        &token,
        json!({ "rows": [
            { "code": "EXIST_1", "name": "a", "node_type_code": "ROOM" },
            { "code": "NEW_OK", "name": "b", "node_type_code": "ROOM" }
        ], "dry_run": false }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(
        again["meta"]["created"], 1,
        "**一列失敗不該讓另一列白做**（每列一個 savepoint）：{}",
        again["meta"]
    );
    assert_eq!(again["meta"]["failed"], 1, "{}", again["meta"]);
    let dup = again["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|o| o["code"] == "EXIST_1")
        .expect("該有 EXIST_1");
    assert_eq!(dup["error_code"], "DUPLICATE_CODE", "{dup}");

    // 空 rows、超量、未知欄位。
    for body in [
        json!({ "rows": [] }),
        json!({ "rows": [{ "code": "X", "name": "x", "node_type_code": "ROOM",
                          "unknown_col": 1 }]}),
    ] {
        let (status, _) = import(ctx, &token, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    ctx.teardown().await;
}

/// BIM 模型詳情：`UPLOADED` 的空 unresolved 要說出「還沒解析」。
#[tokio::test]
async fn d_bim_detail_explains_what_an_empty_unresolved_list_means() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let id = register_model(ctx, &token, "A 棟結構").await;

    let (status, body) = ctx
        .send(authed(get(&format!("/api/v1/bim-models/{id}")), &token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let d = &body["data"];
    assert_eq!(d["status"], "UPLOADED", "還沒被 bim-worker 輪到：{d}");
    assert_eq!(d["element_count"], 0);
    assert_eq!(d["unresolved_count"], 0);
    assert_eq!(d["mapped_node_count"], 0);
    assert_eq!(d["discipline"], "STRUCT");
    assert!(d["storage_key"].as_str().is_some(), "{d}");

    // **空的 unresolved 有兩種意思**，說明字串要講出是哪一種。
    let note = body["meta"]["status_explanation"]
        .as_str()
        .expect("status_explanation");
    assert!(
        note.contains("排隊") && note.contains("不代表"),
        "**`UPLOADED` 的空 unresolved 代表「還沒解析完」而不是「全部對應好了」** \
         —— 那個區別必須講出來：{note}"
    );
    assert_eq!(body["meta"]["awaiting_parse"], true);

    // 不存在 → 404。
    let (status, _) = ctx
        .send(authed(
            get("/api/v1/bim-models/00000000-0000-4000-8000-000000000000"),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    ctx.teardown().await;
}

/// **mappings：三件事一起做，而跨場域的目標要被拒。**
#[tokio::test]
async fn e_mappings_write_the_three_fields_that_had_no_writer() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let model_id = register_model(ctx, &token, "對應測試").await;

    // 模擬解析結果：三個待對應元件。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query(
            r#"UPDATE fms.bim_models
                  SET status = 'PARSED', element_count = 3,
                      unresolved_elements =
                        '[{"bim_element_id":"E1"},{"bim_element_id":"E2"},
                          {"bim_element_id":"E3"}]'::jsonb
                WHERE id = $1::uuid"#,
        )
        .bind(&model_id)
        .execute(&mut *tx)
        .await
        .expect("設 unresolved");
        tx.commit().await.expect("commit");
    }

    // 一個總部節點、一個總部設備、一個**影城**的節點（跨場域，該被拒）。
    let mut tx = ctx.owner_tx().await;
    let hq_node: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM fms.spatial_nodes WHERE facility_id = $1::uuid
          AND deleted_at IS NULL ORDER BY id LIMIT 1",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("HQ 節點");
    let cinema_node: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM fms.spatial_nodes WHERE facility_id = $1::uuid
          AND deleted_at IS NULL ORDER BY id LIMIT 1",
    )
    .bind(FACILITY_CINEMA)
    .fetch_one(&mut *tx)
    .await
    .expect("影城節點");
    tx.commit().await.expect("commit");
    let hq_asset = ctx.seed_asset(FACILITY_HQ, "BIM-ASSET").await;

    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/bim-models/{model_id}/mappings"),
                json!({ "mappings": [
                    { "bim_element_id": "E1", "target_type": "SPATIAL_NODE",
                      "target_id": hq_node },
                    { "bim_element_id": "E2", "target_type": "ASSET",
                      "target_id": hq_asset },
                    { "bim_element_id": "E3", "target_type": "SPATIAL_NODE",
                      "target_id": cinema_node }
                ]}),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["meta"]["applied"], 2, "{}", body["meta"]);
    assert_eq!(body["meta"]["rejected"], 1, "{}", body["meta"]);

    // **跨場域的目標要被拒**，而原因要具名。
    let rejected = body["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|o| o["ok"] == false)
        .expect("該有一筆被拒");
    assert_eq!(rejected["bim_element_id"], "E3", "{rejected}");
    assert_eq!(
        rejected["error_code"], "TARGET_NOT_IN_FACILITY",
        "**跨場域的對應會讓 floor-view 把別棟樓的設備畫進這一層** —— \
         而那個錯誤在畫面上看起來只是「位置怪怪的」：{rejected}"
    );

    // 三件事都做了：計數重算、unresolved 移除。
    assert_eq!(
        body["meta"]["mapped_node_count"], 1,
        "計數要重算：{}",
        body["meta"]
    );
    assert_eq!(body["meta"]["mapped_asset_count"], 1, "{}", body["meta"]);
    assert_eq!(
        body["meta"]["unresolved_count"], 0,
        "**已對應的元件要從待補正清單移除** —— 少了這一步，同一個元件會永遠\
         留在清單裡。（E3 被拒但仍然移除：它已經被人工處理過，留著會讓\
         同一個問題被反覆處理。）：{}",
        body["meta"]
    );

    // 從 detail 端點再確認一次（不是只信 mappings 的回應）。
    let (_, detail) = ctx
        .send(authed(
            get(&format!("/api/v1/bim-models/{model_id}")),
            &token,
        ))
        .await;
    assert_eq!(detail["data"]["mapped_node_count"], 1, "{}", detail["data"]);
    assert_eq!(detail["data"]["unresolved_count"], 0, "{}", detail["data"]);

    // 輸入驗證。
    for body in [
        json!({ "mappings": [] }),
        json!({ "mappings": [{ "bim_element_id": "X", "target_type": "ROOM",
                               "target_id": hq_node }]}),
        json!({ "mappings": [{ "bim_element_id": "", "target_type": "ASSET",
                               "target_id": hq_asset }]}),
    ] {
        let (status, p) = ctx
            .send(authed(
                json_request(
                    "POST",
                    &format!("/api/v1/bim-models/{model_id}/mappings"),
                    body,
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{p}");
    }

    ctx.teardown().await;
}

/// **floor-view：畫不出來的節點數要看得見。**
#[tokio::test]
async fn f_floor_view_says_how_many_nodes_cannot_be_drawn() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/facilities/{FACILITY_HQ}/floor-view")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data");
    assert!(!rows.is_empty(), "示範資料該有空間節點");

    for r in rows {
        assert!(r["geometry"].is_object(), "{r}");
        assert!(r["asset_count"].is_i64(), "{r}");
        assert!(r["open_work_orders"].is_i64(), "{r}");
        assert!(r["active_alarms"].is_i64(), "{r}");
        assert!(r["node_path"].as_str().is_some(), "{r}");
    }

    // **畫不出來的數量。** 示範資料沒有幾何，所以這個數字等於節點數 ——
    // 那正是重點：一張缺了全部房間的圖必須有理由。
    assert_eq!(
        body["meta"]["nodes_without_geometry"]
            .as_i64()
            .unwrap_or(-1),
        rows.len() as i64,
        "**示範資料沒有幾何**（Phase 1 沒有 BIM 解析器），而那個數字必須看得見 \
         —— 否則前端只會畫出一張缺了一半房間的圖而不知道為什麼：{}",
        body["meta"]
    );
    assert_eq!(
        body["meta"]["alarm_severity_order"],
        json!(["INFO", "WARNING", "MINOR", "MAJOR", "CRITICAL"]),
        "**`max(severity)` 是字典序**，前端要照這個順序排嚴重度：{}",
        body["meta"]
    );
    assert!(body["meta"]["floors"].is_array(), "{}", body["meta"]);

    // 依樓層過濾：子集。
    let floor = rows
        .iter()
        .find_map(|r| r["floor_level"].as_i64())
        .unwrap_or(1);
    let (_, one) = ctx
        .send(authed(
            get(&format!(
                "/api/v1/facilities/{FACILITY_HQ}/floor-view?floor_level={floor}"
            )),
            &token,
        ))
        .await;
    let n = one["data"].as_array().map(Vec::len).unwrap_or(0);
    assert!(n <= rows.len(), "過濾後該是子集：{n} vs {}", rows.len());
    assert!(
        one["data"]
            .as_array()
            .expect("data")
            .iter()
            .all(|r| r["floor_level"].as_i64() == Some(floor)),
        "過濾後每一列的樓層都要相符：{}",
        one["data"]
    );

    ctx.teardown().await;
}

/// **floor-view：即時佔用狀態與設備連線狀態。**
///
/// 兩者都重用既有的判定式（`occupancy` 的 repo 函式、`device_connectivity()`），
/// 這裡驗的是**接線接對了**，不是重驗那些判定式本身——那些已經在
/// `reservation` 與 `devices` 各自的測試裡驗過。
#[tokio::test]
async fn h_floor_view_shows_occupancy_and_device_connectivity() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;

    // 009 種的可預約會議室節點——有對應的 bookable_resources 列。
    const MEETING_ROOM_NODE: &str = "10000000-0000-4000-8000-000000000005";

    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/facilities/{FACILITY_HQ}/floor-view")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data");

    let meeting_room = rows
        .iter()
        .find(|r| r["id"] == json!(MEETING_ROOM_NODE))
        .unwrap_or_else(|| panic!("找不到可預約的會議室節點：{body}"));
    assert!(
        ["FREE", "OCCUPIED", "RESERVED", "HELD"]
            .contains(&meeting_room["occupancy_state"].as_str().unwrap_or("")),
        "可預約資源必須有佔用狀態：{meeting_room}"
    );
    assert!(
        !meeting_room.as_object().unwrap().contains_key("title"),
        "不該回 title——這支端點沒有檢查 reservation:read，回這個欄位會繞過\
         私人預約的遮罩：{meeting_room}"
    );
    // 目前是 FREE（示範資料沒有正在進行的預約），所以還沒有佔用時間。
    assert!(
        meeting_room["occupancy_start_at"].is_null() && meeting_room["occupancy_end_at"].is_null(),
        "FREE 的節點不該有佔用時間：{meeting_room}"
    );

    // 非可預約資源（樓層本身）的佔用狀態必須是 null，不是 FREE——
    // FREE 會被誤讀成「可以訂」。
    let floor_node = rows
        .iter()
        .find(|r| r["node_type_code"] == json!("FLOOR"))
        .unwrap_or_else(|| panic!("示範資料該有樓層節點：{body}"));
    assert!(
        floor_node["occupancy_state"].is_null(),
        "樓層本身不是可預約資源，佔用狀態該是 null：{floor_node}"
    );

    for r in rows {
        assert!(r["device_count"].is_i64(), "{r}");
        assert!(r["devices_offline_count"].is_i64(), "{r}");
    }

    // 掛一台離線很久的裝置到會議室節點下，驗證計數真的接上了
    // fms.device_connectivity()，不是回一個永遠是 0 的假欄位。
    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "INSERT INTO fms.devices
           (tenant_id, facility_id, spatial_node_id, device_code, name, device_type,
            status, last_seen_at, offline_alarm_after_seconds)
         VALUES ($1::uuid, $2::uuid, $3::uuid, 'TEST-OFFLINE-DEV', '測試離線裝置',
                 'OCCUPANCY', 'UNKNOWN', clock_timestamp() - interval '1 day', 60)",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(MEETING_ROOM_NODE)
    .execute(&mut *tx)
    .await
    .expect("insert offline device");

    // 一個進行中的預約，驗證 occupancy_start_at/end_at 真的接上了
    // occupancy 的 start_at/end_at，不是只有狀態字串接對。
    sqlx::query(
        "INSERT INTO fms.reservations
           (tenant_id, facility_id, bookable_resource_id, reservation_no,
            resource_type, resource_id, organizer_id, title, status, start_at, end_at)
         VALUES ($1::uuid, $2::uuid, '70000000-0000-4000-8000-000000000001'::uuid,
                 'TEST-FLOORVIEW-001', 'SPATIAL_NODE', $3::uuid, $4::uuid, '測試預約',
                 'CONFIRMED', clock_timestamp() - interval '10 minutes',
                 clock_timestamp() + interval '50 minutes')",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(MEETING_ROOM_NODE)
    .bind(admin_user_id())
    .execute(&mut *tx)
    .await
    .expect("insert active reservation");

    // 一個 CRITICAL 告警，驗證 worst_alarm_rank 真的是排好序的數字
    // （CRITICAL 字母排序在 WARNING 前面，rank 卻該是最大）。
    sqlx::query(
        "INSERT INTO fms.alarms
           (tenant_id, facility_id, spatial_node_id, alarm_no, severity, status, message)
         VALUES ($1::uuid, $2::uuid, $3::uuid, 'TEST-ALARM-001', 'CRITICAL', 'ACTIVE',
                 '測試告警')",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(MEETING_ROOM_NODE)
    .execute(&mut *tx)
    .await
    .expect("insert critical alarm");
    tx.commit().await.expect("commit");

    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/facilities/{FACILITY_HQ}/floor-view")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["data"].as_array().expect("data");
    let meeting_room = rows
        .iter()
        .find(|r| r["id"] == json!(MEETING_ROOM_NODE))
        .unwrap();
    assert!(
        meeting_room["device_count"].as_i64().unwrap() >= 1,
        "剛掛上去的裝置該被算進 device_count：{meeting_room}"
    );
    assert!(
        meeting_room["devices_offline_count"].as_i64().unwrap() >= 1,
        "離線超過門檻的裝置該被算進 devices_offline_count：{meeting_room}"
    );
    assert_eq!(
        meeting_room["occupancy_state"], "RESERVED",
        "CONFIRMED 且時間涵蓋現在的預約該顯示 RESERVED：{meeting_room}"
    );
    assert!(
        meeting_room["occupancy_start_at"].is_string()
            && meeting_room["occupancy_end_at"].is_string(),
        "有進行中的預約時該回佔用時間：{meeting_room}"
    );
    assert_eq!(
        meeting_room["worst_alarm_severity"], "CRITICAL",
        "{meeting_room}"
    );
    assert_eq!(
        meeting_room["worst_alarm_rank"], 5,
        "CRITICAL 是 alarm_severity_order 的第 5 個（1-based），字母排序\
         會誤判成比 WARNING 輕：{meeting_room}"
    );

    ctx.teardown().await;
}

/// 權限：**REQUESTER 讀得到空間，但寫不了、也看不到 BIM。**
///
/// 這一格的第一版把五支全部斷言成 403，而其中兩支回了 200 —— 因為
/// **REQUESTER 真的有 `spatial_node:read`**（報修時要選位置）。查了 008 的
/// 授權才知道：他有 `spatial_node:read`／`asset:read`／`facility:read`，
/// 但沒有 `spatial_node:write` 也沒有任何 `bim_model:*`。
///
/// 所以這一格現在同時斷言**該過的要過**與**該擋的要擋** ——
/// 只斷言後者的話，一個把 `spatial_node:read` 誤改成 `write` 的重構會讓
/// 報修的人選不了位置，而測試全綠。
#[tokio::test]
async fn g_requester_can_read_space_but_cannot_write_or_see_bim() {
    let ctx = &TestContext::setup().await;
    let requester = ctx.login_as(USERNAME_REQUESTER).await;
    let admin = ctx.login().await;
    let model_id = register_model(ctx, &admin, "權限").await;

    // 該過的：REQUESTER 有 spatial_node:read。
    for uri in [
        "/api/v1/spatial-node-types".to_string(),
        format!("/api/v1/facilities/{FACILITY_HQ}/floor-view"),
    ] {
        let (status, body) = ctx.send(authed(get(&uri), &requester)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "**REQUESTER 有 spatial_node:read** —— 報修時要選位置，             把這裡改成需要 write 會讓他選不了：{uri} → {body}"
        );
    }

    // 該擋的：沒有 spatial_node:write，也沒有 bim_model:read／write。
    for r in [
        get(&format!("/api/v1/bim-models/{model_id}")),
        json_request(
            "POST",
            &format!("/api/v1/facilities/{FACILITY_HQ}/spatial-nodes:bulk-import"),
            json!({ "rows": [{ "code": "P1", "name": "p", "node_type_code": "ROOM" }]}),
        ),
        json_request(
            "POST",
            &format!("/api/v1/bim-models/{model_id}/mappings"),
            json!({ "mappings": [{ "bim_element_id": "E", "target_type": "ASSET",
                                   "target_id": "00000000-0000-4000-8000-000000000000" }]}),
        ),
    ] {
        let (status, _) = ctx.send(authed(r, &requester)).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "REQUESTER 沒有 spatial_node:write 也沒有 bim_model:*"
        );
    }

    ctx.teardown().await;
}

/// 模擬 `bim-worker` 已經跑過一輪：直接用 owner 權限塞一個樓層節點、一個掛
/// 在它底下的空間節點、一個掛在空間上的設備，三者的 `bim_model_id` 都指向
/// `model_id`——這就是 `ingest.py` 真正寫進去的形狀（見
/// `services/bim-worker/bim_worker/ingest.py`），這裡不跑 Python，直接造
/// 同樣的資料來測 Rust 這邊的刪除／重置邏輯。回傳 `(floor_id, space_id,
/// asset_id)`。
async fn seed_bim_imported_tree(ctx: &TestContext, model_id: &str) -> (String, String, String) {
    let mut tx = ctx.owner_tx().await;
    let building_id: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM fms.spatial_nodes
          WHERE facility_id = $1::uuid AND node_type_code = 'BUILDING' AND deleted_at IS NULL
          LIMIT 1",
    )
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("HQ 場域該有一個 BUILDING 根節點（seed）");

    let suffix = &uuid::Uuid::new_v4().to_string()[..8];
    let floor_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.spatial_nodes
           (tenant_id, facility_id, parent_id, node_type_code, code, name, bim_model_id)
         VALUES ($1::uuid, $2::uuid, $3::uuid, 'FLOOR', $4, $4, $5::uuid)
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(building_id)
    .bind(format!("T-FLOOR-{suffix}"))
    .bind(model_id)
    .fetch_one(&mut *tx)
    .await
    .expect("insert floor");

    let space_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.spatial_nodes
           (tenant_id, facility_id, parent_id, node_type_code, code, name, bim_model_id)
         VALUES ($1::uuid, $2::uuid, $3::uuid, 'ROOM', $4, $4, $5::uuid)
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(floor_id)
    .bind(format!("T-ROOM-{suffix}"))
    .bind(model_id)
    .fetch_one(&mut *tx)
    .await
    .expect("insert space");

    let asset_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO fms.assets
           (tenant_id, facility_id, spatial_node_id, category_id, asset_code, name,
            status, bim_model_id)
         VALUES ($1::uuid, $2::uuid, $3::uuid,
                 (SELECT id FROM fms.asset_categories LIMIT 1),
                 $4, $4, 'OPERATIONAL', $5::uuid)
         RETURNING id",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(space_id)
    .bind(format!("T-AST-{suffix}"))
    .bind(model_id)
    .fetch_one(&mut *tx)
    .await
    .expect("insert asset");

    tx.commit().await.expect("commit");
    (
        floor_id.to_string(),
        space_id.to_string(),
        asset_id.to_string(),
    )
}

async fn is_soft_deleted(ctx: &TestContext, table: &str, id: &str) -> bool {
    let mut tx = ctx.owner_tx().await;
    let deleted_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(&format!(
        "SELECT deleted_at FROM fms.{table} WHERE id = $1::uuid"
    ))
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .expect("row should still exist (soft delete, not hard delete)");
    deleted_at.is_some()
}

/// `POST /bim-models/{id}:reset`——清掉上一輪匯入的樓層／空間／設備，狀態
/// 退回 `UPLOADED`，好讓 `bim-worker` 之後自動重新解析同一個檔案。
///
/// 這是修過解析器邏輯、想在**不重新上傳檔案**的前提下重新跑一次匯入時要
/// 用的端點——見 `services/bim-worker/bim_worker/ingest.py` 的 `_insert_space`
/// 只做 INSERT、不是 upsert：不清掉舊資料就直接把狀態改回 `UPLOADED`，
/// 下一輪解析會插出重複的 `SPxxxx`。
#[tokio::test]
async fn i_reset_clears_prior_import_and_requeues_for_parsing() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let model_id = register_model(ctx, &token, "重置測試").await;
    let (floor_id, space_id, asset_id) = seed_bim_imported_tree(ctx, &model_id).await;

    let (status, body) = ctx
        .send(authed(
            json_request(
                "POST",
                &format!("/api/v1/bim-models/{model_id}/reset"),
                json!({}),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "重置失敗：{body}");
    assert_eq!(body["status"], "UPLOADED", "重置後要排回佇列：{body}");
    assert_eq!(body["element_count"], 0, "計數要一起清空：{body}");
    assert_eq!(body["mapped_node_count"], 0, "{body}");
    assert_eq!(body["mapped_asset_count"], 0, "{body}");

    // 上一輪匯入的三個節點/資產都要是「軟刪除」，不是繼續掛在模型底下
    // ——不清掉的話下一輪解析會插出重複的節點。
    for (table, id) in [
        ("spatial_nodes", floor_id.as_str()),
        ("spatial_nodes", space_id.as_str()),
        ("assets", asset_id.as_str()),
    ] {
        assert!(
            is_soft_deleted(ctx, table, id).await,
            "重置後 {table}/{id} 該被軟刪除"
        );
    }

    ctx.teardown().await;
}

/// `DELETE /bim-models/{id}`——連同它匯入的樓層／空間／設備一起清掉，
/// 模型紀錄本身直接硬刪（`bim_models` 沒有 `deleted_at`，見 sql/003）。
#[tokio::test]
async fn j_delete_removes_model_and_its_imported_tree() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let model_id = register_model(ctx, &token, "刪除測試").await;
    let (floor_id, space_id, asset_id) = seed_bim_imported_tree(ctx, &model_id).await;

    let (status, _) = ctx
        .send(authed(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/bim-models/{model_id}"))
                .body(Body::empty())
                .unwrap(),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = ctx
        .send(authed(
            get(&format!("/api/v1/bim-models/{model_id}")),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "模型紀錄該被硬刪：{body}");

    for (table, id) in [
        ("spatial_nodes", floor_id.as_str()),
        ("spatial_nodes", space_id.as_str()),
        ("assets", asset_id.as_str()),
    ] {
        assert!(
            is_soft_deleted(ctx, table, id).await,
            "刪除模型後 {table}/{id} 該被軟刪除"
        );
    }

    ctx.teardown().await;
}

/// 匯入的空間底下還有一張未結工單時，刪除／重置都該被擋下——不然使用者
/// 會在不知不覺間讓一張正在處理中的工單失去它指的空間節點（軟刪除的節點
/// 還在，但畫面上的「場域・空間・BIM」再也找不到入口）。
#[tokio::test]
async fn k_delete_and_reset_are_blocked_by_an_open_work_order() {
    let ctx = &TestContext::setup().await;
    let token = ctx.login().await;
    let model_id = register_model(ctx, &token, "擋修測試").await;
    let (_floor_id, space_id, _asset_id) = seed_bim_imported_tree(ctx, &model_id).await;

    let mut tx = ctx.owner_tx().await;
    sqlx::query(
        "INSERT INTO fms.work_orders
           (tenant_id, facility_id, wo_no, work_order_type, source, title,
            status, priority, spatial_node_id, actual_start_at)
         VALUES ($1::uuid, $2::uuid, 'WO-BIM-BLOCK', 'CORRECTIVE', 'MANUAL',
                 '卡住這個空間的工單', 'IN_PROGRESS', 'MEDIUM', $3::uuid,
                 clock_timestamp() - interval '1 hour')",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .bind(&space_id)
    .execute(&mut *tx)
    .await
    .expect("insert blocking work order");
    tx.commit().await.expect("commit");

    for (method, uri) in [
        ("DELETE", format!("/api/v1/bim-models/{model_id}")),
        ("POST", format!("/api/v1/bim-models/{model_id}/reset")),
    ] {
        let (status, body) = ctx
            .send(authed(
                Request::builder()
                    .method(method)
                    .uri(&uri)
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
                &token,
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{method} {uri} 該被未結工單擋下：{body}"
        );
    }

    // 沒有東西被清掉——擋下來是真的擋下來，不是先清一半才發現。
    assert!(
        !is_soft_deleted(ctx, "spatial_nodes", &space_id).await,
        "被擋下的操作不該動到任何資料"
    );

    ctx.teardown().await;
}
