//! 上游 HTTP 客户端：chat 流式请求 / token 刷新 / 模型配置 / 积分查询 / 每日签到。
//!
//! 对应 Go 源文件 `internal/upstream/client.go` 后半（`Client` 及其方法）
//! 与 `internal/upstream/idle.go`（空闲监控，以「读块套 timeout」等价实现）。

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::auth::Auth;

use super::headers::{apply_billing, apply_refresh, origin_referer_for};
use super::payload::prepare_body_for_region;
use super::{classify, truncate, ApiEnvelope, ClientError, ErrKind, UpstreamError};

const DEFAULT_CHAT_BASE_CN: &str = "https://copilot.tencent.com";
const DEFAULT_BILLING_BASE_CN: &str = "https://www.codebuddy.cn";
const DEFAULT_BASE_INTL: &str = "https://www.workbuddy.ai";

/// 聊天/通用 HTTP 总时长上限。
const HTTP_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);

/// 上游 HTTP 客户端。
///
/// 与 Go 侧一致：`http`（短 RPC，120s 总时长）与 `chat`（SSE 专用，无总时长，
/// 首字节由 `header_timeout` 约束、流中空闲由 `idle_timeout` 约束）。
pub struct Client {
    http: reqwest::Client,
    chat_http: reqwest::Client,

    /// 聊天 SSE 首字节前（响应头）超时；零值表示未设置（不限）。
    pub header_timeout: Duration,
    /// 聊天 SSE 流中空闲超时；零值表示禁用空闲监控。
    pub idle_timeout: Duration,

    /// 各模型 supportedEfforts 缓存（[`Client::fetch_models`] 刷新），供请求体 effort 降级。
    efforts: RwLock<HashMap<String, Vec<String>>>,

    /// 出站请求体黑名单指纹脱敏开关（默认 true；false 完全还原）。
    pub sanitize_fingerprints: bool,

    pub chat_base_cn: String,
    pub billing_base_cn: String,
    /// 国际版基址。与国服不同，国际版所有端点（chat / billing /
    /// 签到 / 旅行 / token 刷新）都在同一域名下，因此只需一个 base。
    pub base_intl: String,

    /// 当前生效的显式代理（空串 = 未设置，回落环境变量）。
    /// 国际版（workbuddy.ai）在国内直连不稳定，通常需要它。
    proxy_url: String,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// 生产默认值。
    pub fn new() -> Self {
        Self::with_timeouts(Duration::ZERO, Duration::ZERO)
    }

    /// 带超时配置的构造（header_timeout / idle_timeout 见字段说明）。
    pub fn with_timeouts(header_timeout: Duration, idle_timeout: Duration) -> Self {
        let (http, chat_http) = build_clients("").expect("构建上游 HTTP 客户端失败");
        Self {
            http,
            chat_http,
            header_timeout,
            idle_timeout,
            efforts: RwLock::new(HashMap::new()),
            sanitize_fingerprints: true,
            chat_base_cn: DEFAULT_CHAT_BASE_CN.to_string(),
            billing_base_cn: DEFAULT_BILLING_BASE_CN.to_string(),
            base_intl: DEFAULT_BASE_INTL.to_string(),
            proxy_url: String::new(),
        }
    }

    /// 设置出站代理（空串 = 不使用显式代理，回落环境变量）。
    ///
    /// 与 Go 侧 `SetProxy` 一致：重建两个共享同一代理配置的 client（连接池不重复复用，
    /// 由旧 client 自行回收既有连接）；新请求立即走新代理。启动时调用一次即可。
    ///
    /// 容忍用户只填 host:port（如 127.0.0.1:7890）：自动补 http:// 前缀。
    pub fn set_proxy(&mut self, raw: &str) -> Result<(), String> {
        let raw = raw.trim().to_string();
        let (http, chat_http) = build_clients(&raw)?;
        self.http = http;
        self.chat_http = chat_http;
        self.proxy_url = raw;
        Ok(())
    }

    /// 当前生效的显式代理（空串 = 未设置）。
    pub fn proxy_url(&self) -> &str {
        &self.proxy_url
    }

