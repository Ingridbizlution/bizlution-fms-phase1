//! HTTP 負載驅動器 —— 對真正跑起來的伺服器施加目標規模的負載。
//!
//! # 為什麼是自己寫，而不是 k6／oha
//!
//! 這個情境需要三件現成工具做不到（或要寫一樣多腳本才做到）的事：
//!
//!   1. **250 個不同帳號各自登入並持有自己的 token。** 單一 token 的負載
//!      量不到 `v_user_effective_permissions` 與場域收斂的真實成本 ——
//!      而那是這個系統每一個請求都要付的錢。
//!   2. **登入風暴要與穩態分開量。** argon2 的驗證是刻意昂貴的，因此
//!      「早上八點 250 人同時打卡登入」與「穩態瀏覽」是兩個完全不同的問題。
//!      把它們混在一個平均數裡會兩個都看不見。
//!   3. **兩階段預約**（hold → create）與**工單狀態機**都是有序的鏈，
//!      不是單一請求。
//!
//! 而且：不裝任何主機工具。這個 repo 的既有基準（`audit-overhead-bench`）
//! 也是這個性質 —— 進得了容器就跑得起來。
//!
//! # 這個工具**不判定成敗**
//!
//! 與 `docs/perf-baseline.md` 同一個立場：絕對數字只對量測的那台機器有意義。
//! 它印出數字，由人在容量規劃時讀。要變成門檻，先要有同一台機器的多次觀測。
//!
//! # 操作選擇是**決定性的**，不是隨機
//!
//! 第 i 個 worker 的第 j 次迭代做哪個操作，由 `(i, j)` 算出來。
//! 隨機的話兩次執行的組成不同，而「這次比上次慢 15%」就無法歸因 ——
//! 可能只是這次多做了幾次寫入。
//!
//! # 前置條件
//!
//! ```text
//! cd docker && make up && MIGRATE_MODE=scale make migrate
//! cd ../app && cargo run -p fms-server            # 另一個 shell
//! cargo run -p fms-loadtest --release             # 這一支
//! ```
//!
//! **一定要 `--release`。** debug 版的 reqwest／serde 慢到會讓負載產生端
//! 自己成為瓶頸 —— 那時量到的是這支程式，不是伺服器。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TENANT_CODE: &str = "DEMO_GROUP";
const TENANT_ID: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const FACILITY_ID: &str = "cccccccc-0000-4000-8000-000000000001";
const PASSWORD: &str = "Demo1234!";

/// 遙測上傳用的帳號。`telemetry:ingest` 只有 PLATFORM_ADMIN／TENANT_ADMIN／
/// IOT_INGEST 有 —— 負載帳號是 REQUESTER／TECHNICIAN／FACILITY_ADMIN，
/// 送 ingest 會全部 403，而那會被誤讀成「高負載下開始拒絕請求」。
///
/// 用管理員帳號也**符合真實部署**：閘道器用的是自己的服務帳號，
/// 不是某個老師的帳號。
const INGEST_USER: &str = "admin.chen";

/// 076 的教室節點 id 前綴。用它把夾具的資源與 009 的示範資源分開 ——
/// 兩者的預約政策不同，混在一起會讓一部分建立必然失敗。
const CLASSROOM_ID_PREFIX: &str = "1c001000-";

/// 夾具的教室數。`reservations:create` 的時段分割靠這個數字保證不重疊，
/// 因此它必須與 076 一致。
const CLASSROOMS: usize = 100;

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

