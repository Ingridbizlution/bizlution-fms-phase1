"""``find_asset_model`` 在缺 manufacturer 或 model_no 時必須短路回 None，
不該送一個沒有意義的查詢去資料庫（唯一鍵本身就是兩者的組合）。這部分
不需要真的資料庫連線就能驗證。
"""

from __future__ import annotations

from uuid import uuid4

from bim_worker.matcher import find_asset_model


def test_missing_manufacturer_or_model_short_circuits_without_a_query():
    tenant_id = uuid4()
    # 傳 None 當連線：若邏輯真的短路，就不會嘗試呼叫連線的任何方法，
    # 傳 None 也不會出錯——如果出錯，代表短路失敗、漏呼叫到資料庫層。
    assert find_asset_model(None, tenant_id, None, "DPH-100K") is None
    assert find_asset_model(None, tenant_id, "Delta", None) is None
    assert find_asset_model(None, tenant_id, None, None) is None
    assert find_asset_model(None, tenant_id, "", "") is None
