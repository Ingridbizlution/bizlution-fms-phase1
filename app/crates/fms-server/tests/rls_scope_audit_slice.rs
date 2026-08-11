//! 場域 RLS 的覆蓋率稽核（migration 062）。
//!
//! # 為什麼是「稽核」而不是又一個功能切片
//!
//! 060 修掉三張遙測表的跨場域洩漏，而那個洩漏是**碰巧**被發現的 ——
//! 剛好要加 `/telemetry/series`，才去對照權限宣告的 `min_scope_level`
//! 與那三張表的政策。也就是說偵測機制是運氣。
//!
//! 這個檔案把偵測變成一件會重複發生的事。
//!
//! # `a_` 是結構檢查，而它是這裡最重要的一條
//!
//! 它**列舉資料庫**，找出所有「開了 RLS、沒有 `facility_id` 欄位、但有外鍵
//! 指向某張已場域收斂的表」的表，然後要求它們都有場域政策。
//!
//! 這樣做的價值在於它涵蓋**還不存在的表**：下一個人加一張
//! `work_order_signatures`，這條測試會紅，而不必有人記得要來補。
//!
//! 前面那些洩漏之所以躺了很久，正是因為沒有任何東西問過這個問題。
//!
//! # `b_`／`c_` 是行為檢查，而它們必須自己造資料
//!
//! seed 009 **沒有任何工單、預約或 PM 執行紀錄**（量過，全部 0 列），
//! 所以「總部自己的資料還看得到」這件事光靠 seed 驗不出來 ——
//! 那些表的計數本來就是 0，而 0 同時符合「修對了」與「全藏起來了」。
//!
//! 這一點很重要：062 的風險是**雙向**的。錨定到一個可為 NULL 的外鍵會讓
//! `EXISTS` 永遠是 false，於是整張表對所有人隱藏 —— 那不是洩漏，
//! 是反過來的失效，而它更難發現（症狀是資料不見了，沒有錯誤訊息）。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";

/// 刻意保持租戶級的表，附上理由。
///
/// 這份清單是**允許例外**，不是待辦 —— 每一項都必須說得出為什麼場域化會壞事。
const TENANT_SCOPED_ON_PURPOSE: &[(&str, &str)] = &[(
    "users",
    "租戶管理員必須看得到所有使用者，而場域管理員派工時要看到其他場域的人\
     （026 的 user_accessible_facilities 就是為此）。場域化會讓 /users 與\
     角色指派整條路徑壞掉。",
)];

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 另一個場域的 id（示範資料的信義影城）。
async fn other_facility(ctx: &TestContext) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let id = sqlx::query_scalar(
        "SELECT id FROM fms.facilities
          WHERE tenant_id = $1::uuid AND id <> $2::uuid LIMIT 1",
    )
    .bind(TENANT_ID)
    .bind(FACILITY_HQ)
    .fetch_one(&mut *tx)
    .await
    .expect("示範資料該有第二個場域");
    tx.commit().await.expect("commit");
    id
}

/// 某場域的第一個空間節點。
async fn first_node(ctx: &TestContext, facility: &str) -> uuid::Uuid {
    let mut tx = ctx.owner_tx().await;
    let id =
        sqlx::query_scalar("SELECT id FROM fms.spatial_nodes WHERE facility_id = $1::uuid LIMIT 1")
            .bind(facility)
            .fetch_one(&mut *tx)
            .await
            .expect("該場域要有空間節點");
    tx.commit().await.expect("commit");
    id
}

