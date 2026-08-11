//! 整合測試的共用腳手架。
//!
//! # 隔離模型：每個測試一個資料庫
//!
//! 每次 `TestContext::setup()` 都以 `CREATE DATABASE ... TEMPLATE fms_template`
//! 複製一份全新的、已完成 migration 與種子的資料庫，teardown 時丟棄。
//!
//! 這取代了先前「共用開發資料庫 + 以樣式比對清理」的做法。那個做法的三個
//! 代價都真的踩到過：
//!
//!   1. **每個測試檔只能有一個測試函式** —— 同檔案的測試平行執行，
//!      而清理是全域的，第二個測試的 setup 會刪掉第一個測試的資料。
//!   2. **每加一種資料就要補一條還原**（庫存數量、讀表值、核准旗標、
//!      密碼、佔位…）。漏一條的症狀是「第二次執行才失敗」。
//!   3. 測試會改到開發者正在用的資料庫。
//!
//! 現在這三件事都消失了：測試之間**不可能**互相影響，因為它們看的是不同的
//! 資料庫。清理只剩一行 DROP DATABASE，而且就算漏掉，
//! 下一次建 template 時會一併清乾淨。
//!
//! 前置條件：`make test-template`（CI 已納入）。
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use std::sync::Arc;
use tower::ServiceExt;

pub const TENANT_CODE: &str = "DEMO_GROUP";
pub const TENANT_ID: &str = "aaaaaaaa-0000-4000-8000-000000000001";
pub const ADMIN_USER_ID: &str = "ffffffff-0000-4000-8000-000000000001";
pub const USERNAME: &str = "admin.chen";
/// REQUESTER 角色（只有 work_order:create 與 read_own），用來驗證權限確實有擋。
pub const USERNAME_REQUESTER: &str = "user.huang";
/// FACILITY_ADMIN，範圍只在總部場域。用來驗證場域級 RLS 真的收斂了列。
pub const USERNAME_FACILITY_ADMIN: &str = "fm.lin";
/// TECHNICIAN，範圍在**台北總部大樓** —— 示範資料的多數設備都在那裡。
///
/// 他是 009 後來補上的：在他之前總部沒有任何場域級的執行者，
/// 於是 010 的 T3 只能改用租戶管理員，而「場域級執行者能不能執行工單」
/// 這條最常見的路徑沒有被任何東西走過。
///
/// 與 `tech.wang` 的差別是**場域**：wang 在信義影城。要驗場域收斂用 wang，
/// 要驗「在正確場域的人做得了事」用 liu。
pub const USERNAME_TECHNICIAN_HQ: &str = "tech.liu";
pub const TEST_PASSWORD: &str = "slice-test-password";

/// 需要設定測試密碼的示範使用者。009 的使用者都是目錄來源，`password_hash` 為 NULL。
const TEST_USERS: &[&str] = &[
    USERNAME,
    USERNAME_REQUESTER,
    USERNAME_FACILITY_ADMIN,
    USERNAME_TECHNICIAN_HQ,
];

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

/// 把連線字串裡的資料庫名換掉。測試的每個角色都要連到**同一個**測試資料庫。
fn with_db(base: &str, db: &str) -> String {
    match base.rfind('/') {
        Some(i) => format!("{}/{}", &base[..i], db),
        None => base.to_string(),
    }
}

fn owner_base() -> String {
    env_or(
        "OWNER_DATABASE_URL",
        "postgres://fms_owner:change_me_owner@localhost:5433/fms",
    )
}

fn app_base() -> String {
    env_or(
        "APP_DATABASE_URL",
        "postgres://fms_app:change_me_app@localhost:5433/fms",
    )
}

fn template_db() -> String {
    env_or("TEST_TEMPLATE_DB", "fms_template")
}

