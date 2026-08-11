//! PM 產生器：把維護計畫變成排程占位與工單。
//!
//! # 冪等從何而來
//!
//! 完全來自 004 已有的唯一索引
//! `uq_maintenance_occurrences (plan_id, coalesce(asset_id, 零), scheduled_for)`。
//! 產生器先用 `INSERT ... ON CONFLICT DO NOTHING RETURNING id` 搶占位，
//! 搶不到就跳過。因此：
//!   * 產生器重跑 → 不會有第二張工單
//!   * outbox 事件重放（at-least-once）→ 同上
//!   * 兩個 worker 同時跑 → 由資料庫仲裁，不需要應用層鎖
//!
//! 這是刻意選擇「讓資料庫的既有約束做去重」，而不是在應用層先查再寫 ——
//! 後者在併發下就是 check-then-act 競態。
//!
//! # 兩條觸發路徑，一份產生邏輯
//!
//! CALENDAR 由排程掃描驅動、METER 由 outbox 事件驅動，但兩者產出的東西
//! 完全相同（占位 + 工單 + 回寫）。因此 [`generate_for`] 只寫一次，
//! 兩條路徑都呼叫它。

use uuid::Uuid;

use fms_shared::{Problem, TenantTx};

use crate::repo;
use crate::schedule::{self, PlanSchedule};

/// 一次產生的結果。
#[derive(Debug, Default)]
pub struct Generated {
    pub occurrence_ids: Vec<Uuid>,
    pub work_order_ids: Vec<Uuid>,
    /// 因為占位已存在而跳過的數量。非零是**正常**的
    /// —— 代表冪等生效，不是錯誤。
    pub skipped: usize,
}

/// 為一個計畫在指定時刻產生占位與工單。
///
/// # 為什麼一個時刻可能產生多張工單
///
/// 計畫可以瞄準空間子樹或分類（`ck_plan_target` 允許三種模式之一），
/// 例如「4 樓所有空調每季保養」。那一個時刻對應的是**每台設備一張工單**，
/// 因為工單是派給人去某台機器前面做的事。占位也因此以
/// `(plan, asset, scheduled_for)` 為唯一鍵，而不是 `(plan, scheduled_for)`。
///
/// 瞄準單一設備時清單只有一台；瞄準的設備清單為空時不產生任何東西
/// （例如分類下目前沒有設備），並回報 `skipped = 0` 而非報錯。
pub async fn generate_for(
    tx: &mut TenantTx,
    plan: &repo::PlanRow,
    scheduled_for: chrono::DateTime<chrono::Utc>,
) -> Result<Generated, Problem> {
    let mut out = Generated::default();

    let assets = repo::target_assets(tx, plan.id).await?;
    if assets.is_empty() {
        tracing::warn!(
            plan = %plan.code,
            "維護計畫目前沒有任何瞄準的設備，未產生工單"
        );
        return Ok(out);
    }

    for asset_id in assets {
        let Some(occurrence_id) =
            repo::claim_occurrence(tx, plan.id, Some(asset_id), scheduled_for).await?
        else {
            // 占位已存在：這個 (計畫, 設備, 時刻) 已經產過了。
            out.skipped += 1;
            continue;
        };

        let title = format!("{} - {}", plan.name, scheduled_for.format("%Y-%m-%d"));
        let work_order_id = fms_workorder::repo::create(
            tx,
            fms_workorder::repo::NewWorkOrder {
                facility_id: plan.facility_id,
                work_order_type: "MAINTENANCE",
                title: &title,
                description: Some(&plan.name),
                asset_id: Some(asset_id),
                spatial_node_id: None,
                service_item_id: None,
                reservation_id: None,
                priority: Some(&plan.priority),
                requested_start_at: Some(scheduled_for),
                payload: None,
                team_id: plan.assigned_team_id,
                assignee_id: None,
                // 由計畫產生的工單一律從 SUBMITTED 進入狀態機，不直接跳到
                // ASSIGNED：即使計畫有 assigned_team_id，指派給**誰**仍是
                // 派工者的決定，而狀態機的 ASSIGN 需要 assignee_id。
                status: "SUBMITTED",
                // provenance 是報表的關鍵維度（反應性 vs 計畫性維護的比例），
                // 因此不能沿用 API 預設值。
                source: "PM_PLAN",
                maintenance_plan_id: Some(plan.id),
                maintenance_occurrence_id: Some(occurrence_id),
            },
        )
        .await?;

        // 展開範本的檢查表。沒有這一步，PM 工單到技師手上是一張空白單子，
        // 而契約的 `PATCH .../tasks/{taskId}` 也沒有東西可以回填。
        let expanded =
            fms_workorder::repo::expand_template_checklist(tx, work_order_id, plan.template_id)
                .await?;
        if expanded == 0 {
            tracing::warn!(
                plan = %plan.code,
                "保養範本的 checklist 是空的，產生的工單沒有檢查項目"
            );
        }

        repo::mark_generated(tx, occurrence_id, work_order_id).await?;
        out.occurrence_ids.push(occurrence_id);
        out.work_order_ids.push(work_order_id);
    }

    Ok(out)
}

/// 處理一個到期的 CALENDAR／HYBRID 計畫：產生本次，並把 `next_due_at`
/// 推到下一個排程時刻。
///
/// # 為什麼展開兩個時刻
///
/// 需要「本次」與「下一次」：本次用來產生，下一次用來回寫 `next_due_at`。
/// 若只展開一個，`next_due_at` 會停在原地，下一輪掃描又會選到同一個計畫 ——
/// 工單不會重複（占位擋住了），但每一輪都會白做一次查詢與展開。
///
/// RRULE 序列走完（有 `UNTIL`／`COUNT`）時 `next_due_at` 寫回 NULL，
/// 計畫自然不再被 `plans_due` 選中，不需要另外標記「已結束」。
pub async fn run_calendar_plan(
    tx: &mut TenantTx,
    plan: &repo::PlanRow,
) -> Result<Generated, Problem> {
    let Some(rrule) = plan.rrule.as_deref() else {
        return Err(Problem::internal(std::io::Error::other(format!(
            "calendar plan {} has no rrule",
            plan.code
        ))));
    };
    let Some(due) = plan.next_due_at else {
        return Err(Problem::internal(std::io::Error::other(format!(
            "calendar plan {} has no next_due_at",
            plan.code
        ))));
    };

    let generated = generate_for(tx, plan, due).await?;

    // 從當前到期時刻往後展開，取第一個嚴格晚於它的時刻。
    // `expand` 的 DTSTART 本身會被包含在結果內（RFC 5545 的行為），
    // 因此要過濾掉等於 `due` 的那一個。
    let upcoming = schedule::expand(
        &PlanSchedule {
            rrule,
            dtstart: due,
            timezone: &plan.facility_timezone,
        },
        // 取 3 個就夠找到下一個：DTSTART 本身可能不符合 RRULE 而被略過，
        // 也可能重複，留一點餘裕比剛好抓 2 個穩。
        3,
        None,
    )?;
    let next = upcoming.into_iter().find(|d| *d > due);

    repo::advance_plan(tx, plan.id, next).await?;
    Ok(generated)
}
