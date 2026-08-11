//! Microsoft Graph 的 `CalendarProvider` 實作。
//!
//! # 認證：app-only client-credentials，不是每租戶各存一份密鑰（ADR-14 決定 G）
//!
//! 我們註冊**一個**多租戶 Azure AD 應用，客戶的 M365 系統管理員做一次 admin
//! consent（授予 application permission `Calendars.ReadWrite`／`Place.Read.All`），
//! 之後用 client-credentials flow（我們的 `app_client_id`／`app_client_secret`
//! 全平台共用一份 + 客戶的 `ms_tenant_id`）換 token。因此**這裡沒有 per-tenant
//! 密鑰要存**——`app_client_secret` 由呼叫端從環境變數讀出後傳進
//! `MicrosoftGraphProvider::new`，不是這個型別自己去解析。
//!
//! # 時區：強制要求 Graph 回 UTC，不自己處理時區轉換
//!
//! Graph 預設用信箱自己的時區回傳 `start.dateTime`／`end.dateTime`（一個不帶
//! 偏移量的 naive 字串 + 獨立的 `timeZone` 欄位）。每個請求都帶
//! `Prefer: outlook.timezone="UTC"`——這是 Graph 文件記載的標準做法，讓所有
//! 回傳的時間一律是 UTC，不需要在這裡實作一份時區資料庫轉換邏輯。
//!
//! # 底下 URL 可以換掉，是為了測試
//!
//! `token_endpoint_base`／`graph_api_base` 預設指向真正的 Microsoft 端點，
//! 但建構時可以換成本地測試伺服器的位址（見 `tests/`）——這個 crate 沒有
//! 用 wiremock 之類的套件，跟 `fms-server` 既有測試（`idp_test_connection_slice.rs`）
//! 同一個做法：起一個真的 `axum` 伺服器綁在 `127.0.0.1:0`。

use chrono::{DateTime, Duration, Utc};
use fms_shared::Problem;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::provider::{
    CalendarProvider, DeltaPage, ExternalEvent, ExternalResource, OutboundEvent,
};

/// 打在事件上的自訂屬性名稱，用來偵測「這是我們自己推出去的事件」
/// （ADR-14 決定 I 的防迴圈標記）。前綴是 Graph 擴充屬性慣用的
/// `String {GUID} Name X` 格式；GUID 是本專案固定值，不是隨機產生——
/// 必須每次都一樣，拉入時才比對得到。
const FMS_TAG_PROPERTY_ID: &str =
    "String {c2a35d3a-6b9f-4b1a-9e2b-3f6a2b1d8f10} Name FmsReservationId";

/// 快取的 access token，含到期時間。
struct CachedToken {
    access_token: String,
    expires_at: DateTime<Utc>,
}

pub struct MicrosoftGraphProvider {
    http: reqwest::Client,
    ms_tenant_id: String,
    app_client_id: String,
    app_client_secret: String,
    token_endpoint_base: String,
    graph_api_base: String,
    token: RwLock<Option<CachedToken>>,
}

