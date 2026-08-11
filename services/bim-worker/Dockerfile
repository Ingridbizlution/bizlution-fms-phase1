# =============================================================================
# bim-worker — 常駐輪詢服務，把上傳的 IFC 模型解析成樓層／空間／設備
# =============================================================================
# 跟 app/Dockerfile 的 fms-jobs 同一個角色（背景常駐 worker，沒有對外的
# HTTP listener），只是語言換成 Python——ifcopenshell 沒有對等的 Rust
# 生態，語言邊界見 ADR-09。README.md 說「容器化刻意留白，等真的要部署
# 時再決定」——這就是那個決定：build context 是這個目錄本身，不像
# app/Dockerfile 需要往 repo 根目錄借東西：
#
#   docker build -f services/bim-worker/Dockerfile -t bim-worker services/bim-worker
#
FROM python:3.11-slim

WORKDIR /app

# ifcopenshell 的 manylinux wheel 帶了大部分需要的共享函式庫，但幾何處理
# 那段仍會在執行期探測系統的 OpenGL——沒裝的症狀是 import 階段完全正常
# （「碰巧沒事」），要等第一次真的解析帶幾何的 IFC 檔才炸開，這裡先裝好
# 而不是等出事才補。
RUN apt-get update && apt-get install -y --no-install-recommends \
    libgl1 \
    libglu1-mesa \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 bim

COPY pyproject.toml ./
COPY bim_worker/ bim_worker/
RUN pip install --no-cache-dir .

USER bim
ENTRYPOINT ["python3", "-m", "bim_worker.main"]
