# 本機啟動指南（macOS）

專案位置：`~/Desktop/bizlution-fms-phase1-main`

## 一、先確認 / 安裝這些工具（在 macOS Terminal 執行）

```bash
docker --version        # 需要 Docker Desktop for Mac
cargo --version         # 需要 Rust
node --version          # 需要 Node.js 20+
make --version          # macOS 內建
```

任一個沒有 → 安裝：

```bash
# Docker Desktop
brew install --cask docker
# 打開 Docker Desktop 讓鯨魚變綠

# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Node.js
brew install node
```

## 二、啟動基礎服務（Terminal 視窗 A）

```bash
cd ~/Desktop/bizlution-fms-phase1-main/docker
make up
docker compose run --rm -e MIGRATE_MODE=demo migrate
```

驗證：Postgres localhost:5433、MinIO http://localhost:9001、Mailpit http://localhost:8025

## 三、啟動後端 API（Terminal 視窗 B）

```bash
cd ~/Desktop/bizlution-fms-phase1-main/app
export APP_DATABASE_URL="postgres://fms_app:change_me_app@localhost:5433/fms"
export JWT_SECRET="dev_secret_at_least_32_characters_long_xxxx"
export S3_ENDPOINT="http://localhost:9000"
export S3_ACCESS_KEY="fmsminio"
export S3_SECRET_KEY="change_me_minio"
export CORS_ALLOWED_ORIGINS="http://localhost:5173"
cargo run -p fms-server
# 首次編譯 10-30 分鐘；起來後 http://localhost:8080
```

## 四、啟動前端（Terminal 視窗 C）

```bash
cd ~/Desktop/bizlution-fms-phase1-main/frontend
npm install
npm run dev
# http://localhost:5173
```

## 停止

```bash
cd ~/Desktop/bizlution-fms-phase1-main/docker
make down       # 保留資料
```
