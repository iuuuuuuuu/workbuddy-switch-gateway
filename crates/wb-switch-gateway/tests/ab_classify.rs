//! A/B 对照：上游错误分类必须与 Go 侧 `upstream.Classify` 完全一致。
//!
//! 用例矩阵由临时 Go 探针（`go-gateway/cmd/probe-classify`，用后即删）对**当前**
//! Go 实现运行生成，固化在 `ab_classify_matrix.tsv`：
//! 每行 `status \t body(JSON 编码) \t 期望 kind`。
//! Go 侧分类规则演进（新增余额文案、模型级限流 6004、上下文超长 11115、
//! 嵌套额度码 14018 等）后需重跑探针更新矩阵。

/// 用例矩阵（由 Go 探针生成，61 例）。
const MATRIX: &str = include_str!("ab_classify_matrix.tsv");

#[test]
fn classify_matches_go_matrix() {
    let mut count = 0;
    for line in MATRIX.lines() {
        if line.trim().is_empty() {
            continue;
        }
        count += 1;
        let mut it = line.splitn(3, '\t');
        let status: u16 = it.next().expect("缺 status 列").parse().expect("status 非数字");
        // body 以 JSON 字符串编码存储，解码后即是原始响应体
        let body: String =
            serde_json::from_str(it.next().expect("缺 body 列")).expect("body 列不是合法 JSON 字符串");
        let want = it.next().expect("缺期望列");
        let got = wb_switch_gateway::upstream::classify(status, &body).as_str();
        assert_eq!(got, want, "status={status} body={body:?}");
    }
    assert!(count >= 60, "矩阵行数异常: {count}");
}

/// 模型级限流（6004）与上下文超长（11115）等新分类在矩阵中必须有覆盖，
/// 防止矩阵被误回退成旧版（缺失这些条目）。
#[test]
fn matrix_covers_new_kinds() {
    assert!(MATRIX.contains("model_rate"));
    assert!(MATRIX.contains("context_too_long"));
    assert!(MATRIX.contains("insufficient credits"), "复数形态文案必须有覆盖");
}
