# =============================================================================
# fms-server / fms-jobs — 多階段建置，共用同一個 builder，兩個獨立 runtime target
# =============================================================================
# **build context 必須是 repo 根目錄**，不是這個 app/ 目錄——
# `fms-server/src/docs.rs` 用 `include_str!("../../../../api/openapi.yaml")`
# 把契約與 vendored Swagger UI 嵌進二進位，那個相對路徑數上去要碰到
# repo 根目錄的 `api/`。build context 若只給 `app/`，`api/` 根本不在裡面，
# 編譯期就會直接失敗（找不到檔案）——這是本檔第一版就踩到的坑。
#
#   docker build -f app/Dockerfile --target fms-server -t fms-server .
#   docker build -f app/Dockerfile --target fms-jobs   -t fms-jobs   .
#   （從 repo 根目錄執行，注意結尾的 `.`）
#
# 兩個 target 共用同一個 builder stage，一次編譯兩支二進位——不必為
# fms-jobs 另開一份 Dockerfile 重新編一次整個 workspace 的依賴樹。
#
# `SQLX_OFFLINE=true`：build 環境沒有 Postgres 可連，`query!`／`query_as!`
# 巨集改讀 `app/.sqlx/`（unscoped `cargo sqlx prepare --workspace` 的
# 產生物，已進版控）裡快取好的查詢 metadata 做編譯期驗證。沒設這個變數，
# 這裡的 build 一定會直接失敗。
# =============================================================================

FROM rust:1-slim-bookworm AS builder
WORKDIR /build
ENV SQLX_OFFLINE=true

# rustls 系的加密後端（ring／aws-lc-rs）在編譯期需要 C 編譯器；
# pkg-config 給其餘少數會探測系統函式庫的依賴用。
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

# 只複製 include_str! 真正會碰到的兩棵子樹（app/ 與 api/），不是整個
# repo（docs/、sql/、services/ 等都跟編譯無關，複製進去只是白白變大
# build context）。相對路徑刻意維持跟真實 repo 一致：
# `/build/app/crates/fms-server/src/../../../../api/openapi.yaml`
# 展開後正好是 `/build/api/openapi.yaml`。
COPY app/ app/
COPY api/ api/

WORKDIR /build/app
RUN cargo build --release -p fms-server -p fms-jobs

# -----------------------------------------------------------------------------
# fms-server — 對外服務的 HTTP API
# -----------------------------------------------------------------------------
FROM debian:bookworm-slim AS fms-server

# ca-certificates：這支服務會主動對外發 HTTPS（Microsoft Graph、OTel
# collector），rustls 需要系統的 CA 憑證清單才能驗證對方憑證。
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 fms

COPY --from=builder /build/app/target/release/fms-server /usr/local/bin/fms-server

USER fms
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/fms-server"]

# -----------------------------------------------------------------------------
# fms-jobs — 背景常駐 worker，沒有對外的 HTTP listener
# -----------------------------------------------------------------------------
FROM debian:bookworm-slim AS fms-jobs

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 fms

COPY --from=builder /build/app/target/release/fms-jobs /usr/local/bin/fms-jobs

USER fms
ENTRYPOINT ["/usr/local/bin/fms-jobs"]
