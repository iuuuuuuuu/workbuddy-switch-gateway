//! 上游访问层（Rust 版），对应 Go 源文件 `internal/upstream/*.go`。
//!
//! # 模块与 Go 侧对应关系
//!
//! | 本模块       | Go 源文件        | 内容 |
//! |--------------|------------------|------|
//! | `mod.rs`     | `client.go` 前半 | 错误分类（`ErrKind`/`Classify`）、重置时间解析、友好文案 |
//! | `headers.rs` | `headers.go`     | common / chat / billing / refresh 四类请求头 |
//! | `payload.rs` | `payload.go`     | 出站请求体改写（stream 强制、tool_choice/roles/effort 归一化） |
//! | `sanitize.rs`| `sanitize.go`    | 出站内容指纹脱敏 |
//! | `sse.rs`     | `sse.go`         | SSE 聚合 / 逐帧规范化透传 |
//! | `client.rs`  | `client.go` 后半 + `idle.go` | HTTP 客户端（chat 流式 / token 刷新 / 模型配置 / 积分 / 签到） |
//! | `travel.rs`  | `travel.go`      | growth 域「猫猫旅行」接口 |
//!
//! # 分类优先级（必须与 Go 逐条一致）
//!
//! 1. `402` → 硬冷却（余额）
//! 2. body 命中嵌套业务码 14018（额度耗尽，`error.data.code`）→ 硬冷却
//! 3. body 命中 hardMarkers（小写 + 中文原文双通道）→ 硬冷却
//! 4. body 命中 sessionDeadMarkers → 禁用
//! 5. `429` → 模型级限流（业务码 6004 / 文案）优先，否则账号级软冷却
//! 6. body 命中上下文超长（业务码 11115 / extError / 文案）→ 请求侧错误
//! 7. `404` → 短冷却
//! 8. `>=500` → 服务端错误（喂熔断）
//! 9. `>=400` → 客户端错误（只换号不罚）
//! 10. 其余 → 无错误
//!
//! 注意第 2/3 步在状态码判断**之前**：HTTP 200 但业务 code 非 0 且含余额关键词
//! 的情况也要判成硬冷却；第 6 步刻意排在 429 之后、通用 4xx 之前——上下文超长
//! 是请求侧错误，换号无用，需要独立 kind 让调用方「立即失败」而不是轮转重传。

use serde::Deserialize;

/// 错误分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrKind {
    /// 成功。
    #[default]
    None,
    /// 余额不足（402 或 body 关键词）→ 长冷却。
    HardCredit,
    /// 429 软限流 → 短冷却。
    SoftRate,
    /// session 失效（401 + 12153）→ 禁用。
    SessionDead,
    /// 404 上游偶发 → 短冷却，防雪崩。
    NotFound,
    /// 5xx 上游故障。
    Server,
    /// 其他 4xx / 业务错误。
    Client,
    /// 模型级限流：该账号的**这个模型**额度用尽（429 code=6004）。
    ///
    /// 与整个账号被限速的 [`ErrKind::SoftRate`] 不同——上游明确提示
    /// 「您也可以切换其他模型继续使用」，即该账号的其他模型仍然可用。
    /// 冷却时长取上游给出的重置时间（解析失败回退软冷却）。
    ModelRate,
    /// 请求的上下文超出模型窗口（HTTP 400 code=11115）。
    ///
    /// 这是**请求侧**错误，与账号无关：同一个请求体发给任何账号都会同样失败。
    /// 因此必须与 [`ErrKind::Client`] 区分开——否则会落入「换号重试」路径，
    /// 把整个请求体对着每个账号重传一遍，最后还被包装成 no_healthy_account，
    /// 把排查方向引向「账号故障」。
    ContextTooLong,
}

