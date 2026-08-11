//! 設定載入。JWT 簽章密鑰一律只從環境變數取得，不接受從檔案讀入。

use std::time::Duration;

/// 資料庫連線設定。
#[derive(Debug, Clone)]
pub struct DatabaseSettings {
    /// 應用連線字串，必須是 `fms_app`（非 owner、非超級使用者），
    /// 否則 RLS 形同不存在。
    pub url: String,
    pub max_connections: u32,
}

/// JWT 設定。
#[derive(Debug, Clone)]
pub struct JwtSettings {
    /// 只從 `JWT_SECRET` 讀取。
    pub secret: String,
    pub access_ttl: Duration,
    pub refresh_ttl: Duration,
}

/// 登入失敗節流。
///
/// 計數的是**失敗**，成功即歸零，因此這不是鎖定機制：真實使用者打錯幾次
/// 之後打對就立刻恢復。刻意這樣設計 —— 「N 次失敗鎖住帳號 M 分鐘」
/// 會把暴力破解的防護變成針對已知帳號的阻斷服務。
#[derive(Debug, Clone)]
pub struct LoginThrottleSettings {
    /// 窗內允許的失敗次數。累積到這個數之後，同一個帳號的下一次嘗試回 429。
    pub max_failures: u32,
    pub window: Duration,
}

/// 應用設定。
#[derive(Debug, Clone)]
pub struct Settings {
    pub bind_addr: String,
    pub database: DatabaseSettings,
    pub jwt: JwtSettings,
    pub login_throttle: LoginThrottleSettings,
    /// 往「呼叫端填的網址」發請求時的閘門。見 `safe_http` 模組說明。
    pub outbound: crate::safe_http::OutboundSettings,
    /// 這個部署對外的基底網址，例如 `https://fms.example.com`。
    ///
    /// SSO 的 `redirect_uri` 由它組出來，而**那個值絕對不能來自請求**：
    /// 接受呼叫端給的 redirect_uri 就是一個開放轉址器 —— 攻擊者把它指向自己的
    /// 網站，IdP 就會把授權碼送到那裡去。
    ///
    /// 未設定時 `/auth/sso/*` 回 501 並說明要設什麼，而不是猜一個。
    pub public_base_url: Option<String>,
    /// 允許跨來源請求的前端來源，例如 `https://fms.example.com`。
    ///
    /// **預設空清單 = 完全不加 CORS 層。** 這對純伺服器對伺服器的部署是正確的
    /// 預設，但它同時意味著**瀏覽器一個請求都發不出去** —— 前端的
    /// dev server 對 API 的每一個跨來源請求都會在 preflight 就失敗。
    /// 前端開發環境因此**必須**設這個變數。
    ///
    /// 不支援通配 `*`：這個 API 用 `Authorization` 標頭，而
    /// 「通配來源 + 帶認證」在 CORS 規範裡是**無效組合**（瀏覽器會拒絕）。
    /// 就算規範允許，那也等於任何網站都能拿使用者的 token 打這個 API。
    pub cors_allowed_origins: Vec<String>,
}

