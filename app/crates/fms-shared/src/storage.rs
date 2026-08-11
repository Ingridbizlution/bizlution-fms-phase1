//! 物件儲存（S3／MinIO），WBS S5。
//!
//! # 為什麼 bucket 是私有的、下載一律預簽
//!
//! `docker-compose.yml` 的 `minio-init` 明確把三個 bucket 設為
//! `mc anonymous set none`，註解寫「附件一律走預簽網址」。這不是實作偏好
//! 而是既定的安全邊界：附件裡有設備照片、簽名、廠商報價，
//! 公開可讀的 bucket 等於把租戶隔離拆掉一半 ——
//! 資料庫再怎麼 RLS，物件儲存那一側照樣可以直接下載。
//!
//! # 上傳為什麼**不**用預簽（Phase 1 的取捨）
//!
//! 預簽 PUT 的優點是位元組不經過應用層，適合大檔。代價是資料列必須在
//! 物件存在之前先建立，於是多出一種半完成狀態（列有了、物件沒上傳），
//! 需要完成回呼或清掃工作。Phase 1 的附件是照片與說明書，
//! 直接經 API 上傳沒有半完成狀態、也自然套用既有的權限檢查。
//!
//! **BIM 模型（WBS 4.4／4.5，動輒數百 MB）應改用預簽 PUT**，
//! 那時再處理半完成狀態才划算。這個界線寫在這裡以免日後誤用。

use std::time::Duration;

use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;

use crate::problem::Problem;

/// 物件儲存設定。從環境變數讀取，與 `docker/.env.example` 的名稱一致。
#[derive(Debug, Clone)]
pub struct StorageSettings {
    pub endpoint: String,
    /// 預簽網址要交給**瀏覽器**直接連線，`endpoint` 卻是給伺服器自己用的
    /// （在 docker-compose 網路裡通常是 `http://minio:9000` 這種容器名稱，
    /// 瀏覽器解析不到）。這個欄位是可選的公開位址覆寫——設了就把
    /// `presign_get`／`presign_put` 回的網址前綴換成它，簽章本身不變
    /// （反向代理必須把 `Host` 標頭原樣轉成 `endpoint` 那個值，否則 MinIO
    /// 驗簽會失敗，見 `docker/README.md`）。沒設就維持原樣——本機開發時
    /// `endpoint` 常常對瀏覽器也是可解析的，不需要這層覆寫。
    pub public_endpoint: Option<String>,
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
    pub bucket_attachments: String,
    /// 預簽網址的有效期。契約寫「短期有效」，未給數字。
    ///
    /// 15 分鐘是刻意的折衷：夠久讓使用者點開圖片或下載說明書，
    /// 短到即使網址被轉貼到聊天室也很快失效。要更長就該改成
    /// 每次請求重新預簽，而不是延長有效期。
    pub download_ttl: Duration,
}

impl StorageSettings {
    /// 從環境變數建立。缺少必要變數時回錯 ——
    /// 附件功能沒有「安靜降級」的合理行為：silently 不上傳比失敗更糟。
    pub fn from_env() -> Result<Self, String> {
        fn var(key: &str) -> Result<String, String> {
            std::env::var(key).map_err(|_| format!("{key} is not set"))
        }
        Ok(Self {
            endpoint: var("S3_ENDPOINT")?,
            public_endpoint: std::env::var("S3_PUBLIC_ENDPOINT").ok(),
            access_key: var("S3_ACCESS_KEY")?,
            secret_key: var("S3_SECRET_KEY")?,
            region: std::env::var("S3_REGION").unwrap_or_else(|_| "us-east-1".to_string()),
            bucket_attachments: std::env::var("S3_BUCKET_ATTACHMENTS")
                .unwrap_or_else(|_| "fms".to_string()),
            download_ttl: Duration::from_secs(
                std::env::var("S3_DOWNLOAD_TTL_SECONDS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(900),
            ),
        })
    }
}

/// S3 客戶端與它的設定。
#[derive(Clone)]
pub struct Storage {
    client: Client,
    bucket: String,
    download_ttl: Duration,
    endpoint: String,
    public_endpoint: Option<String>,
}

impl Storage {
    /// 建立客戶端。
    ///
    /// `force_path_style(true)` 對 MinIO 是必要的：預設的 virtual-host 風格
    /// （`bucket.host`）需要 DNS 支援，本機 MinIO 沒有，
    /// 症狀是連線成功但每個請求都 404。
    ///
    /// 刻意不用 `aws-config` 的環境探測：那會嘗試 IMDS、設定檔、
    /// SSO 等一長串來源，在容器裡的失敗模式很難診斷。
    /// 這裡的憑證來源只有一個，明寫出來。
    pub fn new(settings: &StorageSettings) -> Self {
        // **crypto provider 要裝在用得到它的地方。**
        //
        // `observability.rs` 也裝了一次（OTLP 的 reqwest 需要它），但那是
        // `init_telemetry()` 裡面 —— 只有 `fms-server` 與 `fms-jobs` 的 main
        // 會呼叫。**整合測試從不呼叫它**（測試直接組 router），
        // 於是每個測試程序在建 S3 client 時都靠「碰巧沒事」。
        //
        // 完整套件出現過兩次同一個形狀的失敗：aws-smithy 的 hyper 連線器
        // 在建構時 panic，接著同一個 binary 裡所有用到它的格子被 LazyLock
        // 中毒一起拖垮（7 格裡只有 1 格顯示真正的成因）。
        //
        // **那兩次沒有重現，所以「provider 沒裝」是假說而不是證據。**
        // 仍然改：不論那個抖動是不是這個原因，「依賴一個只有別的模組會裝的
        // process 級狀態」本身就是缺口 —— 任何不呼叫 init_telemetry 就建
        // Storage 的程式（測試、未來的 CLI、批次工具）都在賭。
        //
        // `install_default` 在已經有 provider 時回 Err，那不是錯誤，忽略它。
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let credentials = aws_credential_types::Credentials::new(
            settings.access_key.clone(),
            settings.secret_key.clone(),
            None,
            None,
            "fms-static",
        );
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(settings.region.clone()))
            .endpoint_url(settings.endpoint.clone())
            .credentials_provider(credentials)
            .force_path_style(true)
            .build();

