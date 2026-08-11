# Facility Management System — 正式環境 VM 需求文件

給負責開機器的資訊人員。這份文件不假設你熟悉這個專案的程式碼，只需要照著
規格開一台 VM、裝好指定軟體、開對防火牆規則，然後把最後一節要的資訊回傳
即可。

## 1. VM 規格

| 項目 | 建議值 | 備註 |
|---|---|---|
| vCPU | 2 | Phase 1 規模，日後可視流量擴充 |
| 記憶體 | 4 GB | |
| 磁碟 | 40 GB SSD | 資料庫與檔案儲存都在這台機器上，日後成長需擴充 |
| 作業系統 | **Ubuntu 22.04 LTS** | 其他 Linux 亦可，但以下安裝指令以 Ubuntu 為準 |
| 網路 | 一個固定的公網 IP（或會變動但有 DDNS 機制的也可以，但固定 IP 較單純） | |

## 2. 要先裝好的軟體

### Docker Engine + Docker Compose plugin

```bash
curl -fsSL https://get.docker.com | sudo sh
sudo usermod -aG docker $USER
```

安裝完成後登出重新登入（或 `newgrp docker`），確認：

```bash
docker --version
docker compose version
```

### 建立一個部署專用的使用者

不要用 root 執行部署。建立一個名為 `deploy` 的使用者，加入 `docker` 群組：

```bash
sudo useradd -m -s /bin/bash deploy
sudo usermod -aG docker deploy
```

### 把下面這支公開金鑰加進 `deploy` 使用者的 SSH 授權清單

這是我們產生的一組**部署專用**金鑰（只用來從 CI 自動部署，不是任何人的
個人金鑰）。私鑰由我們保管，不會外流；這裡只需要公開金鑰。

```bash
sudo mkdir -p /home/deploy/.ssh
sudo tee -a /home/deploy/.ssh/authorized_keys > /dev/null <<'EOF'
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIITcFbt7393aFAuoYppBMKqhXRgcT5lX5mMKT9/NxTwT fms-deploy@github-actions
EOF
sudo chown -R deploy:deploy /home/deploy/.ssh
sudo chmod 700 /home/deploy/.ssh
sudo chmod 600 /home/deploy/.ssh/authorized_keys
```

## 3. 防火牆／安全群組規則

| 埠 | 方向 | 用途 | 來源限制 |
|---|---|---|---|
| 22 (SSH) | inbound | 管理與自動部署 | **限公司辦公室 IP 或 VPN 範圍**，不要對全網開放 |
| 80 (HTTP) | inbound | API 流量 | 見下方說明——視有沒有既有的反向代理而定 |

**不要**額外開放以下埠對外（這些服務只在伺服器內部使用，不對外服務）：
`5432`（資料庫）、`9000`／`9001`（物件儲存）、`1883`（MQTT）。

**80 埠的來源限制，取決於公司是否已經有機房層級的反向代理**：
- **如果有**（例如另一台主機統一處理 TLS／憑證、再轉發到各台內部 VM——
  跟這次部署 `api.fms.bizlution.ai` 用的架構一樣）：這台 VM 的 80 只需要
  對那台反向代理的位址開放（通常是同一個內部網段），**不需要對整個
  網際網路開放**，也不需要在這台 VM 上處理任何憑證。
- **如果沒有**（這台 VM 直接對外）：需要額外在這台 VM 上加一層反向代理
  （例如 Caddy）處理 TLS，且 80／443 都要對全網開放。目前的
  `docker-compose.prod.yml` 是照第一種情境（有既有反向代理）設定的；
  換成第二種情境需要另外調整，請先跟負責部署的人確認再改防火牆規則。

## 4. DNS

需要一筆網域指到**實際處理對外流量的那個位址**。建議使用
`api.fms.bizlution.ai`（呼應目前已經在用的 `fms.bizlution.ai`〔正式產品
前端〕與 `fmsapi.bizlution.ai`〔API 參考文件站〕），但最終網域名稱由
你們決定，只要告訴我最終選定的名稱即可。

**「實際處理對外流量的位址」是哪一個，取決於公司網路架構**（見第 3 節
的說明）：
- 如果公司已經有機房層級的反向代理（統一處理 TLS、再轉發到各台內部
  VM）：DNS 應該指到**那台反向代理的公網 IP**，不是這台 VM 自己的 IP
  （這台 VM 可能只有內部網段的私有 IP，例如 `192.168.x.x`，從外面根本
  連不到）。這次 `api.fms.bizlution.ai` 用的就是這種架構。
- 如果這台 VM 直接對外：DNS 才指到這台 VM 自己的公網 IP。

DNS 記錄範例：

| 類型 | 名稱 | 目標值 |
|---|---|---|
| A | `api.fms`（或你決定的名稱） | `<實際對外的公網 IP，見上方說明是哪一個>` |

## 5. 完成後請回傳給我的資訊

- [ ] VM 的公網 IP
- [ ] SSH port（若不是預設的 22，請告知實際埠號）
- [ ] 確認已建立 `deploy` 使用者，且已把第 2 節的公開金鑰加進去
- [ ] 確認已裝好 Docker（`docker --version` 的輸出）
- [ ] 最終決定的網域名稱，以及是否已完成 DNS 指向

拿到這些資訊後，我會先手動連上去驗證一次完整的部署流程，確認沒問題後
再串接自動部署（之後每次程式碼更新推上去，就會自動部署到這台機器，
不需要再手動操作）。