    /// 返回该账号的 chat 基址（按区域路由）。
    pub fn chat_base(&self, a: &Auth) -> String {
        if a.is_intl() {
            if !self.base_intl.is_empty() {
                return self.base_intl.clone();
            }
            return DEFAULT_BASE_INTL.to_string();
        }
        self.chat_base_cn.clone()
    }

    /// 返回该账号的 billing 基址（按区域路由）。
    ///
    /// 国服 billing 与 chat 分属不同域名；国际版两者同域。
    pub fn billing_base(&self, a: &Auth) -> String {
        if a.is_intl() {
            if !self.base_intl.is_empty() {
                return self.base_intl.clone();
            }
            return DEFAULT_BASE_INTL.to_string();
        }
        self.billing_base_cn.clone()
    }

    /// 组装出站请求体（脱敏开关由 `sanitize_fingerprints` 控制）。
    ///
    /// intl 为该账号是否国际版（workbuddy.ai）：国际版要求 messages 首条必须是
    /// system（实测首条 user → HTTP 400 code=11128），需要在此补一条。
    fn prepare_body(&self, body: &[u8], intl: bool) -> Vec<u8> {
        let efforts = self.efforts_snapshot();
        prepare_body_for_region(body, self.sanitize_fingerprints, efforts.as_ref(), intl)
    }

    /// 返回 effort 能力缓存副本；None 表示未知（透传不降级）。
    fn efforts_snapshot(&self) -> Option<HashMap<String, Vec<String>>> {
        let guard = self.efforts.read().ok?;
        if guard.is_empty() {
            return None;
        }
        Some(guard.clone())
    }