/// 在指定場域建一張工單，並掛一個檢查項。回傳 `(work_order_id, task_id)`。
///
/// 工單走 API（它有一串必要欄位與狀態機），檢查項走 SQL
/// （契約還沒有建立檢查項的端點 —— `GET /work-orders/{id}/tasks` 都還沒做）。
async fn work_order_with_task(
    ctx: &TestContext,
    token: &str,
    facility: &str,
    title: &str,
) -> (uuid::Uuid, uuid::Uuid) {
    let node = first_node(ctx, facility).await;
    // 工單走 API 而不是 helper：這一格要驗的是**場域級讀者看不到別場域的
    // 子表列**，而工單必須真的存在於那個場域。走 API 順便確認建立路徑本身
    // 有把 facility 記對。
    let (status, wo) = ctx
        .send(authed(
            json_request(
                "POST",
                "/api/v1/work-orders",
                json!({
                    "work_order_type": "CORRECTIVE",
                    "facility_id": facility,
                    // 契約要求 asset_id 或 spatial_node_id 至少一個
                    // （工單必須知道自己在修哪裡）。
                    "spatial_node_id": node,
                    "title": title,
                    "priority": "MEDIUM"
                }),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "建工單失敗：{wo}");
    let wo_id = uuid::Uuid::parse_str(wo["id"].as_str().expect("id")).expect("uuid");

    let task_id = ctx
        .seed_work_order_task(wo_id, 1, &format!("{title} 的檢查項"), true)
        .await;

    (wo_id, task_id)
}

/// 以某個使用者的場域情境數某張表看得到幾列。
///
/// 用 `begin_tenant_tx`（而不是直接 `set_context`）是關鍵：後者**不會設定
/// `app.facility_ids`**，於是 `facility_in_scope()` 一律回 true、場域 RLS 完全失效。
/// 我在調查 060 時第一個 probe 就是這樣寫的，結論整個是假的。
async fn visible_count(ctx: &TestContext, username: &str, sql: &str) -> i64 {
    let mut tx = ctx.tenant_tx_as(username).await;
    let n: i64 = sqlx::query_scalar(sql)
        .fetch_one(tx.conn())
        .await
        .unwrap_or_else(|e| panic!("查 {sql} 失敗：{e}"));
    tx.commit().await.expect("commit");
    n
}

/// **結構檢查：沒有一張子表在場域收斂之外。**
///
/// 這條涵蓋還不存在的表 —— 下一個人加一張工單子表，它會紅。
#[tokio::test]
async fn a_every_child_table_reaching_facility_is_scoped() {
    let ctx = TestContext::setup().await;
    let mut tx = ctx.owner_tx().await;

    // 「有 RLS、沒有 facility_id 欄位、有外鍵指向某張已場域收斂的表、
    //   而自己沒有任何場域政策」的表。
    let offenders: Vec<(String, String)> = sqlx::query_as(
        "WITH scoped AS (
           -- 已經場域收斂的表（不管是靠自己的 facility_id 還是靠父表）
           SELECT DISTINCT p.tablename AS name
             FROM pg_policies p
            WHERE p.schemaname = 'fms' AND p.permissive = 'RESTRICTIVE'
              AND (coalesce(p.qual, '') LIKE '%facility%'
                   OR p.policyname = 'facility_scope_via_parent')
         ), rls AS (
           SELECT c.oid, c.relname
             FROM pg_class c
             JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'fms'
            WHERE c.relkind IN ('r','p') AND c.relrowsecurity
              AND NOT EXISTS (SELECT 1 FROM pg_attribute a
                               WHERE a.attrelid = c.oid AND a.attname = 'facility_id'
                                 AND a.attnum > 0 AND NOT a.attisdropped)
              AND c.relname NOT IN (SELECT name FROM scoped)
         )
         SELECT r.relname::text,
                string_agg(DISTINCT tgt.relname::text, ', ' ORDER BY tgt.relname::text)
           FROM rls r
           JOIN pg_constraint k ON k.conrelid = r.oid AND k.contype = 'f'
           JOIN pg_class tgt ON tgt.oid = k.confrelid
          WHERE tgt.relname IN (SELECT name FROM scoped)
          GROUP BY r.relname
          ORDER BY r.relname",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("列舉未收斂的子表");
    tx.commit().await.expect("commit");

    let allow: Vec<&str> = TENANT_SCOPED_ON_PURPOSE.iter().map(|(t, _)| *t).collect();
    let unexpected: Vec<String> = offenders
        .iter()
        .filter(|(t, _)| !allow.contains(&t.as_str()))
        .map(|(t, parents)| format!("  {t}（外鍵指向 {parents}）"))
        .collect();

    assert!(
        unexpected.is_empty(),
        "這些表開了 RLS、沒有 facility_id、外鍵指向已場域收斂的表，\n\
         但**自己沒有任何場域政策** —— 場域級讀者看不到父列卻看得到它們：\n{}\n\n\
         這與 060／062 修掉的是同一類洩漏。要嘛在 migration 裡補\n\
         `facility_scope_via_parent`，要嘛把它加進 rls_scope_audit_slice.rs 的\n\
         TENANT_SCOPED_ON_PURPOSE **並寫出為什麼場域化會壞事**。",
        unexpected.join("\n")
    );

    // 允許清單裡的每一項都必須真的還在資料庫裡 —— 表被改名或移除之後，
    // 一條過期的例外會安靜地掩護掉一張真正該檢查的表。
    for (t, _why) in TENANT_SCOPED_ON_PURPOSE {
        let mut tx = ctx.owner_tx().await;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class c
               JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'fms'
              WHERE c.relname = $1)",
        )
        .bind(t)
        .fetch_one(&mut *tx)
        .await
        .expect("查表");
        tx.commit().await.expect("commit");
        assert!(exists, "允許清單裡的 {t} 已經不存在了 —— 請把它移除");
    }

    ctx.teardown().await;
}

