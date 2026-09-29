package server

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"

	"workbuddy2api/internal/auth"
)

// realContentSafetyBody 现场真实 11140 响应体。
const realContentSafetyBody = `{"code":11140,"msg":"request illegal",` +
	`"requestId":"957ad95b-7294-4a5d-a630-4fa57847455d",` +
	`"displayMsg":{"en":"The content did not pass the safety review. Please adjust and retry.",` +
	`"zh":"内容未通过安全审查，请调整后重试。"}}`

// TestContentSafetyDoesNotRotateAccounts 核心回归：安全审查失败只打上游一次，不轮换重试。
func TestContentSafetyDoesNotRotateAccounts(t *testing.T) {
	var calls int32
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		atomic.AddInt32(&calls, 1)
		return 403, realContentSafetyBody, false
	})
	p := testPoolWith(
		&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u2", AccessToken: "at2", ExpiresAt: 9999999999},
		&auth.Auth{UID: "u3", AccessToken: "at3", ExpiresAt: 9999999999},
	)
	h := NewHandler(Config{Pool: p, Upstream: up, MaxRotate: 3})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"sensitive"}]}`)))

	if n := atomic.LoadInt32(&calls); n != 1 {
		t.Errorf("安全审查应只打上游 1 次（换号无用），实际 %d 次", n)
	}
}

// TestContentSafetyReturnsContentFilterAndNo403InMessage 核心回归：返回 400 content_filter，消息中绝不带 403。
func TestContentSafetyReturnsContentFilterAndNo403InMessage(t *testing.T) {
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		return 403, realContentSafetyBody, false
	})
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	h := NewHandler(Config{Pool: p, Upstream: up})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/chat/completions",
		strings.NewReader(`{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"test"}]}`)))

	if rec.Code != http.StatusBadRequest {
		t.Errorf("期望 HTTP 400，实际 %d", rec.Code)
	}

	var res struct {
		Error struct {
			Code    string `json:"code"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &res); err != nil {
		t.Fatalf("响应不是合法的 JSON: %v", err)
	}

	if res.Error.Code != "content_filter" {
		t.Errorf("错误码期望 content_filter，实际 %q", res.Error.Code)
	}

	if !strings.Contains(res.Error.Message, "安全审查") && !strings.Contains(res.Error.Message, "安全审核") {
		t.Errorf("错误信息应包含安全审核说明，实际 %q", res.Error.Message)
	}

	// 严防 403 / 401 泄漏导致下游客户端（如 DSH）误判为 AUTH
	if strings.Contains(res.Error.Message, "403") || strings.Contains(res.Error.Message, "401") {
		t.Errorf("错误信息中不得包含 403/401 字符，防止客户端误诊为 AUTH: %q", res.Error.Message)
	}
}

// TestContentSafetyAnthropicReturnsInvalidRequestError 验证 Anthropic 入口映射为 invalid_request_error。
func TestContentSafetyAnthropicReturnsInvalidRequestError(t *testing.T) {
	up := newFakeUpstream(t, func(string) (int, string, bool) {
		return 403, realContentSafetyBody, false
	})
	p := testPoolWith(&auth.Auth{UID: "u1", AccessToken: "at1", ExpiresAt: 9999999999})
	h := NewHandler(Config{Pool: p, Upstream: up})

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/v1/messages",
		strings.NewReader(`{"model":"claude-3-5-sonnet-20241022","max_tokens":100,"messages":[{"role":"user","content":"test"}]}`)))

	if rec.Code != http.StatusBadRequest {
		t.Errorf("期望 HTTP 400，实际 %d", rec.Code)
	}

	var res struct {
		Error struct {
			Type    string `json:"type"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &res); err != nil {
		t.Fatalf("响应不是合法的 JSON: %v", err)
	}

	if res.Error.Type != "invalid_request_error" {
		t.Errorf("Anthropic 错误类型期望 invalid_request_error，实际 %q", res.Error.Type)
	}

	if strings.Contains(res.Error.Message, "403") {
		t.Errorf("错误信息中不得包含 403 字符: %q", res.Error.Message)
	}
}
