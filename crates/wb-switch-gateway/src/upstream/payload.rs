//! 改写发往上游的 chat 请求体：
//! 1. 强制 stream:true（上游拒绝非流式）
//! 2. tool_choice 归一化（上游该字段是 string，对象形式会 400 code=11101）
//! 3. roles 归一化（developer → system，上游 role 白名单不含 developer）
//! 4. reasoning_effort 按模型支持档位降级
//! 5. （国际版）保证 messages 首条是 system
//!
//! 对应 Go 源文件 `internal/upstream/payload.go`。

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::sanitize::sanitize_messages;

/// 单 pass 改写；sanitize=false 时行为完全还原（仅强制 stream + 归一化 tool_choice）。
pub fn prepare_body_opt(src: &[u8], sanitize: bool) -> Vec<u8> {
    prepare_body_opt_with_efforts(src, sanitize, None)
}

/// 在 [`prepare_body_opt`] 基础上按模型 supportedEfforts 降级 reasoning_effort：
/// 仅当请求显式携带且模型不支持该档位时，改为 ≤请求档位的最高支持档；支持档全部高于请求档时取最低档；
/// 未知模型/未知档位/未携带该字段一律透传。efforts 为 None 表示未知（不降级）。
pub fn prepare_body_opt_with_efforts(
    src: &[u8],
    sanitize: bool,
    efforts: Option<&HashMap<String, Vec<String>>>,
) -> Vec<u8> {
    prepare_body_for_region(src, sanitize, efforts, false)
}

/// 在 [`prepare_body_opt_with_efforts`] 基础上按账号区域做协议适配。
///
/// intl=true（国际版 workbuddy.ai）时额外保证 messages 首条是 system——
/// 实测国际版对首条非 system 的请求返回 HTTP 400 code=11128
/// "first message is not system prompt"（同一账号补上 system 首条即 200）。
///
/// 只对国际版做这件事：国服的 11128 是另一种含义（渠道指纹未批准，见 sanitize.rs），
/// 国服并无「首条必须 system」的要求，不能混为一谈。
pub fn prepare_body_for_region(
    src: &[u8],
    sanitize: bool,
    efforts: Option<&HashMap<String, Vec<String>>>,
    intl: bool,
) -> Vec<u8> {
    if src.is_empty() {
        return src.to_vec();
    }
    // Go 侧 json.Unmarshal 进 map[string]any：非对象（数组/标量）直接失败返回原样。
    let Ok(mut obj) = serde_json::from_slice::<Map<String, Value>>(src) else {
        return src.to_vec();
    };
    obj.insert("stream".to_string(), Value::Bool(true));
    normalize_tool_choice(&mut obj);
    normalize_roles(&mut obj);
    // 归一化之后再做国际版适配：developer 已被改写成 system，
    // 此时首条若已是 system 就不必补（否则会给 Codex 之类客户端多插一条）。
    if intl {
        ensure_system_first(&mut obj);
    }
    normalize_reasoning_effort(&mut obj, efforts);
    if sanitize {
        if let Some(msgs) = obj.get_mut("messages").and_then(Value::as_array_mut) {
            sanitize_messages(msgs);
        }
    }
    serde_json::to_vec(&Value::Object(obj)).unwrap_or_else(|_| src.to_vec())
}

/// 保证 messages[0] 是 system（国际版协议要求）。
///
/// 仅在首条不是 system 时补一条**最小**的 system，不改动任何既有消息，也不合并——
/// 合并会改变模型看到的对话结构，风险大于收益。空 messages（或缺失）时不动：
/// 那种请求本就缺少上下文，交给上游报错更诚实。
///
/// 补的内容刻意保持中性（"You are a helpful assistant."），因为这里无法得知
/// 调用方想要的系统提示；它只为满足协议前提，不承载业务语义。
fn ensure_system_first(obj: &mut Map<String, Value>) {
    let Some(Value::Array(msgs)) = obj.get_mut("messages") else {
        return;
    };
    if msgs.is_empty() {
        return;
    }
    let Some(first) = msgs[0].as_object() else {
        return;
    };
    if first
        .get("role")
        .and_then(Value::as_str)
        .map(|r| r.trim().eq_ignore_ascii_case("system"))
        .unwrap_or(false)
    {
        return;
    }
    let original_role = msgs[0].get("role").cloned().unwrap_or(Value::Null);
    let sys = serde_json::json!({"role": "system", "content": "You are a helpful assistant."});
    msgs.insert(0, sys);
    tracing::info!(role = %original_role, "intl payload: messages 首条非 system，已补一条 system");
}