/// 測試用的物件儲存。指向 compose 的 MinIO（`make up` 已建好 bucket 並設為私有）。
///
/// 不 mock：預簽網址是 SigV4 簽章，mock 掉就等於不驗證它 ——
/// 而「網址看起來對但下載回 403」正是這個功能最典型的失敗方式。
///
/// **物件儲存不隨資料庫隔離**：MinIO 沒有便宜的 template 機制，而附件測試
/// 用的鍵含租戶與隨機 uuid，實務上不會相撞。殘留物件只佔開發環境空間。
/// # 已知的本機抖動（macOS，只在滿載跑完整套件時）
///
/// 症狀：某個測試 binary 冒出
///
/// ```text
/// panicked at aws-smithy-http-client/src/hyper_legacy.rs:
///   could not load platform certs: failed to load user trust settings
///   kind: Os(Error { code: -36, message: "I/O error." })
/// ```
///
/// 接著**同一個 binary 裡其餘用到它的測試**全部以
/// `LazyLock instance has previously been poisoned` 失敗 ——
/// 於是 7 格裡只有 1 格顯示真正的成因。看到整組莫名其妙一起掛時，
/// 先找那一格不一樣的。
///
/// 成因：`aws-sdk-s3` 的 `rustls` 特性會載入**平台憑證**，在 macOS 上就是
/// 讀鑰匙圈的使用者信任設定。`cargo test --workspace` 會同時跑十幾個
/// binary，而 macOS 的 Security framework 在那個並發下會回 `-36`（`ioErr`）。
///
/// **刻意不改成 webpki roots。** 那會讓 S3 client 忽略系統信任存放區，
/// 而這是地端產品 —— 客戶的 MinIO 很可能用私有 CA。為了修一個本機抖動
/// 而讓那些部署壞掉是不划算的交易。
///
/// CI 不受影響（Linux 讀 `/etc/ssl/certs`，沒有鑰匙圈）。
///
/// 我先前猜過是 rustls crypto provider 沒安裝並據此改了 `Storage::new`
/// —— **那個猜測是錯的**（改完之後同一個形狀又出現了一次）。
/// 那個改動本身仍然正確（不該依賴只有 `init_telemetry` 會裝的 process 級狀態），
/// 只是與這個抖動無關。
///
/// # 修法：讓 `rustls-native-certs` 讀檔而不是讀鑰匙圈
///
/// 讀 `rustls-native-certs` 0.8.4 的原始碼確認了機制（不是推論）：
///
/// ```text
/// pub fn load_native_certs() -> CertificateResult {
///     let paths = CertPaths::from_env();
///     match (&paths.dirs, &paths.file) {
///         (v, _) if !v.is_empty() => paths.load(),
///         (_, Some(_)) => paths.load(),
///         _ => platform::load_native_certs(),   // ← macOS 這裡讀鑰匙圈
///     }
/// }
/// ```
///
/// 只要 `SSL_CERT_FILE` 有值就走讀檔那一路，**完全不進 Security framework**。
///
/// 所以 [`avoid_macos_keychain`] 在 macOS 上把它指向 `/etc/ssl/cert.pem`
/// （Apple 隨系統附的 OpenSSL 憑證包，實測 128 張憑證）。
///
/// 三個限制條件是刻意的：
///
///   * **只在 macOS**（`cfg`）—— Linux 讀 `/etc/ssl/certs` 本來就沒問題，
///     而在 CI 上指向一個不存在的路徑會把 `paths.load()` 變成錯誤，
///     於是同一個 `.expect()` 照樣 panic。這就是為什麼**不能**用
///     `.cargo/config.toml` 的 `[env]` 做這件事：它不支援按平台分支。
///   * **檔案存在才設** —— 未來的 macOS 若移掉那個檔案，退回原本的行為
///     （偶發抖動）比每次都 panic 好。
///   * **已經有值就不覆蓋** —— 開發者若刻意指向自己的 CA 包，那是他的決定。
///
/// 生產路徑完全不受影響：`Storage::new` 沒有改，伺服器仍然讀系統信任存放區，
/// 地端客戶的私有 CA 照樣有效。這一段只存在於測試腳手架裡。
pub fn test_storage() -> fms_shared::Storage {
    avoid_macos_keychain();
    fms_shared::Storage::new(&fms_shared::StorageSettings {
        endpoint: env_or("S3_ENDPOINT", "http://localhost:9000"),
        public_endpoint: None,
        access_key: env_or("S3_ACCESS_KEY", "fmsminio"),
        secret_key: env_or("S3_SECRET_KEY", "change_me_minio"),
        region: env_or("S3_REGION", "us-east-1"),
        bucket_attachments: env_or("S3_BUCKET_ATTACHMENTS", "fms"),
        download_ttl: std::time::Duration::from_secs(300),
    })
}

/// 測試用的密鑰解析器：一份固定對照表。
///
/// 不用 `EnvSecretResolver`：測試裡改行程的環境變數在 Rust 2024 是 `unsafe`，
/// 而且跨測試執行緒有競態。**更重要的是**，若測試只能走「環境變數沒設」那一條，
/// `secret_reference_resolvable` 的 PASSED 分支就永遠沒有測試 ——
/// 而那正是這格檢查在正常組態下該走的路。
///
/// 這裡只放一個參照：`idp_test_connection_slice` 用它證明同一格檢查在
/// 「解得開」與「解不開」兩種部署下給出不同答案。
pub fn test_secrets() -> std::sync::Arc<dyn fms_shared::SecretResolver> {
    std::sync::Arc::new(fms_shared::StaticSecretResolver::new([(
        "kv/fms/resolvable".to_string(),
        "test-secret-value".to_string(),
    )]))
}

/// 在 macOS 上把 `SSL_CERT_FILE` 指向系統的 PEM 憑證包，避開鑰匙圈。
///
/// 見 [`test_storage`] 的檔頭。用 `OnceLock` 序列化：`set_var` 在多執行緒下
/// 本身是有競爭的，而這裡的呼叫者是每個測試的 `setup()` ——
/// 包在 `OnceLock` 裡保證整個 process 只會寫一次，而且寫在任何 TLS
/// 連線器被建構之前（`test_storage` 是唯一的建構點）。
fn avoid_macos_keychain() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        #[cfg(target_os = "macos")]
        {
            const BUNDLE: &str = "/etc/ssl/cert.pem";
            if std::env::var_os("SSL_CERT_FILE").is_none()
                && std::env::var_os("SSL_CERT_DIR").is_none()
                && std::path::Path::new(BUNDLE).exists()
            {
                std::env::set_var("SSL_CERT_FILE", BUNDLE);
            }
        }
    });
}