impl MicrosoftGraphProvider {
    /// `app_client_secret` 由呼叫端解析（環境變數，見檔頭）——這個型別
    /// 只負責拿它去換 token，不負責存放或解析密鑰參照。
    pub fn new(ms_tenant_id: String, app_client_id: String, app_client_secret: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            ms_tenant_id,
            app_client_id,
            app_client_secret,
            token_endpoint_base: "https://login.microsoftonline.com".to_string(),
            graph_api_base: "https://graph.microsoft.com/v1.0".to_string(),
            token: RwLock::new(None),
        }
    }

    /// 測試專用：把端點換成本地的假伺服器。
    #[doc(hidden)]
    pub fn with_bases(mut self, token_endpoint_base: String, graph_api_base: String) -> Self {
        self.token_endpoint_base = token_endpoint_base;
        self.graph_api_base = graph_api_base;
        self
    }

    /// 拿一個有效的 access token，過期前 60 秒就視為過期並重新換——
    /// 避免「token 在請求送出的路上剛好過期」這種競態。
    async fn access_token(&self) -> Result<String, Problem> {
        {
            let cached = self.token.read().await;
            if let Some(t) = cached.as_ref() {
                if t.expires_at > Utc::now() + Duration::seconds(60) {
                    return Ok(t.access_token.clone());
                }
            }
        }

        let url = format!(
            "{}/{}/oauth2/v2.0/token",
            self.token_endpoint_base, self.ms_tenant_id
        );
        let resp = self
            .http
            .post(&url)
            .form(&[
                ("client_id", self.app_client_id.as_str()),
                ("client_secret", self.app_client_secret.as_str()),
                ("scope", "https://graph.microsoft.com/.default"),
                ("grant_type", "client_credentials"),
            ])
            .send()
            .await
            .map_err(Problem::internal)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Problem::internal(std::io::Error::other(format!(
                "Microsoft token endpoint 回 {status}：{body}"
            ))));
        }

        let parsed: TokenResponse = resp.json().await.map_err(Problem::internal)?;
        let expires_at = Utc::now() + Duration::seconds(parsed.expires_in as i64);
        let access_token = parsed.access_token.clone();
        *self.token.write().await = Some(CachedToken {
            access_token: parsed.access_token,
            expires_at,
        });
        Ok(access_token)
    }

    fn authed_request(
        &self,
        method: reqwest::Method,
        url: &str,
        token: &str,
    ) -> reqwest::RequestBuilder {
        self.http
            .request(method, url)
            .bearer_auth(token)
            // 見檔頭：強制要求 UTC，不然要自己解 timeZone 欄位。
            .header("Prefer", r#"outlook.timezone="UTC""#)
    }

    async fn ensure_ok(resp: reqwest::Response, what: &str) -> Result<reqwest::Response, Problem> {
        if resp.status().is_success() {
            Ok(resp)
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(Problem::internal(std::io::Error::other(format!(
                "Microsoft Graph {what} 回 {status}：{body}"
            ))))
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct RoomListResponse {
    value: Vec<GraphRoom>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
struct GraphRoom {
    #[serde(rename = "emailAddress")]
    email_address: String,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct DeltaResponse {
    value: Vec<GraphEvent>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    delta_link: Option<String>,
}

#[derive(Deserialize)]
struct GraphEvent {
    id: Option<String>,
    subject: Option<String>,
    start: Option<GraphDateTimeTz>,
    end: Option<GraphDateTimeTz>,
    #[serde(rename = "singleValueExtendedProperties")]
    extended_properties: Option<Vec<GraphExtendedProperty>>,
    #[serde(rename = "@removed")]
    removed: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct GraphDateTimeTz {
    #[serde(rename = "dateTime")]
    date_time: String,
}

#[derive(Deserialize)]
struct GraphExtendedProperty {
    id: String,
    value: String,
}

#[derive(Serialize)]
struct CreateEventBody {
    subject: String,
    start: EventDateTime,
    end: EventDateTime,
    #[serde(rename = "singleValueExtendedProperties")]
    extended_properties: Vec<ExtendedPropertyIn>,
}

#[derive(Serialize)]
struct EventDateTime {
    #[serde(rename = "dateTime")]
    date_time: String,
    #[serde(rename = "timeZone")]
    time_zone: &'static str,
}

#[derive(Serialize)]
struct ExtendedPropertyIn {
    id: &'static str,
    value: String,
}

#[derive(Deserialize)]
struct CreateEventResponse {
    id: String,
}

/// Graph 回的 `dateTime` 沒有時區偏移量的字尾（`Prefer: outlook.timezone="UTC"`
/// 讓數值本身是 UTC，但字串格式仍然是 naive），因此手動補 `Z` 再解析。
fn parse_graph_datetime(raw: &str) -> DateTime<Utc> {
    let with_zone = if raw.ends_with('Z') {
        raw.to_string()
    } else {
        format!("{raw}Z")
    };
    // 解析失敗時退回 Unix epoch 而不是 panic——外部 API 的回應不受我們控制，
    // 一個格式異常的事件不該讓整個 delta 頁面的處理連帶失敗。呼叫端看到
    // 明顯錯誤的時間可以自行判斷要不要進 calendar_sync_conflicts。
    with_zone
        .parse::<DateTime<Utc>>()
        .unwrap_or_else(|_| DateTime::from_timestamp(0, 0).unwrap())
}

fn extract_fms_reservation_id(props: &Option<Vec<GraphExtendedProperty>>) -> Option<Uuid> {
    props
        .as_ref()?
        .iter()
        .find(|p| p.id == FMS_TAG_PROPERTY_ID)
        .and_then(|p| Uuid::parse_str(&p.value).ok())
}

#[async_trait::async_trait]
impl CalendarProvider for MicrosoftGraphProvider {
    async fn list_resources(&self) -> Result<Vec<ExternalResource>, Problem> {
        let token = self.access_token().await?;
        let mut resources = Vec::new();
        let mut url = format!("{}/places/microsoft.graph.room", self.graph_api_base);

        loop {
            let resp = self
                .authed_request(reqwest::Method::GET, &url, &token)
                .send()
                .await
                .map_err(Problem::internal)?;
            let resp = Self::ensure_ok(resp, "列出房間資源").await?;
            let page: RoomListResponse = resp.json().await.map_err(Problem::internal)?;

            resources.extend(page.value.into_iter().map(|r| ExternalResource {
                external_id: r.email_address,
                display_name: r.display_name.unwrap_or_default(),
            }));

            match page.next_link {
                Some(next) => url = next,
                None => break,
            }
        }

        Ok(resources)
    }

    async fn fetch_events_delta(
        &self,
        external_resource_id: &str,
        since: Option<&str>,
    ) -> Result<DeltaPage, Problem> {
        let token = self.access_token().await?;

        // 有游標就直接沿用（Graph 的 deltaLink 是完整 URL，已經帶著
        // 上一輪的 $expand／filter 狀態），沒有就做一次初始查詢——
        // 拉過去 7 天到未來 90 天的窗口，不做無界限的全量。
        let mut url = match since {
            Some(link) => link.to_string(),
            None => {
                let start = (Utc::now() - Duration::days(7)).to_rfc3339();
                let end = (Utc::now() + Duration::days(90)).to_rfc3339();
                format!(
                    "{}/users/{}/calendarView/delta?startDateTime={}&endDateTime={}&$expand=singleValueExtendedProperties($filter=id eq '{}')",
                    self.graph_api_base,
                    external_resource_id,
                    urlencoding_minimal(&start),
                    urlencoding_minimal(&end),
                    urlencoding_minimal(FMS_TAG_PROPERTY_ID),
                )
            }
        };

        let mut events = Vec::new();
        let delta_token = loop {
            let resp = self
                .authed_request(reqwest::Method::GET, &url, &token)
                .send()
                .await
                .map_err(Problem::internal)?;
            let resp = Self::ensure_ok(resp, "增量拉取事件").await?;
            let page: DeltaResponse = resp.json().await.map_err(Problem::internal)?;

            for e in page.value {
                let is_deleted = e.removed.is_some();
                let external_event_id = match e.id {
                    Some(id) => id,
                    // 沒有 id 的事件不該發生，但發生時跳過它比讓整輪同步失敗好——
                    // 呼叫端看不到這個事件，下一輪 delta 若它還在會再拉到。
                    None => continue,
                };
                events.push(ExternalEvent {
                    external_event_id,
                    subject: e.subject,
                    start_at: e
                        .start
                        .map(|s| parse_graph_datetime(&s.date_time))
                        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap()),
                    end_at: e
                        .end
                        .map(|s| parse_graph_datetime(&s.date_time))
                        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap()),
                    is_deleted,
                    fms_reservation_id: extract_fms_reservation_id(&e.extended_properties),
                });
            }

            match (page.next_link, page.delta_link) {
                (Some(next), _) => url = next,
                (None, Some(delta)) => break delta,
                (None, None) => return Err(Problem::internal(std::io::Error::other(
                    "Microsoft Graph 的 delta 回應既沒有 @odata.nextLink 也沒有 @odata.deltaLink",
                ))),
            }
        };

        Ok(DeltaPage {
            events,
            next_delta_token: delta_token,
        })
    }

    async fn create_event(
        &self,
        external_resource_id: &str,
        event: &OutboundEvent,
    ) -> Result<String, Problem> {
        let token = self.access_token().await?;
        let url = format!(
            "{}/users/{}/events",
            self.graph_api_base, external_resource_id
        );
        let body = CreateEventBody {
            subject: event.subject.clone(),
            start: EventDateTime {
                date_time: event.start_at.to_rfc3339(),
                time_zone: "UTC",
            },
            end: EventDateTime {
                date_time: event.end_at.to_rfc3339(),
                time_zone: "UTC",
            },
            extended_properties: vec![ExtendedPropertyIn {
                id: FMS_TAG_PROPERTY_ID,
                value: event.reservation_id.to_string(),
            }],
        };

        let resp = self
            .authed_request(reqwest::Method::POST, &url, &token)
            .json(&body)
            .send()
            .await
            .map_err(Problem::internal)?;
        let resp = Self::ensure_ok(resp, "建立事件").await?;
        let created: CreateEventResponse = resp.json().await.map_err(Problem::internal)?;
        Ok(created.id)
    }

    async fn cancel_event(
        &self,
        external_resource_id: &str,
        external_event_id: &str,
    ) -> Result<(), Problem> {
        let token = self.access_token().await?;
        let url = format!(
            "{}/users/{}/events/{}",
            self.graph_api_base, external_resource_id, external_event_id
        );
        let resp = self
            .authed_request(reqwest::Method::DELETE, &url, &token)
            .send()
            .await
            .map_err(Problem::internal)?;
        Self::ensure_ok(resp, "取消事件").await?;
        Ok(())
    }
}

/// 最小的查詢字串編碼——只處理這裡實際會出現的字元（RFC3339 時間戳記與
/// 我們自己固定的屬性字串），不是一個通用的 URL 編碼實作。真的需要通用
/// URL 編碼時應該加 `urlencoding` crate，這裡的輸入完全可控，沒有必要。
fn urlencoding_minimal(s: &str) -> String {
    s.replace(' ', "%20")
        .replace('\'', "%27")
        .replace(':', "%3A")
        .replace('{', "%7B")
        .replace('}', "%7D")
        .replace('+', "%2B")
}
