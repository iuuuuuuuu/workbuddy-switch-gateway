//! 处理上游 SSE 流：聚合成单个 OpenAI 响应，或逐帧规范化后透传给客户端。
//!
//! 对应 Go 源文件 `internal/upstream/sse.go`（聚合 / normalizeFrame / Stream）
//! 与 `internal/upstream/idle.go`（流中空闲监控——Go 用后台 goroutine + cancel
//! 实现，Rust 侧以「每次读块套 timeout」等价实现，空闲即断流并报错）。

use std::collections::BTreeMap;
use std::time::Duration;

use bytes::Bytes;
use serde_json::{Map, Value};
use tokio::sync::mpsc::Sender;

use super::client::ClientError;

/// SSE 透传响应头（Go 侧 Stream() 自设，axum 侧由 server 组装响应时应用）。
pub const SSE_HEADERS: [(&str, &str); 4] = [
    ("Content-Type", "text/event-stream"),
    ("Cache-Control", "no-cache"),
    ("Connection", "keep-alive"),
    ("X-Accel-Buffering", "no"),
];

/// 逐行读取 SSE 流。
///
/// Go 用 `bufio.Reader.ReadString('\n')`；此处以「读块 → 缓冲 → 找 \n」等价实现，
/// 半行/分片由缓冲自然处理。空闲超时由每次读块的 timeout 承担。
struct LineReader {
    resp: reqwest::Response,
    idle: Duration,
    buf: Vec<u8>,
    eof: bool,
}

impl LineReader {
    fn new(resp: reqwest::Response, idle: Duration) -> Self {
        Self { resp, idle, buf: Vec::new(), eof: false }
    }

    async fn next_chunk(&mut self) -> Result<Option<Bytes>, ClientError> {
        let fut = self.resp.chunk();
        if self.idle.is_zero() {
            fut.await
                .map_err(|e| ClientError::Transport(e.to_string()))
        } else {
            match tokio::time::timeout(self.idle, fut).await {
                Ok(r) => r.map_err(|e| ClientError::Transport(e.to_string())),
                Err(_) => Err(ClientError::IdleTimeout(self.idle)),
            }
        }
    }

    /// 返回 `(原始行含换行, 去尾行)`；None 表示流结束。
    async fn next_line(&mut self) -> Result<Option<(String, String)>, ClientError> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = self.buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&raw).into_owned();
                // 与 Go strings.TrimRight(line, "\r\n") 一致：去掉全部尾部 \r 与 \n
                let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
                return Ok(Some((line, trimmed)));
            }
            if self.eof {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                let raw = std::mem::take(&mut self.buf);
                let line = String::from_utf8_lossy(&raw).into_owned();
                let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
                return Ok(Some((line, trimmed)));
            }
            match self.next_chunk().await? {
                Some(chunk) => self.buf.extend_from_slice(&chunk),
                None => self.eof = true,
            }
        }
    }
}

