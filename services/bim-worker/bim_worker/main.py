"""BIM 匯入解析器的常駐迴圈。

與 Rust 那 13 條背景迴圈（``fms-jobs``）同一個形狀：單一連線池、
輪詢待處理列、逐一以服務帳號身分處理、一筆失敗不拖累其他筆、收到
SIGTERM/SIGINT 才停。這裡是 Python 版本，邏輯上是同一個 worker 家族的
第 14 個成員，只是換了語言（見 ADR-09 的邊界）。
"""

from __future__ import annotations

import logging
import os
import signal
import sys
import tempfile
import time
import traceback
from uuid import UUID

import boto3
from dotenv import load_dotenv

from bim_worker import db, ingest
from bim_worker.parser import parse

logger = logging.getLogger("bim_worker")

# BIM 上傳是人工觸發的低頻事件，不像 SLA 逾期需要分鐘級即時性，
# 也不像證照到期那種天級——30 秒讓「上傳後多快開始解析」接近即時，
# 又不會空轉太頻繁。
POLL_INTERVAL_SECONDS = 30

_SUPPORTED_SOURCE_FORMATS = frozenset({"IFC"})


class Shutdown(Exception):
    pass


def _install_signal_handlers() -> None:
    def _handler(signum, frame):  # noqa: ARG001
        raise Shutdown()

    signal.signal(signal.SIGTERM, _handler)
    signal.signal(signal.SIGINT, _handler)


def _s3_client():
    return boto3.client(
        "s3",
        endpoint_url=os.environ["S3_ENDPOINT"],
        aws_access_key_id=os.environ["S3_ACCESS_KEY"],
        aws_secret_access_key=os.environ["S3_SECRET_KEY"],
    )


def _download_model(s3, bucket: str, key: str) -> str:
    fd, path = tempfile.mkstemp(suffix=".ifc")
    os.close(fd)
    s3.download_file(bucket, key, path)
    return path


def _mark_parsing(conn, model_id: UUID) -> None:
    with conn.cursor() as cur:
        cur.execute(
            "UPDATE fms.bim_models SET status = 'PARSING', updated_at = clock_timestamp()"
            " WHERE id = %s",
            (str(model_id),),
        )


def _mark_parsed(conn, model_id: UUID, report: ingest.IngestReport) -> None:
    with conn.cursor() as cur:
        cur.execute(
            "UPDATE fms.bim_models SET"
            " status = 'PARSED', parsed_at = clock_timestamp(), updated_at = clock_timestamp(),"
            " element_count = %s, mapped_node_count = %s, mapped_asset_count = %s,"
            " unresolved_elements = %s, parse_report = %s"
            " WHERE id = %s",
            (
                report.element_count,
                report.mapped_node_count,
                report.mapped_asset_count,
                _to_json(report.unresolved_elements),
                _to_json(report.parse_report),
                str(model_id),
            ),
        )


def _mark_failed(conn, model_id: UUID, reason: str) -> None:
    # 獨立一個交易：解析失敗時上面那個交易已經 rollback，這裡要用一個
    # 乾淨的交易才寫得進去。
    with conn.cursor() as cur:
        cur.execute(
            "UPDATE fms.bim_models SET"
            " status = 'PARSE_FAILED', updated_at = clock_timestamp(),"
            " parse_report = %s"
            " WHERE id = %s",
            (_to_json({"error": reason}), str(model_id)),
        )
    conn.commit()


def _to_json(value) -> str:
    import json

    return json.dumps(value)


def process_one(conn, s3, actor_user_id: UUID, tenant_id: UUID, model_id: UUID) -> None:
    # `find_due_models` 的 `app.is_platform` 是 transaction-local
    # （`set_config` 第三個參數 `true`），它自己那個 rollback 一做，設定就
    # 沒了——這裡若不重新設，FORCE ROW LEVEL SECURITY 會把這個沒有租戶
    # 情境的 SELECT 濾成 0 筆，`fetchone()` 拿到 `None` 而不是錯誤。
    with conn.cursor() as cur:
        cur.execute("SELECT set_config('app.is_platform', 'on', true)")
        cur.execute(
            "SELECT facility_id, storage_bucket, storage_key, source_format"
            " FROM fms.bim_models WHERE id = %s",
            (str(model_id),),
        )
        row = cur.fetchone()
    conn.rollback()  # 上面只是讀，不需要留著交易
    if row is None:
        logger.warning("模型 %s 掃描到之後已經不存在，跳過", model_id)
        return
    facility_id, bucket, key, source_format = row

    if source_format not in _SUPPORTED_SOURCE_FORMATS:
        with db.tenant_transaction(conn, tenant_id, actor_user_id):
            _mark_failed(
                conn,
                model_id,
                f"不支援的來源格式 {source_format}——目前只有 IFC 解析器",
            )
        return

    try:
        with db.tenant_transaction(conn, tenant_id, actor_user_id):
            _mark_parsing(conn, model_id)
    except Exception:
        logger.exception("標記 PARSING 失敗，跳過這個模型：%s", model_id)
        return

    path = None
    try:
        path = _download_model(s3, bucket, key)
        result = parse(path)
        with db.tenant_transaction(conn, tenant_id, actor_user_id) as tx:
            report = ingest.ingest(tx, tenant_id, UUID(str(facility_id)), model_id, result)
            _mark_parsed(tx, model_id, report)
        logger.info(
            "模型 %s 解析完成：%s", model_id, report.parse_report
        )
    except Exception as exc:  # noqa: BLE001 —— 一個模型失敗不該讓整輪掛掉
        logger.exception("模型 %s 解析失敗", model_id)
        try:
            with db.tenant_transaction(conn, tenant_id, actor_user_id):
                _mark_failed(conn, model_id, f"{exc}\n{traceback.format_exc()}")
        except Exception:
            logger.exception("連 PARSE_FAILED 都寫不進去：%s", model_id)
    finally:
        if path and os.path.exists(path):
            os.remove(path)


def run_once(conn, s3, actor_user_id: UUID) -> int:
    due = db.find_due_models(conn)
    for tenant_id, model_id in due:
        process_one(conn, s3, actor_user_id, tenant_id, model_id)
    return len(due)


def main() -> None:
    load_dotenv()
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    _install_signal_handlers()

    dsn = os.environ["OWNER_DATABASE_URL"]
    actor_user_id = UUID(os.environ["BIM_INGEST_WORKER_USER_ID"])

    conn = db.connect(dsn)
    s3 = _s3_client()

    logger.info("BIM 解析器啟動，輪詢間隔 %ss", POLL_INTERVAL_SECONDS)
    try:
        while True:
            try:
                n = run_once(conn, s3, actor_user_id)
                if n:
                    logger.info("這輪處理了 %d 個模型", n)
                else:
                    logger.debug("這輪沒有待處理的模型")
            except Shutdown:
                raise
            except Exception:
                # 資料庫層失敗不該讓迴圈退出：與 Rust 那些 watchdog 同一個
                # 判斷，半個 worker 在跑比完全沒跑更難察覺。
                logger.exception("這輪掃描失敗，下一輪重試")
            time.sleep(POLL_INTERVAL_SECONDS)
    except Shutdown:
        logger.info("收到關機訊號，結束")
    finally:
        conn.close()


if __name__ == "__main__":
    sys.exit(main() or 0)