/// 档位从低到高。
fn effort_rank(s: &str) -> Option<i32> {
    match s {
        "off" => Some(0),
        "minimal" => Some(1),
        "low" => Some(2),
        "medium" => Some(3),
        "high" => Some(4),
        "xhigh" => Some(5),
        "max" => Some(6),
        _ => None,
    }
}

/// 按模型 supportedEfforts 降级 reasoning_effort（snake/camel 双字段兼容）。
///   - 请求档位模型支持 → 原样透传
///   - 请求档位不支持 → 改为 ≤请求档位的最高支持档（降级）
///   - 支持档全部高于请求档 → 取最低支持档（偏离最小）
///   - 未知模型/未知档位/未携带字段/模型未缓存 → 一律透传
fn normalize_reasoning_effort(obj: &mut Map<String, Value>, efforts: Option<&HashMap<String, Vec<String>>>) {
    let Some(efforts) = efforts else { return };
    if efforts.is_empty() {
        return;
    }
    let Some(model) = obj.get("model").and_then(Value::as_str) else {
        return;
    };
    if model.is_empty() {
        return;
    }
    let Some(supported) = efforts.get(model) else { return };
    if supported.is_empty() {
        return;
    }
    let key = if obj.contains_key("reasoning_effort") {
        "reasoning_effort"
    } else if obj.contains_key("reasoningEffort") {
        "reasoningEffort"
    } else {
        return;
    };
    let Some(req_str) = obj.get(key).and_then(Value::as_str) else {
        return;
    };
    let req_str = req_str.trim().to_lowercase();
    let Some(req_idx) = effort_rank(&req_str) else {
        return;
    };
    // 在 ≤请求档位的支持档里选最高档；命中且与请求不同才改写。
    let mut best = String::new();
    let mut best_idx = -1i32;
    for s in supported {
        if let Some(idx) = effort_rank(s.trim().to_lowercase().as_str()) {
            if idx <= req_idx && idx > best_idx {
                best = s.clone();
                best_idx = idx;
            }
        }
    }
    if !best.is_empty() {
        if !best.eq_ignore_ascii_case(&req_str) {
            obj.insert(key.to_string(), Value::String(best.clone()));
            tracing::info!(model, from = %req_str, to = %best, "reasoning_effort downgraded");
        }
        return;
    }
    // 支持档全部高于请求档：取最低支持档。
    let mut lowest = String::new();
    let mut lowest_idx = i32::MAX;
    for s in supported {
        if let Some(idx) = effort_rank(s.trim().to_lowercase().as_str()) {
            if idx < lowest_idx {
                lowest = s.clone();
                lowest_idx = idx;
            }
        }
    }
    if !lowest.is_empty() {
        obj.insert(key.to_string(), Value::String(lowest.clone()));
        tracing::info!(model, from = %req_str, to = %lowest, "reasoning_effort floored");
    }
}

/// 把 messages 里的 developer 角色归一为 system。
///
/// 背景：上游对 messages 的 role 字段做白名单校验，developer 不在白名单内，
/// 命中即 HTTP 400 code=11128。developer 是 OpenAI 新规范里 system 的别名
///（Codex / Cursor 等新客户端用它承载 system 级指令），改写为 system 不丢语义。
///
/// 此归一化是「协议兼容」（补上游 role 白名单），不是「内容脱敏」，
/// 因此有意与 SanitizeFingerprints / sanitize 参数解耦：即使 sanitize=false 也照常归一。
///
/// 只认 developer 这一个值：其余 role（system/user/assistant/tool/任意未知值）一律原样保留，
/// 不合并、不重排、不删除任何消息（上游对多 system 的行为尚未实测，合并会引入新变量）。
fn normalize_roles(obj: &mut Map<String, Value>) {
    let Some(Value::Array(msgs)) = obj.get_mut("messages") else {
        return;
    };
    for (i, m) in msgs.iter_mut().enumerate() {
        let Some(msg) = m.as_object_mut() else { continue };
        let Some(role) = msg.get("role").and_then(Value::as_str) else { continue };
        if role.trim().eq_ignore_ascii_case("developer") {
            msg.insert("role".to_string(), Value::String("system".to_string()));
            tracing::info!(idx = i, "role normalized developer->system");
        }
    }
}