impl ErrKind {
    /// 序列化名，与 Go 侧 `String()` 一致。
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrKind::None => "none",
            ErrKind::HardCredit => "hard_credit",
            ErrKind::SoftRate => "soft_rate",
            ErrKind::SessionDead => "session_dead",
            ErrKind::NotFound => "not_found",
            ErrKind::Server => "server",
            ErrKind::Client => "client",
            ErrKind::ModelRate => "model_rate",
            ErrKind::ContextTooLong => "context_too_long",
        }
    }
}

impl std::fmt::Display for ErrKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 带分类的上游错误。
///
/// `Display` 与 Go 侧 `(*Error).Error()` 同格式：
/// `upstream {kind} (http {status}): {msg}`
#[derive(Debug, Clone)]
pub struct UpstreamError {
    /// 错误分类。
    pub kind: ErrKind,
    /// HTTP 状态码。
    pub status: u16,
    /// 原始响应体。
    pub msg: String,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "upstream {} (http {}): {}", self.kind, self.status, self.msg)
    }
}

impl std::error::Error for UpstreamError {}

/// 客户端层错误：区分传输层失败、解析失败与已分类的上游错误。
///
/// Go 侧传输层失败以普通 error 返回（不参与分类）；此处用类型显式区分，
/// 供转发循环决定「换号重试」还是「立即失败」。
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// 传输层失败（连接、发送、读流）。
    #[error("transport error: {0}")]
    Transport(String),
    /// SSE 流中空闲超过阈值，主动断流（释放租约）。
    #[error("stream idle timeout after {0:?}")]
    IdleTimeout(std::time::Duration),
    /// 响应体 / 信封解析失败。
    #[error("{0}")]
    Parse(String),
    /// 上游返回了已分类的错误响应。
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
}

impl ClientError {
    /// 若是已分类的上游错误则返回其引用。
    pub fn as_upstream(&self) -> Option<&UpstreamError> {
        match self {
            ClientError::Upstream(e) => Some(e),
            _ => None,
        }
    }
}

/// 余额不足关键词（小写比较 + 中文原文比较双通道）。
///
/// 收单复数两种写法：上游国际版（workbuddy.ai）实际返回的是
/// "Credits exhausted. Please visit the link below to purchase add-on packs
/// and get more credits: …"（**复数** Credits），只登记单数会导致
/// strings.Contains 恒不命中 → 被判成软冷却（60 秒）→ 无限重试。
const HARD_MARKERS: &[&str] = &[
    "insufficient credit",
    "insufficient credits",
    "no credit",
    "no credits",
    "credit exhausted",
    "credits exhausted",
    "credit exhaustion",
    "credits exhaustion",
    "out of credit",
    "out of credits",
    "quota exceeded",
    "quota exhaust",
    "payment required",
    "credit not enough",
    "credits not enough",
    "not enough credit",
    "not enough credits",
    "credit used up",
    "credits used up",
    "积分不足",
    "额度不足",
    "余额不足",
    "积分用完",
    "额度用尽",
    "没有积分",
];

/// session 失效标记。
const SESSION_DEAD_MARKERS: &[&str] = &["Offline user session not found", "12153"];

/// 模型级限流的判定依据（429 + 其中之一）。
///
/// 主力信号是上游业务码 6004；文案关键词作为兜底——上游改码不改文案时仍能识别，
/// 但**必须**同时是 429，避免把其他场景的「频率限制」字样误判成模型限流。
const MODEL_RATE_MARKERS: &[&str] = &["超出频率限制", "切换其他模型"];

/// 上游「模型级限流」的业务码。
const MODEL_RATE_CODE: i64 = 6004;

/// 上游「上下文超长」的业务码。
///
/// 实测响应（2026-09-15 现场，prompt 1121509 > 上限 1048576）：
/// `400 {"code":11115,"msg":"prompt is too long: 1119655 tokens > 1048576 maximum",
///      "extError":{"code":"context_length_exceeded",...},
///      "displayMsg":{"en":"The request exceeds the model context limit...",
///                    "zh":"对话内容超出模型长度上限，请精简对话或减少附件后重试。"}}`
const CONTEXT_TOO_LONG_CODE: i64 = 11115;

