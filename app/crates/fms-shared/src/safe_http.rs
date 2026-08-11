//! 往「租戶填的網址」發 HTTP 請求時的安全閘門（SSRF 防護）。
//!
//! # 為什麼需要一整個模組
//!
//! 這是整個系統第一次把**呼叫端提供的字串**當成連線目標。在那之前所有出站
//! 連線的目標都寫在部署設定裡（資料庫、S3、SMTP、OTLP collector）。
//!
//! 一個沒有防護的「測試連線」端點是一支伺服器端埠掃描器：填
//! `https://169.254.169.254/latest/meta-data/iam/...` 就讓伺服器去讀雲端
//! metadata，填 `https://127.0.0.1:5432` 就用回應時間與錯誤訊息探測內網。
//! 而這兩件事在回應裡看起來都只是「連線失敗」或「回了一段 JSON」。
//!
//! 目前的使用者是 `POST /identity-providers/{id}/test-connection`；
//! `/webhooks`（事件外送）會是第二個，因此這裡放在 `fms-shared` 而不是
//! identity 模組裡。
//!
//! # 四道防線，缺任何一道都等於沒有防護
//!
//! 1. **只允許 https。** `http` 讓中間人改掉 discovery 文件的內容；
//!    `file`／`gopher`／`ftp` 之類的 scheme 則完全不該出現。
//! 2. **自己解析 DNS，逐一檢查每一個位址。** 只檢查主機名是沒有用的：
//!    `internal.attacker.com` 可以 A 記錄指向 `127.0.0.1`。
//! 3. **連線時 pin 住剛剛檢查過的那個 IP。** 少了這一步，檢查與連線之間
//!    會再解析一次 DNS，而攻擊者控制的網域可以在那個空隙裡換掉答案
//!    （DNS rebinding）—— 於是檢查通過的是一個位址，連上的是另一個。
//! 4. **不跟隨轉址。** 一個公開網址回 `302 Location: http://127.0.0.1/`
//!    會讓前三道全部失效。
//!
//! # 測試怎麼辦
//!
//! 前三道會擋掉 `127.0.0.1`，而整合測試的模擬伺服器就在那裡。因此有一份
//! **明確列出 host:port 的白名單**（`OUTBOUND_ALLOW_PRIVATE_TARGETS`），
//! 預設是空的。
//!
//! 刻意不做成一個 `allow_private_targets=true` 的布林：那種旗標會在某次
//! 除錯時被打開然後留在生產環境，而它一次放行所有私有位址。白名單的誤用
//! 需要有人指名一個具體的內部主機，範圍窄得多，而且在設定檔裡看得見。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

/// 出站請求的設定。
#[derive(Debug, Clone)]
pub struct OutboundSettings {
    /// 允許連往私有位址的 `host:port` 白名單。**預設空**。
    ///
    /// 比對的是 URL 裡的主機與埠（不是解析後的 IP）—— 因為填設定的人寫的是
    /// 主機名，而讓他去查那個名字現在解析到哪個 IP 只會讓白名單難以維護。
    pub private_target_allowlist: Vec<String>,
    pub connect_timeout: Duration,
    /// 整個請求（含讀回應）的上限。
    pub total_timeout: Duration,
    /// 回應主體讀取上限。超過即中止 —— 一個惡意端點可以回無限長的串流，
    /// 而「測試連線」不該把伺服器的記憶體吃光。
    pub max_response_bytes: usize,
}

impl Default for OutboundSettings {
    fn default() -> Self {
        Self {
            private_target_allowlist: Vec::new(),
            connect_timeout: Duration::from_secs(5),
            total_timeout: Duration::from_secs(10),
            // OIDC 的 discovery 文件通常 1–3 KB；256 KB 給足了餘裕，
            // 同時遠低於「回應大到值得擔心」的量級。
            max_response_bytes: 256 * 1024,
        }
    }
}

