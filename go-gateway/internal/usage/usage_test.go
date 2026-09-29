package usage

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestParseOpenAIUsageStandard(t *testing.T) {
	c, ok := ParseOpenAIUsage(map[string]any{
		"prompt_tokens":     float64(100),
		"completion_tokens": float64(20),
		"total_tokens":      float64(120),
		"prompt_tokens_details": map[string]any{
			"cached_tokens": float64(40),
		},
	})
	if !ok {
		t.Fatal("标准 usage 应被识别")
	}
	if c.Input != 100 || c.Output != 20 || c.CacheRead != 40 || c.CacheWrite != 0 {
		t.Fatalf("解析结果不符: %+v", c)
	}
}

func TestParseOpenAIUsageAliases(t *testing.T) {
	c, ok := ParseOpenAIUsage(map[string]any{
		"input_tokens":              float64(50),
		"output_tokens":             float64(7),
		"prompt_cache_hit_tokens":   float64(12),
		"prompt_cache_write_tokens": float64(3),
	})
	if !ok {
		t.Fatal("别名 usage 应被识别")
	}
	if c.Input != 50 || c.Output != 7 || c.CacheRead != 12 || c.CacheWrite != 3 {
		t.Fatalf("别名解析结果不符: %+v", c)
	}

	// 嵌套 input_tokens_details 数组中的 cached_tokens。
	c2, ok := ParseOpenAIUsage(map[string]any{
		"input_tokens":  float64(30),
		"output_tokens": float64(5),
		"input_tokens_details": []any{
			map[string]any{"cached_tokens": float64(9)},
		},
	})
	if !ok || c2.CacheRead != 9 {
		t.Fatalf("嵌套 details 解析不符: %+v ok=%v", c2, ok)
	}
}

func TestParseOpenAIUsageMissing(t *testing.T) {
	if _, ok := ParseOpenAIUsage(nil); ok {
		t.Fatal("nil 不应被识别")
	}
	if _, ok := ParseOpenAIUsage(map[string]any{"foo": "bar"}); ok {
		t.Fatal("无关对象不应被识别")
	}
	// 明确的零值字段仍算有效 usage（上游可能返回全 0）。
	if _, ok := ParseOpenAIUsage(map[string]any{"prompt_tokens": float64(0), "completion_tokens": float64(0)}); !ok {
		t.Fatal("含 token 字段的零值 usage 应被识别")
	}
}

func TestRecordAggregatesAndFilters(t *testing.T) {
	s := New("")
	base := time.Date(2026, 9, 1, 12, 0, 0, 0, time.Local)
	// 注入时钟：让「近 7 天」恰好吃到 9/1 与 9/6 两笔，排除 10/11 的历史记录。
	s.now = func() time.Time { return base.AddDate(0, 0, 5) }

	s.RecordAt("uid-a", "model-x", base, Counters{Input: 10, Output: 2, CacheRead: 4})
	s.RecordAt("uid-a", "model-x", base, Counters{Input: 5, Output: 1})
	s.RecordAt("uid-b", "model-y", base.AddDate(0, 0, 5), Counters{Input: 100, Output: 20, CacheWrite: 7})
	s.RecordAt("uid-b", "model-y", base.AddDate(0, 0, -40), Counters{Input: 1000, Output: 100})

	all := s.Snapshot(0)
	summary := all["summary"].(map[string]any)
	if got := intOf(summary["input"]); got != 1115 {
		t.Fatalf("全部 input 应为 1115，实际 %d", got)
	}
	if got := intOf(summary["records"]); got != 4 {
		t.Fatalf("全部 records 应为 4，实际 %d", got)
	}
	if got := intOf(summary["total"]); got != 1115+123+7 {
		t.Fatalf("total 口径不符: %d", got)
	}

	// 近 7 天（含今天）：只含 9/1 与 9/6 三笔。
	week := s.Snapshot(7)
	weekSummary := week["summary"].(map[string]any)
	if got := intOf(weekSummary["input"]); got != 115 {
		t.Fatalf("近 7 天 input 应为 115，实际 %d", got)
	}
	weekModels := week["models"].([]map[string]any)
	if len(weekModels) != 2 {
		t.Fatalf("近 7 天应有 2 个模型，实际 %d", len(weekModels))
	}
	if weekModels[0]["key"] != "model-y" {
		t.Fatalf("模型应按 total 降序，实际首位 %v", weekModels[0]["key"])
	}

	// 日序列按日期升序，且范围外记录被过滤。
	daily := week["daily"].([]map[string]any)
	if len(daily) != 2 || daily[0]["key"] != "2026-09-01" || daily[1]["key"] != "2026-09-06" {
		t.Fatalf("daily 序列不符: %v", daily)
	}

	accounts := all["accounts"].([]map[string]any)
	if len(accounts) != 2 {
		t.Fatalf("应有 2 个账号，实际 %d", len(accounts))
	}
	if accounts[0]["key"] != "uid-b" {
		t.Fatalf("账号应按 total 降序，实际首位 %v", accounts[0]["key"])
	}
}

func TestSnapshotUsesInjectedClock(t *testing.T) {
	s := New("")
	fixed := time.Date(2026, 9, 10, 8, 0, 0, 0, time.Local)
	s.now = func() time.Time { return fixed }
	s.RecordAt("u", "m", fixed, Counters{Input: 1, Output: 1})
	s.RecordAt("u", "m", fixed.AddDate(0, 0, -10), Counters{Input: 9})

	week := s.Snapshot(7)
	if got := intOf(week["summary"].(map[string]any)["input"]); got != 1 {
		t.Fatalf("时钟注入下近 7 天 input 应为 1，实际 %d", got)
	}
	if week["rangeDays"] != 7 {
		t.Fatalf("rangeDays 应为 7，实际 %v", week["rangeDays"])
	}
	if all := s.Snapshot(0); all["rangeDays"] != nil {
		t.Fatalf("全部范围 rangeDays 应为 null，实际 %v", all["rangeDays"])
	}
}

