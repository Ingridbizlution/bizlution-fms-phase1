# Swagger UI（vendored）

`GET /docs` 的前端。**刻意放進版控**，不是從 CDN 載。

## 為什麼 vendored 而不是 CDN

這是地端產品，客戶環境**可能沒有外網**。從 CDN 載入的瀏覽器頁面在那些
部署裡會是一片空白，而症狀（頁面開得起來、內容不出現）看起來像伺服器壞了。

代價是 1.7 MB 進版控、也進 binary（`include_str!`）。這個交易是划算的：
換來的是「頁面在任何環境都打得開」。

## 來源

| 項目 | 值 |
|---|---|
| 套件 | [`swagger-ui-dist`](https://www.npmjs.com/package/swagger-ui-dist) |
| 版本 | **5.32.11** |
| tarball | `https://registry.npmjs.org/swagger-ui-dist/-/swagger-ui-dist-5.32.11.tgz` |
| tarball SHA-256 | `966b7c7ea3bc98af2f5f125dac3a971973df20ed1f9c40707d846200d8b462a6` |
| 授權 | Apache-2.0（見 `LICENSE`、`NOTICE`、`swagger-ui-bundle.js.LICENSE.txt`） |

檔案 SHA-256（**未經任何修改**，與 tarball 內容逐位元組相同）：

```
fcb81e2c79e7e3b76ddb9bd7fc791552045040fde05c19d3f98f9213e7f7724d  swagger-ui-bundle.js
ca238f7d7c2cf4480c1e77a9c3b9da915ab216e96ffd354e69076560c650c6de  swagger-ui.css
cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30  LICENSE
0d20d1adef18aee3f40dd258172155521ce702ac445cb5f7b7d60ed32dad2fb2  NOTICE
f22a5ade2354a48bddcb9746c2d17ed94a267fa5dbf72f631f42a72eba0d081c  swagger-ui-bundle.js.LICENSE.txt
```

**檔案刻意不做任何在地修改。** 修改過的 vendored 依賴無法用上游雜湊驗證，
而「這份檔案跟上游到底差在哪」是升級時最花時間的問題。所有在地調整都放在
`fms-server/src/docs.rs` 的 HTML 設定裡。

因此 `swagger-ui.css` 尾端保留了 `sourceMappingURL=swagger-ui.css.map`
的註解，而我們沒有收錄那份 273 KB 的 map。影響只有「開著 devtools 時多一個
**同源** 404」—— 不是外部請求，不影響離網可用性。

## 為什麼是 Swagger UI（而不是 Scalar／Redoc）

評估過三個單檔發行版：

* **Scalar**（2.6 MB）—— 試打預設走 `proxy.scalar.com`。那是外部相依，
  而且會把使用者的 bearer token 送給第三方。可以用 `proxyUrl: ''` 關掉，
  但預設值錯得太危險，bundle 裡也仍留有 `cdn.jsdelivr.net` 的參照。
* **Redoc**（890 KB）—— 最小，但**沒有試打功能**（那是付費版）。
  而「能在瀏覽器裡試打」正是做這個頁面的主要理由之一。
* **Swagger UI**（1.7 MB）—— 有試打、且試打是瀏覽器直接送出，不經任何代理。
  `/docs` 與 API 同源，所以連 CORS 都不會遇到。CSS 內所有圖片都是 `data:`
  URI，沒有任何 `url(http…)` 或 `@import`。

唯一會發出外部請求的是右下角那張
`https://validator.swagger.io/validator?url=…` 的徽章圖 ——
`docs.rs` 以 `validatorUrl: null` 關掉它，並由測試守住。

## 升級

```bash
V=5.x.y
curl -sSL -o /tmp/sui.tgz "https://registry.npmjs.org/swagger-ui-dist/-/swagger-ui-dist-$V.tgz"
shasum -a 256 /tmp/sui.tgz          # 記進上表
tar xzf /tmp/sui.tgz -C /tmp
cp /tmp/package/{swagger-ui-bundle.js,swagger-ui.css,LICENSE,NOTICE,swagger-ui-bundle.js.LICENSE.txt} \
   api/vendor/swagger-ui/
shasum -a 256 api/vendor/swagger-ui/*   # 更新上表
```

升級後務必跑 `cargo test -p fms-server --test openapi_docs_slice`：
`the_docs_page_makes_no_external_requests` 會抓到新版本引進的 CDN 相依，
而那正是這份 vendored 副本要防的東西。
