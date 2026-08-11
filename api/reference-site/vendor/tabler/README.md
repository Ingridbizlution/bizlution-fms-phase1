# Tabler（vendored）

`api/reference-site/` 的視覺樣式。**刻意放進版控**，不是從 CDN 載。

## 為什麼 vendored 而不是 CDN

理由與 `api/vendor/swagger-ui/README.md` 相同：這個網站要能整包資料夾交給
客戶、或發布到內網／離網環境仍然打得開。從 CDN 載入的頁面在沒有外網的環境
會是一片空白，而症狀（頁面開得起來、樣式不出現）看起來像網站壞了。

只 vendor `tabler.min.css` 一個檔案——Tabler 的圖示是內嵌 SVG（`.icon`
class 直接套在手寫的 `<svg>` 元素上，不是 icon webfont），這個網站用到的
圖示數量少，直接把對應的 SVG path 寫進 `app.js`／`index.html`，不需要另外
vendor `@tabler/icons` 整包字型或 SVG sprite。也沒有 vendor
`tabler-vendors.min.css`（那是給 flatpickr／tom-select 等第三方 JS 元件的
外觀，這個網站沒有用到任何 Tabler 的互動元件，只借用它的排版／色票／
sidebar／card／badge 樣式）。

## 來源

| 項目 | 值 |
|---|---|
| 套件 | [`@tabler/core`](https://www.npmjs.com/package/@tabler/core) |
| 版本 | **1.4.0** |
| tarball | `https://registry.npmjs.org/@tabler/core/-/core-1.4.0.tgz` |
| tarball SHA-256 | `12ad1d2a4a8bd254fcc2cc9481071d3408006a392ffb0dec5f84da1fa4622b09` |
| 授權 | MIT（見 `LICENSE`，取自 `tabler/tabler` 對應版本標籤） |

檔案 SHA-256（**未經任何修改**，與 tarball 內 `dist/css/tabler.min.css`
逐位元組相同）：

```
7ef750bd10546a695d0b12767ad8048bd8f3ec5de7daefb1067f9d0daa3d1c9a  tabler.min.css
ef5d45031adce79eeaf17f04a966871137589f9b60d18e4520ade84b291dcd05  LICENSE
```

**檔案刻意不做任何在地修改。** 所有在地調整放在 `api/reference-site/style.css`
（疊在 `tabler.min.css` 之上的少量覆寫），不動這個檔案本身——理由與
swagger-ui 那份 README 相同：修改過的 vendored 依賴無法用上游雜湊驗證。

已確認 `tabler.min.css` 內沒有任何 `@import`，且全部 `url(...)` 都是
`data:image/svg+xml` inline SVG（Tabler 內建的 checkbox／radio／chevron 等
裝飾圖示），沒有一個指向外部主機——離網環境可放心使用。

## 為什麼是 Tabler

使用者明確要求這個網站的 UI 依 [bizluton/tabler](https://github.com/bizluton/tabler)
（`tabler/tabler` 的無改動 fork）設計，不是評估後選出來的——這個網站不是
`/docs` 的替代品（那個仍是 Swagger UI，見 `api/vendor/swagger-ui/README.md`），
是給客戶前端團隊看的、風格上要跟他們熟悉的 admin dashboard 排版一致的參考站。

## 升級

```bash
V=1.x.y
curl -sSL -o /tmp/tabler.tgz "https://registry.npmjs.org/@tabler/core/-/core-$V.tgz"
shasum -a 256 /tmp/tabler.tgz              # 記進上表
tar xzf /tmp/tabler.tgz -C /tmp
cp /tmp/package/dist/css/tabler.min.css api/reference-site/vendor/tabler/
curl -sSL -o api/reference-site/vendor/tabler/LICENSE \
  "https://raw.githubusercontent.com/tabler/tabler/@tabler/core%40$V/LICENSE"
shasum -a 256 api/reference-site/vendor/tabler/*   # 更新上表
```

升級後檢查 `tabler.min.css` 有沒有新增 `@import` 或非 `data:` 的 `url(...)`
（`grep -o 'url([^)]*)' api/reference-site/vendor/tabler/tabler.min.css | grep -vi '^url("\?data:'`
應該沒有輸出）——那正是這份 vendored 副本要防的東西。
