//! 冪等鍵的授權邊界（`docs/security-review-open-items.md` 第 1 項）。
//!
//! 冪等本身的行為（同鍵同 body 回放、同鍵不同 body 422）已由
//! `reservation_slice` 覆蓋。這裡驗的是**誰可以取得那個回放**：
//!
//!   1. 鍵的範圍含使用者 —— 同租戶的另一個人用同一個鍵字串，
//!      得到的是自己的執行結果，不是別人的回應（migration 025）
//!   2. 回放本身仍要過授權 —— 同一個使用者在窗內權限被撤銷後，
//!      重送不再回放（`PendingReplay` + `Authorized`）
//!
//! 用工單建立端點測：它的授權檢查緊接在冪等登記之後，中間沒有資源查詢，
//! 因此測到的就是這一層的行為，不會混進別的失敗原因。

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{json, Value};

const FACILITY_HQ: &str = "cccccccc-0000-4000-8000-000000000001";
/// 4F 空調箱（HQ）—— 有這台設備才滿足 `ck_wo_target`
const SEED_AHU: &str = "20000000-0000-4000-8000-000000000002";

fn create_request(title: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/work-orders")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "work_order_type": "INSPECTION",
                "facility_id": FACILITY_HQ,
                "asset_id": SEED_AHU,
                "title": title,
            })
            .to_string(),
        ))
        .unwrap()
}

fn id_of(body: &Value) -> String {
    body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("回應沒有 id: {body}"))
        .to_string()
}

#[tokio::test]
async fn a_key_belongs_to_the_user_who_issued_it() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login().await;
    // REQUESTER 也持有 work_order:create，因此他的請求會真的被執行 ——
    // 這樣「有沒有回放」才能由結果分辨，而不是被 403 蓋掉。
    let requester = ctx.login_as(USERNAME_REQUESTER).await;

    // 兩個人刻意用**同一個鍵字串**與同一份 body。
    // 025 之前這會讓第二個人拿到第一個人的回應。
    let key = format!("shared-{}", uuid::Uuid::new_v4());
    const TITLE: &str = "冪等鍵歸屬測試";

    let (status, first) = ctx
        .send(authed_idem(create_request(TITLE), &admin, &key))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let admin_id = id_of(&first);

    let (status, second) = ctx
        .send(authed_idem(create_request(TITLE), &requester, &key))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    let requester_id = id_of(&second);

    assert_ne!(
        admin_id, requester_id,
        "另一個使用者用同一個鍵字串時，必須執行他自己的請求，\
         而不是回放別人的回應"
    );

    // 鍵對**自己**仍然有效：綁定使用者不該把冪等本身弄壞。
    let (status, replay) = ctx
        .send(authed_idem(create_request(TITLE), &admin, &key))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{replay}");
    assert_eq!(
        id_of(&replay),
        admin_id,
        "同一個使用者重送同鍵同 body 仍應回放自己的首次結果"
    );

    // 另一個人的鍵也各自獨立地可回放。
    let (status, replay) = ctx
        .send(authed_idem(create_request(TITLE), &requester, &key))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{replay}");
    assert_eq!(id_of(&replay), requester_id);

    ctx.teardown().await;
}

#[tokio::test]
async fn a_replay_is_refused_once_the_permission_is_gone() {
    let ctx = &TestContext::setup().await;
    let admin = ctx.login().await;

    let key = format!("revoked-{}", uuid::Uuid::new_v4());
    const TITLE: &str = "回放授權測試";

    let (status, first) = ctx
        .send(authed_idem(create_request(TITLE), &admin, &key))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");

    // 撤銷這個使用者的全部角色授權。這正是第 1 項殘餘的那種情況：
    // 同一個使用者、同一個鍵，但已經不該再執行這個操作。
    //
    // 存取權杖不受影響（`require_auth` 只驗簽章與 X-Tenant-ID，不查帳號狀態），
    // 因此請求會一路走到授權判定 —— 也就是我們要驗的那一關。
    {
        let mut tx = ctx.owner_tx().await;
        sqlx::query("UPDATE fms.user_role_assignments SET valid_until = now() WHERE user_id = $1")
            .bind(admin_user_id())
            .execute(&mut *tx)
            .await
            .expect("revoke role assignments");
        tx.commit().await.expect("commit revocation");
    }

    let (status, body) = ctx
        .send(authed_idem(create_request(TITLE), &admin, &key))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "權限已撤銷後仍回放，等於回放路徑繞過了授權: {body}"
    );
    assert_eq!(body["code"], "PERMISSION_DENIED");

    ctx.teardown().await;
}