    /// 发请求并解信封；HTTP 非 2xx 或业务 code != 0 时返回分类后的 [`ClientError::Upstream`]。
    pub(crate) async fn do_json(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<Value, ClientError> {
        let resp = req.send().await.map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let raw = read_body_limited(resp, 1 << 20).await?;
        if status >= 400 {
            let kind = classify(status, &raw);
            return Err(ClientError::Upstream(UpstreamError {
                kind,
                status,
                msg: truncate(&raw, 200),
            }));
        }
        let env: ApiEnvelope = serde_json::from_str(&raw).map_err(|e| {
            ClientError::Parse(format!("parse failed: {e} (body: {})", truncate(&raw, 120)))
        })?;
        if env.code != 0 {
            let mut kind = classify(status, &env.msg);
            if kind == ErrKind::None {
                kind = ErrKind::Client;
            }
            return Err(ClientError::Upstream(UpstreamError {
                kind,
                status,
                msg: format!("code={} msg={}", env.code, truncate(&env.msg, 160)),
            }));
        }
        Ok(env.data)
    }

    /// 刷新 access token；成功时更新 `a` 的字段（缺省值保留旧值），
    /// 调用方负责原子写回。并发写回的加锁由调用方（账号池）负责。
    pub async fn refresh_token(&self, a: &mut Auth) -> Result<(), ClientError> {
        if a.refresh_token.trim().is_empty() {
            return Err(ClientError::Parse("no refreshToken".to_string()));
        }
        let url = format!("{}/v2/plugin/auth/token/refresh", self.chat_base(a));
        let req = apply_refresh(self.http.post(&url), a);
        let data = self.do_json(req).await?;
        let tok: RefreshResp = serde_json::from_value(data).unwrap_or_default();
        if tok.access_token.is_empty() {
            return Err(ClientError::Parse(
                "refresh_failed: no accessToken in response — re-login required".to_string(),
            ));
        }
        a.access_token = tok.access_token;
        if !tok.refresh_token.is_empty() {
            a.refresh_token = tok.refresh_token;
        }
        if !tok.domain.is_empty() {
            a.domain = tok.domain;
        }
        // preserveExpiry：响应缺 expiresIn 时保留旧过期时间，避免刷新风暴。
        if tok.expires_in > 0 {
            a.expires_at =
                chrono::Utc::now().timestamp() + tok.expires_in;
        }
        Ok(())
    }

    /// 发 chat 请求并返回 SSE 响应流。
    ///
    /// 非 2xx 时返回 [`ChatOutcome::Error`]（body 供调用方 `classify(status, body)`）；
    /// 只有传输层失败才返回 `Err`。流中空闲监控参数取自
    /// `header_timeout`（首字节）与 `idle_timeout`（流中）。
    pub async fn chat_stream(&self, a: &Auth, body: &[u8]) -> Result<ChatOutcome, ClientError> {
        let url = format!("{}/v2/chat/completions", self.chat_base(a));
        let prepared = self.prepare_body(body, a.is_intl());
        let req = super::headers::apply_chat(
            self.chat_http.post(&url).body(prepared),
            a,
        );
        // 首字节阶段由 header_timeout 管（对应 Go Transport.ResponseHeaderTimeout）；
        // 零值表示未设置，不限。
        let send = req.send();
        let resp = if self.header_timeout.is_zero() {
            send.await.map_err(|e| ClientError::Transport(e.to_string()))?
        } else {
            match tokio::time::timeout(self.header_timeout, send).await {
                Ok(r) => r.map_err(|e| ClientError::Transport(e.to_string()))?,
                Err(_) => {
                    return Err(ClientError::Transport(format!(
                        "response header timeout after {:?}",
                        self.header_timeout
                    )))
                }
            }
        };
        let status = resp.status().as_u16();
        if status >= 400 {
            let raw = read_body_limited(resp, 1 << 20).await?;
            let kind = classify(status, &raw);
            tracing::info!(uid = %a.uid, status, kind = %kind, body = %truncate(&raw, 200), "chat_stream");
            return Ok(ChatOutcome::Error { status, body: raw.into_bytes() });
        }
        Ok(ChatOutcome::Stream { status, resp })
    }

    /// 调上游模型配置接口，返回该账号所在区域的可用模型。
    ///
    /// 数据来源是 `data.agents[name=="cli"].models`（**不是** `data.models`）：
    /// 后者是产品全部模型池，含图片/视频生成、lite 辅助模型等；
    /// 前者是 CLI agent 真正可用的子集（正是客户端选模型时看到的）。
    /// 元数据（contextWindow/maxOutputTokens/efforts）从 `data.models` 按 id 关联补齐。
    pub async fn fetch_models(&self, a: &Auth) -> Result<Vec<ModelInfo>, ClientError> {
        let url = format!("{}{MODELS_CONFIG_PATH}", self.chat_base(a));
        let origin = origin_referer_for(a);
        let req = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", a.access_token))
            .header("Accept", "application/json")
            .header("Origin", origin)
            .header("Referer", format!("{origin}/"))
            .header("User-Agent", MODELS_CONFIG_UA);
        let resp = req.send().await.map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let raw = read_body_limited(resp, 1 << 20).await?;
        if status != 200 {
            return Err(ClientError::Parse(format!(
                "models api status {status}: {}",
                truncate(&raw, 120)
            )));
        }
        let env: ModelsEnv = serde_json::from_str(&raw)
            .map_err(|e| ClientError::Parse(format!("models parse: {e}")))?;
        if env.code != 0 {
            return Err(ClientError::Parse(format!("models api code={}", env.code)));
        }

        // 先按 id 建索引，便于给 cli 清单补元数据。
        // disabled 单独记一份：agents[cli].models 是「这个 agent 允许用哪些模型」的白名单，
        // 而 disabled 是模型级的停用开关，被停用的模型可以仍留在白名单里。
        // 两者都不看会把已停用的模型下发给客户端（选中即报错）。
        let mut meta: HashMap<String, ModelInfo> = HashMap::with_capacity(env.data.models.len());
        let mut disabled: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(env.data.models.len());
        for m in &env.data.models {
            if m.id.is_empty() || meta.contains_key(&m.id) {
                continue;
            }
            meta.insert(
                m.id.clone(),
                ModelInfo {
                    id: m.id.clone(),
                    name: m.name.clone(),
                    context_window: m.max_input_tokens,
                    max_tokens: m.max_output_tokens,
                    efforts: m.reasoning.supported_efforts.clone(),
                    default_effort: m.reasoning.effort.clone(),
                    // 账号级多模态开关为 true 时强制降级为 false：上游语义是
                    // 「即便模型本身支持，该账号也不许用图片」，此时不能宣称支持。
                    supports_images: effective_supports_images(m.supports_images, m.disabled_multimodal),
                },
            );
            if m.disabled {
                disabled.insert(m.id.clone());
            }
        }

        // 取 cli agent 的可用清单（这是客户端真正能选的模型）。
        let cli_models = env
            .data
            .agents
            .iter()
            .find(|ag| ag.name == "cli")
            .map(|ag| ag.models.clone())
            .unwrap_or_default();

        let mut out: Vec<ModelInfo> = Vec::with_capacity(cli_models.len());
        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(cli_models.len());
        for id in &cli_models {
            if id.is_empty() || seen.contains(id) {
                continue;
            }
            if let Some(mi) = meta.get(id) {
                // 上游显式标了 disabled 的不下发（与旧实现一致）。
                // 池里查不到该 id 时无从判断，按「宁可多」返回。
                if disabled.contains(id) {
                    continue;
                }
                seen.insert(id.clone());
                out.push(mi.clone());
                continue;
            }
            // cli 清单里有、models 池里没有：仍要返回（它确实可用），只是元数据未知。
            seen.insert(id.clone());
            out.push(ModelInfo { id: id.clone(), ..Default::default() });
        }

        // 兜底：上游没给 cli agent 时退回全量池（宁可多不可少，保持旧行为）。
        if out.is_empty() {
            for m in &env.data.models {
                if m.disabled || m.id.is_empty() || seen.contains(&m.id) {
                    continue;
                }
                seen.insert(m.id.clone());
                out.push(meta.get(&m.id).cloned().unwrap_or_default());
            }
        }
        if out.is_empty() {
            return Err(ClientError::Parse("models api returned empty list".to_string()));
        }
        // 刷新 effort 能力缓存（供请求体降级；无 supportedEfforts 的模型不入缓存）。
        let cache: HashMap<String, Vec<String>> = out
            .iter()
            .filter(|mi| !mi.efforts.is_empty())
            .map(|mi| (mi.id.clone(), mi.efforts.clone()))
            .collect();
        if let Ok(mut guard) = self.efforts.write() {
            *guard = cache;
        }
        Ok(out)
    }

    /// 查询账号当前可花费积分余额（所有套餐 CycleCapacity 聚合，负值钳 0）。
    pub async fn user_resource(&self, a: &Auth) -> Result<i64, ClientError> {
        Ok(self.user_resource_detail(a).await?.remain)
    }

    /// 查询积分余额与「最近到期」时刻（一次请求同时取回，不额外打上游）。
    pub async fn user_resource_detail(&self, a: &Auth) -> Result<CreditInfo, ClientError> {
        let url = format!("{}/v2/billing/meter/get-user-resource", self.billing_base(a));
        let now = chrono::Local::now();
        let fmt = "%Y-%m-%d %H:%M:%S";
        let body = serde_json::json!({
            "PageNumber": 1,
            "PageSize": 100,
            "ProductCode": "p_tcaca",
            "Status": [0, 3],
            "PackageEndTimeRangeBegin": now.format(fmt).to_string(),
            "PackageEndTimeRangeEnd": (now + chrono::Duration::hours(365 * 101 * 24)).format(fmt).to_string(),
        });
        let req = apply_billing(self.http.post(&url).json(&body), a);
        let data = self.do_json(req).await?;
        let resp: ResourceResp = if data.is_null() {
            Default::default()
        } else {
            serde_json::from_value(data)
                .map_err(|e| ClientError::Parse(format!("resource parse: {e}")))?
        };
        let mut info = CreditInfo::default();
        for acct in resp.response.data.accounts {
            let r = acct.remain();
            info.remain += r;
            // 只有「还有剩余」的套餐才代表真实到期压力；已用尽的套餐到期日再早也无意义。
            if r <= 0 {
                continue;
            }
            let at = acct.expiry_unix();
            if at > 0 && (info.soonest_expire_at == 0 || at < info.soonest_expire_at) {
                info.soonest_expire_at = at;
            }
        }
        Ok(info)
    }

    /// 执行每日签到。已签到（业务 code 非 0）也返回错误，调用方按 msg 区分。
    pub async fn daily_checkin(&self, a: &Auth) -> Result<(), ClientError> {
        let url = format!("{}/v2/billing/meter/daily-checkin", self.billing_base(a));
        let req = apply_billing(self.http.post(&url).body("{}"), a);
        self.do_json(req).await?;
        Ok(())
    }

    /// 读一次 SSE 响应块（带空闲监控）。供 sse 模块与转发层共用。
    pub(crate) async fn read_chunk(
        &self,
        resp: &mut reqwest::Response,
        idle: Duration,
    ) -> Result<Option<bytes::Bytes>, ClientError> {
        let fut = resp.chunk();
        if idle.is_zero() {
            fut.await.map_err(|e| ClientError::Transport(e.to_string()))
        } else {
            match tokio::time::timeout(idle, fut).await {
                Ok(r) => r.map_err(|e| ClientError::Transport(e.to_string())),
                Err(_) => Err(ClientError::IdleTimeout(idle)),
            }
        }
    }
}

/// [`Client::chat_stream`] 的结果。
pub enum ChatOutcome {
    /// 2xx：SSE 响应流（空闲监控由调用方按 `Client::idle_timeout` 施加）。
    Stream {
        status: u16,
        resp: reqwest::Response,
    },
    /// 非 2xx：已读回的响应体，供调用方 `classify(status, body)`。
    Error {
        status: u16,
        body: Vec<u8>,
    },
}

/// 读响应体，上限 `limit` 字节（对应 Go io.LimitReader）。
pub(crate) async fn read_body_limited(
    mut resp: reqwest::Response,
    limit: usize,
) -> Result<String, ClientError> {
    let mut raw: Vec<u8> = Vec::new();
    while raw.len() < limit {
        match resp.chunk().await.map_err(|e| ClientError::Transport(e.to_string()))? {
            Some(chunk) => {
                let remain = limit - raw.len();
                if chunk.len() > remain {
                    raw.extend_from_slice(&chunk[..remain]);
                    break;
                }
                raw.extend_from_slice(&chunk);
            }
            None => break,
        }
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// 构建共用代理配置的一对 client（http / chat）。
fn build_clients(proxy_raw: &str) -> Result<(reqwest::Client, reqwest::Client), String> {
    let mut builder_http = reqwest::Client::builder().timeout(HTTP_TOTAL_TIMEOUT);
    let mut builder_chat = reqwest::Client::builder();
    let proxy = build_proxy(proxy_raw)?;
    if let Some(p) = proxy {
        builder_http = builder_http.proxy(p.clone());
        builder_chat = builder_chat.proxy(p);
    }
    let http = builder_http.build().map_err(|e| format!("代理地址无效: {e}"))?;
    let chat = builder_chat.build().map_err(|e| format!("代理地址无效: {e}"))?;
    Ok((http, chat))
}

/// 构造显式代理；空串返回 None（回落 reqwest 默认的环境变量代理探测，
/// 与 Go http.ProxyFromEnvironment 行为一致）。
fn build_proxy(raw: &str) -> Result<Option<reqwest::Proxy>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let mut s = raw.to_string();
    // 容忍用户只填 host:port（如 127.0.0.1:7890）：补 http:// 前缀。
    if !s.contains("://") {
        s = format!("http://{s}");
    }
    let u = url::Url::parse(&s).map_err(|e| format!("代理地址无效: {e}"))?;
    // 注意 url::Url::parse 对 "http://:8080" 不报错（host 为空），
    // 必须用 host_str 判空，否则会把一个连不上主机的地址当成合法配置。
    if u.host_str().unwrap_or("").is_empty() {
        return Err(format!("代理地址缺少主机名: {s}"));
    }
    let p = reqwest::Proxy::all(&s).map_err(|e| format!("代理地址无效: {e}"))?;
    Ok(Some(p))
}

/// 动态模型信息（含 maxInputTokens/maxOutputTokens）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    /// = maxInputTokens
    pub context_window: i64,
    /// = maxOutputTokens
    pub max_tokens: i64,
    /// reasoning.supportedEfforts（空=未知/固定档）
    pub efforts: Vec<String>,
    /// 上游给的默认思考档 = reasoning.effort（空=未声明）。
    ///
    /// 与 efforts 分开：efforts 是「允许哪些档」，default_effort 是「不指定时用哪档」。
    /// 上游同时给了两者，但默认档未必在 supportedEfforts 里（上游数据未保证），
    /// 因此不要用它去推断 efforts，也不要用 efforts[0] 去冒充它。
    pub default_effort: String,
    /// 是否接受图片输入。
    ///
    /// None **不等于** false：上游 /v3/config 只给对话模型写 supportsImages，
    /// 补全/图片生成等条目整条缺失该字段。缺失时是「未声明」，不是「不支持」——
    /// 谎报成纯文本会让客户端把本可用的图片能力关掉，所以这里保留三态。
    pub supports_images: Option<bool>,
}

/// 拉模型配置用的 User-Agent。
///
/// **必须**用 WorkBuddy 前缀（实测 2026-09-15）：
/// - WorkBuddy/... → 200，返回 WorkBuddy 产品的模型清单
/// - CLI/...       → 200，但返回的是 **CodeBuddy** 产品的清单（另一套模型）
/// - 其他任意 UA    → 400
///
/// 两个清单差异很大且各自都「看起来合理」，很容易误判成上游数据错误。
/// 实测 gpt-6-astra / deepseek-v4.1-flash / kimi-k2.8-preview 在国际版
/// **均可正常调用**，说明 WorkBuddy 前缀拿到的才是本产品真实可用集。
///
/// 另：UA 只影响本接口的返回内容，不影响 /v2/chat/completions（实测两者 chat 结果一致），
/// 因此这里单独覆盖，不动全局 clientUA。
const MODELS_CONFIG_UA: &str = "WorkBuddy/5.5.2 WorkBuddy/5.5.2 CLI/2.137.1";

/// 模型配置接口路径。
///
/// 为什么不用 /console/enterprises/personal/models：
/// 该接口在国际版恒返回 500（openresty 错误页，实测 5/5 账号，
/// 与认证方式/请求头无关），导致国际版永远只能靠硬编码静态表。
/// /v3/config 返回同一份模型数据且两个区域都可用（实测国际版 21、国服 52）。
const MODELS_CONFIG_PATH: &str = "/v3/config";

/// 合并「模型是否支持图片」与「账号级多模态是否被禁用」。
///
/// 三态语义（返回值可能是 None = 未声明）：
/// - supportsImages 缺失 + 未禁用 → None（未声明，客户端按自己的默认处理）
/// - supportsImages=true        → Some(true)
/// - supportsImages=false       → Some(false)
/// - disabledMultimodal=true    → Some(false)（无条件，账号级开关优先）
///
/// disabledMultimodal 优先是刻意的：上游用它表达「该账号不能发图片」，
/// 与模型自身能力无关，此时宣称支持会让客户端发出必然失败的请求。
fn effective_supports_images(supports_images: Option<bool>, disabled_multimodal: bool) -> Option<bool> {
    if disabled_multimodal {
        Some(false)
    } else {
        supports_images
    }
}

#[derive(Debug, Default, Deserialize)]
struct RefreshResp {
    #[serde(default, rename = "accessToken")]
    access_token: String,
    #[serde(default, rename = "refreshToken")]
    refresh_token: String,
    #[serde(default, rename = "expiresIn")]
    expires_in: i64,
    #[serde(default)]
    domain: String,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsEnv {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: ModelsData,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsData {
    #[serde(default, rename = "models")]
    models: Vec<ModelsModel>,
    #[serde(default, rename = "agents")]
    agents: Vec<ModelsAgent>,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsModel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "maxInputTokens")]
    max_input_tokens: i64,
    #[serde(default, rename = "maxOutputTokens")]
    max_output_tokens: i64,
    #[serde(default)]
    disabled: bool,
    /// 指针：区分「显式 false」与「字段缺失」（见 [`ModelInfo::supports_images`]）。
    #[serde(default, rename = "supportsImages")]
    supports_images: Option<bool>,
    /// 账号级多模态开关。实测当前恒为 false/缺失，
    /// 但一旦为 true，即便 supportsImages=true 也不能收图片。
    #[serde(default, rename = "disabledMultimodal")]
    disabled_multimodal: bool,
    #[serde(default)]
    reasoning: ModelsReasoning,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsReasoning {
    #[serde(default)]
    effort: String,
    #[serde(default, rename = "supportedEfforts")]
    supported_efforts: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ModelsAgent {
    #[serde(default)]
    name: String,
    #[serde(default)]
    models: Vec<String>,
}

/// billing get-user-resource 返回的单个套餐。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct ResourcePackage {
    #[serde(rename = "PackageName")]
    package_name: String,
    #[serde(rename = "CapacitySize")]
    capacity_size: i64,
    #[serde(rename = "CapacityRemain")]
    capacity_remain: i64,
    #[serde(rename = "CapacityUsed")]
    capacity_used: i64,
    #[serde(rename = "CycleCapacitySize")]
    cycle_capacity_size: i64,
    #[serde(rename = "CycleCapacityRemain")]
    cycle_capacity_remain: i64,
    #[serde(rename = "CycleCapacityUsed")]
    cycle_capacity_used: i64,
    /// 到期字段（实测国服与国际版均返回）：
    /// - DeductionEndTime 抵扣截止（epoch 毫秒）—— 额度真正失效的时刻，优先采用
    /// - ExpiredTime / CycleEndTime 兼容回退（实测可能是 "2006-01-02 15:04:05" 字符串）
    ///
    /// 声明为 Value 是因为同一字段在不同区域/套餐上出现过数字与字符串两种形态。
    #[serde(rename = "DeductionEndTime")]
    deduction_end_time: Value,
    #[serde(rename = "ExpiredTime")]
    expired_time: Value,
    #[serde(rename = "CycleEndTime")]
    cycle_end_time: Value,
}

impl ResourcePackage {
    /// 该套餐可花费积分（与原聚合口径逐字一致：Cycle* 优先，负值钳 0）。
    fn remain(&self) -> i64 {
        let r = if self.cycle_capacity_size > 0 {
            self.cycle_capacity_remain
        } else if self.cycle_capacity_remain > 0 || self.cycle_capacity_used > 0 {
            self.cycle_capacity_remain
        } else {
            self.capacity_remain
        };
        r.max(0)
    }

