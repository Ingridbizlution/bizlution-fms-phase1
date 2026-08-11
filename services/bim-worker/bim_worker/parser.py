"""IFC 抽取邏輯。純函式，不碰資料庫 —— 這樣才能在沒有 Postgres 的情況下
單元測試（見 ``tests/test_parser.py``）。

# 為什麼用 IfcOpenShell 而不自己寫解析器

見 ADR-09：IFC 是規格不是領域規則，Rust 沒有對等的解析生態，自己寫等於
再造一份 IFC 解析器。這條原則與 ``fms_shared::schedule``（RRULE）、
``fms_shared::cron`` 完全一樣，只是這次的真實來源是 buildingSMART 的 IFC 規格。

# geometry 只做 bounding box（v1）

``spatial_nodes.geometry`` 的欄位註解允許「bbox / polygon / centroid」——
這裡選最簡單、對任何有 3D 表示的元件都穩定可算的 bbox，不做完整多邊形
足跡。足跡需要處理任意形狀的 2D 投影與孔洞，複雜度與這次的範圍不成比例
（見計畫檔的「明確排除」一節）。
"""

from __future__ import annotations

from dataclasses import dataclass, field

import ifcopenshell
import ifcopenshell.geom
import ifcopenshell.util.element as elutil

# 既有的 18 種 spatial_node_types（sql/008_seed_platform.sql）。
# **不新增型別** —— 關鍵字比對只在既有目錄裡選，比不到就用 ROOM 兜底。
_NODE_TYPE_KEYWORDS: list[tuple[str, tuple[str, ...]]] = [
    ("MEETING_ROOM", ("會議", "meeting")),
    ("AUDITORIUM", ("禮堂", "報告廳", "auditorium")),
    ("CLASSROOM", ("教室", "classroom")),
    ("LAB", ("實驗室", "laboratory", "lab")),
    ("MACHINE_ROOM", ("機房", "mechanical room", "machine room")),
    ("PARKING_SPACE", ("停車位", "parking space", "parking stall")),
    ("PARKING", ("停車場", "car park", "parking")),
    ("CORRIDOR", ("走廊", "廊道", "corridor", "hallway")),
    ("SHAFT", ("管道間", "shaft")),
    ("DESK_AREA", ("辦公區", "desk area", "open office")),
]
_DEFAULT_SPACE_TYPE = "ROOM"

# MEP 相關的 IFC 類別 —— 只有這些會被當成「設備」比對 asset_models。
# 刻意不含純被動的管線／風管（IfcFlowSegment／IfcFlowFitting）：那些是
# 分佈路徑，不是可維護的資產個體。
EQUIPMENT_IFC_TYPES = frozenset(
    {
        "IfcFlowTerminal",
        "IfcFlowController",
        "IfcFlowMovingDevice",
        "IfcFlowStorageDevice",
        "IfcEnergyConversionDevice",
    }
)

_MANUFACTURER_PSET = "Pset_ManufacturerTypeInformation"


@dataclass
class ParsedFloor:
    ifc_global_id: str
    name: str
    floor_level: int
    floor_label: str


@dataclass
class ParsedSpace:
    ifc_global_id: str
    name: str
    node_type_code: str
    area_sqm: float | None
    geometry: dict
    storey_global_id: str


@dataclass
class ParsedEquipment:
    ifc_global_id: str
    name: str
    ifc_type: str
    tag: str | None
    manufacturer: str | None
    model_no: str | None
    containing_space_global_id: str | None


@dataclass
class ParseResult:
    floors: list[ParsedFloor] = field(default_factory=list)
    spaces: list[ParsedSpace] = field(default_factory=list)
    equipment: list[ParsedEquipment] = field(default_factory=list)


def match_node_type(name: str | None, long_name: str | None) -> str:
    """依關鍵字把空間名稱比對到既有的 spatial_node_types 目錄。

    比不到任何關鍵字時退回 ``ROOM`` —— 這不是失敗，只是「沒有更精確的
    分類資訊」，比對失敗不該讓整個空間匯入失敗。
    """
    haystack = " ".join(filter(None, [name, long_name])).lower()
    for code, keywords in _NODE_TYPE_KEYWORDS:
        if any(kw.lower() in haystack for kw in keywords):
            return code
    return _DEFAULT_SPACE_TYPE


def _spatial_parent(node) -> object | None:
    """空間階層（Site→Building→Storey→Space）的直接上層節點。

    **不是** ``elutil.get_container``——那支函式走的是
    ``IfcRelContainedInSpatialStructure``（給 ``IfcElement`` 用，例如設備
    放在哪個空間裡），空間節點彼此的巢狀關係走的是 ``IfcRelAggregates``
    （``Decomposes``/``IsDecomposedBy``），兩種是不同的關聯，混用會讓
    ``get_container`` 對空間節點一律回 ``None``。
    """
    decomposes = getattr(node, "Decomposes", None) or []
    for rel in decomposes:
        if rel.is_a("IfcRelAggregates"):
            return rel.RelatingObject
    return None