/// **行為檢查：工單子表跟著父工單收斂，而自己場域的仍然看得到。**
///
/// 兩個方向都驗。只驗一半的話，「把整張表藏起來」也會過。
#[tokio::test]
async fn b_work_order_children_follow_the_parent_both_ways() {
    let ctx = TestContext::setup().await;
    // admin.chen 是租戶級，兩個場域都建得起來。
    let admin = ctx.login().await;
    let other = other_facility(&ctx).await;

    let (_hq_wo, hq_task) = work_order_with_task(&ctx, &admin, FACILITY_HQ, "總部工單").await;
    let (_ot_wo, ot_task) =
        work_order_with_task(&ctx, &admin, &other.to_string(), "別場域工單").await;

    let count_task =
        |id: uuid::Uuid| format!("SELECT count(*) FROM fms.work_order_tasks WHERE id = '{id}'");

    // fm.lin：FACILITY_ADMIN，範圍只在總部。
    assert_eq!(
        visible_count(&ctx, USERNAME_FACILITY_ADMIN, &count_task(hq_task)).await,
        1,
        "總部的檢查項該看得到 —— 若是 0，062 把自己場域的資料也藏了"
    );
    assert_eq!(
        visible_count(&ctx, USERNAME_FACILITY_ADMIN, &count_task(ot_task)).await,
        0,
        "別場域的檢查項不該看得到 —— 這正是 062 之前的洩漏"
    );

    // 租戶級讀者兩邊都看得到（否則上面那條可能是「修成什麼都看不到」）。
    for id in [hq_task, ot_task] {
        assert_eq!(
            visible_count(&ctx, USERNAME, &count_task(id)).await,
            1,
            "租戶級讀者該看得到兩個場域的檢查項"
        );
    }

    ctx.teardown().await;
}

/// 設備子表同樣。`asset_meters` 是我實際量到洩漏的那一張。
#[tokio::test]
async fn c_asset_children_follow_the_parent_both_ways() {
    let ctx = TestContext::setup().await;
    let other = other_facility(&ctx).await;

    // 兩邊各建一台設備與一個計量。示範資料在信義影城只有一台設備，
    // 自己建比較不會依賴 seed 的細節。
    let mut ids = Vec::new();
    for (facility, code) in [
        (FACILITY_HQ.to_string(), "AUDIT-HQ"),
        (other.to_string(), "AUDIT-OTHER"),
    ] {
        let asset = ctx.seed_asset(&facility, code).await;
        let mut tx = ctx.owner_tx().await;
        let meter: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO fms.asset_meters (tenant_id, asset_id, meter_code, name, unit)
             VALUES ($1::uuid, $2::uuid, 'AUDIT_METER', '稽核用計量', 'kWh')
             RETURNING id",
        )
        .bind(TENANT_ID)
        .bind(asset)
        .fetch_one(&mut *tx)
        .await
        .expect("建計量");
        tx.commit().await.expect("commit");
        ids.push(meter);
    }

    let count_meter =
        |id: uuid::Uuid| format!("SELECT count(*) FROM fms.asset_meters WHERE id = '{id}'");

    assert_eq!(
        visible_count(&ctx, USERNAME_FACILITY_ADMIN, &count_meter(ids[0])).await,
        1,
        "總部設備的計量該看得到"
    );
    assert_eq!(
        visible_count(&ctx, USERNAME_FACILITY_ADMIN, &count_meter(ids[1])).await,
        0,
        "別場域設備的計量不該看得到 —— 062 之前這裡是 1（實測過）"
    );

    ctx.teardown().await;
}

/// 062 用的錨點必須全部是 NOT NULL 的外鍵。
///
/// 可為 NULL 的錨點會讓 `EXISTS` 永遠 false，於是整張表對所有人隱藏 ——
/// 那不是洩漏，是反過來的失效，而它更難發現。
///
/// migration 自己也檢查了這件事，但那是在**建立時**檢查。這一條擋的是
/// 「之後有人把某個錨點改成可為 NULL」。
#[tokio::test]
async fn d_every_anchor_column_is_still_not_null() {
    let ctx = TestContext::setup().await;
    let mut tx = ctx.owner_tx().await;

    // 從政策的運算式裡把錨點欄位挖出來，而不是在測試裡再抄一份對應表 ——
    // 兩份清單會漂移。
    let nullable: Vec<String> = sqlx::query_scalar(
        "SELECT p.tablename::text || '.' || a.attname::text
           FROM pg_policies p
           JOIN pg_class c ON c.relname = p.tablename
           JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = 'fms'
           JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0
                              AND NOT a.attisdropped AND NOT a.attnotnull
          WHERE p.schemaname = 'fms'
            AND p.policyname = 'facility_scope_via_parent'
            -- 運算式裡出現的那個欄位就是錨點
            AND coalesce(p.qual, '') LIKE '%.' || a.attname || ')%'",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("查錨點");
    tx.commit().await.expect("commit");

    assert!(
        nullable.is_empty(),
        "這些 facility_scope_via_parent 的錨點欄位可為 NULL：{nullable:?}\n\
         NULL 錨點會讓 EXISTS 永遠 false —— 整張表對所有人隱藏，\
         而症狀是「資料不見了」而沒有任何錯誤訊息。"
    );

    ctx.teardown().await;
}