fn env_num<T: std::str::FromStr>(k: &str, d: T) -> T {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

/// 一次操作的量測結果。**錯誤也要記時間** —— 一個 500 花了 8 秒與
/// 一個 500 花了 2 毫秒是完全不同的故事。
struct Sample {
    op: &'static str,
    micros: u64,
    status: u16,
    /// problem+json 的 `code`。非錯誤時為 None。
    code: Option<String>,
    /// problem+json 的 `detail`。**只有錯誤才留** —— 報告會為每一類
    /// 錯誤印出第一個 detail。少了它，「0.9% 的請求回 422」這句話
    /// 就無法行動：422 有幾十種原因。
    detail: Option<String>,
}

#[derive(Default)]
struct Collector {
    samples: Mutex<Vec<Sample>>,
}

impl Collector {
    fn push(&self, s: Sample) {
        self.samples.lock().expect("collector poisoned").push(s);
    }
}

/// 排序後取百分位。**不用 hdrhistogram**：樣本數在十萬量級，
/// 排一次的成本遠低於引入一個依賴，而精確的百分位比 histogram 的
/// 分桶近似更好解釋。
fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    // 最近秩法。`ceil(p*n) - 1`，夾在合法範圍內。
    let idx = ((p * sorted.len() as f64).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

fn ms(micros: u64) -> f64 {
    micros as f64 / 1000.0
}

/// RFC 3339，**用 `Z` 而不是 `+00:00`**。
///
/// `to_rfc3339()` 產生的是 `2026-08-05T00:00:00+00:00`，而 query string 裡
/// 未編碼的 `+` 會被解成空白 —— 伺服器收到 `2026-08-05T00:00:00 00:00`，
/// 回 400。第一次執行時 `reservations:list` 的 23,845 次請求**全部**是這個
/// 400，而報告看起來只是「這支端點很快」（0.1 ms）。
///
/// 這個陷阱對前端一模一樣：`Date.toISOString()` 產生 `Z`，是對的；
/// 而任何用 `+00:00` 拼 query 的客戶端都會踩到。
fn z(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// 印出一組樣本的百分位表，並回傳每個操作的錯誤率（0.0–1.0）。
///
/// 回傳錯誤率而不只是印出來：呼叫端要據此**判定這份報告有沒有意義**。
/// 見 `assert_measured_something`。
fn report(title: &str, samples: &[Sample], wall: Duration) -> HashMap<&'static str, f64> {
    println!("\n=== {title} ===");
    if samples.is_empty() {
        println!("（沒有樣本）");
        return HashMap::new();
    }

    let mut by_op: HashMap<&str, Vec<u64>> = HashMap::new();
    let mut errors: HashMap<(&str, u16, String), usize> = HashMap::new();
    for s in samples {
        by_op.entry(s.op).or_default().push(s.micros);
        if s.status >= 400 {
            let key = (s.op, s.status, s.code.clone().unwrap_or_else(|| "-".into()));
            *errors.entry(key).or_insert(0) += 1;
        }
    }

    println!(
        "牆上時鐘 {:.2}s，總請求 {}，整體 {:.0} req/s",
        wall.as_secs_f64(),
        samples.len(),
        samples.len() as f64 / wall.as_secs_f64()
    );
    println!(
        "\n{:<28} {:>7} {:>9} {:>9} {:>9} {:>9} {:>7}",
        "操作", "次數", "p50 ms", "p95 ms", "p99 ms", "max ms", "錯誤"
    );

    let mut ops: Vec<&str> = by_op.keys().copied().collect();
    ops.sort();
    for op in ops {
        let mut v = by_op[op].clone();
        v.sort_unstable();
        let errs: usize = errors
            .iter()
            .filter(|((o, _, _), _)| *o == op)
            .map(|(_, n)| *n)
            .sum();
        println!(
            "{:<28} {:>7} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>7}",
            op,
            v.len(),
            ms(pct(&v, 0.50)),
            ms(pct(&v, 0.95)),
            ms(pct(&v, 0.99)),
            ms(*v.last().unwrap()),
            errs
        );
    }

    // 錯誤分類。**不只印總數**：一個 429（節流，設計如此）與一個 500
    // （伺服器崩了）在容量規劃上的意義完全相反。
    if errors.is_empty() {
        println!("\n錯誤：0");
    } else {
        println!("\n錯誤分類（操作／HTTP／code）：");
        let mut ks: Vec<_> = errors.iter().collect();
        ks.sort_by_key(|((o, s, c), _)| (*o, *s, c.clone()));
        for ((op, status, code), n) in ks {
            println!("  {op:<28} {status} {code:<32} ×{n}");
            // 第一個 detail。**這一行是「0.9% 回 422」與
            // 「0.9% 因為 X 回 422」的差別** —— 後者可以行動。
            if let Some(d) = samples
                .iter()
                .find(|s| s.op == *op && s.status == *status && s.detail.is_some())
                .and_then(|s| s.detail.as_deref())
            {
                let short: String = d.chars().take(110).collect();
                println!("      └ {short}");
            }
        }
    }

    let mut rates: HashMap<&'static str, f64> = HashMap::new();
    for s in samples {
        rates.entry(s.op).or_insert(0.0);
    }
    for (op, v) in &by_op {
        let errs = samples
            .iter()
            .filter(|s| s.op == *op && s.status >= 400)
            .count();
        // by_op 的 key 是 &str，而 Sample::op 是 &'static str —— 用後者當 key。
        if let Some(first) = samples.iter().find(|s| s.op == *op) {
            rates.insert(first.op, errs as f64 / v.len() as f64);
        }
    }
    rates
}

/// **一份全是錯誤的負載報告比沒有報告更糟：它看起來像成功。**
///
/// 這個守衛不是防禦性程式碼，是實測踩過之後加的。第一次執行時 8 個操作有
/// 5 個是 100% 錯誤（403／404／422／400），而報告顯示
/// 「整體 1293 req/s、p50 0.1 ms」—— 每一個數字都是真的，
/// 而整份報告的意義是零。錯誤路徑當然快：它們在碰到資料庫之前就返回了。
///
/// 門檻定在 2%：預約建立在同一個時段上撞到彼此是真實的
/// （409 `RESERVATION_CONFLICT`），少量是預期的；而 100% 一定是模型錯了。
fn assert_measured_something(rates: &HashMap<&'static str, f64>) -> bool {
    const MAX_ERROR_RATE: f64 = 0.02;
    let mut bad: Vec<(&str, f64)> = rates
        .iter()
        .filter(|(_, r)| **r > MAX_ERROR_RATE)
        .map(|(op, r)| (*op, *r))
        .collect();
    if bad.is_empty() {
        println!(
            "\n所有操作的錯誤率都在 {:.0}% 以下 —— 這份報告量的是成功路徑。",
            MAX_ERROR_RATE * 100.0
        );
        return true;
    }
    bad.sort_by(|a, b| b.1.partial_cmp(&a.1).expect("finite"));
    eprintln!(
        "\n**這份報告不可信。** 以下操作的錯誤率超過 {:.0}%，\n\
         也就是說它們量到的是錯誤路徑（在碰到資料庫之前就返回）的延遲：",
        MAX_ERROR_RATE * 100.0
    );
    for (op, r) in &bad {
        eprintln!("  {op:<28} {:.1}%", r * 100.0);
    }
    eprintln!(
        "先看上面的「錯誤分類」修好負載模型，再讀任何延遲數字。\n\
         常見原因：token 的角色沒有那個權限（403／404）、\n\
         query 參數名不對（422）、時間戳用了 `+00:00` 而不是 `Z`（400）。"
    );
    false
}

/// 登入一個帳號，回傳 access token 與這次登入花的時間。
async fn login(client: &reqwest::Client, base: &str, username: &str) -> (Option<String>, u64, u16) {
    let body = serde_json::json!({
        "grant_type": "password",
        "tenant_code": TENANT_CODE,
        "username": username,
        "password": PASSWORD,
    });
    let t0 = Instant::now();
    let res = client
        .post(format!("{base}/api/v1/auth/token"))
        .json(&body)
        .send()
        .await;
    let micros = t0.elapsed().as_micros() as u64;
    match res {
        Ok(r) => {
            let status = r.status().as_u16();
            let token = r
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v["access_token"].as_str().map(str::to_string));
            (token, micros, status)
        }
        // 連線層的失敗（連線被拒、逾時）記成 599 —— 它不是 HTTP 狀態，
        // 但混進 2xx 裡會讓成功率虛高。
        Err(_) => (None, micros, 599),
    }
}

/// 帶上三個必要標頭。`X-Request-ID` 每次都給一個新的 —— 它會進伺服器日誌，
/// 出問題時是唯一能把一次慢請求對回伺服器那一側的線索。
fn authed(req: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    req.header("Authorization", format!("Bearer {token}"))
        .header("X-Tenant-ID", TENANT_ID)
        .header("X-Request-ID", uuid::Uuid::new_v4().to_string())
}

/// 送一個請求並收集樣本。
async fn measure(
    coll: &Collector,
    op: &'static str,
    req: reqwest::RequestBuilder,
) -> Option<serde_json::Value> {
    let t0 = Instant::now();
    let res = req.send().await;
    let micros = t0.elapsed().as_micros() as u64;
    match res {
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.json::<serde_json::Value>().await.ok();
            let (code, detail) = if status >= 400 {
                (
                    body.as_ref()
                        .and_then(|b| b["code"].as_str().map(str::to_string)),
                    body.as_ref()
                        .and_then(|b| b["detail"].as_str().map(str::to_string)),
                )
            } else {
                (None, None)
            };
            coll.push(Sample {
                op,
                micros,
                status,
                code,
                detail,
            });
            if status < 400 {
                body
            } else {
                None
            }
        }
        Err(e) => {
            coll.push(Sample {
                op,
                micros,
                status: 599,
                code: Some(if e.is_timeout() {
                    "TIMEOUT".into()
                } else {
                    "CONNECT".into()
                }),
                detail: Some(e.to_string()),
            });
            None
        }
    }
}

