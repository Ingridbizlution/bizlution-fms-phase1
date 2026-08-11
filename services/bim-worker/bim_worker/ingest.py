"""把 ``parser.parse()`` 的抽取結果寫進資料庫。

一個模型一個交易：樓層／空間／設備要嘛全進，要嘛全不進——半成品的空間樹
比完全沒有更難處理（沒有人知道哪些節點是「這次匯入建的」）。

# 插入順序限制

``spatial_nodes`` 的 ``node_path`` 由觸發器算（見 sql/003 的
``trg_spatial_node_path``），``parent_id`` 指向還不存在的節點會拋
``23503``。因此**必須先插樓層、再插空間**，不能打亂順序 batch insert。
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass
from uuid import UUID

from psycopg import Connection

from bim_worker import matcher
from bim_worker.parser import ParseResult

logger = logging.getLogger(__name__)


@dataclass
class IngestReport:
    element_count: int
    mapped_node_count: int
    mapped_asset_count: int
    unresolved_elements: list[dict]
    parse_report: dict


def _find_or_create_building_root(
    conn: Connection, tenant_id: UUID, facility_id: UUID, bim_model_id: UUID
) -> UUID:
    """找這個場域既有的 BUILDING 層級根節點；沒有就建一個。

    多次匯入同一個場域的不同模型（例如分棟上傳）應該共用同一個根節點，
    而不是每次都建一棟新的空建築。
    """
    with conn.cursor() as cur:
        cur.execute(
            "SELECT id FROM fms.spatial_nodes"
            " WHERE facility_id = %s AND node_type_code = 'BUILDING'"
            " AND parent_id IS NULL AND deleted_at IS NULL"
            " ORDER BY created_at LIMIT 1",
            (str(facility_id),),
        )
        row = cur.fetchone()
        if row is not None:
            return UUID(str(row[0]))

        cur.execute(
            "SELECT code, name FROM fms.facilities WHERE id = %s", (str(facility_id),)
        )
        facility_code, facility_name = cur.fetchone()

        cur.execute(
            "INSERT INTO fms.spatial_nodes"
            " (tenant_id, facility_id, parent_id, node_type_code, code, name, bim_model_id)"
            " VALUES (%s, %s, NULL, 'BUILDING', %s, %s, %s)"
            " RETURNING id",
            (
                str(tenant_id),
                str(facility_id),
                facility_code,
                facility_name,
                str(bim_model_id),
            ),
        )
        return UUID(str(cur.fetchone()[0]))


def _insert_floor(
    conn: Connection,
    tenant_id: UUID,
    facility_id: UUID,
    building_id: UUID,
    bim_model_id: UUID,
    floor,
) -> UUID:
    """找這個場域既有的同一個樓層節點；沒有就建一個。

    找既有節點用 ``bim_element_id``（IFC GlobalId）比對，不是用 code——
    code 只是給人看的識別碼，本來就可能撞（兩個不相關的模型都算出
    "F0"），用它當比對鍵會把不相關的節點誤認成同一個，讓後面匯入的模型
    把前面模型的樓層/空間收編過去；``bim_model_id::delete`` 之類的操作
    再依 bim_model_id 砍節點時，就會連帶砍掉不屬於自己的資料。
    比對用 bim_element_id 之後，code 只需要在「同一個場域裡不撞」，
    因此把它算進 code 本身（f"F{floor_level}-{前八碼}"）：同一份檔案
    重新匯入時 code 不變（bim_element_id 沒變），不同模型即使樓層編號
    相同也不會撞 uq_spatial_nodes_facility_code。

    找到既有節點時順手補回 floor_level／floor_label／name——這三個欄位
    在較早的匯入裡可能是舊版程式碼留下的 NULL 或過期值，重新匯入正是
    修正它們的機會，不必额外要求使用者先刪除再重建。
    """
    code = f"F{floor.floor_level}-{floor.ifc_global_id[:8]}"
    with conn.cursor() as cur:
        cur.execute(
            "SELECT id FROM fms.spatial_nodes"
            " WHERE facility_id = %s AND bim_element_id = %s AND deleted_at IS NULL",
            (str(facility_id), floor.ifc_global_id),
        )
        row = cur.fetchone()
        if row is not None:
            node_id = UUID(str(row[0]))
            cur.execute(
                "UPDATE fms.spatial_nodes"
                " SET name = %s, floor_level = %s, floor_label = %s, bim_model_id = %s"
                " WHERE id = %s",
                (floor.name, floor.floor_level, floor.floor_label, str(bim_model_id), str(node_id)),
            )
            return node_id

        cur.execute(
            "INSERT INTO fms.spatial_nodes"
            " (tenant_id, facility_id, parent_id, node_type_code, code, name,"
            "  floor_level, floor_label, bim_model_id, bim_element_id)"
            " VALUES (%s, %s, %s, 'FLOOR', %s, %s, %s, %s, %s, %s)"
            " RETURNING id",
            (
                str(tenant_id),
                str(facility_id),
                str(building_id),
                code,
                floor.name,
                floor.floor_level,
                floor.floor_label,
                str(bim_model_id),
                floor.ifc_global_id,
            ),
        )
        return UUID(str(cur.fetchone()[0]))


def _insert_space(
    conn: Connection,
    tenant_id: UUID,
    facility_id: UUID,
    floor_id: UUID,
    bim_model_id: UUID,
    space,
    floor_level: int,
    floor_label: str,
) -> UUID:
    """找既有的同一個空間節點；沒有就建一個。找既有節點一樣用
    bim_element_id 比對（理由見 _insert_floor 的說明），code 仍然沿用
    IFC GlobalId 前八碼（比照 _insert_asset 的 asset_code 做法）——那已經
    足夠在同一個場域裡不撞，這裡只是把「拿來比對」跟「拿來當 code」的
    邏輯上分開。

    floor_level／floor_label 從所屬樓層繼承——這兩個欄位是 floor-view
    （見 fms-tenancy::spatial_tail::floor_view）用來把節點分到樓層 tab 的
    依據，只設在 FLOOR 節點自己上的話，這裡新建的 SPACE 節點會是 NULL，
    永遠比對不到任何樓層，前端因此篩掉它、即使 geometry 有算出來也畫不出來。
    找到既有節點時一併補回這兩個欄位，修正較早的匯入留下的 NULL。
    """
    code = f"SP{space.ifc_global_id[:8]}"
    with conn.cursor() as cur:
        cur.execute(
            "SELECT id FROM fms.spatial_nodes"
            " WHERE facility_id = %s AND bim_element_id = %s AND deleted_at IS NULL",
            (str(facility_id), space.ifc_global_id),
        )
        row = cur.fetchone()
        if row is not None:
            node_id = UUID(str(row[0]))
            cur.execute(
                "UPDATE fms.spatial_nodes"
                " SET name = %s, area_sqm = %s, geometry = %s,"
                "     floor_level = %s, floor_label = %s, bim_model_id = %s"
                " WHERE id = %s",
                (
                    space.name,
                    space.area_sqm,
                    json.dumps(space.geometry),
                    floor_level,
                    floor_label,
                    str(bim_model_id),
                    str(node_id),
                ),
            )
            return node_id

        cur.execute(
            "INSERT INTO fms.spatial_nodes"
            " (tenant_id, facility_id, parent_id, node_type_code, code, name,"
            "  area_sqm, geometry, bim_model_id, bim_element_id, floor_level, floor_label)"
            " VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s, %s)"
            " RETURNING id",
            (
                str(tenant_id),
                str(facility_id),
                str(floor_id),
                space.node_type_code,
                code,
                space.name,
                space.area_sqm,
                json.dumps(space.geometry),
                str(bim_model_id),
                space.ifc_global_id,
                floor_level,
                floor_label,
            ),
        )
        return UUID(str(cur.fetchone()[0]))


def _insert_asset(
    conn: Connection,
    tenant_id: UUID,
    facility_id: UUID,
    spatial_node_id: UUID | None,
    bim_model_id: UUID,
    equipment,
    matched: matcher.MatchedAssetModel,
    facility_code: str,
) -> None:
    # 沒有任何 trigger 或預設值會產生 asset_code（見 sql/003），必須自己生成。
    # 取 IFC GlobalId 前 8 碼：那本身是全域唯一的識別碼，足以避免碰撞，
    # 且同一個元件重複匯入會產生相同的碼（不是隨機的，方便追查來源）。
    asset_code = f"{facility_code}-BIM-{equipment.ifc_global_id[:8]}"
    with conn.cursor() as cur:
        cur.execute(
            "INSERT INTO fms.assets"
            " (tenant_id, facility_id, spatial_node_id, asset_model_id, category_id,"
            "  asset_code, name, bim_model_id, bim_element_id)"
            " VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s)"
            " ON CONFLICT (tenant_id, lower(asset_code)) WHERE deleted_at IS NULL DO NOTHING",
            (
                str(tenant_id),
                str(facility_id),
                str(spatial_node_id) if spatial_node_id else None,
                str(matched.asset_model_id),
                str(matched.category_id),
                asset_code,
                equipment.name,
                str(bim_model_id),
                equipment.ifc_global_id,
            ),
        )


def ingest(
    conn: Connection,
    tenant_id: UUID,
    facility_id: UUID,
    bim_model_id: UUID,
    result: ParseResult,
) -> IngestReport:
    """把一份解析結果寫進資料庫。呼叫端負責交易邊界（見 ``db.tenant_transaction``）。"""
    with conn.cursor() as cur:
        cur.execute("SELECT code FROM fms.facilities WHERE id = %s", (str(facility_id),))
        (facility_code,) = cur.fetchone()

    building_id = _find_or_create_building_root(conn, tenant_id, facility_id, bim_model_id)

    floor_ids: dict[str, UUID] = {}
    floor_meta: dict[str, tuple[int, str]] = {}
    for floor in result.floors:
        floor_ids[floor.ifc_global_id] = _insert_floor(
            conn, tenant_id, facility_id, building_id, bim_model_id, floor
        )
        floor_meta[floor.ifc_global_id] = (floor.floor_level, floor.floor_label)

    space_ids: dict[str, UUID] = {}
    skipped_spaces = 0
    for space in result.spaces:
        floor_id = floor_ids.get(space.storey_global_id)
        if floor_id is None:
            # 空間指向一個沒有被抽取出來的樓層（理論上不該發生，因為
            # parser 只在 IfcBuildingStorey 底下才收集 IfcSpace）——保守起見
            # 略過並計入統計，不要讓一整批匯入因為一個異常資料而中止。
            skipped_spaces += 1
            continue
        floor_level, floor_label = floor_meta[space.storey_global_id]
        space_ids[space.ifc_global_id] = _insert_space(
            conn, tenant_id, facility_id, floor_id, bim_model_id, space,
            floor_level, floor_label,
        )

    mapped_asset_count = 0
    unresolved: list[dict] = []
    for equipment in result.equipment:
        matched = matcher.find_asset_model(
            conn, tenant_id, equipment.manufacturer, equipment.model_no
        )
        spatial_node_id = space_ids.get(equipment.containing_space_global_id or "")
        if matched is not None:
            _insert_asset(
                conn,
                tenant_id,
                facility_id,
                spatial_node_id,
                bim_model_id,
                equipment,
                matched,
                facility_code,
            )
            mapped_asset_count += 1
        else:
            unresolved.append(
                {
                    "bim_element_id": equipment.ifc_global_id,
                    "name": equipment.name,
                    "ifc_type": equipment.ifc_type,
                    "tag": equipment.tag,
                    "manufacturer": equipment.manufacturer,
                    "model_no": equipment.model_no,
                    "candidate_spatial_node_id": (
                        str(spatial_node_id) if spatial_node_id else None
                    ),
                }
            )

    parse_report = {
        "floors": len(result.floors),
        "spaces": len(result.spaces),
        "spaces_skipped_no_floor": skipped_spaces,
        "equipment_total": len(result.equipment),
        "equipment_matched": mapped_asset_count,
        "equipment_unresolved": len(unresolved),
    }

    return IngestReport(
        element_count=len(result.floors) + len(result.spaces) + len(result.equipment),
        mapped_node_count=len(floor_ids) + len(space_ids),
        mapped_asset_count=mapped_asset_count,
        unresolved_elements=unresolved,
        parse_report=parse_report,
    )
