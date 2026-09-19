//! 构造四类上游请求头（common / chat / billing / refresh）。
//!
//! 对应 Go 源文件 `internal/upstream/headers.go`。
//! 规则来自 docs/api-reference.md §0/§4/§6。

use reqwest::RequestBuilder;

use crate::auth::Auth;

const CLIENT_UA: &str = "CLI/2.63.2 CodeBuddy/2.63.2";
const ORIGIN_REFERER_CN: &str = "https://www.codebuddy.cn";
const ORIGIN_REFERER_INTL: &str = "https://www.workbuddy.ai";

/// 返回该账号所属区域对应的 Origin/Referer。
///
/// 上游按 Origin 判定来源区域，跨区域发送会被拒或落到错误的服务，
/// 因此必须跟随账号区域，不能恒为国服。
pub fn origin_referer_for(a: &Auth) -> &'static str {
    if a.is_intl() {
        ORIGIN_REFERER_INTL
    } else {
        ORIGIN_REFERER_CN
    }
}

/// 设置所有 API 共享的请求头。
pub fn apply_common(req: RequestBuilder, a: &Auth) -> RequestBuilder {
    let origin = origin_referer_for(a);
    req.header("Content-Type", "application/json")
        .header("Accept", "application/json, text/plain, */*")
        .header("X-Requested-With", "XMLHttpRequest")
        .header("Origin", origin)
        .header("Referer", format!("{origin}/"))
        .header("User-Agent", CLIENT_UA)
}

/// 在 common 之上加 chat 专属的账号头。
/// 缺省字段用 X-No-* 约定（与 CodeBuddy 官方 CLI 一致）。
pub fn apply_chat(req: RequestBuilder, a: &Auth) -> RequestBuilder {
    let req = apply_common(req, a);
    let req = if !a.access_token.is_empty() {
        req.header("Authorization", format!("Bearer {}", a.access_token))
    } else {
        req.header("X-No-Authorization", "1")
    };
    let req = if !a.uid.is_empty() {
        req.header("X-User-Id", &a.uid)
    } else {
        req.header("X-No-User-Id", "1")
    };
    let req = if !a.enterprise_id.is_empty() {
        req.header("X-Enterprise-Id", &a.enterprise_id)
    } else {
        req.header("X-No-Enterprise-Id", "1")
    };
    // 安全红线：绝不在 chat 请求里携带 X-Refresh-Token。
    let req = if !a.domain.is_empty() {
        req.header("X-Domain", &a.domain)
    } else {
        req.header("X-No-Department-Info", "1")
    };
    req.header("X-Product", "SaaS")
}

/// billing 接口请求头。
pub fn apply_billing(req: RequestBuilder, a: &Auth) -> RequestBuilder {
    let mut req = req
        .header("Authorization", format!("Bearer {}", a.access_token))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json");
    if !a.uid.is_empty() {
        req = req.header("X-User-Id", &a.uid);
    }
    if !a.enterprise_id.is_empty() {
        req = req
            .header("X-Enterprise-Id", &a.enterprise_id)
            .header("X-Tenant-Id", &a.enterprise_id);
    }
    if !a.domain.is_empty() {
        req = req.header("X-Domain", &a.domain);
    }
    req
}

/// refresh 端点专属头（X-Refresh-Token 只允许出现在这里）。
pub fn apply_refresh(req: RequestBuilder, a: &Auth) -> RequestBuilder {
    let req = apply_common(req, a).header("X-Refresh-Token", &a.refresh_token);
    let req = if !a.enterprise_id.is_empty() {
        req.header("X-Enterprise-Id", &a.enterprise_id)
    } else {
        req
    };
    req.header("X-Auth-Refresh-Source", "workbuddy")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(domain: &str, token: &str, uid: &str) -> Auth {
        Auth {
            domain: domain.to_string(),
            access_token: token.to_string(),
            uid: uid.to_string(),
            ..Default::default()
        }
    }

    fn build(a: &Auth) -> reqwest::RequestBuilder {
        apply_chat(
            reqwest::Client::new().post("https://example.invalid/v2/chat/completions"),
            a,
        )
    }

    #[test]
    fn chat_headers_region_routing() {
        let cn = build(&auth("xxx.codebuddy.cn", "t", "u"));
        assert_eq!(cn.headers()["origin"], "https://www.codebuddy.cn");
        let intl = build(&auth("xxx.workbuddy.ai", "t", "u"));
        assert_eq!(intl.headers()["origin"], "https://www.workbuddy.ai");
    }

    #[test]
    fn chat_headers_no_fallback_markers() {
        let a = Auth::default();
        let h = build(&a).headers().clone();
        assert_eq!(h["x-no-authorization"], "1");
        assert_eq!(h["x-no-user-id"], "1");
        assert_eq!(h["x-no-enterprise-id"], "1");
        assert_eq!(h["x-no-department-info"], "1");
        assert_eq!(h["x-product"], "SaaS");
        assert!(h.get("authorization").is_none(), "缺 token 时不得携带 Authorization");
    }

    #[test]
    fn chat_headers_with_values() {
        let h = build(&auth("codebuddy.cn", "tok", "uid1")).headers().clone();
        assert_eq!(h["authorization"], "Bearer tok");
        assert_eq!(h["x-user-id"], "uid1");
        assert_eq!(h["user-agent"], CLIENT_UA);
        assert_eq!(h["referer"], "https://www.codebuddy.cn/");
    }

    #[test]
    fn billing_headers_tenant() {
        let a = Auth { access_token: "t".into(), enterprise_id: "e1".into(), ..Default::default() };
        let h = apply_billing(
            reqwest::Client::new().post("https://example.invalid"),
            &a,
        )
        .headers()
        .clone();
        assert_eq!(h["authorization"], "Bearer t");
        assert_eq!(h["x-enterprise-id"], "e1");
        assert_eq!(h["x-tenant-id"], "e1");
        assert!(h.get("x-user-id").is_none());
    }

    #[test]
    fn refresh_headers_carry_refresh_token() {
        let a = Auth { refresh_token: "rt".into(), ..Default::default() };
        let h = apply_refresh(
            reqwest::Client::new().post("https://example.invalid"),
            &a,
        )
        .headers()
        .clone();
        assert_eq!(h["x-refresh-token"], "rt");
        assert_eq!(h["x-auth-refresh-source"], "workbuddy");
    }
}