/// 上下文超长的判定文案（中英双通道兜底）。
///
/// 措辞取自上游真实响应，刻意含中英两版 displayMsg——上游按 Accept-Language
/// 切换语言，只认一种会漏判。msg 有多种写法：实测同一业务码下遇到过
/// "prompt is too long"（国际版）与 "input length too long"（国服 glm-5.3）。
const CONTEXT_TOO_LONG_MARKERS: &[&str] = &[
    "context_length_exceeded",
    "prompt is too long",
    "input length too long",
    "exceeds the model context limit",
    "对话内容超出模型长度上限",
    "超出模型长度上限",
];

/// 上游「额度耗尽」的业务码。
///
/// 与 [`MODEL_RATE_CODE`]（6004）的关键区别在于**嵌套层级**：6004 在顶层 `code`，
/// 而 14018 藏在 `error.data.code`。实测该响应的 HTTP 状态是 **429**，
/// 而 429 分支若不识别它就会落进软冷却（仅 60 秒）→ 无限重试。
const CREDIT_EXHAUSTED_CODE: i64 = 14018;

/// 上游统一信封。
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ApiEnvelope {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub msg: String,
    #[serde(default)]
    pub data: serde_json::Value,
}

/// 嵌套 14018 探针：`{"error":{"data":{"code":14018}}}`。
#[derive(Debug, Default, Deserialize)]
struct CreditExhaustedProbe {
    #[serde(default)]
    error: CreditExhaustedError,
}

#[derive(Debug, Default, Deserialize)]
struct CreditExhaustedError {
    #[serde(default)]
    data: CreditExhaustedData,
}

#[derive(Debug, Default, Deserialize)]
struct CreditExhaustedData {
    #[serde(default)]
    code: i64,
}

/// 报告响应体是否为「额度耗尽」业务码（含嵌套层级）。
fn is_credit_exhausted_code(body: &str) -> bool {
    serde_json::from_str::<CreditExhaustedProbe>(body)
        .map(|p| p.error.data.code == CREDIT_EXHAUSTED_CODE)
        .unwrap_or(false)
}

/// extError.code 探针：`{"extError":{"code":"..."}}`。
#[derive(Debug, Default, Deserialize)]
struct ExtErrorProbe {
    #[serde(default)]
    #[serde(rename = "extError")]
    ext_error: ExtErrorInner,
}

#[derive(Debug, Default, Deserialize)]
struct ExtErrorInner {
    #[serde(default)]
    code: String,
}

/// 报告 429 响应体是否为「模型级限流」（该账号该模型额度用尽）。
///
/// 与软限流的区别：软限流是整个账号被限速，而模型级限流只影响当前请求的那个模型，
/// 该账号换模型仍可用——上游文案「您也可以切换其他模型继续使用」明确指出了这一点。
pub fn is_model_rate_limited(body: &str) -> bool {
    if serde_json::from_str::<ApiEnvelope>(body)
        .map(|env| env.code == MODEL_RATE_CODE)
        .unwrap_or(false)
    {
        return true;
    }
    // 文案兜底：上游改码不改文案时仍能识别。
    MODEL_RATE_MARKERS.iter().any(|m| body.contains(m))
}

/// 报告上游响应是否为「请求上下文超出模型窗口」。
///
/// 三路判定，任一命中即成立：业务码 11115、extError.code=context_length_exceeded、
/// 或真实文案关键词。不按 status 门控：上游以 400 为主，但判定依据是业务语义
/// 而非状态码，上游若改用 413 也能识别。
pub fn is_context_too_long(body: &str) -> bool {
    if serde_json::from_str::<ApiEnvelope>(body)
        .map(|env| env.code == CONTEXT_TOO_LONG_CODE)
        .unwrap_or(false)
    {
        return true;
    }
    if serde_json::from_str::<ExtErrorProbe>(body)
        .map(|ext| ext.ext_error.code.eq_ignore_ascii_case("context_length_exceeded"))
        .unwrap_or(false)
    {
        return true;
    }
    let lower = body.to_lowercase();
    CONTEXT_TOO_LONG_MARKERS
        .iter()
        .any(|m| lower.contains(&m.to_lowercase()))
}

