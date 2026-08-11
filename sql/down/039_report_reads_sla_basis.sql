-- 回退 039：報表改回 034 的判定（整批排除 business_hours_only 的政策）。
BEGIN;
SET search_path = fms, public;

CREATE OR REPLACE FUNCTION fms.report_sla_compliance(
  p_group_by   text,
  p_from       date,
  p_to         date,
  p_strictness text DEFAULT 'strict'
) RETURNS TABLE (
  group_key                  text,
  group_label                text,
  response_total             bigint,
  response_met               bigint,
  response_breached          bigint,
  avg_response_minutes       double precision,
  resolution_total           bigint,
  resolution_met             bigint,
  resolution_breached        bigint,
  avg_resolution_minutes     double precision,
  avg_waiting_minutes        double precision,
  reopened                   bigint,
  excluded_no_policy         bigint,
  excluded_in_flight         bigint,
  excluded_abandoned         bigint,
  excluded_business_hours    bigint,
  substituted_business_hours bigint,
  excluded_pm_response       bigint
)
LANGUAGE sql
STABLE
AS $$
WITH params AS (
  SELECT
    -- 日界用 028 已經決定過的時區，不用 session 的 TimeZone ——
    -- 否則同一份報表會因為呼叫端的連線設定而給出不同的區間。
    (p_from::timestamp AT TIME ZONE fms.partition_boundary_timezone())     AS from_ts,
    ((p_to + 1)::timestamp AT TIME ZONE fms.partition_boundary_timezone()) AS to_ts,
    (p_strictness = 'operational')                                        AS lenient
),
-- 每張工單處於 WAITING 類別的總時長（決定 D：不停錶，但要看得見）。
--
-- 一筆轉移記錄的是「離開 from_status」，因此在 from_status 待了多久 =
-- 這筆的 occurred_at 減去**上一筆**的 occurred_at（第一筆用 created_at）。
waiting AS (
  SELECT work_order_id, sum(dur) AS waited
    FROM (
      SELECT t.work_order_id,
             t.occurred_at - coalesce(
               lag(t.occurred_at) OVER (PARTITION BY t.work_order_id ORDER BY t.occurred_at),
               wo.created_at) AS dur
        FROM fms.work_order_transitions t
        JOIN fms.work_orders wo ON wo.id = t.work_order_id
        JOIN fms.work_order_statuses fs ON fs.code = t.from_status
       WHERE fs.category = 'WAITING'
    ) spans
   GROUP BY work_order_id
),
-- 目前還停在 WAITING 裡的那一段。少了它，一張卡在 WAITING_PARTS 三天的
-- 工單會顯示等待 0 分鐘 —— 而那正是決定 D 要讓人看見的情況。
waiting_open AS (
  SELECT wo.id AS work_order_id,
         clock_timestamp() - coalesce(
           (SELECT max(t.occurred_at) FROM fms.work_order_transitions t
             WHERE t.work_order_id = wo.id),
           wo.created_at) AS dur
    FROM fms.work_orders wo
    JOIN fms.work_order_statuses st ON st.code = wo.status
   WHERE st.category = 'WAITING'
),
scoped AS (
  SELECT
    wo.id,
    wo.created_at,
    wo.source,
    wo.status,
    wo.first_responded_at,
    wo.response_due_at,
    wo.resolution_due_at,
    wo.completed_at,
    coalesce(wo.reopened_count, 0)          AS reopened_count,
    coalesce(sp.business_hours_only, false) AS business_hours_only,
    coalesce(w.waited, interval '0')
      + coalesce(wop.dur, interval '0')     AS waited,
    CASE p_group_by
      WHEN 'facility'     THEN wo.facility_id::text
      WHEN 'org'          THEN f.org_id::text
      WHEN 'team'         THEN wo.team_id::text
      WHEN 'service_item' THEN wo.service_item_id::text
      WHEN 'priority'     THEN wo.priority
    END AS gkey,
    CASE p_group_by
      WHEN 'facility'     THEN f.name
      WHEN 'org'          THEN o.name
      WHEN 'team'         THEN tm.name
      WHEN 'service_item' THEN si.name
      WHEN 'priority'     THEN wo.priority
    END AS glabel
    FROM fms.work_orders wo
    CROSS JOIN params pr
    LEFT JOIN fms.sla_policies sp  ON sp.id = wo.sla_policy_id
    LEFT JOIN fms.facilities f     ON f.id  = wo.facility_id
    LEFT JOIN fms.organizations o  ON o.id  = f.org_id
    LEFT JOIN fms.teams tm         ON tm.id = wo.team_id
    LEFT JOIN fms.service_items si ON si.id = wo.service_item_id
    LEFT JOIN waiting w            ON w.work_order_id   = wo.id
    LEFT JOIN waiting_open wop     ON wop.work_order_id = wo.id
   WHERE wo.created_at >= pr.from_ts
     AND wo.created_at <  pr.to_ts
     -- 草稿還沒送出，沒有進入量測（決定 A）。
     AND wo.status <> 'DRAFT'
),
classified AS (
  SELECT s.*,
    -- 「這一輪做完了沒有」：只有狀態碼答得出來（見檔頭）。
    (s.status IN ('COMPLETED', 'VERIFIED', 'CLOSED'))  AS finished,
    (s.status IN ('CANCELLED', 'REJECTED'))            AS abandoned,
    (s.resolution_due_at IS NOT NULL)                  AS has_target,
    -- strict 模式下 business_hours_only 的 policy 算不出正確結果，
    -- 因此整張工單退出兩個分母。
    (s.business_hours_only AND NOT pr.lenient)         AS bh_excluded,
    (s.business_hours_only AND pr.lenient)             AS bh_substituted,
    (s.source = 'PM_PLAN')                             AS pm
    FROM scoped s CROSS JOIN params pr
),
decided AS (
  SELECT c.*,
    -- 解決：做完了就比時刻；沒做完但已過期限也是定論（逾期就是逾期）。
    (c.has_target AND NOT c.abandoned AND NOT c.bh_excluded
     AND (c.finished OR c.resolution_due_at < clock_timestamp()))          AS resol_decided,
    (c.finished AND c.completed_at <= c.resolution_due_at)                 AS resol_met,
    -- 回應：有人接下了就比時刻；沒人接下但已過期限也是定論。
    -- PM 工單不計回應（決定 G）。
    (c.response_due_at IS NOT NULL AND NOT c.abandoned AND NOT c.bh_excluded
     AND NOT c.pm
     AND (c.first_responded_at IS NOT NULL
          OR c.response_due_at < clock_timestamp()))                       AS resp_decided,
    (c.first_responded_at IS NOT NULL
     AND c.first_responded_at <= c.response_due_at)                        AS resp_met
    FROM classified c
)
SELECT
  d.gkey                                                        AS group_key,
  coalesce(max(d.glabel), '(未指派)')                            AS group_label,

  count(*) FILTER (WHERE d.resp_decided)                        AS response_total,
  count(*) FILTER (WHERE d.resp_decided AND d.resp_met)         AS response_met,
  count(*) FILTER (WHERE d.resp_decided AND NOT d.resp_met)     AS response_breached,
  -- 從未回應的逾期工單沒有回應時長可平均 —— 把它當 0 會讓平均值變好看，
  -- 當「窗口長度」則是憑空發明一個數字。它們仍然計入上面的分母，
  -- 只是不進這個平均。
  --
  -- 這件事**由 `avg` 忽略 NULL 自動完成**：`first_responded_at` 是 NULL 時
  -- 整個減法就是 NULL。第一版在 FILTER 裡多寫了
  -- `AND d.first_responded_at IS NOT NULL`，突變測試把它拿掉、九個測試全部
  -- 照過 —— 那個條件從來沒有作用。移掉它，因為一句聲稱自己在保護什麼
  -- 而其實沒有的程式碼，比沒有那句更容易誤導人。
  round(avg(extract(epoch FROM (d.first_responded_at - d.created_at)) / 60.0)
        FILTER (WHERE d.resp_decided), 1)::float8               AS avg_response_minutes,

  count(*) FILTER (WHERE d.resol_decided)                       AS resolution_total,
  count(*) FILTER (WHERE d.resol_decided AND d.resol_met)       AS resolution_met,
  count(*) FILTER (WHERE d.resol_decided AND NOT d.resol_met)   AS resolution_breached,
  round(avg(extract(epoch FROM (d.completed_at - d.created_at)) / 60.0)
        FILTER (WHERE d.resol_decided AND d.finished), 1)::float8 AS avg_resolution_minutes,

  round(avg(extract(epoch FROM d.waited) / 60.0)
        FILTER (WHERE d.waited > interval '0'), 1)::float8      AS avg_waiting_minutes,
  count(*) FILTER (WHERE d.reopened_count > 0)                  AS reopened,

  count(*) FILTER (WHERE NOT d.has_target)                      AS excluded_no_policy,
  count(*) FILTER (WHERE d.has_target AND NOT d.abandoned
                     AND NOT d.bh_excluded AND NOT d.resol_decided)
                                                                AS excluded_in_flight,
  count(*) FILTER (WHERE d.abandoned)                           AS excluded_abandoned,
  count(*) FILTER (WHERE d.bh_excluded)                         AS excluded_business_hours,
  count(*) FILTER (WHERE d.bh_substituted)                      AS substituted_business_hours,
  count(*) FILTER (WHERE d.pm AND d.response_due_at IS NOT NULL
                     AND NOT d.abandoned AND NOT d.bh_excluded) AS excluded_pm_response
  FROM decided d
 GROUP BY d.gkey
 ORDER BY d.gkey NULLS LAST;
$$;

COMMENT ON FUNCTION fms.report_sla_compliance(text, date, date, text) IS
  'ADR-12 量測鏈第 4 段。SECURITY INVOKER —— 場域範圍的使用者只算到自己看得見的工單。'
  '不讀 sla_state（那取決於掃描跑過沒有）；期間以 created_at 篩選（resolution_due_at 會在重開時變動）。'
  '回應與解決有不同的分母，因此刻意沒有單一的 compliance_pct。';


COMMIT;
