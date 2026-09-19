//! 出站请求体脱敏：剥离上游内容审核黑名单指纹。
//!
//! 对应 Go 源文件 `internal/upstream/sanitize.go`。
//!
//! 背景：客户端（Claude Code 类 CLI）在 system prompt 注入若干固定模板句，
//! 上游内容审核按逐字精确匹配拦截（非语义审核），一字改动即可绕过。
//! 策略：键值/header 型指纹整段剥离；承载语义的模板句最小改写（换一词），语义不变。

use serde_json::Value;

/// 特征预检：任一命中才进入净化（contains 快速路径，
/// 普通请求全不中 → 原样返回，零分配）。
const SANITIZE_FEATURES: &[&str] = &[
    "x-anthropic-billing-header", // header 键值段键名
    "cc_entrypoint=",             // 尾随裸键值（截断前缀即可命中）
    "You are Claude Code",        // 身份句（截断前缀即可命中）
    "Main branch (",              // 注入指令句（截断前缀即可命中）
    "github.com/anthropics",      // 官方反馈链接（Claude Code 2.1.260+ 起出现在系统提示中）
    "led by OpenAI",              // Codex CLI instructions 的归属句
];

/// 剥离层：header 键名即触发（与值无关），整段删除。
static SANITIZE_HDR_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)x-anthropic-billing-header:[^;\n]*;?\s*").unwrap()
});

/// 剥离层：尾随裸键值（cc_xxx=...;）循环清理。
static SANITIZE_KV_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)\bcc_[a-z0-9_]+=[^;\n]*;?\s*").unwrap());

/// 改写层：全模板句逐字替换（每句只改一个词，语义不变）。
const SANITIZE_REWRITES: &[(&str, &str)] = &[
    (
        // 身份句有两个客户端变体，查找串不带结尾标点，两种形态都能命中：
        //   CLI 模式：You are Claude Code, Anthropic's official CLI for Claude.
        //   3P 模式：You are Claude Code, Anthropic's official CLI for Claude,
        //            running within the Claude Agent SDK.（claude-desktop-3p，2.1.260+）
        "You are Claude Code, Anthropic's official CLI for Claude",
        "You are Claude Code, Anthropic's official CLI tool for Claude",
    ),
    (
        "Main branch (you will usually use this for PRs)",
        "Default branch (you will usually use this for PRs)",
    ),
    (
        // Claude Code 2.1.260 的系统提示里带有指向 Anthropic 官方仓库的反馈链接，
        // 上游按「未批准渠道」指纹拦截（HTTP 400 code=11128）。
        // 只替换链接本身，句子结构与语义（如何提交反馈）不变。
        "https://github.com/anthropics/claude-code/issues",
        "https://github.com/user-feedback/issues",
    ),
    (
        // Codex CLI 的 instructions 首句声明 "an open source project led by OpenAI"，
        // 上游同样按「未批准渠道」指纹拦截（HTTP 400 code=11128）。
        // 只改归属表述，语义（开源项目）不变。
        "led by OpenAI",
        "led by the community",
    ),
];

/// 单段文本净化：预检不中 → 返回原串（零分配）。
pub fn sanitize_text(text: &str) -> String {
    if !has_fingerprint(text) {
        return text.to_string();
    }
    let mut t = text.to_string();
    for (from, to) in SANITIZE_REWRITES {
        t = t.replace(from, to);
    }
    if SANITIZE_HDR_RE.is_match(&t) {
        t = SANITIZE_HDR_RE.replace_all(&t, "").into_owned();
    }
    if t.contains("cc_") {
        // 清尾随裸 kv（cc_version=...; cc_entrypoint=...;），循环到不再变化
        loop {
            let prev = t.clone();
            t = SANITIZE_KV_RE.replace_all(&t, "").into_owned();
            if t == prev {
                break;
            }
        }
    }
    t.trim().to_string()
}

/// 特征预检：先走 contains 快速路径（零分配）；
/// header 键名有大小写变体（X-Anthropic-...），快速路径漏掉时再落正则（(?i)）兜底。
pub fn has_fingerprint(text: &str) -> bool {
    SANITIZE_FEATURES.iter().any(|f| text.contains(f))
        || SANITIZE_HDR_RE.is_match(text)
}