/// 按 HTTP 状态码 + body 判定错误类别。
///
/// 与 Go 侧 `Classify` 逐条对齐，包括「body 关键词判定先于状态码」「模型级限流
/// 优先于账号级软限流」「上下文超长早于通用 4xx」的顺序。
pub fn classify(status: u16, body: &str) -> ErrKind {
    if status == 402 {
        return ErrKind::HardCredit;
    }
    // 额度耗尽的业务码优先判定：它的 HTTP 状态是 429，若不先拦，
    // 会落进下面的 429 分支被判成软冷却（仅 60 秒冷却）→ 无限重试。
    if is_credit_exhausted_code(body) {
        return ErrKind::HardCredit;
    }
    let lower = body.to_lowercase();
    for m in HARD_MARKERS {
        // 双通道：小写比较（英文）或原文比较（中文，大小写不敏感但中文无大小写）
        if lower.contains(&m.to_lowercase()) || body.contains(m) {
            return ErrKind::HardCredit;
        }
    }
    for m in SESSION_DEAD_MARKERS {
        if body.contains(m) {
            return ErrKind::SessionDead;
        }
    }
    if status == 429 {
        // 模型级限流优先于账号级软限流：两者的冷却粒度与时长都不同
        //（模型级按 uid+model 冷却到上游给的重置时间）。
        if is_model_rate_limited(body) {
            return ErrKind::ModelRate;
        }
        return ErrKind::SoftRate;
    }
    // 上下文超长必须早于通用 4xx 判定：它是请求侧错误，换号无用，
    // 需要独立 kind 让调用方「立即失败」而不是轮转重传整个请求体。
    // 放在 429 之后是有意的：429 一律按限流归类，保持既有语义不变。
    if is_context_too_long(body) {
        return ErrKind::ContextTooLong;
    }
    if status == 404 {
        return ErrKind::NotFound;
    }
    if status >= 500 {
        return ErrKind::Server;
    }
    if status >= 400 {
        return ErrKind::Client;
    }
    // HTTP 200 但业务 code 非 0 且含余额关键词的情况已被上面 HARD_MARKERS 捕获。
    ErrKind::None
}

/// 上游（CodeBuddy 国服）的业务时区偏移（秒）：CST（UTC+8）。
///
/// 用于解释**无时区后缀**的重置时间字面量。为什么不用本地时区：上游是国服服务，
/// 其自然日/重置时刻都按 CST 计；用容器本地时区解释会让同一份响应在开发机（+08:00）
/// 与 UTC 容器上得出相差 8 小时的结果。中国无夏令时，固定 +8，不依赖 tzdata。
const UPSTREAM_ZONE_OFFSET_SECS: i32 = 8 * 60 * 60;