func TestFlushAndReloadRoundTrip(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "usage.json")

	s := New(path)
	at := time.Date(2026, 9, 2, 15, 4, 5, 0, time.Local)
	s.RecordAt("uid-a", "model-x", at, Counters{Input: 10, Output: 3, CacheRead: 5, CacheWrite: 1})
	s.Flush()

	if _, err := os.Stat(path); err != nil {
		t.Fatalf("usage.json 应已落盘: %v", err)
	}
	// 原子写不应残留临时文件。
	if _, err := os.Stat(path + ".tmp"); err == nil {
		t.Fatal("临时文件应已被 rename")
	}

	s2 := New(path)
	all := s2.Snapshot(0)
	summary := all["summary"].(map[string]any)
	if intOf(summary["input"]) != 10 || intOf(summary["output"]) != 3 ||
		intOf(summary["cacheRead"]) != 5 || intOf(summary["cacheWrite"]) != 1 ||
		intOf(summary["records"]) != 1 {
		t.Fatalf("重启后数据不一致: %v", summary)
	}
}

func TestFlushSkipsWhenNoPathOrNoChanges(t *testing.T) {
	// 纯内存模式：Flush 不产生文件。
	s := New("")
	s.RecordAt("u", "m", time.Now(), Counters{Input: 1})
	s.Flush()

	dir := t.TempDir()
	path := filepath.Join(dir, "usage.json")
	s2 := New(path)
	s2.Flush() // 无变更：不应创建文件
	if _, err := os.Stat(path); err == nil {
		t.Fatal("无变更时不应落盘")
	}
}

func TestLoadIgnoresCorruptFile(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "usage.json")
	if err := os.WriteFile(path, []byte("{not-json"), 0o600); err != nil {
		t.Fatal(err)
	}
	s := New(path)
	s.RecordAt("u", "m", time.Now(), Counters{Input: 2})
	if got := intOf(s.Snapshot(0)["summary"].(map[string]any)["input"]); got != 2 {
		t.Fatalf("损坏文件应被忽略并从空状态开始，实际 %d", got)
	}
}

func TestCountersValueCacheHitRate(t *testing.T) {
	empty := Counters{}
	if empty.Value()["cacheHitRate"] != nil {
		t.Fatal("无输入时 cacheHitRate 应为 nil")
	}
	// cacheRead 大于 input（异常数据）时 uncachedInput 不应为负。
	odd := Counters{Input: 5, Output: 1, CacheRead: 8, Records: 1}
	if got := intOf(odd.Value()["uncachedInput"]); got != 0 {
		t.Fatalf("uncachedInput 不应为负，实际 %d", got)
	}

	// JSON 序列化：nil 命中率输出为 null。
	raw, err := json.Marshal(empty.Value())
	if err != nil {
		t.Fatal(err)
	}
	var decoded map[string]any
	if err := json.Unmarshal(raw, &decoded); err != nil {
		t.Fatal(err)
	}
	if decoded["cacheHitRate"] != nil {
		t.Fatalf("cacheHitRate 应序列化为 null，实际 %v", decoded["cacheHitRate"])
	}
}

func TestRecordRequestAndSnapshot(t *testing.T) {
	s := New("")
	now := time.Date(2026, 9, 29, 12, 0, 0, 0, time.Local)
	s.now = func() time.Time { return now }

	s.RecordRequest(RequestRecord{
		Seq: 1, Ts: now.Add(-48 * time.Hour).UnixMilli(), Model: "glm-5.1", UID: "uid1", Status: 200,
	})
	s.RecordRequest(RequestRecord{
		Seq: 2, Ts: now.Add(-1 * time.Hour).UnixMilli(), Model: "deepseek-v4", UID: "uid2", Status: 200,
	})
	s.RecordRequest(RequestRecord{
		Seq: 3, Ts: now.UnixMilli(), Model: "deepseek-v4", UID: "uid2", Status: 503,
	})

	snapAll := s.RequestsSnapshot(0, 10)
	if snapAll.Total != 3 || snapAll.Returned != 3 {
		t.Fatalf("全部查询期望 3 条，实际 total=%d returned=%d", snapAll.Total, snapAll.Returned)
	}
	if snapAll.Requests[0].Seq != 3 || snapAll.Requests[1].Seq != 2 || snapAll.Requests[2].Seq != 1 {
		t.Fatalf("排序应倒序（最新在前），实际顺序: %d, %d, %d", snapAll.Requests[0].Seq, snapAll.Requests[1].Seq, snapAll.Requests[2].Seq)
	}

	snapToday := s.RequestsSnapshot(1, 10)
	if snapToday.Total != 2 || snapToday.Returned != 2 {
		t.Fatalf("今日查询期望 2 条，实际 total=%d returned=%d", snapToday.Total, snapToday.Returned)
	}

	snapLimit := s.RequestsSnapshot(0, 1)
	if snapLimit.Total != 3 || snapLimit.Returned != 1 {
		t.Fatalf("limit 截断期望 total=3 returned=1，实际 total=%d returned=%d", snapLimit.Total, snapLimit.Returned)
	}
}