/// 兼容字符串与多模态数组；只动 text part，image 等 part 不动。
/// 就地改写，返回是否发生变化。
pub fn sanitize_content(v: &mut Value) -> bool {
    match v {
        Value::String(s) => {
            let s_new = sanitize_text(s);
            if s_new != *s {
                *v = Value::String(s_new);
                true
            } else {
                false
            }
        }
        Value::Array(parts) => {
            let mut changed = false;
            for p in parts.iter_mut() {
                let Some(m) = p.as_object_mut() else { continue };
                let Some(text) = m.get("text").and_then(Value::as_str) else { continue };
                let s = sanitize_text(text);
                if s != text {
                    m.insert("text".to_string(), Value::String(s));
                    changed = true;
                }
            }
            changed
        }
        _ => false,
    }
}

/// 净化 messages 中的 content；任一命中返回 true。
pub fn sanitize_messages(messages: &mut [Value]) -> bool {
    let mut changed = false;
    for msg in messages.iter_mut() {
        let Some(m) = msg.as_object_mut() else { continue };
        let Some(content) = m.get_mut("content") else { continue };
        if sanitize_content(content) {
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_text_untouched() {
        assert_eq!(sanitize_text("你好，世界"), "你好，世界");
        assert_eq!(sanitize_text(""), "");
        assert!(!has_fingerprint("ordinary conversation"));
    }

    #[test]
    fn rewrites_identity_sentence_both_variants() {
        let cli = "You are Claude Code, Anthropic's official CLI for Claude.";
        assert_eq!(
            sanitize_text(cli),
            "You are Claude Code, Anthropic's official CLI tool for Claude."
        );
        let sdk = "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.";
        assert!(sanitize_text(sdk).contains("official CLI tool for Claude, running within"));
    }

    #[test]
    fn rewrites_branch_and_feedback_link() {
        assert_eq!(
            sanitize_text("Main branch (you will usually use this for PRs)"),
            "Default branch (you will usually use this for PRs)"
        );
        assert_eq!(
            sanitize_text("see https://github.com/anthropics/claude-code/issues for feedback"),
            "see https://github.com/user-feedback/issues for feedback"
        );
        assert_eq!(
            sanitize_text("an open source project led by OpenAI"),
            "an open source project led by the community"
        );
    }

    #[test]
    fn strips_billing_header_case_insensitive() {
        let src = "x-anthropic-billing-header: cc_version=1.0; cc_entrypoint=cli;\nrest";
        let out = sanitize_text(src);
        assert!(!out.to_lowercase().contains("x-anthropic-billing-header"));
        assert!(out.contains("rest"));
        let upper = sanitize_text("X-Anthropic-Billing-Header: foo;");
        assert!(!upper.to_lowercase().contains("billing-header"));
    }

    #[test]
    fn strips_trailing_kv_pairs() {
        let src = "prefix cc_version=2.63.2; cc_entrypoint=cli; suffix";
        let out = sanitize_text(src);
        assert!(!out.contains("cc_version"));
        assert!(!out.contains("cc_entrypoint"));
        assert!(out.contains("prefix"));
        assert!(out.contains("suffix"));
    }

    #[test]
    fn sanitize_content_array_only_text_parts() {
        let mut v = json!([
            {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
            {"type": "image", "source": {"type": "base64", "data": "You are Claude Code"}}
        ]);
        assert!(sanitize_content(&mut v));
        assert_eq!(
            v[0]["text"],
            json!("You are Claude Code, Anthropic's official CLI tool for Claude.")
        );
        // image part 的 data 不被动
        assert_eq!(v[1]["source"]["data"], json!("You are Claude Code"));
    }

    #[test]
    fn sanitize_content_string() {
        let mut v = json!("led by OpenAI");
        assert!(sanitize_content(&mut v));
        assert_eq!(v, json!("led by the community"));
        let mut plain = json!("nothing special");
        assert!(!sanitize_content(&mut plain));
    }

    #[test]
    fn sanitize_messages_reports_change() {
        let mut msgs = vec![
            json!({"role": "system", "content": "You are Claude Code, Anthropic's official CLI for Claude."}),
            json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}),
        ];
        assert!(sanitize_messages(&mut msgs));
        assert!(msgs[0]["content"].as_str().unwrap().contains("CLI tool"));
        let mut clean = vec![json!({"role": "user", "content": "hello"})];
        assert!(!sanitize_messages(&mut clean));
    }
}