/// 从上游报错文案里提取「重置时刻」（Unix 秒）；解析不出返回 None。
///
/// 实测文案（2026-09-15 现场）：
/// `您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。`
///
/// 时区处理：
/// - "UTC+8" / "UTC+08:00" → 固定偏移（**实测文案用的就是这种**）
/// - "Z"                   → UTC
/// - 无时区后缀 / 后缀非法  → 按上游业务时区（CST）解释，而非容器本地时区
///
/// 只返回**未来**的时刻：解析出过去的时间说明文案里的重置点已过（如重放旧日志），
/// 此时返回 None 交给调用方回退固定冷却，避免写入一个立即失效的冷却。
pub fn parse_reset_time(body: &str, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
    static RESET_TIME_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // 捕获组 1 = 时间字面量（日期 + 时间），捕获组 2 = 时区后缀（如 "UTC+8" / "UTC+08:00"）。
        // 时区后缀可选，仅为容错：实测到的上游文案一律带 "UTC+8"。
        regex::Regex::new(r"(\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2})(?:\s*(UTC[+-]\d{1,2}(?::\d{2})?|Z))?").unwrap()
    });
    let m = RESET_TIME_RE.captures(body)?;
    let literal = m.get(1)?.as_str().replacen('T', " ", 1);
    let offset_secs = match m.get(2).map(|g| g.as_str().trim()) {
        None | Some("") => UPSTREAM_ZONE_OFFSET_SECS,
        Some("Z") => 0,
        Some(tz) => parse_utc_offset_secs(tz).unwrap_or(UPSTREAM_ZONE_OFFSET_SECS),
    };
    let naive = chrono::NaiveDateTime::parse_from_str(&literal, "%Y-%m-%d %H:%M:%S").ok()?;
    let offset = chrono::FixedOffset::east_opt(offset_secs)?;
    let ts = naive.and_local_timezone(offset).single()?;
    if ts <= now {
        return None;
    }
    Some(ts.timestamp())
}

/// 解析 "UTC+8" / "UTC-05:30" 形式的固定偏移（秒）；非法返回 None。
fn parse_utc_offset_secs(s: &str) -> Option<i32> {
    let rest = s.strip_prefix("UTC")?;
    let (sign, rest) = match rest.chars().next()? {
        '+' => (1, &rest[1..]),
        '-' => (-1, &rest[1..]),
        _ => return None,
    };
    let (hours, minutes) = match rest.split_once(':') {
        Some((h, mm)) => (h.parse::<i32>().ok()?, mm.parse::<i32>().ok()?),
        None => (rest.parse::<i32>().ok()?, 0),
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60))
}

/// 把上游的原始错误体提炼成一句可读的原因，供客户端展示。
///
/// 此前直接把整段上游 JSON 拼进 OpenAI 错误体的 message，客户端看到的是
/// 「all accounts unavailable (cooling/disabled): upstream soft_rate (http 429):
/// {"error":{"data":{"code":14018,...}}}」—— 又长又难懂。
///
/// 返回空串表示没有更优的表述，调用方应回退到原始文案。
pub fn friendly_message(kind: ErrKind, status: u16, body: &str) -> String {
    // 业务码优先，但**只有响应体里真的带 14018 时才把该码写进文案**：
    // 硬冷却也可能来自 HTTP 402 或关键词命中，此时硬写「上游 14018」
    // 会让用户拿着一个与响应不符的码去排查。
    if is_credit_exhausted_code(body) {
        return "账号额度已耗尽（上游 14018）：请为该账号充值，或等待签到 / 免费额度恢复后重试".to_string();
    }
    match kind {
        ErrKind::HardCredit => {
            "账号额度已耗尽：请为该账号充值，或等待签到 / 免费额度恢复后重试".to_string()
        }
        ErrKind::ModelRate => "该账号在此模型上已达频率上限，已按上游给出的重置时间冷却；同一账号的其他模型仍可用".to_string(),
        ErrKind::SoftRate => "账号被上游限流（HTTP 429），已短暂冷却，稍后会自动重试".to_string(),
        ErrKind::SessionDead => "账号登录态已失效，需在「账号管理」页重新登录".to_string(),
        ErrKind::NotFound => "上游返回 404（接口或模型不存在），已短暂冷却并切换账号".to_string(),
        ErrKind::Server if status > 0 => {
            format!("上游服务异常（HTTP {status}），已切换到其他账号")
        }
        _ => String::new(),
    }
}