        Self {
            client: Client::from_conf(config),
            bucket: settings.bucket_attachments.clone(),
            download_ttl: settings.download_ttl,
            endpoint: settings.endpoint.clone(),
            public_endpoint: settings.public_endpoint.clone(),
        }
    }

    fn to_public_url(&self, url: String) -> String {
        rewrite_public_url(url, &self.endpoint, &self.public_endpoint)
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// 上傳物件。
    pub async fn put(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<(), Problem> {
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body));
        if let Some(ct) = content_type {
            req = req.content_type(ct);
        }
        req.send().await.map_err(|e| {
            // 物件儲存壞掉是基礎設施問題，不是客戶端的錯 → 500。
            // 但錯誤內容要進 log，否則「上傳失敗」在生產完全無法診斷。
            Problem::internal(std::io::Error::other(format!("s3 put failed: {e}")))
        })?;
        Ok(())
    }

    /// 產生短期有效的下載網址（契約的 `Attachment.download_url`）。
    ///
    /// `response_content_disposition` 帶上原始檔名：物件鍵是 uuid 形式，
    /// 不設這個標頭使用者下載到的檔案會叫 `9f3c...`，沒有副檔名也打不開。
    pub async fn presign_get(&self, key: &str, file_name: &str) -> Result<String, Problem> {
        let config = PresigningConfig::expires_in(self.download_ttl).map_err(|e| {
            Problem::internal(std::io::Error::other(format!(
                "invalid presigning config: {e}"
            )))
        })?;

        let presigned = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .response_content_disposition(content_disposition(file_name))
            .presigned(config)
            .await
            .map_err(|e| {
                Problem::internal(std::io::Error::other(format!("s3 presign failed: {e}")))
            })?;

        Ok(self.to_public_url(presigned.uri().to_string()))
    }

    /// 產生短期有效的**上傳**網址，供客戶端直傳儲存體。
    ///
    /// # 為什麼需要它，而附件不需要
    ///
    /// `attachments::create` 是把 bytes 收進 API 再 `put()` —— 對照片與
    /// PDF（幾 MB）那樣做沒問題，而且省掉一次往返。
    ///
    /// **BIM 模型不是那個量級。** 一個 IFC 動輒數百 MB，把它塞過 API
    /// 伺服器會佔住一條連線好幾分鐘、吃掉記憶體、而且逾時的失敗模式很難查
    /// （客戶端看到的是連線中斷，不是「檔案太大」）。
    /// 契約因此把 BIM 的上傳設計成「先取預簽網址直傳、再註冊」。
    ///
    /// # 為什麼帶 `content_type`
    ///
    /// 預簽的 PUT 若在簽章時指定了 `Content-Type`，客戶端就**必須**送出
    /// 相同的值，否則 S3 拒絕。這是刻意的：少了它，一個把 IFC 標成
    /// `text/html` 的上傳會被瀏覽器當網頁開啟 —— 而那個物件是預簽可讀的。
    pub async fn presign_put(&self, key: &str, content_type: &str) -> Result<String, Problem> {
        let config = PresigningConfig::expires_in(self.download_ttl).map_err(|e| {
            Problem::internal(std::io::Error::other(format!(
                "invalid presigning config: {e}"
            )))
        })?;

        let presigned = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type)
            .presigned(config)
            .await
            .map_err(|e| {
                Problem::internal(std::io::Error::other(format!("s3 presign failed: {e}")))
            })?;

        Ok(self.to_public_url(presigned.uri().to_string()))
    }

    /// 刪除物件。附件的刪除是軟刪除（資料列留著供稽核），
    /// 但物件要真的清掉 —— 留著會持續計費，而且軟刪除的意義是
    /// 「這筆紀錄還在」不是「這個檔案還能下載」。
    pub async fn delete(&self, key: &str) -> Result<(), Problem> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                Problem::internal(std::io::Error::other(format!("s3 delete failed: {e}")))
            })?;
        Ok(())
    }
}