/// 測試用設定。刻意不經 `Settings::from_env()`：測試要能在沒有 `.env`
/// 的環境下跑。
///
/// `max_failures` 壓到 3（生產預設 10）：門檻越高，驗證節流的測試就要送越多
/// 次失敗登入，而每一次都是一輪 argon2。窗長維持在實際值 ——
/// 測試不依賴窗到期，縮短它只會讓「窗到期後歸零」變成一個計時競賽。
pub fn test_settings(database_url: &str) -> fms_shared::Settings {
    fms_shared::Settings {
        bind_addr: "127.0.0.1:0".into(),
        database: fms_shared::DatabaseSettings {
            url: database_url.to_string(),
            max_connections: 5,
        },
        jwt: fms_shared::JwtSettings {
            secret: "test-only-secret-at-least-32-characters".into(),
            access_ttl: std::time::Duration::from_secs(900),
            refresh_ttl: std::time::Duration::from_secs(604800),
        },
        login_throttle: fms_shared::LoginThrottleSettings {
            max_failures: 3,
            window: std::time::Duration::from_secs(300),
        },
        // **白名單刻意留空。** 需要它的測試（test-connection 打本機模擬
        // 伺服器）自己覆寫，見 `test_settings_allowing`。預設放空是為了讓
        // 「SSRF 防護在測試環境是開著的」成為預設狀態 —— 一份預設就放行
        // 私有位址的測試設定，會讓所有「該被擋下來」的斷言都失去意義。
        // SSO 需要它組 redirect_uri。測試給一個固定值 ——
        // 未設定時 /auth/sso/* 會回 501，而那條路徑 sso_slice 自己會驗。
        public_base_url: Some("https://fms.test.example.com".to_string()),
        outbound: fms_shared::OutboundSettings::default(),
        // **刻意留空。** 空清單的意思是「不加 CorsLayer」，因此絕大多數測試
        // 跑的是「沒有 CORS」的 router —— 那與生產部署未設定該變數時一致。
        //
        // 需要驗 CORS 的測試用 `setup_with` 自己填（見 `cors_slice.rs`）。
        // 預設就填一個來源會讓「這個部署到底有沒有開 CORS」在測試裡看不出差別。
        cors_allowed_origins: Vec::new(),
    }
}

/// 產生器以哪個使用者身分寫入（測試用；生產應配置 SERVICE_ACCOUNT）。
pub fn admin_user_id() -> uuid::Uuid {
    uuid::Uuid::parse_str(ADMIN_USER_ID).expect("valid uuid")
}

// =============================================================================
// 逾時看門狗
// =============================================================================
//
// # 為什麼需要它
//
// CI 上 `work_order_slice::state_machine` 卡住 17 分鐘，把整個 app job 撞到
// 30 分鐘的 timeout。job 被取消時**沒有任何輸出**指出是哪一個測試、卡在哪裡
// —— libtest 只給了一行「has been running for over 60 seconds」，
// 而那一行不含任何位置資訊。
//
// 那一次的成因已經修掉（見 `teardown`），但**「卡住看起來像 job timeout
// 而不是測試失敗」是一整類缺陷**：任何一個沒有上界的 await 都會長成同一個
// 形狀。這個 repo 裡至少還有兩條這種路徑 —— Postgres 的列鎖等待沒有
// `lock_timeout`（`transition_work_order` 會 `SELECT … FOR UPDATE`），
// 以及任何在測試裡開著交易又去打會寫同一列的 API 的寫法。
//
// 所以這裡給每個測試一個上界，讓下一次得到的是「哪一個測試、最後做到哪裡」
// 而不是一個被取消的 job。
//
// # 為什麼是結束整個 process
//
// libtest 沒有從外部中止單一測試的機制 —— 沒有辦法讓另一個執行緒把某個
// 測試標成失敗。犧牲同批其他測試的結果、換一個明確且立刻的訊號，在 CI 上
// 是划算的：job 本來就要失敗，差別只在於 2 分鐘還是 30 分鐘，
// 以及有沒有指出兇手。
//
// 以 `FMS_TEST_TIMEOUT_SECS` 調整；設 `0` 關閉（在除錯器裡逐步執行時需要）。

const DEFAULT_TEST_TIMEOUT_SECS: u64 = 120;

/// 活著的時候看門狗在跑，drop 就取消。放在 `TestContext` 裡，
/// 因此涵蓋範圍正好是「測試持有情境的那段時間」。
struct Watchdog {
    /// 只用來偵測 drop：執行緒的 `recv_timeout` 會在通道斷開時立刻回來，
    /// 因此正常結束的測試不會留下一個睡滿 120 秒的執行緒。
    _cancel: std::sync::mpsc::Sender<()>,
}

