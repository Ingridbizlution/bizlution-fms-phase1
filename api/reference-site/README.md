# FMS API 參考文件（獨立靜態站）

給客戶前端團隊看的 API 參考站——左側依 `api/openapi.yaml` 的 12 個 `tags`
分類列出所有端點，右側是說明、參數表、範例 curl／範例回應，並支援
**互動測試**：填好左側「測試設定」的 Base URL（選填 Bearer Token／
X-Tenant-ID）後，每支 operation 頁面都能直接編輯參數與 Request Body、
按下「送出請求」，真的對那個 Base URL 發 `fetch()`，顯示真實的狀態碼／
headers／回應內容。

UI 依 [Tabler](https://github.com/bizluton/tabler) 的視覺語言設計（vendored
CSS，見 `vendor/tabler/README.md`）。

## 這個網站跟後端完全解耦

不需要跑 `docker compose` 或 `cargo run` 才能看——純 HTML／CSS／JS，開一個
本機靜態伺服器或發布到 GitHub Pages 就能看。資料來源是
`api/openapi.yaml` 與 `docs/FRONTEND-GETTING-STARTED.md`，兩者都是這個 repo
既有、已經測試強制同步的內容，這個網站不重寫、不新增任何內容，只是換一種
呈現方式。

## 本機預覽

```bash
# 1. 產生 data/（openapi.yaml → JSON，Markdown → HTML）
python3 -m pip install pyyaml markdown
python3 api/reference-site/build.py

# 2. 開一個本機伺服器（不要直接用 file:// 開 index.html——
#    瀏覽器會擋 fetch 讀本機檔案，頁面會一直卡在載入失敗）
cd api/reference-site
python3 -m http.server 8080
# 開 http://localhost:8080
```

修改了 `api/openapi.yaml` 或 `docs/FRONTEND-GETTING-STARTED.md` 之後要重跑
`build.py` 才會反映到網站上（`data/` 是產生物，不是原始資料）。

## 檔案說明

| 檔案 | 用途 |
|---|---|
| `index.html` | 版面骨架：navbar + 左側 sidebar 容器 + 右側 main 容器 |
| `app.js` | 渲染邏輯：抓 `data/*`、依 tags 畫 sidebar、hash routing、參數／schema 表格、curl 與範例回應產生器 |
| `style.css` | 疊在 `vendor/tabler/tabler.min.css` 上的少量覆寫 |
| `build.py` | 把 `openapi.yaml`／`FRONTEND-GETTING-STARTED.md` 轉成 `data/` 底下的 JSON／HTML |
| `data/` | `build.py` 的產生物，不手改 |
| `vendor/tabler/` | vendored Tabler CSS（見該資料夾的 `README.md`） |

## 怎麼發布

實際發布的正式網址是 **Cloudflare Pages**（`https://fmsapi.bizlution.ai`，
專案名稱 `bizlution-fmsapi`），不是 GitHub Pages——這個網站原本設計成
可以發到任一種靜態站平台，最後選 Cloudflare Pages 是因為公司網域本來就
在 Cloudflare 上管理。

目前是手動發布，還沒接 CI/CD 自動化：

```bash
python3 api/reference-site/build.py
npx wrangler pages deploy api/reference-site --project-name bizlution-fmsapi --branch=main
```

（`--branch=main` 不能省——沒有它 wrangler 會把目前的 git 分支名稱當成
preview 分支部署，不會更新正式網域，這是踩過的坑。）

`.github/workflows/publish-reference-site.yml`（發到 GitHub Pages 的版本）
已經拿掉：那是這個網站第一版的設計，實際採用 Cloudflare Pages 之後
從沒真的啟用過（repo 的 Pages 功能從未開通），每次 push 都會顯示失敗，
是誤導性的雜訊，不是還在用的東西。日後若要把上面兩行指令自動化，
需要一個範圍限縮在這個 Pages 專案的 Cloudflare API token（不是個人
`wrangler login` 的 OAuth token），存成 repo secret。

## 為什麼是這樣做（設計取捨）

- **零 Node／npm 建置鏈**：這個 repo 只有 Rust 與 Python，`build.py` 用
  Python 一次性把 YAML／Markdown 轉成瀏覽器能直接吃的 JSON／HTML，`app.js`
  因此不需要 vendor 任何 YAML 或 Markdown 解析器，純 DOM 操作。
- **零外部請求**：Tabler 的 CSS 是 vendored 進版控的成品（見
  `vendor/tabler/README.md`），不是從 CDN 載——這個網站要能整包資料夾
  交給客戶、或在完全離網的環境打開。`build.py` 最後會自動掃一次
  `index.html`／`app.js`／`style.css`／vendored CSS，抓到任何指向外部主機的
  `http(s)://` 參照就中止產生。
- **互動測試（Try it out）**：Base URL／Token／X-Tenant-ID 只存在瀏覽器的
  `localStorage`，只會用在使用者主動按下「送出請求」時，只送到使用者
  自己填的 Base URL——這個網站本身仍然是零外部請求（`build.py` 的檢查
  只擋寫死在原始碼裡的外部參照，不影響使用者自己輸入的動態網址）。
  跟 `/docs`（Swagger UI）不重複：那邊是後端起來後的完整互動文件，這邊
  的優勢是不需要跑後端就能瀏覽整份文件，Try it out 是額外加值，不是
  取代 `/docs`。
- **CORS 是瀏覽器強制的，這裡繞不過**：要對某個環境（本機／staging／
  正式）實際送出測試請求，那個環境的 `CORS_ALLOWED_ORIGINS` 必須包含
  這個網站的來源（正式站是 `https://fmsapi.bizlution.ai`，本機預覽是
  `http://localhost:<port>`）。沒有的話會看到「請求失敗：Failed to
  fetch」，頁面會提示可能原因；這不是這個網站的 bug，是瀏覽器本身的
  安全機制。
