-- 回退 031。還原 008 的授權：FACILITY_ADMIN 拿回 role:assign
-- （一筆在它宣告的範圍內永遠用不到的授權），ORG_MANAGER 失去 organization:write。
--
-- 需要平台情境，理由與 031 相同（見那個檔的檔頭）：role_permissions 掛了 029
-- 的稽核觸發器，而稽核列在沒有租戶情境時 tenant_id 是 NULL，會被 audit_log
-- 的 tenant_isolation 擋下。
--
-- down migration 是**逆序**執行的，因此 down/031 跑在 down/029 之前 ——
-- 觸發器此時還在。roundtrip 就是這樣抓到它的。
SET app.is_platform = 'on';

BEGIN;
SET search_path = fms, public;

INSERT INTO fms.role_permissions (role_id, permission_code)
SELECT r.id, 'role:assign' FROM fms.roles r WHERE r.code = 'FACILITY_ADMIN'
ON CONFLICT DO NOTHING;

DELETE FROM fms.role_permissions rp
 USING fms.roles r
 WHERE rp.role_id = r.id
   AND r.code = 'ORG_MANAGER'
   AND rp.permission_code = 'organization:write';

COMMIT;