/// 混合負載的權重表，展開成 100 格。
///
/// 形狀取自一所校園的真實作息：多數人在看行事曆與自己的工單，
/// 少數人在寫。**不是均勻分佈** —— 均勻分佈會讓寫入路徑佔 50%，
/// 那是壓力測試常見的失真，量出來的數字對真實部署沒有參考價值。
/// 哪一種帳號送這個請求。
///
/// **這不是裝飾，是負載測試能不能量到東西的前提。** 第一次執行時
/// 每個操作都用 REQUESTER 的 token，結果 8 個操作有 5 個是 100% 錯誤：
/// `occupancy` 與 `dashboard` 回 403（REQUESTER 沒有 `reservation:read`
/// 與 `report:read`），`wo:available-actions` 回 404（只有
/// `work_order:read_own`，別人的工單一律不存在）。
///
/// 那些 403／404 **全部是正確的授權行為**。錯的是負載模型：
/// 真實部署裡送這些請求的不是同一種帳號。
#[derive(Clone, Copy, PartialEq)]
enum Role {
    /// 教職員（225 個）—— 訂教室、看自己的工單。
    Requester,
    /// 技師（20 個）—— 打開派給自己的工單。
    Technician,
    /// 場域管理員（5 個）—— 儀表板與佔用地圖。
    ///
    /// 5 個帳號承擔 20% 的請求量看起來失衡，但那**正是真實形狀**：
    /// 100 間教室有 100 面牆面板在輪詢，而它們共用少數幾個看板帳號。
    FacilityAdmin,
    /// 閘道器的服務帳號 —— 遙測上傳。
    Gateway,
}

