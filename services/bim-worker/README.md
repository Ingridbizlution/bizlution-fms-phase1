# BIM 匯入解析器

見 `sql/080_bim_ingest_worker_service_account.sql` 與
[BIM 匯入解析器 + 2D 樓層圖儀表板](../../docs/adr/ADR-09-application-language.md) 的
語言邊界決定。這是輪詢 `bim_models.status = 'UPLOADED'` 的常駐服務，把 IFC
檔案解析成樓層／空間／設備，與 Rust 那 13 條背景迴圈（`fms-jobs`）同一個
形狀，只是換了語言（Rust 沒有對等的 IFC 解析生態）。

## 執行

```bash
cd services/bim-worker
python3 -m venv .venv && source .venv/bin/activate
pip install -e ".[dev]"

export OWNER_DATABASE_URL="postgres://fms_owner:<password>@localhost:5433/fms"
export BIM_INGEST_WORKER_USER_ID="f5000000-0000-4000-8000-000000000003"  # 示範租戶服務帳號，見 080
export S3_ENDPOINT="http://localhost:9000"
export S3_ACCESS_KEY="fmsminio"
export S3_SECRET_KEY="<password>"

python3 -m bim_worker.main
```

`OWNER_DATABASE_URL` 用 `fms_owner`（不是 `fms_app`）——需要平台情境才能跨
租戶找到期模型，理由與 `fms-jobs` 的 `PM_GENERATOR_USER_ID` 完全一樣，見
`bim_worker/db.py` 的模組檔頭。

生產環境的容器化（Dockerfile／compose 條目）刻意留白：這個 repo 目前沒有
任何 app 服務跑在 docker-compose 裡（見 `docker/README.md`），幫這一個服務
破例會不一致，等真的要部署時再決定。

## 測試

```bash
pip install -e ".[dev]"
pytest tests/
```

單元測試（`tests/test_parser.py`）只驗證 IFC 抽取邏輯，不碰資料庫——
`tests/fixtures/make_fixture.py` 用 IfcOpenShell 自己的 API 產生一個最小
測試檔（1 棟建築、2 個樓層、4 個空間、5 個設備，其中 2 個刻意比對不到
既有的 `asset_models` 型錄）。寫入邏輯（`ingest.py`）的正確性由對真實
Postgres 的端對端驗證覆蓋，不在這裡重複。

## 範圍（v1）

- 只解析 `source_format = 'IFC'`；其餘格式（RVT/NWD/DWG）標成
  `PARSE_FAILED` 並在 `parse_report` 說明原因，不假裝處理
- 空間幾何只做 bounding box（`{"type": "bbox", "min": [x,y], "max": [x,y]}`），
  不做完整多邊形足跡
- 設備比對 `fms.asset_models`（依 `manufacturer` + `model_no`，既有的
  `uq_asset_models_key` 唯一鍵）；比不到就進 `bim_models.unresolved_elements`
  待人工用既有的 `POST /bim-models/{id}/mappings` 端點手動處理，不猜測建立
- 3D 檢視器（Autodesk Forge/APS，對應 `bim_models.viewer_urn` 欄位）不在
  這次範圍內