/// 按上游 Go struct（string 类型）改写 OpenAI tool_choice。
///   - "none"            → 删 tool_choice + 删 tools/functions
///   - {"type":"none"}   → 同上
///   - {"type":"auto"/"required"} → 字符串 "auto"/"required"
///   - {"type":"function","function":{"name":"x"}} → 字符串 "x"
///   - 其他对象/非标量 → 删 tool_choice
fn normalize_tool_choice(obj: &mut Map<String, Value>) {
    fn suppress(obj: &mut Map<String, Value>) {
        obj.remove("tools");
        obj.remove("functions");
    }
    let Some(tc) = obj.get("tool_choice").cloned() else {
        return;
    };
    match tc {
        Value::String(v) => {
            if v.trim().eq_ignore_ascii_case("none") {
                obj.remove("tool_choice");
                suppress(obj);
            }
        }
        Value::Object(v) => {
            let typ = v
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_lowercase();
            match typ.as_str() {
                "none" => {
                    obj.remove("tool_choice");
                    suppress(obj);
                }
                "auto" | "required" => {
                    obj.insert("tool_choice".to_string(), Value::String(typ));
                }
                "function" => {
                    let mut name = v
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if name.is_empty() {
                        name = v
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                    }
                    let name = name.trim().to_string();
                    obj.insert(
                        "tool_choice".to_string(),
                        Value::String(if name.is_empty() { "auto".to_string() } else { name }),
                    );
                }
                _ => {
                    obj.remove("tool_choice");
                }
            }
        }
        _ => {
            obj.remove("tool_choice");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn prep(src: Value, sanitize: bool) -> Value {
        serde_json::from_slice(&prepare_body_opt(
            serde_json::to_vec(&src).unwrap().as_slice(),
            sanitize,
        ))
        .unwrap()
    }

    #[test]
    fn forces_stream_true() {
        let out = prep(json!({"model": "m", "stream": false}), false);
        assert_eq!(out["stream"], json!(true));
    }

    #[test]
    fn invalid_or_empty_src_passthrough() {
        assert!(prepare_body_opt(b"", false).is_empty());
        assert_eq!(prepare_body_opt(b"[1,2]", false), b"[1,2]");
        assert_eq!(prepare_body_opt(b"not json", false), b"not json");
    }

    #[test]
    fn tool_choice_object_forms() {
        let out = prep(json!({"tool_choice": {"type": "auto"}}), false);
        assert_eq!(out["tool_choice"], json!("auto"));

        let out = prep(json!({"tool_choice": {"type": "function", "function": {"name": "get_weather"}}}), false);
        assert_eq!(out["tool_choice"], json!("get_weather"));

        let out = prep(json!({"tool_choice": {"type": "function"}, "name": "n"}), false);
        assert_eq!(out["tool_choice"], json!("n"));

        let out = prep(json!({"tool_choice": {"type": "function"}}), false);
        assert_eq!(out["tool_choice"], json!("auto"));

        let out = prep(json!({"tool_choice": {"type": "bogus"}}), false);
        assert!(out.get("tool_choice").is_none());

        // "none" 连 tools 一起删
        let out = prep(json!({"tool_choice": "none", "tools": [1], "functions": {}}), false);
        assert!(out.get("tool_choice").is_none());
        assert!(out.get("tools").is_none());
        assert!(out.get("functions").is_none());
        let out = prep(json!({"tool_choice": {"type": "none"}, "tools": [1]}), false);
        assert!(out.get("tools").is_none());

        // 大小写不敏感
        let out = prep(json!({"tool_choice": "NONE"}), false);
        assert!(out.get("tool_choice").is_none());
    }

    #[test]
    fn developer_role_normalized_even_without_sanitize() {
        let out = prep(
            json!({"messages": [{"role": "developer", "content": "be nice"}, {"role": "user", "content": "hi"}]}),
            false,
        );
        assert_eq!(out["messages"][0]["role"], json!("system"));
        assert_eq!(out["messages"][1]["role"], json!("user"));
    }

    #[test]
    fn unknown_roles_untouched() {
        let out = prep(
            json!({"messages": [{"role": "system"}, {"role": "user"}, {"role": "assistant"}, {"role": "tool"}]}),
            false,
        );
        assert_eq!(out["messages"][0]["role"], json!("system"));
        assert_eq!(out["messages"][3]["role"], json!("tool"));
    }

    #[test]
    fn ensure_system_first_only_for_intl() {
        let src = json!({"messages": [{"role": "user", "content": "hi"}]});
        let cn = serde_json::from_slice::<Value>(&prepare_body_for_region(
            &serde_json::to_vec(&src).unwrap(),
            false,
            None,
            false,
        ))
        .unwrap();
        assert_eq!(cn["messages"].as_array().unwrap().len(), 1);

        let intl = serde_json::from_slice::<Value>(&prepare_body_for_region(
            &serde_json::to_vec(&src).unwrap(),
            false,
            None,
            true,
        ))
        .unwrap();
        let msgs = intl["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], json!("system"));
        assert_eq!(msgs[0]["content"], json!("You are a helpful assistant."));
        assert_eq!(msgs[1]["role"], json!("user"));
    }

    #[test]
    fn ensure_system_first_skips_existing_system_and_empty() {
        let src = json!({"messages": [{"role": " SYSTEM ", "content": "x"}]});
        let out = serde_json::from_slice::<Value>(&prepare_body_for_region(
            &serde_json::to_vec(&src).unwrap(),
            false,
            None,
            true,
        ))
        .unwrap();
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);

        let src = json!({"messages": []});
        let out = serde_json::from_slice::<Value>(&prepare_body_for_region(
            &serde_json::to_vec(&src).unwrap(),
            false,
            None,
            true,
        ))
        .unwrap();
        assert_eq!(out["messages"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn sanitize_applies_to_messages() {
        let src = json!({"messages": [{"role": "system", "content": "You are Claude Code, Anthropic's official CLI for Claude."}]});
        let out = prep(src, true);
        assert!(out["messages"][0]["content"].as_str().unwrap().contains("CLI tool"));

        // 多模态数组形态
        let src = json!({"messages": [{"role": "system", "content": [{"type": "text", "text": "led by OpenAI"}]}]});
        let out = prep(src, true);
        assert_eq!(out["messages"][0]["content"][0]["text"], json!("led by the community"));
    }

    #[test]
    fn effort_downgrade_picks_highest_supported_below_request() {
        let mut efforts = HashMap::new();
        efforts.insert("m1".to_string(), vec!["low".to_string(), "high".to_string()]);
        let src = json!({"model": "m1", "reasoning_effort": "medium"});
        let out = serde_json::from_slice::<Value>(&prepare_body_opt_with_efforts(
            &serde_json::to_vec(&src).unwrap(),
            false,
            Some(&efforts),
        ))
        .unwrap();
        assert_eq!(out["reasoning_effort"], json!("low"));

        // camel 字段
        let src = json!({"model": "m1", "reasoningEffort": "max"});
        let out = serde_json::from_slice::<Value>(&prepare_body_opt_with_efforts(
            &serde_json::to_vec(&src).unwrap(),
            false,
            Some(&efforts),
        ))
        .unwrap();
        assert_eq!(out["reasoningEffort"], json!("high"));
    }

    #[test]
    fn effort_floor_when_all_supported_above_request() {
        let mut efforts = HashMap::new();
        efforts.insert("m1".to_string(), vec!["medium".to_string(), "high".to_string()]);
        let src = json!({"model": "m1", "reasoning_effort": "low"});
        let out = serde_json::from_slice::<Value>(&prepare_body_opt_with_efforts(
            &serde_json::to_vec(&src).unwrap(),
            false,
            Some(&efforts),
        ))
        .unwrap();
        assert_eq!(out["reasoning_effort"], json!("medium"));
    }

    #[test]
    fn effort_passthrough_when_unknown() {
        let mut efforts = HashMap::new();
        efforts.insert("other".to_string(), vec!["low".to_string()]);
        let src = json!({"model": "m1", "reasoning_effort": "high"});
        let out = serde_json::from_slice::<Value>(&prepare_body_opt_with_efforts(
            &serde_json::to_vec(&src).unwrap(),
            false,
            Some(&efforts),
        ))
        .unwrap();
        assert_eq!(out["reasoning_effort"], json!("high"));

        // 未知档位/非字符串档位/未知模型 → 透传
        let src = json!({"model": "m1", "reasoning_effort": "turbo"});
        let out = serde_json::from_slice::<Value>(&prepare_body_opt_with_efforts(
            &serde_json::to_vec(&src).unwrap(),
            false,
            Some(&efforts),
        ))
        .unwrap();
        assert_eq!(out["reasoning_effort"], json!("turbo"));

        let src = json!({"model": "m1", "reasoning_effort": 3});
        let out = serde_json::from_slice::<Value>(&prepare_body_opt_with_efforts(
            &serde_json::to_vec(&src).unwrap(),
            false,
            Some(&efforts),
        ))
        .unwrap();
        assert_eq!(out["reasoning_effort"], json!(3));
    }
}