const MIX: &[(&str, usize, Role)] = &[
    ("reservations:list", 30, Role::Requester),
    ("work-orders:list-mine", 20, Role::Technician),
    ("occupancy", 15, Role::FacilityAdmin),
    // **這裡的角色是一個發現，不是設計。** `GET /facilities/{id}/availability`
    // 要 `reservation:read`（handlers.rs 第 43 行），而 REQUESTER **沒有那個
    // 權限** —— 也就是說一個老師可以 `reservation:create` 卻查不到哪間教室
    // 是空的，只能盲訂然後吃 409。
    //
    // 真實的送出者應該是教職員。這裡改用場域管理員**只是為了讓這一格量得到
    // 東西**；權限資料要不要改是產品決定，見 docs/perf-baseline.md 的發現一節。
    ("availability", 10, Role::FacilityAdmin),
    ("reservations:create", 8, Role::Requester),
    ("wo:available-actions", 7, Role::Technician),
    ("dashboard", 5, Role::FacilityAdmin),
    ("telemetry:ingest", 5, Role::Gateway),
];

fn mix_table() -> Vec<(&'static str, Role)> {
    let mut t = Vec::with_capacity(100);
    for (op, w, role) in MIX {
        for _ in 0..*w {
            t.push((*op, *role));
        }
    }
    assert_eq!(t.len(), 100, "MIX 的權重必須加總為 100");
    t
}

/// 076 的帳號編號分佈：1..225 REQUESTER、226..245 TECHNICIAN、
/// 246..250 FACILITY_ADMIN。**與 076 的角色指派同一份事實** ——
/// 兩邊不一致的話負載會全部打在錯的角色上。
fn role_of(index_one_based: usize) -> Role {
    match index_one_based {
        n if n > 245 => Role::FacilityAdmin,
        n if n > 225 => Role::Technician,
        _ => Role::Requester,
    }
}

