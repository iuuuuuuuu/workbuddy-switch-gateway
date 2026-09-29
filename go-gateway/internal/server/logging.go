// logging.go 请求级表格日志：每个 /v1/chat/completions 请求结束后打印一行到 stdout。
package server

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"strings"
	"sync/atomic"
	"time"

	"workbuddy2api/internal/usage"
)

// chatSeq 进程级请求序号。
var chatSeq atomic.Int64

// chatLogEnabled 聊天表格日志总开关。生产恒 true；
// 测试包经 TestMain 置 false 关闭 stdout 噪音，需要断言行输出的测试用 withChatLog 临时开启（R5）。
var chatLogEnabled = true

// chatStat 单个 chat 请求的日志统计；handler 挂 defer，请求出口后落一行。
type chatStat struct {
	seq    int64
	start  time.Time
	model  string
	entry  string // "chat" | "messages" | "responses"
	stream bool
	mode   string // "stream" | "sync"
	uid    string // 完整 uid，展示时只取前 8 位
	ttfb   time.Duration
	toks   int // <0 表示 usage 缺失 → 显示 "-"
	status int

	// counters/hasCounters 为网关 Token 用量统计的采集结果（与日志字段解耦：
	// 日志只关心 output，统计需要完整的输入/输出/缓存计量）。
	counters    usage.Counters
	hasCounters bool

	logged bool
}

// setCounters 记录一次可用的完整计量。
func (s *chatStat) setCounters(c usage.Counters) {
	s.counters = c
	s.hasCounters = true
}

// setUsageMap 从上游 usage 对象提取统计计量；字段不可识别时忽略。
func (s *chatStat) setUsageMap(u map[string]any) {
	if c, ok := usage.ParseOpenAIUsage(u); ok {
		s.setCounters(c)
	}
}

// newChatStat 以请求进入 handler 的时刻为起点构造统计对象；toks 默认 -1（usage 缺失）。
func newChatStat(now time.Time, body []byte, stream bool) *chatStat {
	mode := "sync"
	if stream {
		mode = "stream"
	}
	seq := chatSeq.Add(1)
	return &chatStat{
		seq:    seq,
		start:  now,
		model:  parseModelFromBody(body),
		entry:  "chat",
		stream: stream,
		mode:   mode,
		toks:   -1,
	}
}

// done 幂等落一行表格日志。
func (s *chatStat) done() {
	if s.logged {
		return
	}
	s.logged = true
	logChatRowWithSeq(s.seq, s.ttfb, time.Since(s.start), s.model, s.mode, s.uid, s.status, s.toks)
}

// chatStatsReader 在流式透传时抓取 SSE 末帧的 usage（completion_tokens 用于日志，
// 完整计量用于 Token 用量统计），并记录首个 data 帧的 TTFB；原始字节原样返回给下游透传。
// 注意：不做 rune 估算，token 数一律采信上游 usage。
type chatStatsReader struct {
	br       *bufio.Reader
	start    time.Time
	ttfb     time.Duration
	seen     bool // 已见过首个 data 帧（TTFB 只记一次）
	hasUsage bool // 末帧是否带 usage
	tokens   int
	pend     []byte // 已读未返回的行缓存

	counters    usage.Counters
	hasCounters bool
}

// newChatStatsReaderSince 以 since 为 TTFB 计时起点（通常是请求进入 handler 的时刻）。
func newChatStatsReaderSince(r io.Reader, since time.Time) *chatStatsReader {
	return &chatStatsReader{br: bufio.NewReaderSize(r, 64*1024), start: since}
}

// TTFB 返回首个 data 帧到达耗时；无帧时为 0。
func (s *chatStatsReader) TTFB() time.Duration { return s.ttfb }

// Tokens 返回末帧 usage.completion_tokens 与是否缺失；无 usage 时 ok=false。
func (s *chatStatsReader) Tokens() (int, bool) { return s.tokens, s.hasUsage }

// Usage 返回末帧 usage 的完整计量（输入/输出/缓存）与是否可用。
func (s *chatStatsReader) Usage() (usage.Counters, bool) { return s.counters, s.hasCounters }

