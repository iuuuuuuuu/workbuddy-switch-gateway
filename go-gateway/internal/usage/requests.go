package usage

import (
	"time"
)

const (
	maxMemoryRequests = 10000
	maxSavedRequests  = 1000
)

// RequestRecord 单条请求的明细记录（与前端 GatewayRequestRecord 严格对齐）。
type RequestRecord struct {
	Seq          int64    `json:"seq"`
	Ts           int64    `json:"ts"` // Unix 毫秒
	Model        string   `json:"model"`
	UID          string   `json:"uid"`
	Entry        string   `json:"entry"`
	Stream       bool     `json:"stream"`
	Region       string   `json:"region"`
	Status       int      `json:"status"`
	Input        int64    `json:"input"`
	Output       int64    `json:"output"`
	CacheRead    int64    `json:"cacheRead"`
	CacheWrite   int64    `json:"cacheWrite"`
	Total        int64    `json:"total"`
	CacheHitRate *float64 `json:"cacheHitRate"`
	TtfbMs       int64    `json:"ttfbMs"`
	TotalMs      int64    `json:"totalMs"`
	Tps          float64  `json:"tps"`
}

// RequestsSnapshot /usage/requests 响应体。
type RequestsSnapshot struct {
	Enabled     bool            `json:"enabled"`
	GeneratedAt int64           `json:"generatedAt"`
	RangeDays   *int            `json:"rangeDays,omitempty"`
	Total       int             `json:"total"`
	Returned    int             `json:"returned"`
	Requests    []RequestRecord `json:"requests"`
}

// RecordRequest 记录一条请求明细；内存环形上限 maxMemoryRequests。
func (s *Stats) RecordRequest(r RequestRecord) {
	s.mu.Lock()
	defer s.mu.Unlock()

	s.requests = append(s.requests, r)
	if len(s.requests) > maxMemoryRequests {
		s.requests = s.requests[len(s.requests)-maxMemoryRequests:]
	}
	s.dirty.Store(true)
}

// RequestsSnapshot 按天数范围与上限查询请求明细，按时间倒序（最新在前）。
func (s *Stats) RequestsSnapshot(days int, limit int) RequestsSnapshot {
	s.mu.Lock()
	defer s.mu.Unlock()

	now := s.clock()
	var cutoffMs int64
	var rangeDaysPtr *int
	if days > 0 {
		rangeDaysPtr = &days
		cutoffTime := time.Date(now.Year(), now.Month(), now.Day(), 0, 0, 0, 0, now.Location()).AddDate(0, 0, -(days - 1))
		cutoffMs = cutoffTime.UnixMilli()
	}

	if limit <= 0 {
		limit = 1000
	}

	var matched []RequestRecord
	for i := len(s.requests) - 1; i >= 0; i-- {
		r := s.requests[i]
		if days > 0 && r.Ts < cutoffMs {
			continue
		}
		matched = append(matched, r)
	}

	total := len(matched)
	returned := total
	if returned > limit {
		returned = limit
	}

	result := make([]RequestRecord, returned)
	copy(result, matched[:returned])

	return RequestsSnapshot{
		Enabled:     true,
		GeneratedAt: now.UnixMilli(),
		RangeDays:   rangeDaysPtr,
		Total:       total,
		Returned:    returned,
		Requests:    result,
	}
}