fn start_watchdog(breadcrumb: Arc<std::sync::Mutex<String>>) -> Watchdog {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let secs = env_or("FMS_TEST_TIMEOUT_SECS", "")
        .parse::<u64>()
        .unwrap_or(DEFAULT_TEST_TIMEOUT_SECS);
    if secs == 0 {
        return Watchdog { _cancel: tx };
    }

    // libtest 以測試名稱命名執行緒，而 `#[tokio::test]` 的 current-thread
    // runtime 就在那個執行緒上跑測試本體 —— 因此這裡拿得到的正是測試名稱。
    let test = std::thread::current()
        .name()
        .unwrap_or("<unnamed test thread>")
        .to_string();

    std::thread::spawn(move || {
        if rx
            .recv_timeout(std::time::Duration::from_secs(secs))
            .is_ok()
        {
            return; // 沒有人會送值，這條只是為了完整性
        }
        if matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ) {
            return; // 測試已結束，看門狗被取消
        }
        let last = breadcrumb.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // **不能用 `eprintln!`。** libtest 靠 `std::io::set_output_capture`
        // 攔截輸出，而那個 thread-local 會被 `thread::spawn` 傳給子執行緒
        // （std 刻意如此，好讓測試裡開的執行緒的 `println!` 也被歸戶）。
        // 於是這段訊息會被收進「該測試的輸出」裡 —— 而該測試永遠不會結束，
        // 那份輸出也就永遠不會被印出來。實測：沒有 `--nocapture` 時
        // 整段訊息消失，只剩下 exit code。
        //
        // `std::io::stderr()` 直接寫真正的 fd，不經過那層攔截。
        use std::io::Write as _;
        let mut err = std::io::stderr();
        let _ = write!(
            err,
            "\n\
             ==================== 測試逾時 ====================\n\
             測試：{test}\n\
             上界：{secs}s（FMS_TEST_TIMEOUT_SECS）\n\
             最後一個檢查點：{last}\n\
             \n\
             整個 process 到此為止 —— libtest 無法只中止單一測試。\n\
             這是刻意的：卡住必須長得像失敗，而不是像 job timeout。\n\
             ==================================================\n"
        );
        let _ = err.flush();
        std::process::exit(101);
    });

    Watchdog { _cancel: tx }
}

pub struct TestContext {
    pub pool: PgPool,
    /// 本測試專屬的資料庫名稱。teardown 時丟棄。
    db_name: String,
    /// 最後一個檢查點，逾時時由看門狗印出來。見 [`TestContext::mark`]。
    breadcrumb: Arc<std::sync::Mutex<String>>,
    /// drop 即取消。欄位順序無關 —— 它不依賴其他欄位。
    _watchdog: Watchdog,
    /// **在 setup 時建立一次**，`router()` 只 clone 它。
    ///
    /// 不能每次 `send()` 都重建：`IdentityState` 現在帶著登入失敗計數器，
    /// 重建等於每個請求都拿到一份新的空計數，節流永遠不會生效 ——
    /// 而那正是 `auth_hardening_slice` 要驗的東西。生產環境的
    /// `build_state` 也只呼叫一次，共用狀態因此與生產一致。
    state: Arc<fms_identity::IdentityState>,
}

impl TestContext {
    /// 複製一份全新的資料庫並回傳連到它的情境。
    pub async fn setup() -> Self {
        Self::setup_with(|_| {}).await
    }

    /// `setup` 加上一個改設定的機會。
    ///
    /// 只有 `test-connection` 用得到：它要連本機的模擬伺服器，而 SSRF 閘門
    /// 預設擋掉私有位址（見 `fms_shared::safe_http`）。放寬必須**在測試裡
    /// 明寫**，不能靠一份預設就放行的測試設定 —— 那會讓所有「該被擋下來」
    /// 的斷言失去意義。
    pub async fn setup_with(tweak: impl FnOnce(&mut fms_shared::Settings)) -> Self {
        let breadcrumb = Arc::new(std::sync::Mutex::new("TestContext::setup".to_string()));
        let _watchdog = start_watchdog(breadcrumb.clone());

        // 資料庫名稱要是合法識別字：uuid 去掉連字號。
        let db_name = format!("fms_test_{}", uuid::Uuid::new_v4().simple());

        // CREATE DATABASE 不能在有連線的來源上執行，也不能在交易裡執行，
        // 因此連到 maintenance 資料庫（postgres）發出指令後立刻關閉。
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&with_db(&owner_base(), "postgres"))
            .await
            .expect(
                "connect to the maintenance database as fms_owner \
                 (did you run `make test-template`?)",
            );
        admin
            .execute(
                format!(
                    "CREATE DATABASE {db_name} TEMPLATE {} OWNER fms_owner",
                    template_db()
                )
                .as_str(),
            )
            .await
            .expect("create the per-test database from the template");
        admin.close().await;

