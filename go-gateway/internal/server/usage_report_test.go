package server

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/usage"
)

// usageHandler 构建带统计器的 handler（单账号 u1，上游返回 sseOK）。
func usageHandler(t *testing.T, key string) (*Handler, *usage.Stats) {
	t.Helper()
	up := newFakeUpstream(t, func(string) (int, string, bool) { return 200, sseOK, true })
	stats := usage.New("")
	h := NewHandler(Config{
		Pool:     testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999}),
		Upstream: up,
		APIKey:   key,
		Usage:    stats,
	})
	return h, stats
}

func usageRequest(t *testing.T, h *Handler, path string) map[string]any {
	t.Helper()
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("GET", path, nil))
	if rec.Code != 200 {
		t.Fatalf("GET %s code=%d body=%s", path, rec.Code, rec.Body)
	}
	var body map[string]any
	if err := json.Unmarshal(rec.Body.Bytes(), &body); err != nil {
		t.Fatalf("GET %s not json: %v", path, err)
	}
	return body
}

func TestUsageEndpointDisabledWithoutStats(t *testing.T) {
	up := newFakeUpstream(t, func(string) (int, string, bool) { return 200, sseOK, true })
	h := NewHandler(Config{
		Pool:     testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999}),
		Upstream: up,
	})
	body := usageRequest(t, h, "/usage")
	if body["enabled"] != false {
		t.Fatalf("未装配统计器时 enabled 应为 false: %v", body)
	}
}

func TestUsageRecordsChatAndMessagesRequests(t *testing.T) {
	h, _ := usageHandler(t, "")

	// 流式 chat：末帧 usage prompt=1/completion=1。
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.2","stream":true,"messages":[]}`)))
	if rec.Code != 200 {
		t.Fatalf("stream chat code=%d", rec.Code)
	}

	// 非流式 chat：本地聚合后 usage 同样来自上游。
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.2","messages":[]}`)))
	if rec.Code != 200 {
		t.Fatalf("sync chat code=%d body=%s", rec.Code, rec.Body)
	}

	// Anthropic Messages 入口：流式翻译路径也要记账。
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/messages",
		strings.NewReader(`{"model":"claude-sonnet-4-6","stream":true,"max_tokens":16,"messages":[{"role":"user","content":"hi"}]}`)))
	if rec.Code != 200 {
		t.Fatalf("messages code=%d body=%s", rec.Code, rec.Body)
	}

	body := usageRequest(t, h, "/usage")
	if body["enabled"] != true {
		t.Fatalf("enabled 应为 true: %v", body)
	}
	summary := body["summary"].(map[string]any)
	if got := numOf(summary["records"]); got != 3 {
		t.Fatalf("records 应为 3，实际 %d (%v)", got, summary)
	}
	if got := numOf(summary["input"]); got != 3 {
		t.Fatalf("input 应为 3，实际 %d", got)
	}
	if got := numOf(summary["output"]); got != 3 {
		t.Fatalf("output 应为 3，实际 %d", got)
	}

	// chat 两次（glm-5.2）与 messages 一次（客户端请求名 claude-sonnet-4-6）各成一组。
	models := body["models"].([]any)
	if len(models) != 2 {
		t.Fatalf("应有 2 个模型分组，实际 %d: %v", len(models), models)
	}
	first := models[0].(map[string]any)
	if first["key"] != "glm-5.2" {
		t.Fatalf("模型分组应按用量降序、首位 glm-5.2: %v", first)
	}
	if got := numOf(first["records"]); got != 2 {
		t.Fatalf("glm-5.2 应有 2 条记录，实际 %d", got)
	}
	accounts := body["accounts"].([]any)
	if len(accounts) != 1 || accounts[0].(map[string]any)["key"] != "u1" {
		t.Fatalf("账号分组不符: %v", accounts)
	}
}

func TestUsageDaysFilterKeepsInRangeOnly(t *testing.T) {
	h, stats := usageHandler(t, "")

	// 40 天前的一笔历史记录，只应出现在「全部」快照里。
	stats.RecordAt("u1", "glm-5.2", time.Now().AddDate(0, 0, -40), usage.Counters{Input: 1000, Output: 100})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"glm-5.2","stream":true,"messages":[]}`)))
	if rec.Code != 200 {
		t.Fatalf("chat code=%d", rec.Code)
	}

	all := usageRequest(t, h, "/usage")
	if got := numOf(all["summary"].(map[string]any)["input"]); got != 1001 {
		t.Fatalf("全部范围 input 应为 1001，实际 %d", got)
	}
	if all["rangeDays"] != nil {
		t.Fatalf("全部范围 rangeDays 应为 null: %v", all["rangeDays"])
	}

	week := usageRequest(t, h, "/usage?days=7")
	if got := numOf(week["summary"].(map[string]any)["input"]); got != 1 {
		t.Fatalf("近 7 天 input 应为 1，实际 %d", got)
	}
	if week["rangeDays"] != float64(7) {
		t.Fatalf("rangeDays 应为 7: %v", week["rangeDays"])
	}
	// 非法 days 视为全部。
	odd := usageRequest(t, h, "/usage?days=abc")
	if got := numOf(odd["summary"].(map[string]any)["input"]); got != 1001 {
		t.Fatalf("非法 days 应回落到全部，实际 %d", got)
	}
}

func TestUsageEndpointRequiresAuth(t *testing.T) {
	h, _ := usageHandler(t, "sk-test")
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("GET", "/usage", nil))
	if rec.Code != http.StatusUnauthorized {
		t.Fatalf("无凭据应 401，实际 %d", rec.Code)
	}

	req := httptest.NewRequest("GET", "/usage", nil)
	req.Header.Set("Authorization", "Bearer sk-test")
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != 200 {
		t.Fatalf("带凭据应 200，实际 %d", rec.Code)
	}
}

func TestUsageRequestsEndpoint(t *testing.T) {
	h, stats := usageHandler(t, "")

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"deepseek-v4","stream":true,"messages":[]}`)))
	if rec.Code != 200 {
		t.Fatalf("chat code=%d", rec.Code)
	}

	body := usageRequest(t, h, "/usage/requests?days=1&limit=50")
	if body["enabled"] != true {
		t.Fatalf("enabled 应为 true: %v", body)
	}
	reqs, ok := body["requests"].([]any)
	if !ok || len(reqs) == 0 {
		t.Fatalf("requests 应有数据: %v", body)
	}
	first := reqs[0].(map[string]any)
	if first["model"] != "deepseek-v4" {
		t.Errorf("model 应为 deepseek-v4, 实际 %v", first["model"])
	}
	if first["entry"] != "chat" {
		t.Errorf("entry 应为 chat, 实际 %v", first["entry"])
	}

	// 禁用统计器时的行为
	hNoStats := NewHandler(Config{
		Pool: testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999}),
	})
	noStatsBody := usageRequest(t, hNoStats, "/usage/requests")
	if noStatsBody["enabled"] != false {
		t.Fatalf("未装配统计器时 enabled 应为 false: %v", noStatsBody)
	}
	_ = stats
}