/// 把預簽網址的 `scheme://host[:port]` 前綴換成公開位址。
///
/// 簽章覆蓋的是 path 跟 query，前綴替換不會讓簽章失效，但反向代理轉發時
/// 必須把 `Host` 標頭原樣設回 `endpoint`（不是它自己收到的那個公開主機
/// 名），否則 MinIO 用實際收到的 `Host` 重算簽章會跟 query 裡的
/// `X-Amz-Signature` 對不上——見 `docker/README.md` 的反向代理設定範例。
fn rewrite_public_url(url: String, endpoint: &str, public_endpoint: &Option<String>) -> String {
    match public_endpoint {
        Some(public) if public != endpoint => match url.strip_prefix(endpoint) {
            Some(rest) => format!("{}{rest}", public.trim_end_matches('/')),
            None => url,
        },
        _ => url,
    }
}

/// 依 RFC 6266 組出 `Content-Disposition`。
///
/// # 為什麼不能只寫 `filename="檔名"`
///
/// `filename=` 的值限定 ISO-8859-1。直接放 UTF-8 的中文檔名是**不合法的
/// 標頭**，而 MinIO 的處理方式是整個丟掉 —— 症狀是使用者下載到一個叫
/// 物件鍵那串 uuid 的檔案，而且沒有任何錯誤。對繁體中文的部署而言
/// 這不是邊緣案例，是常態。
///
/// 正確做法是同時給兩者：`filename=` 放 ASCII 退化版供舊客戶端，
/// `filename*=UTF-8''<百分比編碼>` 放真正的檔名，現代瀏覽器優先取後者。
fn content_disposition(file_name: &str) -> String {
    // ASCII 退化版：非 ASCII 與會提前結束標頭值的字元都換成底線。
    let fallback: String = file_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();

    // RFC 5987 的 attr-char 之外一律百分比編碼。
    let mut encoded = String::with_capacity(file_name.len() * 3);
    for b in file_name.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric()
            || matches!(
                c,
                '!' | '#' | '$' | '&' | '+' | '-' | '.' | '^' | '_' | '`' | '|' | '~'
            )
        {
            encoded.push(c);
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{b:02X}");
        }
    }

    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

/// 物件鍵的組成。
///
/// `tenant/entity_type/entity_id/uuid_檔名` —— 前綴帶租戶是刻意的：
/// 日後要做 bucket 政策、生命週期規則、或按租戶計費，都以前綴為單位。
/// 檔名保留在鍵尾只為了人在 MinIO console 裡看得懂，
/// 不作為識別 —— 識別靠中間的 uuid。
pub fn object_key(
    tenant_id: uuid::Uuid,
    entity_type: &str,
    entity_id: uuid::Uuid,
    file_name: &str,
) -> String {
    // 鍵裡的斜線會被當成路徑分隔，`..` 會讓人以為能跳出前綴。
    // 兩者都要在組鍵時剔除，而不是信任呼叫端已經清理過。
    let safe: String = file_name
        .chars()
        .map(|c| match c {
            '/' | '\\' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    format!(
        "{tenant_id}/{}/{entity_id}/{}_{}",
        entity_type.to_lowercase(),
        uuid::Uuid::new_v4(),
        safe.replace("..", "_")
    )
}

#[cfg(test)]
mod tests {
    use super::rewrite_public_url;

    #[test]
    fn no_public_endpoint_leaves_url_unchanged() {
        let url = "http://minio:9000/fms/key?X-Amz-Signature=abc".to_string();
        assert_eq!(
            rewrite_public_url(url.clone(), "http://minio:9000", &None),
            url
        );
    }

    #[test]
    fn public_endpoint_replaces_only_the_prefix() {
        let url = "http://minio:9000/fms/key?X-Amz-Signature=abc".to_string();
        let rewritten = rewrite_public_url(
            url,
            "http://minio:9000",
            &Some("https://demo.fms.bizlution.ai/storage".to_string()),
        );
        assert_eq!(
            rewritten,
            "https://demo.fms.bizlution.ai/storage/fms/key?X-Amz-Signature=abc"
        );
    }

    #[test]
    fn public_endpoint_equal_to_internal_endpoint_is_a_no_op() {
        let url = "http://minio:9000/fms/key?X-Amz-Signature=abc".to_string();
        let rewritten = rewrite_public_url(
            url.clone(),
            "http://minio:9000",
            &Some("http://minio:9000".to_string()),
        );
        assert_eq!(rewritten, url);
    }

    #[test]
    fn trailing_slash_on_public_endpoint_does_not_double_up() {
        let url = "http://minio:9000/fms/key".to_string();
        let rewritten = rewrite_public_url(
            url,
            "http://minio:9000",
            &Some("https://demo.fms.bizlution.ai/storage/".to_string()),
        );
        assert_eq!(rewritten, "https://demo.fms.bizlution.ai/storage/fms/key");
    }
}