        // 測試使用者的密碼。設在**自己的**資料庫裡，因此不需要還原 ——
        // 這是隔離帶來的直接簡化：先前必須記住每個被改動的值並在 teardown 復原。
        let owner = PgPoolOptions::new()
            .max_connections(2)
            .connect(&with_db(&owner_base(), &db_name))
            .await
            .expect("connect as fms_owner");
        let mut conn = owner.acquire().await.expect("acquire");
        sqlx::query("SET app.is_platform = 'on'")
            .execute(&mut *conn)
            .await
            .expect("platform context");
        let hash = fms_identity::password::hash(TEST_PASSWORD).expect("hash");
        for user in TEST_USERS {
            sqlx::query("UPDATE fms.users SET password_hash = $1 WHERE username::text = $2")
                .bind(&hash)
                .bind(user)
                .execute(&mut *conn)
                .await
                .expect("set hash");
        }
        drop(conn);
        owner.close().await;

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&with_db(&app_base(), &db_name))
            .await
            .expect("connect as fms_app");

        let mut settings = test_settings(&with_db(&app_base(), &db_name));
        tweak(&mut settings);
        let state = Arc::new(fms_identity::IdentityState::new(pool.clone(), settings));

        Self {
            pool,
            db_name,
            breadcrumb,
            _watchdog,
            state,
        }
    }

    /// 記下「現在在做什麼」。逾時的時候這是唯一能指出卡在哪裡的線索。
    ///
    /// 只標在**會等待外部**的入口（HTTP 請求、開交易、teardown）——
    /// 那些正是可能沒有上界的地方，也剛好足以把範圍縮到一兩行。
    fn mark(&self, what: impl Into<String>) {
        *self.breadcrumb.lock().unwrap_or_else(|e| e.into_inner()) = what.into();
    }

    /// 丟棄本測試的資料庫。
    ///
    /// 測試若在中途 panic 就不會執行 —— 殘留的資料庫會在下一次
    /// `make test-template` 時被一併清掉，因此不需要在這裡做防禦性處理。
    ///
    /// # 為什麼 `pool.close()` 要有上界
    ///
    /// 這是 CI 上一次 30 分鐘 job timeout 的直接成因（GitHub Actions run
    /// 30776028242）。`work_order_slice::state_machine` 的所有斷言都過了，
    /// 然後在這一行靜止 17 分鐘直到 job 被取消。
    ///
    /// sqlx 的 `Pool::close()` 會等到 **`max_connections` 個號誌全部拿得到**
    /// 才回來（sqlx 0.8.6 `pool/inner.rs`：`for permits in 1..=max_connections
    /// { … semaphore.acquire(permits).await }`）。測試若還握著一個沒有 drop 的
    /// `tenant_tx`，那一個號誌永遠不會還 —— 於是 close() 無限等待。
    /// 它**不是** acquire timeout 管得到的路徑，也不會有任何錯誤訊息。
    ///
    /// 而它是**間歇**的，這是最難查的部分：close() 每關掉一條 idle 連線就會
    /// 釋出一個號誌（sqlx 註解稱之為 "a previously leaked permit"），
    /// 只要當下 idle 佇列裡有一條連線就補得上缺口、close() 就回得來。
    /// 上一個請求的連線是由**背景 task** 歸還的
    /// （`PoolConnection::drop` → `rt::spawn(return_to_pool())`），
    /// 那個 task 有沒有在 close() 之前跑完純粹是排程競爭 ——
    /// 在 runner 滿載時才輸。實測（sqlx 0.8.6，max_connections=5，一條
    /// 交易未歸還）：idle=0 → 永久卡住；idle≥1 → 立刻回來。
    ///
    /// 因此這裡改成有界等待。逾時之後仍然照常 DROP（`WITH (FORCE)` 會踢掉
    /// 殘留的 backend，所以資料庫清得掉），**然後才** panic ——
    /// 順序是刻意的：先清理再失敗，不留殘骸。
    ///
    /// 結果是這一類錯誤從「一個被取消的 job」變成「一個指名道姓的失敗測試」。
    pub async fn teardown(&self) {
        self.mark("teardown");

        // 必須先關閉連線池：還有連線時 DROP DATABASE 會被擋下。
        // `WITH (FORCE)` 是額外保險（axum router 可能還握著連線）。
        //
        // 10 秒遠超過正常值（正常是毫秒級，而且只有真的漏掉連線才會逾時：
        // 「上一個請求的連線還在歸還途中」這種暫時性佔用會自己解開 ——
        // 歸還時發現池已關閉就直接關掉連線並釋出號誌）。
        let closed = tokio::time::timeout(std::time::Duration::from_secs(10), self.pool.close())
            .await
            .is_ok();

        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&with_db(&owner_base(), "postgres"))
            .await
            .expect("connect to the maintenance database");
        admin
            .execute(format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.db_name).as_str())
            .await
            .expect("drop the per-test database");
        admin.close().await;

        assert!(
            closed,
            "pool.close() 逾時：本測試結束時還握著至少一條沒有歸還的連線。\
             找出測試裡還開著的 `tenant_tx()`／`tenant_tx_mut()`／`tenant_tx_as()`，\
             在 teardown 之前 `drop(...)` 或 `commit()` 它 —— \
             以區塊 `{{ … }}` 包住那段查詢是這個檔案裡既有的寫法。"
        );
    }

    /// `fms_owner` 連線池（連到本測試的資料庫）。
    /// PM 產生器與 no-show 掃描需要它：跨租戶掃描要平台情境，
    /// `fms_app` 會拿到空清單。
    pub async fn owner_pool(&self) -> PgPool {
        self.mark("owner_pool()");
        PgPoolOptions::new()
            .max_connections(4)
            .connect(&with_db(&owner_base(), &self.db_name))
            .await
            .expect("connect as fms_owner")
    }

    /// `fms_owner` + 平台情境的交易，供測試佈置資料用
    /// （例如把計畫的 `next_due_at` 拉到現在讓產生器選中它）。
    pub async fn owner_tx(&self) -> sqlx::Transaction<'static, sqlx::Postgres> {
        let owner = self.owner_pool().await;
        let mut tx = owner.begin().await.expect("begin");
        sqlx::query("SELECT set_config('app.is_platform', 'on', true)")
            .execute(&mut *tx)
            .await
            .expect("platform context");
        tx
    }

    /// 開一個已注入租戶情境的 `fms_app` 交易（唯讀用途，不 commit）。
    ///
    /// 為什麼一定是**交易**而不是連線：`fms.set_context()` 用的是
    /// `set_config(..., true)`，那是 **transaction-local**。在 autocommit
    /// 連線上呼叫，情境只在那一個隱含交易內有效 —— 於是
    /// `current_tenant_id()` 是 NULL、RLS 一律拒絕，症狀是 `RowNotFound`。
    pub async fn tenant_tx(&self) -> sqlx::Transaction<'static, sqlx::Postgres> {
        self.mark("tenant_tx()");
        let mut tx = self.pool.begin().await.expect("begin");
        sqlx::query("SELECT fms.set_context($1::uuid, $2::uuid, false)")
            .bind(TENANT_ID)
            .bind(ADMIN_USER_ID)
            .execute(&mut *tx)
            .await
            .expect("set_context");
        tx
    }

    /// 可寫入的租戶情境交易，給「契約沒有端點但需要驗證資料庫行為」的情況
    /// （例如空間節點搬移：003 的觸發器是被驗證的對象）。
    pub async fn tenant_tx_mut(&self) -> fms_shared::TenantTx {
        self.mark("tenant_tx_mut()");
        fms_shared::begin_tenant_tx(
            &self.pool,
            fms_shared::TenantContext::background(
                uuid::Uuid::parse_str(TENANT_ID).expect("valid uuid"),
                admin_user_id(),
                fms_shared::ActorType::System,
            ),
        )
        .await
        .expect("begin tenant tx")
    }

    /// 以**指定使用者**的身分開一個租戶情境交易。
    ///
    /// 為什麼要走 `begin_tenant_tx` 而不是直接 `SELECT fms.set_context(...)`：
    /// **`set_context` 不設 `app.facility_ids`**（它只設 tenant_id／user_id／
    /// is_platform），而 `facility_in_scope()` 讀的正是那個 GUC ——
    /// 空的時候 `current_facility_ids()` 回 NULL，於是所有 `facility_scope`
    /// 政策**全部放行**。填那個 GUC 是 `begin_tenant_tx` 的工作
    /// （見 `fms-shared/src/db.rs` 的說明；007 的註解本來就寫
    ///  「The API sets app.facility_ids」）。
    ///
    /// 因此任何想驗證場域級 RLS 的測試都必須從這裡開始。直接呼叫
    /// `set_context` 的測試會看到「政策沒有作用」，而那是測試的設定錯誤，
    /// 不是政策的缺陷 —— 實測踩過一次。
    pub async fn tenant_tx_as(&self, username: &str) -> fms_shared::TenantTx {
        let user_id: uuid::Uuid = {
            let mut owner = self.owner_tx().await;
            sqlx::query_scalar("SELECT id FROM fms.users WHERE username::text = $1")
                .bind(username)
                .fetch_one(&mut *owner)
                .await
                .unwrap_or_else(|e| panic!("找不到使用者 {username}: {e}"))
        };
        // 標在查完使用者之後：查詢那一段由 `owner_pool()` 自己的標記涵蓋，
        // 在這裡先標會被它蓋掉，反而指錯地方。
        self.mark(format!("tenant_tx_as({username})"));
        fms_shared::begin_tenant_tx(
            &self.pool,
            fms_shared::TenantContext::background(
                uuid::Uuid::parse_str(TENANT_ID).expect("valid uuid"),
                user_id,
                fms_shared::ActorType::User,
            ),
        )
        .await
        .expect("begin tenant tx")
    }

    /// 供併發測試用的 router。
    ///
    /// 與 `router()` 的差別**只有可見性**：回傳的是一份擁有所有權的 `Router`，
    /// 因此不借用 `TestContext`，可以 `tokio::spawn` 到別的執行緒上。
    ///
    /// `send()` 系列做不到這件事 —— 它們的 future 借用 `&self`，
    /// 只能在同一個 task 裡 `join!`，而那是輪詢併發而不是真並行。
    /// 見 `concurrency_correctness_slice.rs` 檔頭。
    pub fn router_for_race(&self) -> axum::Router {
        self.router()
    }

    fn router(&self) -> axum::Router {
        // state 是 setup 時建立的那一份（見欄位說明）。router 本身每次重建
        // 沒有成本也沒有狀態，因此不必快取。
        fms_server::build_router(
            (*self.state).clone(),
            test_storage(),
            test_secrets(),
            String::new(),
            String::new(),
        )
    }

    /// 本測試的登入節流門檻，取自 setup 時建立的設定。
    pub fn max_login_failures(&self) -> u32 {
        self.state.settings.login_throttle.max_failures
    }

    pub async fn send(&self, req: Request<Body>) -> (StatusCode, Value) {
        let (s, _, v) = self.send_with_headers(req).await;
        (s, v)
    }

    /// 回傳完整的 `Response`，供需要檢查 ETag 以外標頭的測試使用
    /// （例如 429 的 `Retry-After`）。
    pub async fn send_raw(&self, req: Request<Body>) -> axum::response::Response {
        self.mark(format!("{} {}", req.method(), req.uri()));
        self.router().oneshot(req).await.expect("router call")
    }

    /// 回傳 `(status, ETag, body)`。
    pub async fn send_with_headers(
        &self,
        req: Request<Body>,
    ) -> (StatusCode, Option<String>, Value) {
        self.mark(format!("{} {}", req.method(), req.uri()));
        let res = self.router().oneshot(req).await.expect("router call");
        let status = res.status();
        let etag = res
            .headers()
            .get(axum::http::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = res.into_body().collect().await.expect("body").to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, etag, json)
    }

    pub async fn login(&self) -> String {
        self.login_as(USERNAME).await
    }

    /// 以指定使用者登入。驗證權限用：同一個端點在不同角色下的結果必須不同。
    pub async fn login_as(&self, username: &str) -> String {
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/token")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "grant_type": "password",
                    "tenant_code": TENANT_CODE,
                    "username": username,
                    "password": TEST_PASSWORD
                })
                .to_string(),
            ))
            .unwrap();
        let (status, body) = self.send(req).await;
        assert_eq!(status, StatusCode::OK, "login failed: {body}");
        body["access_token"].as_str().unwrap().to_string()
    }
}