/// 把上下文超长的上游响应体提炼成一条**保留原文**的客户端消息。
///
/// 为什么必须保留上游原文：下游客户端（如 DeepSeek Harness）靠文案模式识别上下文溢出
///（`prompt is too long` / `context_length_exceeded` / `exceeds the model context limit`），
/// 据此触发自动压缩并重试。若只回自己的措辞，客户端就认不出这是溢出，
/// 只会把它当成普通失败。
///
/// 输出形如：`<上游 msg>（<中文 displayMsg>）`，两种语言的特征串都在，
/// 中文提示同时给人类看。上游字段缺失时逐级回退，最终回退到原始 body。
pub fn context_too_long_message(body: &str) -> String {
    #[derive(Default, Deserialize)]
    struct Env {
        #[serde(default)]
        msg: String,
        #[serde(default, rename = "extError")]
        ext_error: ExtErrorMessage,
        #[serde(default, rename = "displayMsg")]
        display_msg: DisplayMessage,
    }
    #[derive(Default, Deserialize)]
    struct ExtErrorMessage {
        #[serde(default)]
        message: String,
    }
    #[derive(Default, Deserialize)]
    struct DisplayMessage {
        #[serde(default)]
        zh: String,
        #[serde(default)]
        en: String,
    }
    let env = serde_json::from_str::<Env>(body).unwrap_or_default();

    let mut primary = env.msg.trim().to_string();
    if primary.is_empty() {
        primary = env.ext_error.message.trim().to_string();
    }
    let mut hint = env.display_msg.zh.trim().to_string();
    if hint.is_empty() {
        hint = env.display_msg.en.trim().to_string();
    }

    if primary.is_empty() && hint.is_empty() {
        return truncate(body.trim(), 400);
    }
    if primary.is_empty() {
        return hint;
    }
    if hint.is_empty() || primary.contains(&hint) {
        return primary;
    }
    format!("{primary}（{hint}）")
}

