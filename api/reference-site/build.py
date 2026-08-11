#!/usr/bin/env python3
"""將 api/openapi.yaml 與 docs/FRONTEND-GETTING-STARTED.md 轉成這個網站要用的靜態資料。

瀏覽器端只吃現成的 JSON／HTML，不需要在 JS 裡放 YAML 或 Markdown 解析器——
所有解析都在這裡（CI 或本機）做一次。輸出：
  api/reference-site/data/openapi.json        openapi.yaml 轉 JSON，並補上每個
                                               description 對應的 description_html
  api/reference-site/data/getting-started.html FRONTEND-GETTING-STARTED.md 轉好的 HTML

本機預覽：從 repo 根目錄跑 `python3 api/reference-site/build.py`，
再 `cd api/reference-site && python3 -m http.server` 開瀏覽器看。
"""
import json
import re
import sys
from datetime import date
from pathlib import Path

import markdown
import yaml

ROOT = Path(__file__).resolve().parents[2]
SITE_DIR = Path(__file__).resolve().parent
OPENAPI_YAML = ROOT / "api" / "openapi.yaml"
GETTING_STARTED_MD = ROOT / "docs" / "FRONTEND-GETTING-STARTED.md"
DATA_DIR = SITE_DIR / "data"

HTTP_METHODS = {"get", "post", "put", "patch", "delete", "head", "options", "trace"}


def add_description_html(node):
    """遞迴走過整份 spec，任何字典若帶有 `description`，補上渲染好的
    `description_html`。這樣一來不管是 operation、parameter 還是 schema
    的說明文字，右側面板都能直接插入現成的 HTML——app.js 不需要知道
    Markdown 長什麼樣子。
    """
    if isinstance(node, dict):
        desc = node.get("description")
        if isinstance(desc, str) and desc.strip():
            node["description_html"] = markdown.markdown(desc, extensions=["extra"])
        for value in node.values():
            add_description_html(value)
    elif isinstance(node, list):
        for item in node:
            add_description_html(item)


def count_operations_from_raw_text(raw_text):
    """獨立於 yaml.safe_load 之外，用縮排比對數一次 operation 數。
    防的是 YAML 解析（重複鍵、anchor 展開錯誤等）默默漏資料——
    那種錯誤解析完全不會拋例外，只會讓網站少幾支端點卻沒人發現。
    """
    method_pattern = re.compile(r"^ {4}(" + "|".join(HTTP_METHODS) + r"):\s*$")
    in_paths = False
    count = 0
    for line in raw_text.splitlines():
        if re.match(r"^paths:\s*$", line):
            in_paths = True
            continue
        if in_paths and re.match(r"^[A-Za-z]", line):
            break  # paths: 區塊結束，遇到下一個頂層鍵
        if in_paths and method_pattern.match(line):
            count += 1
    return count


def count_operations_from_spec(spec):
    count = 0
    for methods in spec.get("paths", {}).values():
        if not isinstance(methods, dict):
            continue
        count += sum(1 for key in methods if key in HTTP_METHODS)
    return count


def find_external_references(*paths):
    """這個網站要能在完全離網的環境打開。抓的是會被瀏覽器實際發出請求的
    http(s) 參照（`src=`／`href=`／CSS 的 `url(...)`）——`data:` URI 與純文字
    註解裡提到的網址（例如授權條款連結）不算，那些不會觸發任何請求。
    """
    offenders = []
    for path in paths:
        text = path.read_text(encoding="utf-8")
        for m in re.finditer(r'(?:src|href)\s*=\s*["\']https?://[^"\']+', text):
            offenders.append(f"{path.name}: {m.group(0)}")
        for m in re.finditer(r'url\(\s*["\']?(https?://[^)"\']+)', text):
            offenders.append(f"{path.name}: {m.group(0)}")
    return offenders


def main():
    raw_text = OPENAPI_YAML.read_text(encoding="utf-8")
    spec = yaml.safe_load(raw_text)

    expected = count_operations_from_raw_text(raw_text)
    actual = count_operations_from_spec(spec)
    if expected != actual:
        sys.exit(
            f"build.py: operation 數不一致（獨立計數 {expected} vs 解析後 {actual}）"
            "——yaml.safe_load 疑似漏掉了什麼，中止產生，不要發布不完整的資料。"
        )

    add_description_html(spec)

    # 版面標題要顯示「這份參考文件是哪個版本、哪天發布的」——版本沿用
    # openapi.yaml 自己宣告的 info.version（唯一權威，不在這裡另編一個）;
    # 日期是「這次 build.py 產生資料的日期」，也就是實際發布的日期，
    # 每次重新建置／發布都會自動更新，不需要手動維護。
    spec["info"]["x-generated-at"] = date.today().isoformat()

    DATA_DIR.mkdir(parents=True, exist_ok=True)
    (DATA_DIR / "openapi.json").write_text(
        json.dumps(spec, ensure_ascii=False, indent=2), encoding="utf-8"
    )

    getting_started_html = markdown.markdown(
        GETTING_STARTED_MD.read_text(encoding="utf-8"), extensions=["extra"]
    )
    (DATA_DIR / "getting-started.html").write_text(getting_started_html, encoding="utf-8")

    offenders = find_external_references(
        SITE_DIR / "index.html",
        SITE_DIR / "app.js",
        SITE_DIR / "style.css",
        SITE_DIR / "vendor" / "tabler" / "tabler.min.css",
    )
    if offenders:
        sys.exit(
            "build.py: 發現指向外部主機的參照，這個網站必須離線可用：\n"
            + "\n".join(offenders)
        )

    print(f"build.py: OK — {actual} operations，data/ 已產生，零外部請求")


if __name__ == "__main__":
    main()
