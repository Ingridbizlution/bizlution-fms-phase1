"""資料庫存取層。

核心不變量對齊 Rust 的 ``fms_shared::db::begin_tenant_tx``（app/crates/fms-shared/src/db.rs:93-128）
—— 這裡逐字複製那個 SQL 呼叫序列，不是自己發明一套。背景作業的判斷方式與
``fms_maintenance::pm_worker::PmGenerator`` 完全一樣：同一個 ``fms_owner`` 連線做兩件事——

1. 跨租戶找待處理列（需要平台情境，唯讀，查完 rollback）
2. 逐一以該租戶的服務帳號身分寫入（``fms.set_context`` + facility_scope）

連線角色必須是 ``fms_owner``（``fms_platform`` 成員）：``set_config('app.is_platform', ...)``
在非平台角色上會被 013 的硬化擋下；而 ``facility_scope`` 對 owner 一樣生效
（FORCE ROW LEVEL SECURITY），所以逐租戶寫入時的隔離不會因為連線角色是
owner 而失效。
"""

from __future__ import annotations

import contextlib
from typing import Iterator
from uuid import UUID

import psycopg
from psycopg import Connection

ALL_ZERO_UUID = "00000000-0000-0000-0000-000000000000"


def connect(dsn: str) -> Connection:
    return psycopg.connect(dsn, autocommit=False)


def find_due_models(conn: Connection) -> list[tuple[UUID, UUID]]:
    """跨租戶找出待解析的 BIM 模型（``status = 'UPLOADED'``）。

    唯讀，查完立刻 rollback —— 這裡不該持有任何鎖或半開的交易。
    """
    with conn.cursor() as cur:
        cur.execute("SELECT set_config('app.is_platform', 'on', true)")
        cur.execute(
            "SELECT tenant_id, id FROM fms.bim_models"
            " WHERE status = 'UPLOADED' ORDER BY created_at"
        )
        rows = cur.fetchall()
    conn.rollback()
    return [(UUID(str(r[0])), UUID(str(r[1]))) for r in rows]


@contextlib.contextmanager
def tenant_transaction(
    conn: Connection, tenant_id: UUID, actor_user_id: UUID
) -> Iterator[Connection]:
    """租戶情境的交易。成功時 commit，發生例外時 rollback 並重新拋出。

    ``actor_user_id`` 必須是持有 TENANT 範圍角色指派的服務帳號
    （見 sql/080），否則 ``user_accessible_facilities`` 回傳空集合，
    這裡會把 ``app.facility_ids`` 設成全零 UUID 哨兵，facility_scope
    政策因此濾掉每一列 —— 症狀是寫入「成功」但查不到任何東西，沒有錯誤。
    """
    with conn.cursor() as cur:
        cur.execute(
            "SELECT fms.set_context(%s, %s, false),"
            " fms.set_request_context(%s, %s)",
            (str(tenant_id), str(actor_user_id), None, "SERVICE_ACCOUNT"),
        )
        cur.execute(
            "SELECT facility_id FROM fms.user_accessible_facilities(%s)",
            (str(actor_user_id),),
        )
        facility_ids = [str(row[0]) for row in cur.fetchall()]
        ids_csv = ",".join(facility_ids) if facility_ids else ALL_ZERO_UUID
        cur.execute("SELECT set_config('app.facility_ids', %s, true)", (ids_csv,))
    try:
        yield conn
    except Exception:
        conn.rollback()
        raise
    else:
        conn.commit()
