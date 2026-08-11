from __future__ import annotations

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent))

from fixtures.make_fixture import build  # noqa: E402


@pytest.fixture(scope="session")
def fixture_ifc_path(tmp_path_factory) -> str:
    """產生一次，整個測試 session 共用（IFC 檔案是唯讀的，不需要每個測試各建一份）。"""
    path = tmp_path_factory.mktemp("bim") / "fixture.ifc"
    build(str(path))
    return str(path)