/// 加上 Authorization 與 X-Tenant-ID。
pub fn authed(req: Request<Body>, token: &str) -> Request<Body> {
    let (mut parts, body) = req.into_parts();
    parts.headers.insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {token}").parse().unwrap(),
    );
    parts
        .headers
        .insert("x-tenant-id", TENANT_ID.parse().unwrap());
    Request::from_parts(parts, body)
}

pub fn authed_idem(req: Request<Body>, token: &str, key: &str) -> Request<Body> {
    let (mut parts, body) = authed(req, token).into_parts();
    parts
        .headers
        .insert("idempotency-key", key.parse().unwrap());
    Request::from_parts(parts, body)
}

pub fn authed_if_match(req: Request<Body>, token: &str, version: &str) -> Request<Body> {
    let (mut parts, body) = authed(req, token).into_parts();
    parts
        .headers
        .insert(axum::http::header::IF_MATCH, version.parse().unwrap());
    Request::from_parts(parts, body)
}

// =============================================================================
// Fixture helper
// =============================================================================
//
// # 為什麼這些要集中
//
// 在此之前每個測試檔各自手寫 `INSERT INTO fms.…`，而欄位、enum 值與必填
// 靠記憶。那產生了五次同一種失敗，每一次都花一輪來回才查出來：
//
//   * 寫死一個不存在的 seed uuid（`dddddddd-…-001`）→ 外鍵違反
//   * `work_orders.requested_by` 不存在（必填的是 `source`）
//   * `work_order_type` 不含 `PREVENTIVE`（是 `MAINTENANCE`）
//   * `assets.status` 不是 `IN_SERVICE`（是 `OPERATIONAL`）
//   * `ck_plan_trigger`：CALENDAR 型必須有 `rrule`
//
// 「下次更小心」不是修法 —— 把 schema 知識放一處才是。
//
// # 每個 helper 的約束都是量出來的，不是記得的
//
// 下面的註解記著 `\d` 與 `pg_constraint` 的實測結果（2026-08-03）。
// 改 schema 時這裡會壞，而那正是它該有的行為：**一處壞掉勝過六處靜默漂移**。
//
// # 刻意不遷移舊檔案
//
// `meter_value_rule_slice.rs` 也手刻 `telemetry_points`，但它更早、能跑、
// 沒壞。改它是不必要的擴散。這些 helper 給新檔案與這一輪寫的檔案用。

