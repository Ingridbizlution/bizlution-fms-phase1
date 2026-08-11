-- 回退 074。
--
-- **DROP TABLE 會讓所有 SCIM token 消失，而它們無法重建。**
-- token 只存雜湊，明文在發放時回傳過一次就不存在了 —— 回退之後每一個
-- 身分來源都要重新發放，並在 Entra ID 那一側重新貼上新值。
-- 寫在這裡，因為執行回退的人不會去讀 074 的檔頭。
--
-- **稽核遮蔽會一起回退。** 也就是說回退之後，users.password_hash 會重新
-- 開始被寫進 audit_log。074 已經清掉的既有列**不會**復原（那是好事），
-- 但新的寫入會再次外洩雜湊。這是回退的真實代價，不是可以忽略的細節。
BEGIN;

-- identity_providers／users 都掛了 029 的稽核觸發器，而 audit_log 有 RLS。
-- 回退腳本沒有租戶情境，因此需要平台情境（071、072 的 down 踩過同一件事）。
SELECT set_config('app.is_platform', 'on', true);

DROP FUNCTION IF EXISTS fms.authenticate_scim_token(text);

-- 這條政策加在 **identity_providers** 上（不是 scim_tokens），
-- 因此下面的 DROP TABLE 帶不走它。少了這一行，roundtrip 的 schema 比對
-- 會看到一條多出來的政策（072 的 down 在索引上踩過同一件事）。
DROP POLICY IF EXISTS idp_scim_authenticate ON fms.identity_providers;

-- 還原 029 的觸發器函式（沒有遮蔽清單的版本）。
--
-- 這裡是完整的函式定義而不是「刪掉遮蔽那一段」—— 回退腳本必須能在不參照
-- 074 的情況下獨立執行，而 CREATE OR REPLACE 是唯一能保證結果與 029 一致的寫法。
CREATE OR REPLACE FUNCTION fms.trg_audit_row()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
  v_before   jsonb;
  v_after    jsonb;
  v_rec      jsonb;
  v_diff     text[];
  v_action   varchar(60);
BEGIN
  IF TG_OP = 'INSERT' THEN
    v_action := 'CREATE';
    v_after  := to_jsonb(NEW);
    v_rec    := v_after;
  ELSIF TG_OP = 'UPDATE' THEN
    v_action := 'UPDATE';
    v_before := to_jsonb(OLD);
    v_after  := to_jsonb(NEW);
    v_rec    := v_after;
    SELECT array_agg(key ORDER BY key) INTO v_diff
    FROM jsonb_each(v_after) a
    WHERE a.value IS DISTINCT FROM v_before -> a.key;
    IF v_diff IS NULL THEN
      RETURN NULL;
    END IF;
  ELSE
    v_action := 'DELETE';
    v_before := to_jsonb(OLD);
    v_rec    := v_before;
  END IF;

  INSERT INTO fms.audit_log
    (tenant_id, actor_user_id, actor_type, action, entity_type, entity_id,
     facility_id, before_data, after_data, diff_keys, request_id)
  VALUES (
    coalesce((v_rec ->> 'tenant_id')::uuid, fms.current_tenant_id()),
    fms.current_user_id(),
    CASE
      WHEN coalesce(current_setting('app.actor_type', true), '')
           IN ('USER','SERVICE_ACCOUNT','SYSTEM','DIRECTORY_SYNC')
      THEN current_setting('app.actor_type', true)
      ELSE 'USER'
    END,
    v_action,
    upper(TG_TABLE_NAME),
    (v_rec ->> 'id')::uuid,
    (v_rec ->> 'facility_id')::uuid,
    v_before,
    v_after,
    v_diff,
    nullif(coalesce(current_setting('app.request_id', true), ''), '')
  );

  RETURN NULL;
END;
$$;

COMMENT ON FUNCTION fms.trg_audit_row() IS
  '通用稽核觸發器。actor 來自 set_context 注入的 app.user_id，'
  ' request_id／actor_type 來自 set_request_context。刻意沒有 EXCEPTION 處理：'
  ' 稽核寫不進去就該讓業務寫入一起失敗，否則它只是一個有時候會記錄的 log。';

-- 074 改寫了 002 的欄位註解，回退時還原成原本沒有註解的狀態。
COMMENT ON COLUMN fms.identity_providers.scim_token_ref IS NULL;

DROP TABLE IF EXISTS fms.scim_tokens;

COMMIT;