/// 读取完整 SSE 流，聚合 delta.content 为单个 OpenAI chat.completion 响应。
/// 分片/半行由内部缓冲处理；遇到 "data: [DONE]" 结束。
/// tool_calls 以流式 delta 到达（按 index 合并：首片带 id/type/name，后续只带 arguments 片段）。
pub async fn aggregate(
    resp: reqwest::Response,
    idle: Duration,
) -> Result<Value, ClientError> {
    let mut lr = LineReader::new(resp, idle);
    let mut id = String::new();
    let mut model = String::new();
    let mut created = 0f64;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut role = "assistant".to_string();
    let mut finish_reason = "stop".to_string();
    let mut usage: Option<Value> = None;
    let mut got_any_content = false;
    let mut valid_events = 0usize;
    let mut tool_calls: BTreeMap<i64, Value> = BTreeMap::new();

    while let Some((_, line)) = lr.next_line().await? {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        if payload == "[DONE]" {
            // 上游显式结束：停止读取，DONE 之后的任何数据一律忽略。
            break;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            // 解析失败沿用静默 continue（不计有效事件）
            continue;
        };
        // 有效事件计数：仅 JSON 解析成功的数据帧计入。
        valid_events += 1;
        let obj = chunk.as_object().unwrap();
        if id.is_empty() {
            if let Some(v) = obj.get("id").and_then(Value::as_str) {
                id = v.to_string();
            }
        }
        if model.is_empty() {
            if let Some(v) = obj.get("model").and_then(Value::as_str) {
                model = v.to_string();
            }
        }
        if let Some(v) = obj.get("created").and_then(Value::as_f64) {
            if created == 0.0 {
                created = v;
            }
        }
        if let Some(u) = obj.get("usage") {
            if u.is_object() {
                usage = Some(u.clone());
            }
        }
        let Some(choices) = obj.get("choices").and_then(Value::as_array) else {
            continue;
        };
        for ci in choices {
            let Some(c) = ci.as_object() else { continue };
            if let Some(fr) = c.get("finish_reason").and_then(Value::as_str) {
                if !fr.is_empty() {
                    finish_reason = fr.to_string();
                }
            }
            if let Some(delta) = c.get("delta").and_then(Value::as_object) {
                if let Some(r2) = delta.get("role").and_then(Value::as_str) {
                    if !r2.is_empty() {
                        role = r2.to_string();
                    }
                }
                if let Some(txt) = delta.get("content").and_then(Value::as_str) {
                    content.push_str(txt);
                    // 只有**非空**正文才锁死 message 回退路径：
                    // 首帧常带 "content":""（仅含 role 的保活/开场帧），
                    // 若空串也置位，后续「完整消息放在 message 里」的上游
                    // 形态就再也读不到内容，客户端只会收到空回复。
                    if !txt.is_empty() {
                        got_any_content = true;
                    }
                }
                if let Some(rc) = delta.get("reasoning_content").and_then(Value::as_str) {
                    reasoning.push_str(rc);
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(Value::as_array) {
                    for tc in tcs {
                        let Some(call) = tc.as_object() else { continue };
                        // index 必须是数字：上游偶发把它发成 JSON 字符串（"0"/"1"），
                        // 只认数字会让所有调用落到槽位 0，多个并行工具调用被合并成一条。
                        let idx = index_of_tool_call(call);
                        let merged = tool_calls
                            .entry(idx)
                            .or_insert_with(|| serde_json::json!({ "index": idx }));
                        merge_tool_call_delta(merged, call);
                    }
                }
            }
            // 有的上游把完整消息放在 message 里（非 delta）
            if !got_any_content {
                if let Some(txt) = c
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_str)
                {
                    content.push_str(txt);
                }
            }
        }
    }
    if valid_events == 0 {
        // 上游返回 200 但没有任何有效数据事件（空流/只有 [DONE]/只有注释行）：
        // 不再合成空 content 的假成功响应，直接报错，由 handler 映射为 502 upstream_parse。
        return Err(ClientError::Parse(
            "upstream stream contained no valid data events".to_string(),
        ));
    }
    if id.is_empty() {
        id = format!("chatcmpl-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0));
    }
    if created == 0.0 {
        created = chrono::Utc::now().timestamp() as f64;
    }
    let mut message = serde_json::json!({ "role": role, "content": content });
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    if !tool_calls.is_empty() {
        // BTreeMap 按 index 升序输出（Go 侧 sortInts + 逐个 append 等价）
        message["tool_calls"] = Value::Array(tool_calls.values().cloned().collect());
    }
    let mut resp_obj = serde_json::json!({
        "id": id,
        "object": "chat.completion",
        "created": created as i64,
        "model": model,
        "choices": [
            {
                "index": 0,
                "message": message,
                "finish_reason": finish_reason,
            }
        ],
    });
    if let Some(u) = usage {
        resp_obj["usage"] = u;
    }
    Ok(resp_obj)
}

/// 取 tool_call 分片的 index。
///
/// 兼容三种上游形态（实测均出现过）：
/// - 数字：{"index":0}          （标准）
/// - 字符串数字：{"index":"0"}  （部分上游把 int 序列化成字符串）
/// - 缺省：无 index 字段        （视为单调用，回落 0）
///
/// 为什么必须兼容字符串：只认数字时字符串 index 会静默变成 0，
/// 使多个并行工具调用全部合并进槽位 0——名字被后者覆盖、参数被拼接，
/// 客户端拿到一个损坏的工具调用（见回归测试）。
fn index_of_tool_call(call: &Map<String, Value>) -> i64 {
    match call.get("index") {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok().filter(|&n| n >= 0).unwrap_or(0),
        _ => 0,
    }
}

/// 把流式 tool_call 片段合并到累计对象：
/// id/type/function.name 直覆盖（后续分片通常缺省），function.arguments 拼接。
fn merge_tool_call_delta(merged: &mut Value, delta: &Map<String, Value>) {
    let m = merged.as_object_mut().unwrap();
    if let Some(v) = delta.get("id").and_then(Value::as_str) {
        if !v.is_empty() {
            m.insert("id".to_string(), Value::String(v.to_string()));
        }
    }
    if let Some(v) = delta.get("type").and_then(Value::as_str) {
        if !v.is_empty() {
            m.insert("type".to_string(), Value::String(v.to_string()));
        }
    }
    let Some(df) = delta.get("function").and_then(Value::as_object) else {
        return;
    };
    let mf = match m.get_mut("function").and_then(Value::as_object_mut) {
        Some(mf) => mf,
        None => {
            m.insert("function".to_string(), Value::Object(Map::new()));
            m.get_mut("function").and_then(Value::as_object_mut).unwrap()
        }
    };
    if let Some(v) = df.get("name").and_then(Value::as_str) {
        if !v.is_empty() {
            mf.insert("name".to_string(), Value::String(v.to_string()));
        }
    }
    if let Some(v) = df.get("arguments").and_then(Value::as_str) {
        if !v.is_empty() {
            let prev = mf.get("arguments").and_then(Value::as_str).unwrap_or("");
            mf.insert("arguments".to_string(), Value::String(format!("{prev}{v}")));
        }
    }
}

/// 以 OpenAI 流式规范白名单重建帧：仅保留标准字段，
/// 剔除上游噪声（finish_reason:"" → null、空 content/refusal、空 tool_calls 列表、
/// 空占位 function_call、顶层未知字段），空 delta 键一律省略，
/// usage 缺失 → null，保证任意标准客户端按规范解析。
pub fn normalize_frame(obj: &Value) -> Value {
    let empty = Map::new();
    let o = obj.as_object().unwrap_or(&empty);
    let mut out = Map::new();
    for k in ["id", "object", "created", "model", "system_fingerprint", "service_tier"] {
        if let Some(v) = o.get(k) {
            if !v.is_null() {
                out.insert(k.to_string(), v.clone());
            }
        }
    }
    out.entry("object".to_string())
        .or_insert_with(|| Value::String("chat.completion.chunk".to_string()));
    out.entry("id".to_string())
        .or_insert_with(|| Value::String("chatcmpl-wb2api".to_string()));
    if let Some(chs) = o.get("choices").and_then(Value::as_array) {
        let mut nchs = Vec::with_capacity(chs.len());
        for ci in chs {
            let Some(c) = ci.as_object() else { continue };
            let mut nc = Map::new();
            if let Some(idx) = c.get("index") {
                nc.insert("index".to_string(), idx.clone());
            }
            let mut delta = Map::new();
            if let Some(d) = c.get("delta").and_then(Value::as_object) {
                for k in ["role", "content", "reasoning_content", "refusal"] {
                    if let Some(v) = d.get(k).and_then(Value::as_str) {
                        if !v.is_empty() {
                            delta.insert(k.to_string(), Value::String(v.to_string()));
                        }
                    }
                }
                if let Some(tcs) = d.get("tool_calls").and_then(Value::as_array) {
                    if !tcs.is_empty() {
                        delta.insert("tool_calls".to_string(), Value::Array(tcs.clone()));
                    }
                }
                if let Some(fc) = d.get("function_call") {
                    if !fc.is_null() {
                        // 空占位 function_call（name/arguments 全空）视为噪声剔除
                        let keep = match fc.as_object() {
                            Some(m) => {
                                let n = m.get("name").and_then(Value::as_str).unwrap_or("");
                                let a = m.get("arguments").and_then(Value::as_str).unwrap_or("");
                                !(n.is_empty() && a.is_empty())
                            }
                            None => true,
                        };
                        if keep {
                            delta.insert("function_call".to_string(), fc.clone());
                        }
                    }
                }
            }
            nc.insert("delta".to_string(), Value::Object(delta));
            let fr = c.get("finish_reason").and_then(Value::as_str).unwrap_or("");
            nc.insert(
                "finish_reason".to_string(),
                if fr.is_empty() { Value::Null } else { Value::String(fr.to_string()) },
            );
            nchs.push(Value::Object(nc));
        }
        out.insert("choices".to_string(), Value::Array(nchs));
    }
    out.insert(
        "usage".to_string(),
        o.get("usage").cloned().unwrap_or(Value::Null),
    );
    // error 帧必须原样保留：上游常在 HTTP 200 的流中途发
    // {"error":{...}}（如 code=11128 渠道未批准、账号被封）来表示失败。
    // 白名单重建会把它降级成一个普通的 "chat.completion.chunk"，客户端于是
    // 把「截断的回答 + 正常 [DONE]」当成一次成功，永远不知道请求失败了。
    // 保留 error 字段让客户端/上层能识别终止性错误。
    if let Some(e) = o.get("error") {
        if !e.is_null() {
            out.insert("error".to_string(), e.clone());
        }
    }
    Value::Object(out)
}

/// 透传统计：有效转发的数据帧数。
#[derive(Debug, Default, Clone, Copy)]
pub struct StreamStats {
    pub valid_frames: usize,
}

/// 逐帧透传上游 SSE 到 `tx`（规范化后 flush），保证恰好写一个 [DONE]。
///
/// 对应 Go 侧 `Stream(w, r)`：状态码 200 与 SSE 头由调用方（axum 响应构造）设置；
/// 这里只负责帧处理。流式策略：逐帧透传（规范化已剥空 content 噪声），
/// 恢复与上游一致的平滑流式。
///
/// 返回的 `StreamStats.valid_frames == 0` 时以 Err 结束（空流），
/// 但此时错误帧与 [DONE] 已写出，客户端可正常收尾。
pub async fn pipe_stream(
    resp: reqwest::Response,
    idle: Duration,
    tx: Sender<Result<Bytes, std::io::Error>>,
) -> Result<StreamStats, ClientError> {
    async fn put(
        tx: &Sender<Result<Bytes, std::io::Error>>,
        data: &[u8],
    ) -> Result<(), ClientError> {
        tx.send(Ok(Bytes::copy_from_slice(data)))
            .await
            .map_err(|_| ClientError::Transport("client disconnected".to_string()))
    }

    let mut lr = LineReader::new(resp, idle);
    let mut valid_frames = 0usize;
    loop {
        let Some((raw, trimmed)) = lr.next_line().await? else {
            break;
        };
        if trimmed.starts_with("data: [DONE]") {
            // 上游显式结束：停止读取，DONE 之后的任何数据（含垃圾帧）一律不再透传。
            // [DONE] 统一在循环结束后写出，保证恰好一个。
            break;
        }
        if let Some(payload) = trimmed.strip_prefix("data: ") {
            // 仅 JSON 解析成功时计数记为一次有效转发（解析失败照常降级原样写出，但不计数）。
            let mut payload_out = payload.to_string();
            if let Ok(obj) = serde_json::from_str::<Value>(payload) {
                valid_frames += 1;
                if let Ok(norm) = serde_json::to_string(&normalize_frame(&obj)) {
                    payload_out = norm;
                }
            }
            let frame = format!("data: {payload_out}\n\n");
            put(&tx, frame.as_bytes()).await?;
        } else if !trimmed.is_empty() {
            // 注释/其他行：原样透传（raw 含原始换行）
            put(&tx, raw.as_bytes()).await?;
        }
        // 空行（帧分隔）吞掉：本函数自产 "\n\n"
    }
    // 空流（0 有效帧）：先写一帧 error（绕过 normalizeFrame 原样保留 error 字段），
    // 再补 [DONE] 保证客户端能正常收尾，并返回非 Ok 供调用方记录。
    if valid_frames == 0 {
        put(
            &tx,
            b"data: {\"error\":{\"message\":\"empty upstream stream\",\"type\":\"upstream_error\"}}\n\n",
        )
        .await?;
    }
    // 保证恰好写一个 [DONE]（上游漏发时兜底补上）。
    put(&tx, b"data: [DONE]\n\n").await?;
    drop(tx);
    if valid_frames == 0 {
        return Err(ClientError::Parse(
            "upstream stream contained no valid data events".to_string(),
        ));
    }
    Ok(StreamStats { valid_frames })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 用固定 SSE 文本构造一个 reqwest::Response（借 axum/tower 本地服务）。
    /// 这里改用更轻的方式：直接把文本喂给等价的同步解析路径不可行，
    /// 因此用本地 hyper 服务回放。测试辅助：启动一次性 axum 服务。
    async fn serve(body: &'static str) -> reqwest::Response {
        use axum::routing::get;
        let app = axum::Router::new().route(
            "/",
            get(move || async move {
                axum::http::Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .body(body.to_string())
                    .unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        reqwest::get(format!("http://{addr}/")).await.unwrap()
    }

    #[tokio::test]
    async fn aggregate_merges_deltas_and_tool_calls() {
        let resp = serve(concat!(
            "data: {\"id\":\"c1\",\"model\":\"m\",\"created\":100,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"Hel\",\"tool_calls\":[{\"index\":0,\"id\":\"call1\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\":\"}}]}}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"lo\",\"tool_calls\":[{\"index\":\"0\",\"function\":{\"arguments\":\"1\\\"}\"}},{\"index\":1,\"id\":\"call2\",\"type\":\"function\",\"function\":{\"name\":\"g\",\"arguments\":\"{}\"}}]}}],\"usage\":{\"total_tokens\":9}}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        let out = aggregate(resp, Duration::ZERO).await.unwrap();
        assert_eq!(out["id"], json!("c1"));
        assert_eq!(out["object"], json!("chat.completion"));
        assert_eq!(out["created"], json!(100));
        assert_eq!(out["model"], json!("m"));
        assert_eq!(out["choices"][0]["message"]["content"], json!("Hello"));
        assert_eq!(out["choices"][0]["finish_reason"], json!("tool_calls"));
        let calls = out["choices"][0]["message"]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2, "字符串 index 不应把两个调用并进槽位 0");
        assert_eq!(calls[0]["id"], json!("call1"));
        assert_eq!(calls[0]["function"]["name"], json!("f"));
        assert_eq!(calls[0]["function"]["arguments"], json!("{\"a\":1}"));
        assert_eq!(calls[1]["id"], json!("call2"));
        assert_eq!(out["usage"]["total_tokens"], json!(9));
    }

    #[tokio::test]
    async fn aggregate_message_fallback_only_without_content() {
        // 首帧 content 为空 → 未锁死回退路径 → message.content 计入
        let resp = serve(concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"full\"}}]}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        let out = aggregate(resp, Duration::ZERO).await.unwrap();
        assert_eq!(out["choices"][0]["message"]["content"], json!("full"));

        // 已有非空 delta content → 不再回退
        let resp = serve(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"real\"}}]}\n\n",
            "data: {\"choices\":[{\"message\":{\"content\":\"junk\"}}]}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        let out = aggregate(resp, Duration::ZERO).await.unwrap();
        assert_eq!(out["choices"][0]["message"]["content"], json!("real"));
    }

    #[tokio::test]
    async fn aggregate_reasoning_and_defaults() {
        let resp = serve("data: {\"choices\":[{\"delta\":{\"content\":\"x\",\"reasoning_content\":\"think\"}}]}\n\ndata: [DONE]\n\n").await;
        let out = aggregate(resp, Duration::ZERO).await.unwrap();
        assert_eq!(out["choices"][0]["message"]["reasoning_content"], json!("think"));
        assert!(out["id"].as_str().unwrap().starts_with("chatcmpl-"));
        assert!(out["created"].as_i64().unwrap() > 0);
        assert_eq!(out["choices"][0]["finish_reason"], json!("stop"));
        assert_eq!(out["choices"][0]["message"]["role"], json!("assistant"));
    }

    #[tokio::test]
    async fn aggregate_empty_stream_errors() {
        let resp = serve("data: [DONE]\n\n").await;
        let err = aggregate(resp, Duration::ZERO).await.unwrap_err();
        assert!(err.to_string().contains("no valid data events"));

        let resp = serve(": keep-alive comment\n\n").await;
        assert!(aggregate(resp, Duration::ZERO).await.is_err());
    }

    #[test]
    fn normalize_frame_whitelist() {
        let frame = json!({
            "id": "c1", "object": "chat.completion.chunk", "created": 1, "model": "m",
            "junk_top": true,
            "choices": [{"index": 0, "junk": 1, "delta": {"role": "assistant", "content": "", "reasoning_content": "r", "refusal": "", "tool_calls": [], "function_call": {"name": "", "arguments": ""}}, "finish_reason": ""}],
            "usage": null
        });
        let out = normalize_frame(&frame);
        assert!(out.get("junk_top").is_none());
        let ch = &out["choices"][0];
        assert!(ch.get("junk").is_none());
        let delta = &ch["delta"];
        assert_eq!(delta["role"], json!("assistant"));
        assert_eq!(delta["reasoning_content"], json!("r"));
        assert!(delta.get("content").is_none(), "空 content 应剔除");
        assert!(delta.get("refusal").is_none());
        assert!(delta.get("tool_calls").is_none(), "空 tool_calls 列表应剔除");
        assert!(delta.get("function_call").is_none(), "空占位 function_call 应剔除");
        assert_eq!(ch["finish_reason"], Value::Null, "空 finish_reason 应为 null");
        assert_eq!(out["usage"], Value::Null, "usage 缺失应为 null");
    }

    #[test]
    fn normalize_frame_defaults_and_error_passthrough() {
        let out = normalize_frame(&json!({"choices": []}));
        assert_eq!(out["object"], json!("chat.completion.chunk"));
        assert_eq!(out["id"], json!("chatcmpl-wb2api"));

        let out = normalize_frame(&json!({"error": {"code": 11128, "message": "denied"}}));
        assert_eq!(out["error"]["code"], json!(11128));

        let out = normalize_frame(&json!({"function_call_frame": 0, "choices": [{"delta": {"function_call": {"name": "f"}}}]}));
        assert_eq!(out["choices"][0]["delta"]["function_call"]["name"], json!("f"));
    }

    #[tokio::test]
    async fn pipe_stream_normalizes_and_terminates() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
        let resp = serve(concat!(
            ": comment\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "junk line\n",
            "data: [DONE]\n\n",
            "data: {\"after\":\"done\"}\n\n",
        ))
        .await;
        let handle = tokio::spawn(pipe_stream(resp, Duration::ZERO, tx));
        let mut buf = String::new();
        while let Some(Ok(b)) = rx.recv().await {
            buf.push_str(&String::from_utf8_lossy(&b));
        }
        let stats = handle.await.unwrap().unwrap();
        assert_eq!(stats.valid_frames, 1, "空 content 帧规范化后仍是一次有效转发");
        assert!(buf.contains(": comment\n"), "注释行原样透传");
        assert!(buf.contains("junk line\n"));
        assert!(!buf.contains("\"content\":\"\""), "空 content 应被剥掉");
        assert!(buf.contains("\"content\":\"hi\""));
        assert_eq!(buf.matches("data: [DONE]").count(), 1, "恰好一个 [DONE]");
        assert!(!buf.contains("\"after\""), "DONE 之后不再透传");
    }

    #[tokio::test]
    async fn pipe_stream_empty_injects_error_frame() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let resp = serve("data: [DONE]\n\n").await;
        let handle = tokio::spawn(pipe_stream(resp, Duration::ZERO, tx));
        let mut buf = String::new();
        while let Some(Ok(b)) = rx.recv().await {
            buf.push_str(&String::from_utf8_lossy(&b));
        }
        assert!(handle.await.unwrap().is_err(), "空流应报 no valid events");
        assert!(buf.contains("\"empty upstream stream\""));
        assert_eq!(buf.matches("data: [DONE]").count(), 1);
    }

    #[tokio::test]
    async fn pipe_stream_mid_stream_error_frame_survives() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let resp = serve(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"part\"}}]}\n\n",
            "data: {\"error\":{\"code\":11128,\"message\":\"channel not approved\"}}\n\n",
            "data: [DONE]\n\n",
        ))
        .await;
        let handle = tokio::spawn(pipe_stream(resp, Duration::ZERO, tx));
        let mut buf = String::new();
        while let Some(Ok(b)) = rx.recv().await {
            buf.push_str(&String::from_utf8_lossy(&b));
        }
        assert!(handle.await.unwrap().is_ok());
        assert!(
            buf.contains("\"error\":{\"code\":11128"),
            "HTTP 200 流中途的 error 帧必须保留"
        );
    }

    #[tokio::test]
    async fn aggregate_tolerates_half_lines() {
        // 半行/分片：TCP 分片把一行拆成多块 —— LineReader 缓冲必须拼回
        let resp = serve(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n",
            "data: [DONE]\n",
        ))
        .await;
        let out = aggregate(resp, Duration::ZERO).await.unwrap();
        assert_eq!(out["choices"][0]["message"]["content"], json!("ab"));
    }

    #[test]
    fn index_of_tool_call_forms() {
        let m = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(index_of_tool_call(&m(json!({"index": 2}))), 2);
        assert_eq!(index_of_tool_call(&m(json!({"index": "3"}))), 3);
        assert_eq!(index_of_tool_call(&m(json!({}))), 0);
        assert_eq!(index_of_tool_call(&m(json!({"index": "x"}))), 0);
        assert_eq!(index_of_tool_call(&m(json!({"index": -1}))), 0, "负数不合法回落 0");
    }
}