/// 目標被拒絕的原因。
///
/// 每一種都要能對呼叫端說明白 —— 「連線失敗」對整合的人毫無幫助，
/// 而這些拒絕全部發生在**還沒連線之前**，所以我們知道確切原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejected {
    Unparsable(String),
    /// scheme 不是 https。
    NotHttps(String),
    /// URL 裡帶了 `user:pass@`。那是憑證，不該出現在設定值裡，而且
    /// 某些函式庫會把它變成 Authorization 標頭送出去。
    HasCredentials,
    NoHost,
    DnsFailed(String),
    /// 解析成功但沒有任何位址。
    NoAddress,
    /// 解析出來的位址是私有／保留位址。`kind` 說明是哪一類。
    PrivateAddress {
        addr: IpAddr,
        kind: &'static str,
    },
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unparsable(u) => write!(f, "`{u}` 不是一個合法的網址"),
            Self::NotHttps(s) => write!(
                f,
                "scheme 是 `{s}`，只接受 https —— http 讓中間人可以改掉回應內容"
            ),
            Self::HasCredentials => write!(
                f,
                "網址裡不可包含 user:password —— 憑證請走密鑰管理服務的參照"
            ),
            Self::NoHost => write!(f, "網址沒有主機名"),
            Self::DnsFailed(e) => write!(f, "主機名解析失敗：{e}"),
            Self::NoAddress => write!(f, "主機名解析不到任何位址"),
            Self::PrivateAddress { addr, kind } => write!(
                f,
                "主機名解析到 {addr}（{kind}）—— 伺服器不會連往內部位址。\
                 若這是刻意的內網整合，請把該主機加入 OUTBOUND_ALLOW_PRIVATE_TARGETS"
            ),
        }
    }
}

/// 檢查通過的目標。
#[derive(Debug, Clone)]
pub struct Checked {
    pub url: reqwest::Url,
    pub host: String,
    pub port: u16,
    /// **檢查過的那一個位址。** 連線必須 pin 住它，見模組說明第 3 點。
    pub addr: SocketAddr,
    /// 這個目標是因為在白名單裡才通過的（位址本身是私有的）。
    /// 呼叫端應該把它回報出去 —— 一個「因為白名單才成立」的成功結果
    /// 與一般的成功不同。
    pub allowlisted: bool,
}

/// 判斷一個位址是否屬於不該連往的範圍；回傳它屬於哪一類。
///
/// **`None` 才是可以連的。** 這個方向是刻意的：新增一類要擋的位址時，
/// 忘記在某處處理的後果是回傳 `Some`（擋下來），而不是放行。
pub fn private_address_kind(addr: IpAddr) -> Option<&'static str> {
    match addr {
        IpAddr::V4(v4) => v4_kind(v4),
        IpAddr::V6(v6) => {
            // IPv4-mapped（`::ffff:127.0.0.1`）與 NAT64（`64:ff9b::7f00:1`）
            // 內嵌一個 v4 位址。不展開就檢查的話，`::ffff:169.254.169.254`
            // 會直接通過 —— 而它連上的是雲端 metadata。
            if let Some(v4) = mapped_v4(v6) {
                return v4_kind(v4).or(Some("IPv4-mapped 位址"));
            }
            v6_kind(v6)
        }
    }
}

fn v4_kind(v4: Ipv4Addr) -> Option<&'static str> {
    let o = v4.octets();
    if v4.is_unspecified() {
        return Some("未指定位址 0.0.0.0");
    }
    if v4.is_loopback() {
        return Some("loopback");
    }
    if v4.is_private() {
        return Some("RFC 1918 私有網段");
    }
    if v4.is_link_local() {
        // 169.254.169.254 是各家雲端的 metadata 端點 —— SSRF 最常見的目標。
        return Some("link-local（含雲端 metadata 169.254.169.254）");
    }
    if v4.is_broadcast() {
        return Some("廣播位址");
    }
    if v4.is_documentation() {
        return Some("文件用保留位址");
    }
    if v4.is_multicast() {
        return Some("multicast");
    }
    // 以下幾類 std 沒有對應的判斷函式。
    if o[0] == 100 && (64..128).contains(&o[1]) {
        return Some("CGNAT 100.64.0.0/10");
    }
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return Some("benchmarking 198.18.0.0/15");
    }
    if o == [192, 0, 0, 0] || (o[0] == 192 && o[1] == 0 && o[2] == 0) {
        return Some("IETF 協定指派 192.0.0.0/24");
    }
    if o[0] >= 240 {
        return Some("保留位址 240.0.0.0/4");
    }
    None
}