    /// 该套餐的到期时刻（Unix 秒）；0 = 未知。
    fn expiry_unix(&self) -> i64 {
        for v in [&self.deduction_end_time, &self.expired_time, &self.cycle_end_time] {
            let at = parse_expiry_unix(v);
            if at > 0 {
                return at;
            }
        }
        0
    }
}

/// 把秒/毫秒 epoch 统一成秒（上游混用两种精度）。
fn normalize_epoch(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    if n > 1_000_000_000_000 {
        return n / 1000;
    }
    n
}

/// 解析上游到期字段，兼容 epoch 秒/毫秒与常见日期字符串。
fn parse_expiry_unix(v: &Value) -> i64 {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                normalize_epoch(i)
            } else if let Some(f) = n.as_f64() {
                normalize_epoch(f as i64)
            } else {
                0
            }
        }
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return 0;
            }
            if let Ok(n) = s.parse::<i64>() {
                return normalize_epoch(n);
            }
            for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M:%S%.f"] {
                if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
                    if let Ok(local) = naive.and_local_timezone(chrono::Local) {
                        if let Some(ts) = local.single() {
                            return ts.timestamp();
                        }
                    }
                }
            }
            // RFC3339（带时区偏移）
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(s) {
                return ts.timestamp();
            }
            // 仅日期：按当日 23:59:59 计（额度一般用到当天结束）。
            if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                let naive = d.and_hms_opt(23, 59, 59).unwrap();
                if let Ok(local) = naive.and_local_timezone(chrono::Local) {
                    if let Some(ts) = local.single() {
                        return ts.timestamp();
                    }
                }
            }
            0
        }
        _ => 0,
    }
}

/// 账号积分余额与到期信息（billing get-user-resource 的归一化结果）。
#[derive(Debug, Clone, Copy, Default)]
pub struct CreditInfo {
    /// 所有套餐可花费积分之和（负值钳 0）。
    pub remain: i64,
    /// 仍有剩余积分的套餐中最早的到期时刻（Unix 秒）；0 = 未知。
    ///
    /// 供账号池做「按到期紧迫度分层」选号：先烧快过期的额度，避免积分作废。
    pub soonest_expire_at: i64,
}

#[derive(Debug, Default, Deserialize)]
struct ResourceResp {
    #[serde(default)]
    response: ResourceResponse,
}

#[derive(Debug, Default, Deserialize)]
struct ResourceResponse {
    #[serde(default)]
    data: ResourceData,
}

#[derive(Debug, Default, Deserialize)]
struct ResourceData {
    #[serde(default, rename = "Accounts")]
    accounts: Vec<ResourcePackage>,
}