impl Settings {
    /// 從環境變數載入。缺少必要變數即失敗，不提供不安全的預設值。
    pub fn from_env() -> Result<Self, String> {
        let database_url = std::env::var("APP_DATABASE_URL")
            .map_err(|_| "APP_DATABASE_URL 未設定（須為 fms_app 的連線字串）".to_string())?;
        let secret = std::env::var("JWT_SECRET").map_err(|_| "JWT_SECRET 未設定".to_string())?;
        if secret.len() < 32 {
            return Err("JWT_SECRET 過短（至少 32 字元）".to_string());
        }

        Ok(Self {
            bind_addr: std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            database: DatabaseSettings {
                url: database_url,
                max_connections: std::env::var("DB_MAX_CONNECTIONS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10),
            },
            jwt: JwtSettings {
                secret,
                access_ttl: Duration::from_secs(15 * 60),
                refresh_ttl: Duration::from_secs(7 * 24 * 60 * 60),
            },
            // 預設 10 次／5 分鐘：一個帳號每天最多被試約 2880 次，
            // 對任何有長度要求的密碼都遠不足以窮舉；同時真實使用者
            // 在五分鐘內打錯十次的情況極少。兩個值都可覆寫，
            // 因為合理的門檻取決於客戶的密碼政策。
            login_throttle: LoginThrottleSettings {
                max_failures: env_parse("LOGIN_MAX_FAILURES", 10),
                window: Duration::from_secs(env_parse("LOGIN_FAILURE_WINDOW_SECS", 300)),
            },
            // 沒有預設值。猜一個（例如從 bind_addr 組）會產生一個 IdP 那邊
            // 沒有註冊的 redirect_uri，而症狀是使用者跳到 IdP 之後被拒絕，
            // 錯誤訊息在 IdP 那一側。
            public_base_url: std::env::var("PUBLIC_BASE_URL")
                .ok()
                .map(|s| s.trim_end_matches('/').to_string())
                .filter(|s| !s.is_empty()),
            // 預設空。空清單的意思是「不加 CORS 層」，而不是「允許全部」——
            // 見欄位說明。設了值會記一筆 info（不是 warn：允許前端來源是
            // 正常設定，不像 OUTBOUND_ALLOW_PRIVATE_TARGETS 那樣是例外）。
            cors_allowed_origins: parse_origins("CORS_ALLOWED_ORIGINS"),
            outbound: crate::safe_http::OutboundSettings {
                // **預設空**。這份白名單是 SSRF 防護的唯一出口，
                // 而它的正當用途只有一個：整合測試指向本機的模擬伺服器。
                // 設了值就會記一筆 warn —— 一個放行內網位址的部署應該有人知道。
                private_target_allowlist: parse_allowlist("OUTBOUND_ALLOW_PRIVATE_TARGETS"),
                ..Default::default()
            },
        })
    }
}

/// 讀取可選的數值型環境變數。格式錯誤時退回預設值並警告 ——
/// 一個打錯的節流參數不該讓服務起不來，但也不該無聲無息。
fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    match std::env::var(key) {
        Err(_) => default,
        Ok(raw) => raw.trim().parse().unwrap_or_else(|_| {
            tracing::warn!(key, value = %raw, "無法解析為數值，改用預設值");
            default
        }),
    }
}

/// 讀取 `host:port` 白名單（逗號分隔）。
///
/// 設了值就 warn。這不是雜訊：這份清單讓伺服器可以連往內部位址，
/// 而它最常見的誤用是「除錯時打開、忘了關掉」——
/// 那時唯一的線索就是啟動日誌裡有沒有這一行。
fn parse_allowlist(var: &str) -> Vec<String> {
    let raw = match std::env::var(var) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let list: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if !list.is_empty() {
        tracing::warn!(
            targets = ?list,
            "{var} 已設定 —— 這些主機即使解析到私有位址也會被允許連線。\
             生產環境不該有這個設定。"
        );
    }
    list
}

/// 讀取 CORS 的來源允許清單（逗號分隔的 origin，例如
/// `http://localhost:3000,https://fms.example.com`）。
///
/// **`*` 會被拒絕並忽略。** 這個 API 的每一個請求都帶 `Authorization`，
/// 而 CORS 規範不允許「通配來源 + 憑證」—— 瀏覽器會直接拒絕那個回應，
/// 於是設 `*` 的人得到的是「明明設了卻還是被擋」，一個很難查的症狀。
/// 因此這裡在啟動時就把它擋掉並說出原因。
///
/// 尾斜線會被去掉：`Origin` 標頭永遠不帶路徑或尾斜線，
/// 而 `https://x.com/` 與 `https://x.com` 對字串比對是兩個值 ——
/// 那個差別會讓一個看起來正確的設定完全不生效。
fn parse_origins(var: &str) -> Vec<String> {
    let raw = match std::env::var(var) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut list = Vec::new();
    for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if item == "*" {
            tracing::error!(
                "{var} 含通配 `*` —— 已忽略。這個 API 使用 Authorization 標頭，\
                 而 CORS 不允許「通配來源 + 憑證」；瀏覽器會拒絕該回應。\
                 請逐一列出前端來源。"
            );
            continue;
        }
        list.push(item.trim_end_matches('/').to_string());
    }
    if !list.is_empty() {
        tracing::info!(origins = ?list, "{var} 已設定，瀏覽器可從這些來源呼叫 API");
    }
    list
}