impl TestContext {
    /// 建一台設備。
    ///
    /// 必填（NOT NULL 且無預設）：`tenant_id, facility_id, category_id,
    /// asset_code, name`。`category_id` 從既有分類取第一個 ——
    /// 那一欄常被忘記，而症狀是 not-null 違反。
    ///
    /// `status` 用 `OPERATIONAL`：`assets_status_check` 允許
    /// PLANNED／IN_STORAGE／INSTALLING／OPERATIONAL／DEGRADED／DOWN／
    /// UNDER_MAINTENANCE／DECOMMISSIONED。**沒有 `IN_SERVICE`。**
    pub async fn seed_asset(&self, facility_id: &str, code: &str) -> uuid::Uuid {
        let mut tx = self.owner_tx().await;
        let id = sqlx::query_scalar(
            "INSERT INTO fms.assets
               (tenant_id, facility_id, spatial_node_id, category_id,
                asset_code, name, status)
             VALUES ($1::uuid, $2::uuid,
                     (SELECT id FROM fms.spatial_nodes WHERE facility_id = $2::uuid LIMIT 1),
                     (SELECT id FROM fms.asset_categories LIMIT 1),
                     $3, $3 || ' 測試設備', 'OPERATIONAL')
             RETURNING id",
        )
        .bind(TENANT_ID)
        .bind(facility_id)
        .bind(code)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("seed_asset({code}) 失敗：{e}"));
        tx.commit().await.expect("commit");
        id
    }

    /// 建一張工單。
    ///
    /// 必填（NOT NULL 且無預設）：`tenant_id, facility_id, wo_no,
    /// work_order_type, title`。**沒有 `requested_by` 這一欄。**
    ///
    /// 三個 CHECK 值得記住：
    ///   * `work_orders_work_order_type_check`：MAINTENANCE／SERVICE／
    ///     INSPECTION／CORRECTIVE／PROJECT。**沒有 `PREVENTIVE`。**
    ///   * `ck_wo_target`：`asset_id` 或 `spatial_node_id` 至少一個。
    ///   * `ck_wo_service_item`：SERVICE 型必須有 `service_item_id`
    ///     —— 所以這個 helper 不產生 SERVICE 型。
    ///
    /// `status` 給 `IN_PROGRESS` 且 `actual_start_at` 設在兩小時前：
    /// 登工時與領備品都需要工單在執行中。要別的狀態請自己 UPDATE ——
    /// 走狀態機的轉移由 `work_order_slice.rs` 驗，不是這裡的事。
    pub async fn seed_work_order(&self, facility_id: &str, title: &str) -> uuid::Uuid {
        let mut tx = self.owner_tx().await;
        let id = sqlx::query_scalar(
            "INSERT INTO fms.work_orders
               (tenant_id, facility_id, wo_no, work_order_type, source, title,
                status, priority, spatial_node_id, actual_start_at)
             VALUES ($1::uuid, $2::uuid,
                     'WO-T-' || substr(md5(random()::text), 1, 10),
                     'CORRECTIVE', 'MANUAL', $3, 'IN_PROGRESS', 'MEDIUM',
                     (SELECT id FROM fms.spatial_nodes WHERE facility_id = $2::uuid LIMIT 1),
                     clock_timestamp() - interval '2 hours')
             RETURNING id",
        )
        .bind(TENANT_ID)
        .bind(facility_id)
        .bind(title)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("seed_work_order({title}) 失敗：{e}"));
        tx.commit().await.expect("commit");
        id
    }

    /// 掛一個檢查項到工單上。
    ///
    /// 必填：`tenant_id, work_order_id, seq, title`。
    /// `input_type` 的 CHECK：CHECKBOX／NUMBER／TEXT／PHOTO／SIGNATURE／SELECT。
    /// `(work_order_id, seq)` 有唯一鍵，所以 `seq` 要自己排開。
    pub async fn seed_work_order_task(
        &self,
        work_order_id: uuid::Uuid,
        seq: i32,
        title: &str,
        is_required: bool,
    ) -> uuid::Uuid {
        let mut tx = self.owner_tx().await;
        let id = sqlx::query_scalar(
            "INSERT INTO fms.work_order_tasks
               (tenant_id, work_order_id, seq, title, input_type, is_required)
             VALUES ($1::uuid, $2::uuid, $3, $4, 'CHECKBOX', $5)
             RETURNING id",
        )
        .bind(TENANT_ID)
        .bind(work_order_id)
        .bind(seq)
        .bind(title)
        .bind(is_required)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("seed_work_order_task({title}) 失敗：{e}"));
        tx.commit().await.expect("commit");
        id
    }

    /// 建一個 CALENDAR 型的保養計畫。
    ///
    /// 兩個 CHECK 是這裡最容易踩的：
    ///   * `ck_plan_trigger`：CALENDAR **必須有 `rrule`**（METER 必須有
    ///     `meter_code` 與 `meter_threshold`）。
    ///   * `ck_plan_target`：`asset_id`／`spatial_node_id`／`category_id`
    ///     **恰好一個**（不是至少一個）—— 給兩個也會違反。
    ///
    /// `completion_grace_days` 是 063 的欄位，合規報表的準時判定用它。
    pub async fn seed_maintenance_plan(
        &self,
        facility_id: &str,
        code: &str,
        completion_grace_days: i32,
    ) -> uuid::Uuid {
        let mut tx = self.owner_tx().await;
        let id = sqlx::query_scalar(
            "INSERT INTO fms.maintenance_plans
               (tenant_id, facility_id, template_id, code, name, trigger_type, rrule,
                spatial_node_id, completion_grace_days, next_due_at)
             SELECT $1::uuid, $2::uuid, t.id, $3, $3 || ' 計畫', 'CALENDAR',
                    'FREQ=MONTHLY;INTERVAL=1',
                    (SELECT id FROM fms.spatial_nodes WHERE facility_id = $2::uuid LIMIT 1),
                    $4::int, now()
               FROM fms.maintenance_templates t LIMIT 1
             RETURNING id",
        )
        .bind(TENANT_ID)
        .bind(facility_id)
        .bind(code)
        .bind(completion_grace_days)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("seed_maintenance_plan({code}) 失敗：{e}"));
        tx.commit().await.expect("commit");
        id
    }

    /// 建一台裝置與一個數值型計量點，回傳 `(device_id, point_id)`。
    ///
    /// `ck_device_target`：`asset_id` 或 `spatial_node_id` 至少一個。
    /// `uq_devices_code` 是 `(tenant_id, lower(device_code)) WHERE deleted_at IS NULL`
    /// —— 不分大小寫，所以 `code` 要自己排開。
    pub async fn seed_device_with_point(
        &self,
        facility_id: &str,
        code: &str,
    ) -> (uuid::Uuid, uuid::Uuid) {
        let mut tx = self.owner_tx().await;
        let device: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO fms.devices
               (tenant_id, facility_id, spatial_node_id, device_code, name, device_type)
             VALUES ($1::uuid, $2::uuid,
                     (SELECT id FROM fms.spatial_nodes WHERE facility_id = $2::uuid LIMIT 1),
                     $3, $3 || ' 測試裝置', 'SENSOR')
             RETURNING id",
        )
        .bind(TENANT_ID)
        .bind(facility_id)
        .bind(code)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("seed_device({code}) 失敗：{e}"));

        let point: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO fms.telemetry_points
               (tenant_id, device_id, point_code, name, data_type, unit)
             VALUES ($1::uuid, $2::uuid, $3 || '_PT', $3 || ' 測試點位', 'NUMBER', 'C')
             RETURNING id",
        )
        .bind(TENANT_ID)
        .bind(device)
        .bind(code)
        .fetch_one(&mut *tx)
        .await
        .unwrap_or_else(|e| panic!("seed_point({code}) 失敗：{e}"));

        tx.commit().await.expect("commit");
        (device, point)
    }
}