def _bbox_geometry(element) -> dict:
    """算 3D mesh 再投影到 XY 平面取 min/max。沒有可算的表示時回空字典——
    與既有欄位的語意一致：空 ``{}`` 代表「沒有人匯入幾何」，不是形狀為空。
    """
    settings = ifcopenshell.geom.settings()
    settings.set("use-world-coords", True)
    try:
        shape = ifcopenshell.geom.create_shape(settings, element)
    except Exception:
        return {}
    verts = shape.geometry.verts
    if not verts:
        return {}
    xs = verts[0::3]
    ys = verts[1::3]
    return {
        "type": "bbox",
        "min": [round(min(xs), 3), round(min(ys), 3)],
        "max": [round(max(xs), 3), round(max(ys), 3)],
    }


def _space_area_sqm(space) -> float | None:
    """從 IfcElementQuantity 找面積數量。沒有就回 None——不要猜。"""
    for rel in getattr(space, "IsDefinedBy", []) or []:
        if not rel.is_a("IfcRelDefinesByProperties"):
            continue
        definition = rel.RelatingPropertyDefinition
        if not definition.is_a("IfcElementQuantity"):
            continue
        for q in definition.Quantities:
            if q.is_a("IfcQuantityArea"):
                return float(q.AreaValue)
    return None


def _manufacturer_model(element) -> tuple[str | None, str | None]:
    """先看標準 Pset_ManufacturerTypeInformation，再退而掃描全部 pset 找
    含 manufacturer/model 關鍵字的屬性名稱（有些來源檔案不遵照標準 pset 名）。
    """
    psets = elutil.get_psets(element)
    std = psets.get(_MANUFACTURER_PSET, {})
    manufacturer = std.get("Manufacturer")
    model = std.get("ModelReference") or std.get("ModelLabel")
    if manufacturer or model:
        return manufacturer, model

    for props in psets.values():
        for key, value in props.items():
            if not isinstance(value, str):
                continue
            lowered = key.lower()
            if manufacturer is None and "manufactur" in lowered:
                manufacturer = value
            if model is None and "model" in lowered:
                model = value
    return manufacturer, model


def parse(path: str) -> ParseResult:
    """解析一個 IFC 檔案，回傳樓層／空間／設備的中介表示。

    這支函式只做抽取，不做任何資料庫比對或寫入 —— 比對邏輯在
    ``matcher.py``，寫入邏輯在 ``ingest.py``。分開是為了讓抽取邏輯可以
    離線單元測試。
    """
    ifc = ifcopenshell.open(path)
    result = ParseResult()

    storeys = sorted(
        ifc.by_type("IfcBuildingStorey"),
        key=lambda s: (s.Elevation if s.Elevation is not None else 0.0),
    )
    for level, storey in enumerate(storeys):
        result.floors.append(
            ParsedFloor(
                ifc_global_id=storey.GlobalId,
                name=storey.Name or storey.GlobalId,
                floor_level=level,
                floor_label=storey.Name or f"F{level}",
            )
        )

    for space in ifc.by_type("IfcSpace"):
        container = _spatial_parent(space)
        if container is None or not container.is_a("IfcBuildingStorey"):
            # 沒有掛在任何樓層下的空間：匯入時無法決定它的 parent，略過
            # 並讓它落在 parse_report 的統計裡，而不是靜默略過還算成功。
            continue
        result.spaces.append(
            ParsedSpace(
                ifc_global_id=space.GlobalId,
                name=space.LongName or space.Name or space.GlobalId,
                node_type_code=match_node_type(space.Name, space.LongName),
                area_sqm=_space_area_sqm(space),
                geometry=_bbox_geometry(space),
                storey_global_id=container.GlobalId,
            )
        )

    for ifc_type in EQUIPMENT_IFC_TYPES:
        for element in ifc.by_type(ifc_type):
            container = elutil.get_container(element, ifc_class="IfcSpace")
            manufacturer, model_no = _manufacturer_model(element)
            result.equipment.append(
                ParsedEquipment(
                    ifc_global_id=element.GlobalId,
                    name=element.Name or element.GlobalId,
                    ifc_type=ifc_type,
                    tag=getattr(element, "Tag", None),
                    manufacturer=manufacturer,
                    model_no=model_no,
                    containing_space_global_id=(
                        container.GlobalId if container is not None else None
                    ),
                )
            )

    return result