fn mapped_v4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v6.segments();
    // ::ffff:a.b.c.d
    if s[0..5] == [0, 0, 0, 0, 0] && s[5] == 0xffff {
        return Some(Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        ));
    }
    // 64:ff9b::/96（NAT64）
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return Some(Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        ));
    }
    None
}

fn v6_kind(v6: Ipv6Addr) -> Option<&'static str> {
    let s = v6.segments();
    if v6.is_unspecified() {
        return Some("未指定位址 ::");
    }
    if v6.is_loopback() {
        return Some("loopback ::1");
    }
    if v6.is_multicast() {
        return Some("multicast");
    }
    // fc00::/7 —— `Ipv6Addr::is_unique_local` 在 stable 尚未可用。
    if s[0] & 0xfe00 == 0xfc00 {
        return Some("unique local fc00::/7");
    }
    // fe80::/10
    if s[0] & 0xffc0 == 0xfe80 {
        return Some("link-local fe80::/10");
    }
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return Some("文件用保留位址 2001:db8::/32");
    }
    // 2002::/16（6to4）與 2001::/32（Teredo）都內嵌 v4 位址，而那個 v4
    // 可以是內網的。拆解它們的規則各不相同，因此一律擋掉 ——
    // 這兩種轉換機制早已淘汰，沒有正當的整合會用到。
    if s[0] == 0x2002 {
        return Some("6to4 2002::/16（內嵌 IPv4）");
    }
    if s[0] == 0x2001 && s[1] == 0x0000 {
        return Some("Teredo 2001::/32（內嵌 IPv4）");
    }
    if s[0] == 0x0100 && s[1..4] == [0, 0, 0] {
        return Some("discard-only 100::/64");
    }
    None
}

/// 解析並檢查一個目標網址。
///
/// 回傳的 [`Checked`] 帶著**已檢查的那個位址**，[`get_capped`] 會 pin 住它。
pub async fn resolve_and_check(raw: &str, s: &OutboundSettings) -> Result<Checked, Rejected> {
    let url = reqwest::Url::parse(raw).map_err(|_| Rejected::Unparsable(raw.to_string()))?;
    if url.scheme() != "https" {
        return Err(Rejected::NotHttps(url.scheme().to_string()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Rejected::HasCredentials);
    }
    let host = url.host_str().ok_or(Rejected::NoHost)?.to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let resolved = resolve_and_check_host(&host, port, s).await?;

    Ok(Checked {
        url,
        host: resolved.host,
        port: resolved.port,
        addr: resolved.addr,
        allowlisted: resolved.allowlisted,
    })
}

/// 檢查過的 `host:port`，沒有 URL。
///
/// LDAP 走裸 TCP，沒有網址；但 `ldap_host` 同樣是**呼叫端填的字串**，
/// 因此需要一樣的位址檢查 —— 少了它，「測試 LDAP 連線」就是一支
/// 指定主機與埠的連線測試工具，也就是一支埠掃描器。
#[derive(Debug, Clone)]
pub struct CheckedHost {
    pub host: String,
    pub port: u16,
    pub addr: SocketAddr,
    pub allowlisted: bool,
}

/// 解析並檢查一個 `host:port`。[`resolve_and_check`] 在剝掉 URL 之後就是它。
pub async fn resolve_and_check_host(
    host: &str,
    port: u16,
    s: &OutboundSettings,
) -> Result<CheckedHost, Rejected> {
    if host.is_empty() {
        return Err(Rejected::NoHost);
    }
    let allowlisted = s
        .private_target_allowlist
        .iter()
        .any(|entry| entry == &format!("{host}:{port}") || entry == host);

    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| Rejected::DnsFailed(e.to_string()))?
        .collect();
    let first = *addrs.first().ok_or(Rejected::NoAddress)?;

    if !allowlisted {
        // **檢查每一個位址，不是只檢查要用的那一個。** 一個同時有公開與
        // 內網 A 記錄的主機名，若只檢查第一個，重試或連線失敗的 fallback
        // 就可能連上內網那一個。全部都必須是公開位址。
        for a in &addrs {
            if let Some(kind) = private_address_kind(a.ip()) {
                return Err(Rejected::PrivateAddress { addr: a.ip(), kind });
            }
        }
    }

    Ok(CheckedHost {
        host: host.to_string(),
        port,
        addr: first,
        allowlisted,
    })
}