// parseSSELine 解析一行 "data: {...}"：首帧记 TTFB，含 usage 时采信精确计量。
func (s *chatStatsReader) parseSSELine(line string) {
	line = strings.TrimRight(line, "\r\n")
	if !strings.HasPrefix(line, "data: ") {
		return
	}
	payload := strings.TrimPrefix(line, "data: ")
	if payload == "[DONE]" {
		return
	}
	if !s.seen {
		s.seen = true
		s.ttfb = time.Since(s.start)
		// 保证不变式：见到 data 帧 ⇒ TTFB > 0。
		//
		// 必要性（实测）：Windows 时钟粒度约 511µs（30 万次 time.Now() 采样仅
		// 3 个不同值）。透传本地内存流时首帧可在同一次时钟滴答内到达，
		// time.Since 返回精确 0。而下游 logChatRow 以 ttfb > 0 为据打印耗时、
		// 否则打印 "-"，0 会被误报成「未收到任何数据帧」。
		// 这里补齐 1ns：不伪造真实耗时（仍是纳秒级真值），只消除哨兵值歧义。
		if s.ttfb <= 0 {
			s.ttfb = time.Nanosecond
		}
	}
	var chunk struct {
		Usage map[string]any `json:"usage"`
	}
	if json.Unmarshal([]byte(payload), &chunk) != nil || chunk.Usage == nil {
		return
	}
	s.hasUsage = true
	s.tokens = numOf(chunk.Usage["completion_tokens"])
	if c, ok := usage.ParseOpenAIUsage(chunk.Usage); ok {
		s.counters = c
		s.hasCounters = true
	}
}

// Read 返回原始数据，同时解析统计 TTFB/token。
func (s *chatStatsReader) Read(p []byte) (int, error) {
	if len(s.pend) > 0 {
		n := copy(p, s.pend)
		s.pend = s.pend[n:]
		return n, nil
	}
	line, err := s.br.ReadString('\n')
	if line != "" {
		s.parseSSELine(line)
		s.pend = []byte(line)
		n := copy(p, s.pend)
		s.pend = s.pend[n:]
		return n, nil
	}
	return 0, err
}

// parseModelFromBody 从请求 JSON 取 model 字段，缺省标 "-"。
func parseModelFromBody(body []byte) string {
	var obj struct {
		Model string `json:"model"`
	}
	if err := json.Unmarshal(body, &obj); err != nil || obj.Model == "" {
		return "-"
	}
	return obj.Model
}

// completionTokens 从 Aggregate 返回的响应中提取 usage.completion_tokens；缺失返回 -1。
func completionTokens(resp map[string]any) int {
	u, ok := resp["usage"].(map[string]any)
	if !ok {
		return -1
	}
	v, ok := u["completion_tokens"].(float64)
	if !ok {
		return -1
	}
	return int(v)
}

// uidPrefix 只显示 uid 前 8 位；空 uid 显示 "-"。
func uidPrefix(uid string) string {
	if uid == "" {
		return "-"
	}
	if len(uid) > 8 {
		return uid[:8]
	}
	return uid
}

// logChatRow 打印一行请求级表格日志（直接输出 stdout，无 log 时间戳前缀）。
// toks<0 表示 usage 缺失，显示 "-"。
func logChatRow(ttfb, total time.Duration, model, mode, uid string, status int, toks int) {
	logChatRowWithSeq(chatSeq.Add(1), ttfb, total, model, mode, uid, status, toks)
}

// logChatRowWithSeq 带指定 seq 打印一行请求级表格日志。
func logChatRowWithSeq(seq int64, ttfb, total time.Duration, model, mode, uid string, status int, toks int) {
	if !chatLogEnabled {
		return
	}
	if len(model) > 11 {
		model = model[:11]
	}
	tokField := "-"
	tokpsField := "-"
	if toks >= 0 {
		tokField = fmt.Sprintf("%d", toks)
		if total > 0 {
			tokpsField = fmt.Sprintf("%.1f", float64(toks)/total.Seconds())
		} else {
			tokpsField = "0.0"
		}
	}
	ttfbMS := "-"
	if ttfb > 0 {
		ttfbMS = fmt.Sprintf("%dms", ttfb.Milliseconds())
	}
	fmt.Fprintf(os.Stdout, "| #%03d | %s | %s | %s | %d | uid=%s | TTFB=%s | tok=%s | %stok/s | total=%.1fs |\n",
		seq,
		time.Now().Format("15:04:05"),
		model,
		mode,
		status,
		uidPrefix(uid),
		ttfbMS,
		tokField,
		tokpsField,
		total.Seconds(),
	)
}
