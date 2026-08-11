//! 密鑰參照解析器。
//!
//! `identity_providers` 有三個 `*_ref` 欄位（`client_secret_ref`、
//! `ldap_bind_secret_ref`、`metadata_xml_ref`）。契約對它們的說明是
//! 「密鑰管理服務中的參照名稱」—— 也就是說**資料庫裡存的是名字，不是密鑰**。
//! 這是對的設計（見 058 檔頭），但在這個模組之前它沒有另一半：
//! 沒有任何程式碼能把那個名字換成值，因此那三欄是純粹的裝飾。
//!
//! # 這個模組不會讓 SSO 登入變成可用
//!
//! 說清楚，因為容易被誤讀：`/auth/sso/{code}/callback` 仍然回 501。
//! token 交換還缺 **id_token 的 JWKS 簽章驗證**，而那需要一個真實 IdP 才能
//! 驗得出對錯。有了密鑰只是把兩個缺口減成一個。
//!
//! 它現在唯一的消費者是 `test-connection` 的 `secret_reference_resolvable`
//! 那一格。那不是一個小地方：**「我在 IdP 設了 `client_secret_ref=OKTA_PROD`，
//! 但部署忘了提供對應的環境變數」是一個真實且常見的組態錯誤**，而在此之前
//! 它完全不可觀察 —— 要等到 Phase 2 有人試著登入才會炸。
//!
//! # 為什麼是環境變數而不是 Vault／KMS 客戶端
//!
//! ADR-13 決策 A。理由是接口先於實作：`SecretResolver` 這個 trait 讓
//! 呼叫端寫成「我需要這個參照的值」，而不是「我要去 Vault 拿」。之後換成
//! Vault／AWS Secrets Manager／Azure Key Vault 是新增一個 impl，
//! 呼叫端一行都不用改。反過來先接一個 KMS 客戶端就是替一個還沒有客戶要求的
//! 部署形態付前置成本。
//!
//! 環境變數同時是 12-factor 的預設做法，而這套系統已經用它傳資料庫密碼、
//! JWT 簽章金鑰、S3 憑證 —— IdP 密鑰不需要一套更特別的機制。
//!
//! # 密鑰不進 `Settings`
//!
//! 解析發生在**用的那一刻**，不是啟動時載入。`Settings` 會被 clone 進每一個
//! handler 的 state，而且我們對它有 `Debug`。啟動時把密鑰讀進去就等於讓它有
//! 機會出現在任何一行 `tracing::debug!` 裡。

use std::collections::HashMap;
use std::fmt;

/// 環境變數的固定前綴。
///
/// 有前綴才能讓「哪些環境變數是密鑰」在部署腳本裡一眼看得出來，
/// 也讓 `client_secret_ref = "PATH"` 這種手滑不會意外讀到系統的 `PATH`。
const ENV_PREFIX: &str = "IDP_SECRET_";

/// 一個解析出來的密鑰值。
///
/// 沒有 `Serialize`、`Display`、`Clone`；`Debug` 只印長度。這些都是刻意的 ——
/// 這個型別存在的理由就是讓「不小心把密鑰印出去」需要明確寫出
/// `.expose()` 才做得到。
pub struct Secret(String);

impl Secret {
    /// 取出明文。呼叫點應該就是真的要把它送出去的那一行。
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 連長度都是資訊，但印出「有沒有設」是診斷必需；長度洩漏的程度
        // 遠低於它在 log 裡的價值。
        write!(f, "Secret(<{} bytes redacted>)", self.0.len())
    }
}

/// 解析失敗的兩種形態。**兩者都不含密鑰值**，可以直接放進 API 回應。
#[derive(Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// 參照合法，但部署沒有提供。
    ///
    /// `expected` 是它去找的環境變數名 —— 這一欄是整個模組最有用的東西：
    /// 錯誤訊息直接告訴維運要設什麼，不必去讀命名規則的文件。
    NotConfigured { expected: String },
    /// 參照名稱本身不能用。
    InvalidReference(&'static str),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured { expected } => {
                write!(f, "這個部署沒有提供對應的密鑰。請設定環境變數 {expected}")
            }
            Self::InvalidReference(why) => write!(f, "參照名稱不合法：{why}"),
        }
    }
}

