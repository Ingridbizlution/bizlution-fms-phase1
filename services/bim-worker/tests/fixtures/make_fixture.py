"""產生一個最小但完整的 IFC 測試檔：1 棟建築、2 個樓層、每層數個空間、
數個設備（其中部分刻意比對不到既有型錄）。

不手刻 IFC-SPF 文字（極度冗長且容易寫錯關聯順序）——用 IfcOpenShell 自己
的 high-level API 產生，這是產生測試 fixture 的標準做法。

用法：``python make_fixture.py <輸出路徑>``
"""

from __future__ import annotations

import sys

import ifcopenshell
import ifcopenshell.api
import ifcopenshell.api.aggregate
import ifcopenshell.api.context
import ifcopenshell.api.geometry
import ifcopenshell.api.project
import ifcopenshell.api.pset
import ifcopenshell.api.root
import ifcopenshell.api.spatial
import ifcopenshell.api.unit
import numpy as np


def _place_at(f, element, x: float, y: float) -> None:
    """把元件的世界座標移到 (x, y)——用來讓 bbox 測試能分辨「有沒有套用
    use-world-coords」：預設（僅 local）算出來的 bbox 會全部堆在原點附近，
    只有正確吃到世界座標時，不同空間的 bbox 才會落在對得上的位置。
    """
    matrix = np.eye(4)
    matrix[0][3] = x
    matrix[1][3] = y
    ifcopenshell.api.run("geometry.edit_object_placement", f, product=element, matrix=matrix)


def _add_rectangular_footprint(f, body_context, element, width: float, depth: float) -> None:
    """給一個空間掛一個簡單的長方體 3D 表示，讓 bbox 幾何抽取有東西可算。"""
    representation = ifcopenshell.api.run(
        "geometry.add_wall_representation",
        f,
        context=body_context,
        length=width,
        height=3.0,
        thickness=depth,
    )
    ifcopenshell.api.run(
        "geometry.assign_representation", f, product=element, representation=representation
    )


def build(path: str) -> None:
    f = ifcopenshell.api.run("project.create_file", version="IFC4")
    project = ifcopenshell.api.run("root.create_entity", f, ifc_class="IfcProject", name="測試專案")
    ifcopenshell.api.run("unit.assign_unit", f)
    model_context = ifcopenshell.api.run("context.add_context", f, context_type="Model")
    body_context = ifcopenshell.api.run(
        "context.add_context",
        f,
        context_type="Model",
        context_identifier="Body",
        target_view="MODEL_VIEW",
        parent=model_context,
    )

    site = ifcopenshell.api.run("root.create_entity", f, ifc_class="IfcSite", name="Site")
    building = ifcopenshell.api.run("root.create_entity", f, ifc_class="IfcBuilding", name="測試大樓")
    ifcopenshell.api.run("aggregate.assign_object", f, relating_object=project, products=[site])
    ifcopenshell.api.run("aggregate.assign_object", f, relating_object=site, products=[building])

    floors_spec = [
        (0.0, "1F", [("R101", "會議室 A", 0.0, 0.0), ("R102", "機房", 20.0, 0.0)]),
        (4.0, "2F", [("R201", "辦公區", 0.0, 30.0), ("R202", None, 20.0, 30.0)]),
    ]

    equipment_spec = [
        # (樓層序, 空間 code, IFC 類別, 名稱, manufacturer, model_no) —— 前三個
        # 對到 017 種下的平台型錄，最後兩個刻意比不到。
        (0, "R102", "IfcEnergyConversionDevice", "UPS-1", "Delta", "DPH-100K"),
        (0, "R102", "IfcFlowMovingDevice", "AHU-1", "Trane", "CSAA-020"),
        (1, "R201", "IfcFlowTerminal", "PROJ-1", "Barco", "SP4K-15C"),
        (1, "R201", "IfcFlowTerminal", "PROJ-2", "NoSuchBrand", "XYZ-000"),
        (0, "R101", "IfcFlowController", "VALVE-1", None, None),
    ]

    storeys = []
    space_by_code: dict[str, object] = {}
    for elevation, name, spaces in floors_spec:
        storey = ifcopenshell.api.run(
            "root.create_entity", f, ifc_class="IfcBuildingStorey", name=name
        )
        storey.Elevation = elevation
        ifcopenshell.api.run(
            "aggregate.assign_object", f, relating_object=building, products=[storey]
        )
        storeys.append(storey)

        for code, long_name, x, y in spaces:
            space = ifcopenshell.api.run("root.create_entity", f, ifc_class="IfcSpace", name=code)
            space.LongName = long_name
            ifcopenshell.api.run(
                "aggregate.assign_object", f, relating_object=storey, products=[space]
            )
            _add_rectangular_footprint(f, body_context, space, width=5.0, depth=4.0)
            _place_at(f, space, x, y)
            space_by_code[code] = space

    floor_codes = ["R101", "R102", "R201", "R202"]
    for floor_idx, space_code, ifc_class, name, manufacturer, model_no in equipment_spec:
        space = space_by_code[space_code]
        element = ifcopenshell.api.run("root.create_entity", f, ifc_class=ifc_class, name=name)
        ifcopenshell.api.run(
            "spatial.assign_container", f, relating_structure=space, products=[element]
        )
        if manufacturer or model_no:
            pset = ifcopenshell.api.run(
                "pset.add_pset", f, product=element, name="Pset_ManufacturerTypeInformation"
            )
            ifcopenshell.api.run(
                "pset.edit_pset",
                f,
                pset=pset,
                properties={"Manufacturer": manufacturer, "ModelReference": model_no},
            )

    f.write(path)


if __name__ == "__main__":
    build(sys.argv[1] if len(sys.argv) > 1 else "fixture.ifc")
