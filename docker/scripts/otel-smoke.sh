#!/bin/sh
# =============================================================================
# 驗證 span 真的從應用程式抵達 collector
# =============================================================================
# WBS 1.7 記著：「`otlp_endpoint()` 已備妥開關，但**未接上 exporter，也未經
# 驗證**」。而 `observability.rs` 的模組檔頭更早就寫著「要匯出到 collector 時
# 由 `init_telemetry` 掛上 tracing-opentelemetry」—— **那支函式一直不存在**。
#
# 這是這個 codebase 反覆出現的那個缺陷類別的最後一個活著的例子：
# 一份宣告，沒有人讀它。
#
# -----------------------------------------------------------------------------
# 為什麼不用 curl 直接戳 collector
# -----------------------------------------------------------------------------
# 那也會在 collector 的 log 裡看到東西，但它證明的是「collector 會收」，
# 不是「我們的程式會送」。兩者之間的那段空隙正好就是上面那個缺口。
#
# 因此走 `cargo run --example otlp_smoke` —— 同一支 `init_telemetry`、
# 同一個 `TelemetryGuard`、同一條 flush 路徑。
#
# -----------------------------------------------------------------------------
# 防空轉
# -----------------------------------------------------------------------------
# 每次跑用一個唯一 marker，並且**只看這次執行之後**的 collector log。
# 少了這兩件事，上一次跑留下的輸出會讓這次無條件通過 ——
# 而那種測試會在真的壞掉的那天說「通過」。
# =============================================================================
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
PORT="${OTEL_HTTP_PORT:-4318}"
ENDPOINT="http://localhost:${PORT}"
MARKER="smoke-$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"

echo "==> OTLP 端到端驗證（marker=$MARKER）"

# collector 就緒了嗎。
#
# **這一格的第一版有兩個錯，兩個都只有真的跑一次才會現形**（CI 首跑抓到）：
#
#   1. **沒有等待。** `docker compose up -d` 回來時容器是 Started，
#      但 collector 進程還沒 bind 4318。實測容器啟動後 0.03 秒就檢查，
#      必然失敗。
#   2. **用 GET 探測。** OTLP 的 HTTP receiver 只收 POST；對 GET 它可能
#      直接斷線，那樣這個檢查會**永遠**失敗而不只是太早 ——
#      而錯誤訊息會叫人「先跑 make otel」，把人指向錯的方向。
#
# 因此改成：**POST 一個空 body，只要拿到任何 HTTP 狀態碼就算可達**
# （400／415 都代表「連上了、HTTP 講得通、只是這個 body 它不收」）。
# curl 的 %{http_code} 在連不上時是 000，那才是不可達。
echo -n "  [1] 等 collector 就緒"
READY=0
i=0
while [ "$i" -lt 30 ]; do
  CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 \
         -X POST -H 'Content-Type: application/x-protobuf' \
         --data-binary '' "$ENDPOINT/v1/traces" 2>/dev/null || echo 000)
  # **判「是不是一個真的 HTTP 狀態碼」，不是「等於字串 000」。**
  # 第一版寫 `[ "$CODE" != "000" ]`，而 curl 實際印出的是 `000000`
  # （某些版本會對每次連線嘗試各印一次）—— `000000 != 000` 成立，
  # 於是就緒檢查**假通過**。比對字串相等在這裡是錯的形狀。
  case "$CODE" in
    [1-5][0-9][0-9])
      READY=1
      break
      ;;
  esac
  echo -n "."
  sleep 1
  i=$((i + 1))
done
echo ""
if [ "$READY" != "1" ]; then
  echo "  !! $ENDPOINT 在 30 秒內沒有回應任何 HTTP 狀態 —— collector 沒起來？" >&2
  (cd "$ROOT/docker" && docker compose logs --tail 30 otel-collector) >&2 || true
  exit 1
fi
echo "  [1] collector 在 $ENDPOINT（HTTP $CODE）"

# 只看這一刻之後的 log。**必須在 collector 就緒之後才取這個時間點** ——
# 若在等待之前取，等待的那幾秒也會被納入視窗，而那段時間 collector 正在
# 印自己的啟動訊息，會讓後面的 grep 範圍變大（雖然 marker 是唯一的，
# 但視窗越小越不容易被別的東西干擾）。
# `--since` 用 RFC3339；退一秒避免邊界誤差。
SINCE=$(date -u -v-1S '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
        || date -u -d '1 second ago' '+%Y-%m-%dT%H:%M:%SZ')

cd "$ROOT/app"
if ! OTEL_EXPORTER_OTLP_ENDPOINT="$ENDPOINT" OTLP_SMOKE_MARKER="$MARKER" \
     cargo run --quiet -p fms-shared --example otlp_smoke; then
  echo "  !! 送出 span 的程式失敗了" >&2
  exit 1
fi
echo "  [2] 應用端已送出"

# 批次匯出器與 collector 的 batch processor 各有一段延遲（設定是 1s）。
# 輪詢而不是固定 sleep：固定 sleep 不是太短就是太慢。
cd "$ROOT/docker"
FOUND=0
i=0
while [ "$i" -lt 30 ]; do
  if docker compose logs --since "$SINCE" otel-collector 2>/dev/null | grep -q "$MARKER"; then
    FOUND=1
    break
  fi
  sleep 1
  i=$((i + 1))
done

if [ "$FOUND" != "1" ]; then
  echo "  !! collector 在 30 秒內沒有收到 marker=$MARKER" >&2
  LINES=$(docker compose logs --since "$SINCE" otel-collector 2>/dev/null | wc -l | tr -d ' ')
  if [ "$LINES" -lt 2 ]; then
    echo "     collector 的 log 是**空的**（$LINES 行）。應用端若沒有報 flush 失敗，" >&2
    echo "     那 span 很可能已經送到了，而問題在 collector 沒有輸出 ——" >&2
    echo "     檢查 otel/collector.yaml 的 telemetry.logs.level（debug exporter 走 info）。" >&2
  fi
  echo "     最近的 collector 輸出（$LINES 行）：" >&2
  docker compose logs --since "$SINCE" --tail 40 otel-collector >&2 || true
  exit 1
fi
echo "  [3] collector 收到了 marker=$MARKER"

# 服務名也要對 —— 少了它，「有 span 抵達」不足以證明是**我們**送的。
if docker compose logs --since "$SINCE" otel-collector 2>/dev/null \
   | grep -q "fms-otlp-smoke"; then
  echo "  [4] service.name = fms-otlp-smoke"
else
  echo "  !! 收到了 span 但沒有預期的 service.name" >&2
  exit 1
fi

echo ""
echo "==> OTLP 端到端通過：init_telemetry → OTLP/HTTP → collector"