/// 把一個密鑰參照換成值。
pub trait SecretResolver: Send + Sync {
    fn resolve(&self, reference: &str) -> Result<Secret, ResolveError>;
}

/// 參照名稱 → 環境變數名。
///
/// 規則：去掉頭尾空白、非 ASCII 英數字一律換成 `_`、全部大寫、加上前綴。
/// 這讓 `okta/prod/client-secret` 變成 `IDP_SECRET_OKTA_PROD_CLIENT_SECRET`。
///
/// # 這個轉換會碰撞，而那是可接受的
///
/// `a/b` 與 `a-b` 映到同一個名字。不做去重或雜湊來迴避：**維運需要看著
/// 環境變數名就知道它對應哪個參照**，而 `IDP_SECRET_A_B_7f3c9e` 這種名字
/// 會讓那件事變成不可能。碰撞的代價是兩個 provider 共用一個密鑰（會在
/// `test-connection` 顯示為兩個都 PASSED），而預期的環境變數名有回傳給
/// 呼叫端，因此看得出來。
fn env_var_name(reference: &str) -> Result<String, ResolveError> {
    let trimmed = reference.trim();
    if trimmed.is_empty() {
        return Err(ResolveError::InvalidReference("空字串"));
    }
    let mapped: String = trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    // 全部都被換成底線代表這個參照沒有任何可辨識的內容（例如 `///`）。
    // 那不是「沒設定」，是設定本身有問題 —— 要分開報，否則維運會去設一個
    // 叫 `IDP_SECRET____` 的環境變數。
    if !mapped.chars().any(|c| c != '_') {
        return Err(ResolveError::InvalidReference(
            "轉換後只剩底線，沒有任何英數字",
        ));
    }
    Ok(format!("{ENV_PREFIX}{mapped}"))
}

/// 查到的原始值 → `Secret`，**空字串視為沒設定**。
///
/// 空的密鑰送出去只會得到對方的 401，而症狀出現在遠端 —— 在這裡就擋掉，
/// 錯誤訊息還能說出該設哪個環境變數。`FOO=` 在 shell 腳本裡是很容易寫出來的
/// 東西（例如 `IDP_SECRET_X=$UNSET_VAR`）。
///
/// 抽成函式而不是寫在 `EnvSecretResolver` 裡面，是為了讓兩個實作共用同一條
/// 規則：測試無法安全地改行程的環境變數，若這條規則只存在於讀環境變數的那一行，
/// 它就永遠沒有測試 —— 而「空字串被當成有效密鑰」是一個沉默的錯誤。
fn from_raw(name: String, raw: Option<&str>) -> Result<Secret, ResolveError> {
    match raw {
        Some(v) if !v.is_empty() => Ok(Secret(v.to_string())),
        _ => Err(ResolveError::NotConfigured { expected: name }),
    }
}

/// 從環境變數解析。這是正式部署用的實作。
pub struct EnvSecretResolver;

impl SecretResolver for EnvSecretResolver {
    fn resolve(&self, reference: &str) -> Result<Secret, ResolveError> {
        let name = env_var_name(reference)?;
        let raw = std::env::var(&name).ok();
        from_raw(name, raw.as_deref())
    }
}

/// 測試用的固定對照表。
///
/// 存在的理由：`EnvSecretResolver` 的 PASSED 分支沒有這個就無法測 ——
/// 測試裡改行程的環境變數在 Rust 2024 是 `unsafe`，而且跨測試執行緒有競態。
pub struct StaticSecretResolver(HashMap<String, String>);

impl StaticSecretResolver {
    pub fn new(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        Self(entries.into_iter().collect())
    }
}

