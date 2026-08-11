"""驗證 ``bim_worker.parser.parse()`` 的抽取邏輯。

不碰資料庫——這支測試只驗證「IFC 檔案裡的東西有沒有被正確抽取與分類」，
比對 asset_models、寫入 spatial_nodes/assets 是 ``ingest.py`` 的責任，
由端對端測試（對真的 Postgres）覆蓋，不在這裡重複。
"""

from __future__ import annotations

import pytest

from bim_worker.parser import match_node_type, parse


def test_floors_are_extracted_in_elevation_order(fixture_ifc_path):
    result = parse(fixture_ifc_path)
    assert [f.floor_label for f in result.floors] == ["1F", "2F"]
    assert [f.floor_level for f in result.floors] == [0, 1]


def test_spaces_are_grouped_under_the_right_floor(fixture_ifc_path):
    result = parse(fixture_ifc_path)
    assert len(result.spaces) == 4
    floor1_id = result.floors[0].ifc_global_id
    floor2_id = result.floors[1].ifc_global_id
    floor1_spaces = [s.name for s in result.spaces if s.storey_global_id == floor1_id]
    floor2_spaces = [s.name for s in result.spaces if s.storey_global_id == floor2_id]
    assert sorted(floor1_spaces) == ["會議室 A", "機房"]
    assert sorted(floor2_spaces) == ["R202", "辦公區"]


def test_node_type_keyword_matching():
    assert match_node_type("R101", "會議室 A") == "MEETING_ROOM"
    assert match_node_type("R102", "機房") == "MACHINE_ROOM"
    assert match_node_type("R201", "辦公區") == "DESK_AREA"
    # 沒有關鍵字命中的必須退回既有目錄的 ROOM，不是自創一個新型別。
    assert match_node_type("R202", None) == "ROOM"


def test_spaces_get_a_bbox_geometry_when_a_representation_exists(fixture_ifc_path):
    result = parse(fixture_ifc_path)
    for space in result.spaces:
        assert space.geometry["type"] == "bbox"
        assert space.geometry["max"][0] > space.geometry["min"][0]
        assert space.geometry["max"][1] > space.geometry["min"][1]


def test_bbox_reflects_world_position_not_just_local_shape(fixture_ifc_path):
    """fixture 把每個空間放在不同的世界座標偏移（見 make_fixture.py 的
    ``_place_at``）——若幾何算的是 local（元件自己的座標系）而非 world
    （相對整棟樓），所有空間的 bbox 會疊在原點附近，偏移差會是 0。
    這正是曾經發生過的 bug：``ifcopenshell.geom.settings()`` 預設不開
    ``use-world-coords``。
    """
    result = parse(fixture_ifc_path)
    by_name = {s.name: s for s in result.spaces}
    r101 = by_name["會議室 A"]  # 放在 (0, 0)
    r102 = by_name["機房"]  # 放在 (20, 0)
    r201 = by_name["辦公區"]  # 放在 (0, 30)

    assert r102.geometry["min"][0] - r101.geometry["min"][0] == pytest.approx(20.0, abs=0.01)
    assert r201.geometry["min"][1] - r101.geometry["min"][1] == pytest.approx(30.0, abs=0.01)


def test_equipment_is_extracted_with_manufacturer_and_model(fixture_ifc_path):
    result = parse(fixture_ifc_path)
    assert len(result.equipment) == 5
    by_name = {e.name: e for e in result.equipment}

    assert by_name["UPS-1"].manufacturer == "Delta"
    assert by_name["UPS-1"].model_no == "DPH-100K"
    assert by_name["AHU-1"].manufacturer == "Trane"
    assert by_name["AHU-1"].model_no == "CSAA-020"
    assert by_name["PROJ-1"].manufacturer == "Barco"
    assert by_name["PROJ-1"].model_no == "SP4K-15C"

    # 刻意沒有比對得到的品牌——parser 只負責忠實抽取，「比不到」是
    # matcher／ingest 的責任，這裡只確認 manufacturer/model_no 有被讀出來，
    # 不是被 parser 自己過濾掉了。
    assert by_name["PROJ-2"].manufacturer == "NoSuchBrand"
    assert by_name["PROJ-2"].model_no == "XYZ-000"

    # 沒有掛任何屬性集的設備：兩者都該是 None，不是空字串。
    assert by_name["VALVE-1"].manufacturer is None
    assert by_name["VALVE-1"].model_no is None


def test_equipment_is_linked_to_its_containing_space(fixture_ifc_path):
    result = parse(fixture_ifc_path)
    space_names = {s.ifc_global_id: s.name for s in result.spaces}
    by_name = {e.name: e for e in result.equipment}

    assert space_names[by_name["VALVE-1"].containing_space_global_id] == "會議室 A"
    assert space_names[by_name["UPS-1"].containing_space_global_id] == "機房"
    assert space_names[by_name["PROJ-1"].containing_space_global_id] == "辦公區"