/// 對一個已檢查的 `host:port` 做 TCP 連線測試。回傳建立連線所花的毫秒數。
///
/// **只測得出「有東西在那個埠上聽」。** 它不做 TLS 交握，也不說話 ——
/// 因此不能用來斷言「LDAP 服務正常」，只能斷言「連得上」。
/// 呼叫端有責任把這個界線說清楚，見
/// `identity_providers::test_connection` 的 `checks_not_performed`。
pub async fn tcp_probe(checked: &CheckedHost, s: &OutboundSettings) -> Result<u128, String> {
    let started = std::time::Instant::now();
    let attempt = tokio::time::timeout(
        s.connect_timeout,
        tokio::net::TcpStream::connect(checked.addr),
    )
    .await;
    match attempt {
        Ok(Ok(stream)) => {
            drop(stream);
            Ok(started.elapsed().as_millis())
        }
        Ok(Err(e)) => Err(format!("TCP 連線失敗：{e}")),
        Err(_) => Err(format!(
            "TCP 連線在 {} 秒內未完成",
            s.connect_timeout.as_secs()
        )),
    }
}

/// 抓取一個已檢查的目標，回傳 `(狀態碼, 主體)`。主體最多讀
/// [`OutboundSettings::max_response_bytes`] 個位元組。
///
/// # 為什麼每次都建一個新的 Client
///
/// `reqwest::Client` 的 `resolve()` 是 **client 層級**的設定，而我們要 pin 的
/// 位址每個請求都不同。共用一個 client 就沒辦法 pin，也就失去第 3 道防線。
/// 代價是每次多一次 TLS 設定初始化 —— 對一個由管理者手動觸發的「測試連線」
/// 來說完全不重要。
pub async fn get_capped(
    checked: &Checked,
    s: &OutboundSettings,
) -> Result<(reqwest::StatusCode, Vec<u8>), String> {
    let client = reqwest::Client::builder()
        // 第 4 道防線。一個公開網址回 302 到 127.0.0.1 會讓前三道全部失效。
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(s.connect_timeout)
        .timeout(s.total_timeout)
        // 第 3 道防線：連線用剛剛檢查過的那個 IP，不再解析一次 DNS。
        .resolve(&checked.host, checked.addr)
        .build()
        .map_err(|e| format!("無法建立 HTTP 客戶端：{e}"))?;

    let res = client
        .get(checked.url.clone())
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| {
            // reqwest 的錯誤訊息含網址，而網址是呼叫端自己填的，回傳無妨。
            format!("請求失敗：{e}")
        })?;
    let status = res.status();

    let mut body = Vec::new();
    let mut stream = res;
    while let Some(chunk) = stream
        .chunk()
        .await
        .map_err(|e| format!("讀取回應失敗：{e}"))?
    {
        let room = s.max_response_bytes.saturating_sub(body.len());
        if room == 0 {
            // 截斷而不是報錯：已經讀到的部分足以判斷這個端點回的是什麼，
            // 而「回應過大」本身就是一個值得回報的事實。
            break;
        }
        let take = room.min(chunk.len());
        body.extend_from_slice(&chunk[..take]);
    }
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每一個範圍都要擋。這張表是這個模組的全部價值 ——
    /// 少一列就是一條可用的 SSRF 路徑，而症狀是「測試連線成功」。
    #[test]
    fn every_internal_range_is_rejected() {
        for (raw, expect_kind_contains) in [
            ("127.0.0.1", "loopback"),
            ("127.1.2.3", "loopback"),
            ("0.0.0.0", "未指定"),
            ("10.0.0.1", "1918"),
            ("172.16.0.1", "1918"),
            ("172.31.255.255", "1918"),
            ("192.168.1.1", "1918"),
            ("169.254.169.254", "link-local"),
            ("100.64.0.1", "CGNAT"),
            ("100.127.255.255", "CGNAT"),
            ("198.18.0.1", "benchmarking"),
            ("192.0.0.1", "192.0.0.0/24"),
            ("192.0.2.1", "文件"),
            ("240.0.0.1", "保留"),
            ("255.255.255.255", "廣播"),
            ("224.0.0.1", "multicast"),
            ("::1", "loopback"),
            ("::", "未指定"),
            ("fc00::1", "unique local"),
            ("fd12:3456::1", "unique local"),
            ("fe80::1", "link-local"),
            ("ff02::1", "multicast"),
            ("2001:db8::1", "文件"),
            ("2002::1", "6to4"),
            ("2001:0:1::1", "Teredo"),
            ("::ffff:127.0.0.1", "loopback"),
            ("::ffff:169.254.169.254", "link-local"),
            ("64:ff9b::7f00:1", "loopback"),
        ] {
            let addr: IpAddr = raw.parse().unwrap_or_else(|e| panic!("{raw}: {e}"));
            let kind = private_address_kind(addr)
                .unwrap_or_else(|| panic!("{raw} 沒有被擋下來 —— 這是一條 SSRF 路徑"));
            assert!(
                kind.contains(expect_kind_contains),
                "{raw} 被歸類成「{kind}」，預期含「{expect_kind_contains}」"
            );
        }
    }

    /// 公開位址必須放行。全部擋掉的實作也會讓上面那格通過。
    #[test]
    fn public_addresses_are_allowed() {
        for raw in [
            "1.1.1.1",
            "8.8.8.8",
            "20.190.190.1", // Entra ID 的網段之一
            "203.0.114.1",  // 緊貼文件用網段 203.0.113.0/24 的外側
            "2606:4700::1111",
            "2404:6800:4008::2004",
        ] {
            let addr: IpAddr = raw.parse().unwrap();
            assert_eq!(
                private_address_kind(addr),
                None,
                "{raw} 被誤擋成內部位址（{:?}）",
                private_address_kind(addr)
            );
        }
    }

    #[tokio::test]
    async fn scheme_and_credentials_are_checked_before_dns() {
        let s = OutboundSettings::default();
        // http 不接受。**注意主機名是不存在的** —— 若實作先解析 DNS 再檢查
        // scheme，這一格會變成 DnsFailed，也就是說錯誤訊息會誤導整合的人。
        assert_eq!(
            resolve_and_check("http://nonexistent.invalid/x", &s)
                .await
                .err(),
            Some(Rejected::NotHttps("http".into()))
        );
        assert!(matches!(
            resolve_and_check("file:///etc/passwd", &s).await,
            Err(Rejected::NotHttps(_))
        ));
        assert_eq!(
            resolve_and_check("https://user:pw@nonexistent.invalid/x", &s)
                .await
                .err(),
            Some(Rejected::HasCredentials)
        );
        assert!(matches!(
            resolve_and_check("not a url", &s).await,
            Err(Rejected::Unparsable(_))
        ));
    }

    #[tokio::test]
    async fn loopback_is_rejected_unless_allowlisted() {
        let mut s = OutboundSettings::default();
        assert!(matches!(
            resolve_and_check("https://127.0.0.1:9443/x", &s).await,
            Err(Rejected::PrivateAddress { .. })
        ));

        // 白名單要**同時**比對主機與埠。
        s.private_target_allowlist = vec!["127.0.0.1:9443".to_string()];
        let checked = resolve_and_check("https://127.0.0.1:9443/x", &s)
            .await
            .expect("白名單內的目標應該通過");
        assert!(
            checked.allowlisted,
            "通過了但沒有標記 allowlisted —— 呼叫端無法分辨這是特例"
        );

        // 同一個主機、不同的埠不在白名單裡。
        assert!(matches!(
            resolve_and_check("https://127.0.0.1:9444/x", &s).await,
            Err(Rejected::PrivateAddress { .. })
        ));
    }
}