impl SecretResolver for StaticSecretResolver {
    fn resolve(&self, reference: &str) -> Result<Secret, ResolveError> {
        // 一樣走 env_var_name 與 from_raw，這樣「參照不合法」與「空值」的行為
        // 兩個實作完全一致 —— 若測試用的實作比較寬鬆，測試就會通過而正式環境炸掉。
        let name = env_var_name(reference)?;
        from_raw(name, self.0.get(reference).map(String::as_str))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_maps_to_a_prefixed_upper_snake_env_var() {
        assert_eq!(
            env_var_name("okta/prod/client-secret").unwrap(),
            "IDP_SECRET_OKTA_PROD_CLIENT_SECRET"
        );
        assert_eq!(env_var_name("  entra  ").unwrap(), "IDP_SECRET_ENTRA");
    }

    /// 前綴是防護，不只是慣例：沒有它，`client_secret_ref = "PATH"`
    /// 會讓伺服器把系統的 `PATH` 當成密鑰送給 IdP。
    #[test]
    fn a_reference_named_like_a_system_variable_does_not_read_it() {
        let name = env_var_name("PATH").unwrap();
        assert_eq!(name, "IDP_SECRET_PATH");
        assert_ne!(name, "PATH");
    }

    #[test]
    fn empty_and_symbol_only_references_are_rejected_separately_from_not_configured() {
        assert_eq!(
            env_var_name("   "),
            Err(ResolveError::InvalidReference("空字串"))
        );
        assert_eq!(
            env_var_name("///"),
            Err(ResolveError::InvalidReference(
                "轉換後只剩底線，沒有任何英數字"
            ))
        );
    }

    /// 「沒設定」必須帶著該設什麼。這是這個錯誤存在的全部價值。
    #[test]
    fn not_configured_names_the_env_var_to_set() {
        let err = EnvSecretResolver.resolve("okta-prod").unwrap_err();
        assert_eq!(
            err,
            ResolveError::NotConfigured {
                expected: "IDP_SECRET_OKTA_PROD".to_string()
            }
        );
        assert!(err.to_string().contains("IDP_SECRET_OKTA_PROD"));
    }

    #[test]
    fn static_resolver_returns_the_value_and_reports_misses_with_the_env_var_name() {
        let r = StaticSecretResolver::new([("okta-prod".to_string(), "s3cr3t".to_string())]);
        assert_eq!(r.resolve("okta-prod").unwrap().expose(), "s3cr3t");
        assert_eq!(
            r.resolve("azure").unwrap_err(),
            ResolveError::NotConfigured {
                expected: "IDP_SECRET_AZURE".to_string()
            }
        );
        // 不合法的參照在兩個實作裡是同一種錯，不是 NotConfigured。
        assert_eq!(
            r.resolve("").unwrap_err(),
            ResolveError::InvalidReference("空字串")
        );
    }

    /// **設成空字串等於沒設定。**
    ///
    /// `IDP_SECRET_X=` 在 shell 腳本裡很容易寫出來（例如引用了一個未設定的
    /// 變數）。若空字串被當成有效密鑰，伺服器會拿一個空字串去跟 IdP 換 token，
    /// 得到一個遠端的 401 —— 症狀離原因很遠。這裡回 `NotConfigured`，
    /// 訊息還能說出該設哪個環境變數。
    #[test]
    fn an_empty_value_counts_as_not_configured() {
        let r = StaticSecretResolver::new([("blank".to_string(), String::new())]);
        assert_eq!(
            r.resolve("blank").unwrap_err(),
            ResolveError::NotConfigured {
                expected: "IDP_SECRET_BLANK".to_string()
            }
        );
    }

    /// `Debug` 不能洩漏值 —— 這個型別存在的理由就是這件事。
    #[test]
    fn debug_does_not_print_the_secret() {
        let s = Secret("hunter2".to_string());
        let printed = format!("{s:?}");
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("redacted"), "{printed}");
    }
}