/// 与 Go 侧 `truncate` 一致：先 TrimSpace，再按字节截断。
///
/// Go 的 `s[:n]` 是纯字节切片（可能切在多字节字符中间）；Rust 字符串不允许
/// 非字符边界切片，这里回退到**不越过 n 的最近字符边界**——仅在极端
/// 多字节场景下与 Go 输出相差几个字节，展示用途无实质差异。
pub(crate) fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.len() > n {
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_based_classification() {
        assert_eq!(classify(402, ""), ErrKind::HardCredit);
        assert_eq!(classify(429, ""), ErrKind::SoftRate);
        assert_eq!(classify(404, ""), ErrKind::NotFound);
        assert_eq!(classify(500, ""), ErrKind::Server);
        assert_eq!(classify(503, ""), ErrKind::Server);
        assert_eq!(classify(400, ""), ErrKind::Client);
        assert_eq!(classify(200, ""), ErrKind::None);
        assert_eq!(classify(302, ""), ErrKind::None);
        assert_eq!(classify(418, ""), ErrKind::Client);
    }

    #[test]
    fn hard_markers_override_status() {
        // 即使是 200，只要 body 含余额关键词也判硬冷却
        assert_eq!(classify(200, "insufficient credit"), ErrKind::HardCredit);
        assert_eq!(classify(200, "Insufficient Credit"), ErrKind::HardCredit); // 大小写不敏感
        assert_eq!(classify(200, "insufficient credits"), ErrKind::HardCredit); // 复数形态（国际版实测）
        assert_eq!(
            classify(
                200,
                "Credits exhausted. Please visit the link below to purchase add-on packs and get more credits: …"
            ),
            ErrKind::HardCredit
        );
        assert_eq!(classify(500, "quota exceeded"), ErrKind::HardCredit);
        // 中文原文通道
        assert_eq!(classify(200, "余额不足"), ErrKind::HardCredit);
        assert_eq!(classify(200, "您的积分不足"), ErrKind::HardCredit);
        assert_eq!(classify(200, "额度用尽"), ErrKind::HardCredit);
    }

    #[test]
    fn session_dead_markers() {
        assert_eq!(
            classify(401, r#"{"code":12153,"msg":"Offline user session not found"}"#),
            ErrKind::SessionDead
        );
        assert_eq!(classify(200, "12153"), ErrKind::SessionDead);
        assert_eq!(classify(500, "Offline user session not found"), ErrKind::SessionDead);
    }

    #[test]
    fn hard_markers_take_priority_over_session_dead() {
        // hardMarkers 判定在 sessionDeadMarkers 之前
        assert_eq!(classify(401, "12153 余额不足"), ErrKind::HardCredit);
    }

    #[test]
    fn model_rate_only_at_429() {
        let body = r#"{"code":6004,"msg":"您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。"}"#;
        assert_eq!(classify(429, body), ErrKind::ModelRate);
        assert_eq!(classify(429, "超出频率限制"), ErrKind::ModelRate);
        assert_eq!(classify(429, "请切换其他模型"), ErrKind::ModelRate);
        // 非 429 不按模型限流归类（业务码只在 429 分支检查；文案标记需 429 门控）
        assert_eq!(classify(200, "超出频率限制"), ErrKind::None);
        assert_eq!(classify(400, r#"{"code":6004,"msg":"x"}"#), ErrKind::Client);
    }

    #[test]
    fn context_too_long_classification() {
        let body = r#"{"code":11115,"msg":"prompt is too long: 1119655 tokens > 1048576 maximum","extError":{"code":"context_length_exceeded","type":"invalid_request_error"},"displayMsg":{"en":"The request exceeds the model context limit...","zh":"对话内容超出模型长度上限，请精简对话或减少附件后重试。"}}"#;
        assert_eq!(classify(400, body), ErrKind::ContextTooLong);
        assert_eq!(
            classify(400, r#"{"extError":{"code":"context_length_exceeded"}}"#),
            ErrKind::ContextTooLong
        );
        assert_eq!(classify(400, "prompt is too long"), ErrKind::ContextTooLong);
        assert_eq!(classify(400, "input length too long"), ErrKind::ContextTooLong);
        assert_eq!(classify(413, "prompt is too long"), ErrKind::ContextTooLong);
        assert_eq!(
            classify(400, "对话内容超出模型长度上限，请精简对话或减少附件后重试。"),
            ErrKind::ContextTooLong
        );
        // 429 一律按限流归类（上下文超长判定在 429 之后）
        assert_eq!(classify(429, "prompt is too long"), ErrKind::SoftRate);
    }

    #[test]
    fn nested_credit_exhausted_code() {
        let body = r#"{"error":{"data":{"code":14018,"msg":"Credits exhausted. Please visit the link below to purchase add-on packs and get more credits: …"}}}"#;
        assert_eq!(classify(429, body), ErrKind::HardCredit);
        assert_eq!(
            classify(200, r#"{"error":{"data":{"code":14018,"msg":"x"}}}"#),
            ErrKind::HardCredit
        );
        assert_eq!(
            classify(500, r#"{"error":{"data":{"code":14018}}}"#),
            ErrKind::HardCredit
        );
        // hard 优先于 model rate
        assert_eq!(
            classify(429, r#"{"code":6004,"msg":"余额不足"}"#),
            ErrKind::HardCredit
        );
    }

    #[test]
    fn err_kind_display_matches_go() {
        assert_eq!(ErrKind::HardCredit.as_str(), "hard_credit");
        assert_eq!(ErrKind::SoftRate.as_str(), "soft_rate");
        assert_eq!(ErrKind::SessionDead.as_str(), "session_dead");
        assert_eq!(ErrKind::NotFound.as_str(), "not_found");
        assert_eq!(ErrKind::Server.as_str(), "server");
        assert_eq!(ErrKind::Client.as_str(), "client");
        assert_eq!(ErrKind::ModelRate.as_str(), "model_rate");
        assert_eq!(ErrKind::ContextTooLong.as_str(), "context_too_long");
        assert_eq!(ErrKind::None.as_str(), "none");
    }

    #[test]
    fn upstream_error_display_format() {
        let e = UpstreamError {
            kind: ErrKind::SoftRate,
            status: 429,
            msg: "rate limited".into(),
        };
        assert_eq!(e.to_string(), "upstream soft_rate (http 429): rate limited");
    }

    #[test]
    fn parse_reset_time_with_utc8_suffix() {
        use chrono::TimeZone;
        let now = chrono::Utc.with_ymd_and_hms(2026, 9, 15, 5, 0, 0).unwrap();
        let ts = parse_reset_time(
            "您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。",
            now,
        )
        .expect("应解析出重置时刻");
        assert_eq!(ts, chrono::Utc.with_ymd_and_hms(2026, 9, 15, 5, 25, 47).unwrap().timestamp());
    }

    #[test]
    fn parse_reset_time_without_suffix_uses_cst() {
        use chrono::TimeZone;
        let now = chrono::Utc.with_ymd_and_hms(2026, 9, 15, 0, 0, 0).unwrap();
        let ts = parse_reset_time("将在 2026-09-15 13:25:47 重置", now).expect("无后缀按 CST(+8) 解释");
        assert_eq!(ts, chrono::Utc.with_ymd_and_hms(2026, 9, 15, 5, 25, 47).unwrap().timestamp());
    }

    #[test]
    fn parse_reset_time_rejects_past() {
        use chrono::TimeZone;
        let now = chrono::Utc.with_ymd_and_hms(2026, 9, 16, 0, 0, 0).unwrap();
        assert_eq!(parse_reset_time("将在 2026-09-15 13:25:47 UTC+8 重置", now), None);
        assert_eq!(parse_reset_time("没有任何时间信息", now), None);
    }

    #[test]
    fn parse_utc_offset_validation() {
        assert_eq!(parse_utc_offset_secs("UTC+8"), Some(8 * 3600));
        assert_eq!(parse_utc_offset_secs("UTC-05:30"), Some(-5 * 3600 - 30 * 60));
        assert_eq!(parse_utc_offset_secs("UTC+24"), None);
        assert_eq!(parse_utc_offset_secs("UTC+8:61"), None);
        assert_eq!(parse_utc_offset_secs("CST+8"), None);
    }

    #[test]
    fn friendly_message_by_kind() {
        assert_eq!(
            friendly_message(ErrKind::HardCredit, 402, ""),
            "账号额度已耗尽：请为该账号充值，或等待签到 / 免费额度恢复后重试"
        );
        // 只有 body 真带 14018 才写码
        assert!(friendly_message(ErrKind::HardCredit, 402, r#"{"error":{"data":{"code":14018}}}"#)
            .contains("14018"));
        assert!(friendly_message(ErrKind::SoftRate, 429, "").contains("429"));
        assert!(friendly_message(ErrKind::Server, 502, "").contains("502"));
        assert_eq!(friendly_message(ErrKind::Client, 400, ""), "");
    }

    #[test]
    fn context_too_long_message_keeps_original_text() {
        let body = r#"{"msg":"prompt is too long: 1119655 tokens > 1048576 maximum","displayMsg":{"en":"The request exceeds the model context limit...","zh":"对话内容超出模型长度上限，请精简对话或减少附件后重试。"}}"#;
        let msg = context_too_long_message(body);
        assert!(msg.contains("prompt is too long"));
        assert!(msg.contains("对话内容超出模型长度上限"));
        assert!(msg.starts_with("prompt is too long"));
        // 字段缺失回退原文（截断 400 字节）
        assert_eq!(context_too_long_message("plain error"), "plain error");
    }

    #[test]
    fn truncate_on_char_boundary() {
        assert_eq!(truncate("  hello  ", 10), "hello");
        assert_eq!(truncate("abcdef", 3), "abc");
        // 中文按字符边界回退，不产生非法 UTF-8
        let t = truncate("积分不足余额", 7);
        assert!(t.chars().count() <= 4);
    }
}