#[tokio::main]
async fn main() {
    let base = env_or("LOAD_BASE_URL", "http://127.0.0.1:8080");
    let users: usize = env_num("LOAD_USERS", 250);
    let secs: u64 = env_num("LOAD_SECONDS", 60);
    let ingest_batch: usize = env_num("LOAD_INGEST_BATCH", 20);
    // **每次執行一個不同的時段基準。**
    //
    // 操作的選擇是決定性的（見檔頭），但**預約要訂哪個時段不能是決定性的**：
    // 076 的資料是持久的，所以第二次執行會撞到第一次留下的預約 ——
    // 實測第三次執行時 `reservations:create` 有 3,264 次是
    // 409 `RESERVATION_CONFLICT`，也就是那一格量的變成衝突處理。
    //
    // 用執行開始的 epoch 秒當偏移。同一次執行內仍然完全決定性
    // （時段只由 worker／iter／salt 決定），跨執行則落在不同的時段上。
    // 可用 `LOAD_SLOT_SALT` 固定它來重現某一次執行。
    let slot_salt: usize = env_num(
        "LOAD_SLOT_SALT",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as usize)
            .unwrap_or(0),
    );

    println!("目標：{base}");
    println!("並發使用者 {users}，穩態 {secs} 秒，每批讀值 {ingest_batch} 筆");
    println!("時段鹽 LOAD_SLOT_SALT={slot_salt}（要重現這一次就固定它）");

    // 連線池要夠大。預設的 idle 上限會讓 250 個並發共用少數連線，
    // 於是量到的是**連線建立**而不是伺服器處理 —— 這一行是必要的。
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(users * 2)
        .timeout(Duration::from_secs(30))
        .build()
        .expect("build http client");

    // ---------------------------------------------------------------------
    // 階段 1：登入風暴
    // ---------------------------------------------------------------------
    // 所有帳號在同一個 Barrier 後同時送出。`tokio::spawn` 的排程順序
    // **不保證同時開始** —— 沒有 Barrier 的話 250 個登入會被拉成一條斜坡，
    // 而斜坡量不到我們要找的東西（同時抵達時的排隊）。
    // 這個教訓來自 `concurrency_correctness_slice`。
    let coll_login = Arc::new(Collector::default());
    let barrier = Arc::new(tokio::sync::Barrier::new(users));
    let t_login = Instant::now();

    let mut handles = Vec::with_capacity(users);
    for i in 0..users {
        let client = client.clone();
        let base = base.clone();
        let barrier = barrier.clone();
        let coll = coll_login.clone();
        handles.push(tokio::spawn(async move {
            let username = format!("load{:03}", i + 1);
            barrier.wait().await;
            let (token, micros, status) = login(&client, &base, &username).await;
            coll.push(Sample {
                op: "auth:token",
                micros,
                status,
                code: if status >= 400 {
                    Some(format!("HTTP{status}"))
                } else {
                    None
                },
                detail: None,
            });
            token
        }));
    }

    // token 依角色分池。**順序即角色** —— handles 是按 i 依序 push 的，
    // 所以第 i 個結果對應 load{i+1}，而 role_of 用的是同一個編號。
    let mut tokens: Vec<String> = Vec::with_capacity(users);
    let mut pool_requester: Vec<String> = Vec::new();
    let mut pool_technician: Vec<String> = Vec::new();
    let mut pool_admin: Vec<String> = Vec::new();
    for (i, h) in handles.into_iter().enumerate() {
        if let Ok(Some(t)) = h.await {
            match role_of(i + 1) {
                Role::Requester => pool_requester.push(t.clone()),
                Role::Technician => pool_technician.push(t.clone()),
                Role::FacilityAdmin => pool_admin.push(t.clone()),
                Role::Gateway => {}
            }
            tokens.push(t);
        }
    }
    let login_wall = t_login.elapsed();

    {
        let s = coll_login.samples.lock().expect("poisoned");
        report(&format!("階段 1：{users} 個帳號同時登入"), &s, login_wall);
    }
    println!(
        "\n取得 token：{}/{}（教職員 {}、技師 {}、場域管理員 {}）",
        tokens.len(),
        users,
        pool_requester.len(),
        pool_technician.len(),
        pool_admin.len()
    );
    // 任何一個池空掉，對應的操作就會全部打在錯的角色上（或 panic）。
    // 這裡就停 —— 讓它跑下去只會產出一份看起來正常的錯誤路徑報告。
    if pool_requester.is_empty() || pool_technician.is_empty() || pool_admin.is_empty() {
        eprintln!(
            "角色池有空的 —— 076 的角色指派與 role_of() 對不上。\n\
             需要 LOAD_USERS >= 250 才會涵蓋到技師（226+）與場域管理員（246+）。"
        );
        std::process::exit(1);
    }
    if tokens.is_empty() {
        eprintln!(
            "沒有任何帳號登入成功 —— 先確認 MIGRATE_MODE=scale 跑過（076 建的 load001..250）"
        );
        std::process::exit(1);
    }

    // 遙測用的管理員 token。
    let (ingest_token, _, ingest_status) = login(&client, &base, INGEST_USER).await;
    let ingest_token = match ingest_token {
        Some(t) => t,
        None => {
            eprintln!("{INGEST_USER} 登入失敗（HTTP {ingest_status}）—— 遙測那一格會全部失敗");
            std::process::exit(1);
        }
    };

    // 只跑階段 1 就結束。用途：把登入風暴當成**外部干擾源**注入到另一個
    // 正在跑穩態的行程上，以回答「登入會不會拖慢不相關的請求」——
    // 那是 argon2 有沒有佔住 tokio worker 的決定性實驗。
    if std::env::var("LOAD_LOGIN_ONLY").is_ok() {
        println!("\nLOAD_LOGIN_ONLY：跳過穩態階段。");
        return;
    }

    // ---------------------------------------------------------------------
    // 佈置：一次抓好 worker 要用的 id，不要每次迭代都去查
    // ---------------------------------------------------------------------
    // 每次迭代先查一次 id 會讓那支查詢佔掉一半的請求量，
    // 而混合比例就不是上面寫的那個了。
    let admin = &ingest_token;
    // **抓 `resource_id`，不是 `id`。** `POST /reservations` 的 `resource_id`
    // 是**底層節點（或設備）的 id**：`repo::find_bookable` 比對的是
    // `spatial_node_id = $1 OR asset_id = $1`。清單端點同時回兩個欄位，
    // 拿錯的那個會得到 404「resource is not bookable」——
    // 而那個訊息把人指向 `is_bookable` 這個完全正確的欄位。
    let all_resources = fetch_field(
        &client,
        admin,
        &format!("{base}/api/v1/facilities/{FACILITY_ID}/bookable-resources?limit=200"),
        "resource_id",
    )
    .await;
    // **只留 076 建的 100 間教室。**
    //
    // 清單裡還有 009 的三個示範資源，而它們的政策不同 ——「共享工位」的
    // `min_duration_minutes` 是 60，而這支腳本訂 50 分鐘的時段，
    // 於是那個資源上的每一次建立都是 422「最短 60 分鐘」（實測 33 次）。
    //
    // 不改成訂 60 分鐘來繞過：那會讓時段長度由「最嚴格的那個舊資源」決定，
    // 而下一個人加一個 min 90 的資源時同樣的問題會再出現一次。
    // 用前綴把夾具的資源挑出來，政策就完全在 076 的掌握裡。
    let resources: Vec<String> = all_resources
        .into_iter()
        .filter(|id| id.starts_with(CLASSROOM_ID_PREFIX))
        .collect();
    // 技師只看得到派給自己的工單（`work_order:read_own`）。
    // **每個技師用自己的 token 各查一次。** 用管理員抓全部 100 張再隨機挑，
    // 有 95% 會落在別的技師頭上，而伺服器會正確地回 404 ——
    // 於是那一格 100% 是錯誤路徑（第二次執行實測 3,234 次全部 404）。
    let mut work_orders: Vec<Vec<String>> = Vec::with_capacity(pool_technician.len());
    for t in &pool_technician {
        work_orders.push(
            fetch_field(
                &client,
                t,
                &format!("{base}/api/v1/work-orders?mine=true&limit=50"),
                "id",
            )
            .await,
        );
    }

    let wo_total: usize = work_orders.iter().map(Vec::len).sum();
    println!(
        "佈置：{} 個可訂資源、{} 位技師共 {} 張自己的工單（最少 {}）",
        resources.len(),
        work_orders.len(),
        wo_total,
        work_orders.iter().map(Vec::len).min().unwrap_or(0)
    );
    // 任何一位技師手上是空的，`wo:available-actions` 對他就沒有東西可打。
    // 這是 076 的派工分佈問題（自我檢查 (f) 守的就是這件事），在這裡再確認一次
    // ——因為夾具與負載腳本是兩份對「誰有什麼」的理解，兩份都要對。
    if resources.len() != CLASSROOMS || wo_total == 0 || work_orders.iter().any(Vec::is_empty) {
        eprintln!(
            "佈置不完整：教室 {}（預期 {CLASSROOMS}）、技師 {} 位、工單 {} 張。\n\
             076 是否跑過（MIGRATE_MODE=scale）？每位技師都必須至少有一張工單，\n\
             而教室數必須剛好 {CLASSROOMS} —— 時段分割的不重疊性靠它。",
            resources.len(),
            work_orders.len(),
            wo_total
        );
        std::process::exit(1);
    }

    // ---------------------------------------------------------------------
    // 階段 2：穩態混合負載
    // ---------------------------------------------------------------------
    let coll = Arc::new(Collector::default());
    let table = Arc::new(mix_table());
    let resources = Arc::new(resources);
    let work_orders = Arc::new(work_orders);
    let iters = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + Duration::from_secs(secs);
    let t_steady = Instant::now();

    let pools = Arc::new(Pools {
        requester: pool_requester,
        technician: pool_technician,
        admin: pool_admin,
        gateway: ingest_token.clone(),
    });

    // 並發度仍然是 250 —— 那是客戶給的「同時在線人數」。
    // 每個 worker 依操作所屬的角色去對應的池借一個 token，
    // 因此**系統收到的請求組成**符合 MIX，而每一個請求都由有權限的帳號送出。
    let mut handles = Vec::with_capacity(users);
    for i in 0..users {
        let client = client.clone();
        let base = base.clone();
        let coll = coll.clone();
        let table = table.clone();
        let resources = resources.clone();
        let work_orders = work_orders.clone();
        let iters = iters.clone();
        let pools = pools.clone();
        handles.push(tokio::spawn(async move {
            let mut j = 0usize;
            // 這個 worker 已經建立過幾次預約。**用它而不是 `j` 算時段**：
            // `j` 數的是所有操作，而建立只佔 8% —— 用 `j` 算的話時段會
            // 跳著走，而「一個 worker 不會撞到自己」這個保證就不成立。
            let mut creates = 0usize;
            while Instant::now() < deadline {
                // 決定性選擇。`* 31` 讓相鄰 worker 在同一時刻做不同的事 ——
                // 否則 250 個 worker 會整批同時打同一支端點，
                // 那是一個週期性的尖峰，不是穩態。
                let (op, role) = table[(i * 31 + j) % 100];
                let (slot, token) = pools.pick(role, i + j);
                one_op(
                    &client,
                    &base,
                    &coll,
                    token,
                    op,
                    i,
                    j,
                    slot,
                    &resources,
                    &work_orders,
                    ingest_batch,
                    slot_salt,
                    creates,
                )
                .await;
                if op == "reservations:create" {
                    creates += 1;
                }
                iters.fetch_add(1, Ordering::Relaxed);
                j += 1;
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    let steady_wall = t_steady.elapsed();

    let trustworthy = {
        let s = coll.samples.lock().expect("poisoned");
        let rates = report(
            &format!("階段 2：{users} 個並發使用者的混合負載"),
            &s,
            steady_wall,
        );
        assert_measured_something(&rates)
    };

    println!("\n混合比例（每 100 次操作）與送出者的角色：");
    for (op, w, role) in MIX {
        let who = match role {
            Role::Requester => "教職員 REQUESTER",
            Role::Technician => "技師 TECHNICIAN",
            Role::FacilityAdmin => "場域管理員 FACILITY_ADMIN",
            Role::Gateway => "閘道器 TENANT_ADMIN",
        };
        println!("  {op:<28} {w:>3}  {who}");
    }
    println!(
        "\n注意：這些是**這一台機器**上的絕對值。跨機器比較沒有意義 ——\n\
         見 docs/perf-baseline.md 的說明。"
    );

    // 非零離開碼。這支工具**不判定效能好壞**（那要人讀），
    // 但它必須判定「這次量測本身有沒有意義」。
    if !trustworthy {
        std::process::exit(2);
    }
}

/// 按角色分好的 token 池。
struct Pools {
    requester: Vec<String>,
    technician: Vec<String>,
    admin: Vec<String>,
    gateway: String,
}

impl Pools {
    /// 依角色取一個 token。`salt` 讓同一個 worker 的連續迭代不會一直
    /// 借到同一個帳號 —— 5 個管理員帳號若被固定綁在幾個 worker 上，
    /// 那幾個帳號的節流計數與連線親和性會偏離真實形狀。
    /// 回傳 `(池內索引, token)`。索引是必要的：技師只看得到派給自己的工單
    /// （`work_order:read_own`），所以 `wo:available-actions` 必須用**這個**
    /// 技師的工單清單 —— 用全部 100 張裡隨便一張，95% 會是正確的 404。
    fn pick(&self, role: Role, salt: usize) -> (usize, &str) {
        match role {
            Role::Requester => {
                let i = salt % self.requester.len();
                (i, &self.requester[i])
            }
            Role::Technician => {
                let i = salt % self.technician.len();
                (i, &self.technician[i])
            }
            Role::FacilityAdmin => {
                let i = salt % self.admin.len();
                (i, &self.admin[i])
            }
            Role::Gateway => (0, &self.gateway),
        }
    }
}

/// 執行一次操作。
#[allow(clippy::too_many_arguments)]
async fn one_op(
    client: &reqwest::Client,
    base: &str,
    coll: &Collector,
    token: &str,
    op: &'static str,
    worker: usize,
    iter: usize,
    // 這個 token 在它自己的角色池裡的索引。技師用它取自己的工單清單。
    slot: usize,
    resources: &[String],
    work_orders: &[Vec<String>],
    ingest_batch: usize,
    slot_salt: usize,
    // 這個 worker 已建立過幾次預約。決定它這一次要訂哪個時段。
    creates: usize,
) {
    let res_id = &resources[worker % resources.len()];

    match op {
        "reservations:list" => {
            let from = chrono::Utc::now();
            let to = from + chrono::Duration::days(7);
            measure(
                coll,
                op,
                authed(
                    client.get(format!(
                        "{base}/api/v1/reservations?resource_id={res_id}&from={}&to={}&limit=50",
                        z(from),
                        z(to)
                    )),
                    token,
                ),
            )
            .await;
        }
        "work-orders:list-mine" => {
            measure(
                coll,
                op,
                authed(
                    client.get(format!("{base}/api/v1/work-orders?mine=true&limit=25")),
                    token,
                ),
            )
            .await;
        }
        "occupancy" => {
            measure(
                coll,
                op,
                authed(
                    client.get(format!("{base}/api/v1/facilities/{FACILITY_ID}/occupancy")),
                    token,
                ),
            )
            .await;
        }
        "availability" => {
            // **`from`/`to`，不是 `date`；`resource_ids` 是複數且逗號分隔。**
            // 第一版送 `date=&resource_id=`，10 次有 10 次是
            // 422「from is required」—— 而報告顯示 0.1 ms，看起來像最快的端點。
            let from = chrono::Utc::now() + chrono::Duration::days(1);
            let to = from + chrono::Duration::days(1);
            measure(
                coll,
                op,
                authed(
                    client.get(format!(
                        "{base}/api/v1/facilities/{FACILITY_ID}/availability?from={}&to={}&resource_ids={res_id}",
                        z(from),
                        z(to)
                    )),
                    token,
                ),
            )
            .await;
        }
        "wo:available-actions" => {
            // **這個技師自己的工單。** `work_orders[slot]` 是第 slot 個技師
            // 在佈置階段用自己的 token 查到的清單。
            let mine = &work_orders[slot % work_orders.len()];
            if mine.is_empty() {
                return;
            }
            let wo_id = &mine[iter % mine.len()];
            measure(
                coll,
                op,
                authed(
                    client.get(format!(
                        "{base}/api/v1/work-orders/{wo_id}/available-actions"
                    )),
                    token,
                ),
            )
            .await;
        }
        "dashboard" => {
            measure(
                coll,
                op,
                authed(
                    client.get(format!(
                        "{base}/api/v1/reports/facility-dashboard?facility_id={FACILITY_ID}&period=7d"
                    )),
                    token,
                ),
            )
            .await;
        }
        "reservations:create" => {
            // 時段對齊到 10 分鐘（資源的 slot_granularity_minutes），
            // 並且**每個 worker 各自一個時段**：這一格量的是預約建立的
            // 成本，不是排他約束的衝突處理。衝突行為由
            // `concurrency_correctness_slice` 專門驗，混進來只會讓
            // 這裡的延遲被 409 的路徑污染。
            // **時段的不重疊是由建構保證的，不是靠機率。**
            //
            // 上一版用雜湊式的 `(worker*7 + iter*13 + salt) % 60`，時段空間
            // 3,240 格／資源、每個資源約 30 次建立 —— 生日碰撞算出來約 14%，
            // 實測 8.6%（270 次 409）。**那個比例足以污染 p95。**
            //
            // 現在的分割：250 個 worker 分佈在 100 間教室上，
            // 同一間教室最多 3 個 worker（w、w+100、w+200）。
            // 給每個 worker 一段互不相交的起點區間（18 格 × 10 分鐘）：
            //
            //   worker / 100 == 0 → 步進 0..17   （08:00–10:50）
            //   worker / 100 == 1 → 步進 18..35  （11:00–13:50）
            //   worker / 100 == 2 → 步進 36..53  （14:00–16:50）
            //
            // 同一間教室的三個 worker 因此永遠不會撞到彼此；
            // 而 `iter / 18` 換一天，所以一個 worker 也不會撞到自己。
            //
            // 衝突處理本身由 `concurrency_correctness_slice` 專門驗 ——
            // 這一格要量的是**成功建立**的成本。
            let band = worker / CLASSROOMS; // 0、1 或 2
                                            // **一格是 60 分鐘，不是 10 分鐘。**
                                            //
                                            // 資源的 `buffer_after_minutes = 10`，所以一筆 50 分鐘的預約
                                            // 實際佔用 60 分鐘（08:00–08:50 加緩衝到 09:00）。
                                            // 上一版用 10 分鐘步進，於是**相鄰的兩格必然重疊** ——
                                            // 實測 77.6% 的建立是 409。緩衝時間不在時段裡看得見，
                                            // 這是這一輪最容易漏掉的一個約束。
                                            //
                                            //   band 0 → 08、09、10 時
                                            //   band 1 → 11、12、13 時
                                            //   band 2 → 14、15、16 時
                                            //
                                            // 同一間教室最多 3 個 worker（w、w+100、w+200），各佔一個 band，
                                            // 因此彼此不可能重疊；`creates / 3` 換一天，所以也不會撞到自己。
            let step = ((band * 3 + (creates % 3)) * 60) as i64;
            // 60 天內輪替 → 一個 worker 有 180 個時段。一次 60 秒的執行
            // 每個 worker 約建立 12 次，離 180 還很遠。
            // **執行超過約 15 分鐘就會繞回來並開始撞自己** —— 那時 409 會
            // 上升，而報告的守衛會擋下來（那正是它存在的理由）。
            let day = 1 + ((creates / 3 + slot_salt) % 60) as i64;
            let base_time = chrono::Utc::now()
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .expect("midnight")
                .and_utc()
                + chrono::Duration::days(day)
                + chrono::Duration::minutes(8 * 60 + step * 10);
            let body = serde_json::json!({
                "resource_id": res_id,
                "title": format!("負載 w{worker} i{iter}"),
                "party_size": 20,
                "start_at": z(base_time),
                "end_at": z(base_time + chrono::Duration::minutes(50)),
            });
            measure(
                coll,
                op,
                authed(client.post(format!("{base}/api/v1/reservations")), token)
                    // 冪等鍵：真實客戶端會帶，而它多一次資料庫往返。
                    // 不帶會讓這一格比生產路徑便宜。
                    .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
                    .json(&body),
            )
            .await;
        }
        "telemetry:ingest" => {
            // 1000 台裝置輪流上傳。每批 `ingest_batch` 台。
            let start = (worker * ingest_batch + iter) % 1000;
            let readings: Vec<serde_json::Value> = (0..ingest_batch)
                .map(|k| {
                    let dev = ((start + k) % 1000) + 1;
                    serde_json::json!({
                        "device_code": format!("DEV_{dev:04}"),
                        "point_code": "POINT_TEMP",
                        "observed_at": z(chrono::Utc::now()),
                        // 落在 valid_min/max 內。超界的值會被記成逐筆錯誤，
                        // 那量到的是驗證路徑而不是寫入路徑。
                        "value_num": 20.0 + (dev % 15) as f64,
                    })
                })
                .collect();
            measure(
                coll,
                op,
                authed(
                    client.post(format!("{base}/api/v1/telemetry:batch-ingest")),
                    token,
                )
                .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
                .json(&serde_json::json!({ "readings": readings })),
            )
            .await;
        }
        other => panic!("MIX 裡有未實作的操作：{other}"),
    }
}

/// 抓一份清單裡每一列的某個欄位。
///
/// 佈置階段用。失敗時回空清單，由呼叫端判斷並給出可行動的訊息 ——
/// 這裡 panic 的話錯誤訊息會是 reqwest 的內部字串，讀的人不知道要跑 076。
async fn fetch_field(client: &reqwest::Client, token: &str, url: &str, field: &str) -> Vec<String> {
    let Ok(res) = authed(client.get(url), token).send().await else {
        return Vec::new();
    };
    if !res.status().is_success() {
        eprintln!("佈置查詢 {url} 回 HTTP {}", res.status());
        return Vec::new();
    }
    let Ok(body) = res.json::<serde_json::Value>().await else {
        return Vec::new();
    };
    body["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x[field].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}
