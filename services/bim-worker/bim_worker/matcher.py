"""設備比對 fms.asset_models 型錄。

比對鍵是既有的唯一索引 ``uq_asset_models_key``
（sql/003_spatial_assets.sql:234-236）：``(coalesce(tenant_id, 零 uuid),
lower(manufacturer), lower(model_no))``——租戶私有型錄優先於平台型錄，
兩者都用同一把大小寫不敏感的鍵。比不到就是比不到，**不猜、不模糊比對**：
使用者已經決定「比不到進待審清單」，模糊比對只會製造看似合理但其實錯誤
的歸類，比沒有歸類更糟。
"""

from __future__ import annotations

from dataclasses import dataclass
from uuid import UUID

from psycopg import Connection


@dataclass
class MatchedAssetModel:
    asset_model_id: UUID
    category_id: UUID


def find_asset_model(
    conn: Connection, tenant_id: UUID, manufacturer: str | None, model_no: str | None
) -> MatchedAssetModel | None:
    """比對一個 (manufacturer, model_no) 對到既有型錄。

    兩者缺一即視為無法比對（唯一鍵本身就是兩者的組合，缺一個就不構成
    有意義的鍵）——回 None，呼叫端負責把它送進 unresolved_elements。
    """
    if not manufacturer or not model_no:
        return None

    with conn.cursor() as cur:
        cur.execute(
            """
            SELECT id, category_id
              FROM fms.asset_models
             WHERE (tenant_id = %(tenant_id)s OR tenant_id IS NULL)
               AND lower(manufacturer) = lower(%(manufacturer)s)
               AND lower(model_no) = lower(%(model_no)s)
               AND is_active
             ORDER BY tenant_id NULLS LAST
             LIMIT 1
            """,
            {
                "tenant_id": str(tenant_id),
                "manufacturer": manufacturer,
                "model_no": model_no,
            },
        )
        row = cur.fetchone()
    if row is None:
        return None
    return MatchedAssetModel(asset_model_id=UUID(str(row[0])), category_id=UUID(str(row[1])))
