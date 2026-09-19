//! growth 域「猫猫旅行」接口：状态查询 / 派出 / 领奖 / 领养 / 协议。
//!
//! 对应 Go 源文件 `internal/upstream/travel.go`。
//! 全部走 chatBase（copilot.tencent.com，不带 /v2 前缀）+ BillingHeaders，信封同 [`Client::do_json`]。

use serde_json::{json, Value};

use crate::auth::Auth;

use super::client::{Client, ClientError};
use super::headers::apply_billing;

/// growth 域路径（实测）。
const TRAVEL_STATUS_PATH: &str = "/activity/growth/buddy/travel/status";
const TRAVEL_DEPART_PATH: &str = "/activity/growth/buddy/travel/depart";
const TRAVEL_CLAIM_PATH: &str = "/activity/growth/buddy/travel/claim";
const BUDDY_INFO_PATH: &str = "/activity/growth/buddy/info";
const BUDDY_FIRST_PATH: &str = "/activity/growth/buddy/first";
const BUDDY_AGREEMENT_PATH: &str = "/activity/growth/buddy/agreement";

/// 领养门槛未达标的业务错误关键词（HTTP 400 时出现）。
const BUDDY_TASK_INCOMPLETE_MARKER: &str = "first_buddy task not completed yet";

/// 账号当前猫档案；None（data.buddy 为 null）表示无猫。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct Buddy {
    pub id: i64,
    pub name: String,
}

/// 猫猫旅行状态。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct TravelState {
    /// idle / traveling / arrived
    pub state: String,
    /// 今日已派出过（自然日 00:00 CST 重置）
    #[serde(rename = "daily_limit_reached")]
    pub daily_limit_reached: bool,
    /// 在途/到站记录 id，claim 必带
    #[serde(rename = "record_id")]
    pub record_id: i64,
    /// 到站可领奖励积分
    #[serde(rename = "reward_credit")]
    pub reward_credit: i64,
}

impl Client {
    /// 发 growth 域请求并解信封；body 为 None 时不带请求体。
    /// 错误语义与 [`Client::do_json`] 一致：HTTP 非 2xx / 业务 code != 0 → 已分类错误。
    async fn growth_json(
        &self,
        a: &Auth,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, ClientError> {
        let url = format!("{}{}", self.chat_base(a), path);
        let mut req = self.http.request(method, &url);
        req = apply_billing(req, a);
        if let Some(b) = body {
            req = req.json(&b);
        }
        self.do_json(req).await
    }

    /// 查询猫猫旅行状态。
    pub async fn travel_status(&self, a: &Auth) -> Result<TravelState, ClientError> {
        let data = self.growth_json(a, reqwest::Method::GET, TRAVEL_STATUS_PATH, None).await?;
        serde_json::from_value(data)
            .map_err(|e| ClientError::Parse(format!("travel status parse: {e}")))
    }

    /// 派出猫旅行；locationID 实测 1~4（收益/时长区间相同）。
    pub async fn travel_depart(&self, a: &Auth, location_id: i64) -> Result<(), ClientError> {
        self.growth_json(
            a,
            reqwest::Method::POST,
            TRAVEL_DEPART_PATH,
            Some(json!({ "location_id": location_id })),
        )
        .await?;
        Ok(())
    }

    /// 领取到站奖励，返回 reward_credit。
    pub async fn travel_claim(&self, a: &Auth, record_id: i64) -> Result<i64, ClientError> {
        let data = self
            .growth_json(
                a,
                reqwest::Method::POST,
                TRAVEL_CLAIM_PATH,
                Some(json!({ "record_id": record_id })),
            )
            .await?;
        // 奖励字段缺失不视为失败：调用方按 0 记日志即可。
        let resp: RewardResp = serde_json::from_value(data).unwrap_or_default();
        Ok(resp.reward_credit)
    }

    /// 查询当前猫档案；返回 None 表示无猫（data.buddy 为 null）。
    pub async fn buddy_info(&self, a: &Auth) -> Result<Option<Buddy>, ClientError> {
        let data = self.growth_json(a, reqwest::Method::GET, BUDDY_INFO_PATH, None).await?;
        let buddy = data.get("buddy").cloned().unwrap_or(Value::Null);
        // null / 缺字段都按无猫处理（空对象会解析成默认 Buddy，与 Go 行为一致）。
        if buddy.is_null() {
            return Ok(None);
        }
        serde_json::from_value(buddy)
            .map(Some)
            .map_err(|e| ClientError::Parse(format!("buddy info parse: {e}")))
    }

    /// 领养第一只猫。无猫且已过 conversation 门槛时送 300 分。
    /// 门槛未达标返回 HTTP 400（见 [`is_buddy_task_incomplete`]），属预期行为，调用方静默跳过。
    pub async fn buddy_first(&self, a: &Auth) -> Result<(), ClientError> {
        self.growth_json(a, reqwest::Method::POST, BUDDY_FIRST_PATH, Some(json!({})))
            .await?;
        Ok(())
    }

    /// 同意协议（幂等，重复调用无副作用）。
    pub async fn buddy_agreement(&self, a: &Auth) -> Result<(), ClientError> {
        self.growth_json(
            a,
            reqwest::Method::POST,
            BUDDY_AGREEMENT_PATH,
            Some(json!({ "agree": true })),
        )
        .await?;
        Ok(())
    }
}

#[derive(Debug, Default, serde::Deserialize)]
struct RewardResp {
    #[serde(default, rename = "reward_credit")]
    reward_credit: i64,
}

/// 判定「领养门槛未达标」：HTTP 400 + first_buddy 关键词。
/// 该错误当日不应重试（避免对上游重试轰炸）。
pub fn is_buddy_task_incomplete(err: &ClientError) -> bool {
    let Some(ue) = err.as_upstream() else { return false };
    if ue.status != reqwest::StatusCode::BAD_REQUEST.as_u16() {
        return false;
    }
    ue.msg.to_lowercase().contains(BUDDY_TASK_INCOMPLETE_MARKER)
}
