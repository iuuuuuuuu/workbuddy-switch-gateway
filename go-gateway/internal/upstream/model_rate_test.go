package upstream

import (
	"testing"
	"time"
)

// 用 Issue/需求现场的真实报错文案验证解析。
//
// 上游原始响应体（429）：
//
//	{"code":"6004","msg":"您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，
//	 您也可以切换其他模型继续使用。","requestId":"62b8393f-..."}
func TestParseResetTimeRealSample(t *testing.T) {
	const real = `{"code":"6004","msg":"您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。","requestId":"62b8393f-e7ce-46e6-912e-6e610a8d1e01"}`

	// now 必须显式取 UTC，不能写 time.Local —— ParseResetTime 只接受"未来"时刻，
	// 而比较的是**绝对时刻**。文案里的 13:25:47 UTC+8 等于 05:25:47Z，若 now 用
	// 本地时区，在 UTC 的机器（CI）上 now=10:00Z 就晚于重置点，该样本被判为"过去"
	// 而解析失败，测试只在 +08:00 的本机能过 —— 那是测试的环境依赖，不是实现问题。
	now := time.Date(2026, 9, 15, 0, 0, 0, 0, time.UTC)
	got, ok := ParseResetTime(real, now)
	if !ok {
		t.Fatalf("未解析出重置时间（now=%s）", now.Format(time.RFC3339))
	}
	// UTC+8 的 13:25:47 == 2026-09-15T05:25:47Z。got.Equal 比较的是 instant，
	// 故断言同样与本机时区无关。
	want := time.Date(2026, 9, 15, 5, 25, 47, 0, time.UTC)
	if !got.Equal(want) {
		t.Errorf("解析结果=%s (%s)，期望 %s (%s)", got.UTC().Format(time.RFC3339), got.Location(), want.Format(time.RFC3339), want.Location())
	}
	// 关键：与 now 的间隔应恰为 5h25m47s（绝对时刻，与时区无关）
	t.Logf("now=%s  重置=%s  间隔=%s", now.Format(time.RFC3339), got.UTC().Format(time.RFC3339), got.Sub(now))
}

func TestParseResetTimeVariants(t *testing.T) {
	// 基准用 UTC 00:00，确保下面各时区的 13:25:47 都落在"未来"。
	base := time.Date(2026, 9, 15, 0, 0, 0, 0, time.UTC)
	cases := []struct {
		name string
		body string
		ok   bool
		want string // RFC3339 in UTC
	}{
		{
			"UTC+8",
			`{"code":6004,"msg":"将在 2026-09-15 13:25:47 UTC+8 重置"}`,
			true, "2026-09-15T05:25:47Z",
		},
		{
			"UTC+08:00",
			`{"code":6004,"msg":"将在 2026-09-15 13:25:47 UTC+08:00 重置"}`,
			true, "2026-09-15T05:25:47Z",
		},
		{
			"UTC-5",
			`{"code":6004,"msg":"将在 2026-09-15 13:25:47 UTC-5 重置"}`,
			true, "2026-09-15T18:25:47Z",
		},
		{
			"Z 后缀",
			`{"code":6004,"msg":"将在 2026-09-15 13:25:47 Z 重置"}`,
			true, "2026-09-15T13:25:47Z",
		},
		{
			"ISO T 分隔符",
			`{"code":6004,"msg":"reset at 2026-09-15T13:25:47 UTC+8"}`,
			true, "2026-09-15T05:25:47Z",
		},
		{
			"已过去的时刻 → 不采用",
			`{"code":6004,"msg":"将在 2026-09-14 05:00:00 UTC+8 重置"}`,
			false, "",
		},
		{
			"无时间字面量",
			`{"code":6004,"msg":"您的使用量已超出频率限制"}`,
			false, "",
		},
		{
			"非法月日",
			`{"code":6004,"msg":"将在 2026-13-45 99:99:99 UTC+8 重置"}`,
			false, "",
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got, ok := ParseResetTime(c.body, base)
			if ok != c.ok {
				t.Fatalf("ok=%v want %v (got=%s)", ok, c.ok, got)
			}
			if !c.ok {
				return
			}
			want, _ := time.Parse(time.RFC3339, c.want)
			if !got.Equal(want) {
				t.Errorf("got=%s want=%s", got.UTC().Format(time.RFC3339), c.want)
			}
		})
	}
}

// 无时区后缀时按**上游业务时区（CST）**解释，而不是容器本地时区。
//
// 实测的 6004 文案一律带 "UTC+8"，故该分支正常不会走到；保留它是容错，且必须
// 用固定业务时区而非 time.Local —— 否则同一份响应在 +08:00 的开发机与 UTC 的
// 容器/CI 上会得出相差 8 小时的时刻，UTC 下甚至会把尚未到期的重置点判成"已过期"。
func TestParseResetTimeNoZoneUsesUpstreamZone(t *testing.T) {
	body := `{"code":6004,"msg":"将在 2026-09-15 13:25:47 重置"}`
	// now 显式用 UTC，确保断言与运行机器时区无关。
	now := time.Date(2026, 9, 15, 0, 0, 0, 0, time.UTC)
	got, ok := ParseResetTime(body, now)
	if !ok {
		t.Fatal("未解析出")
	}
	// CST 13:25:47 == UTC 05:25:47
	want := time.Date(2026, 9, 15, 5, 25, 47, 0, time.UTC)
	if !got.Equal(want) {
		t.Errorf("got=%s want=%s（无时区后缀应按 CST=UTC+8 解释）",
			got.UTC().Format(time.RFC3339), want.Format(time.RFC3339))
	}
}

// 解析结果不得随运行机器时区变化。
//
// 本项目踩过的真实坑：同一份代码在 +08:00 的本机通过、在 UTC 的 CI 失败。
// 这里用同一个瞬时的多种时区表示去解析，断言得到同一结果。
func TestParseResetTimeIsTimezoneIndependent(t *testing.T) {
	const body = `{"code":6004,"msg":"将在 2026-09-15 13:25:47 UTC+8 重置"}`

	// 同一个瞬时（2026-09-15 02:00:00 UTC），用不同 Location 表示。
	instant := time.Date(2026, 9, 15, 2, 0, 0, 0, time.UTC)
	zones := []*time.Location{
		time.UTC,
		time.FixedZone("CST", 8*3600),
		time.FixedZone("EST", -5*3600),
	}
	var first time.Time
	for i, loc := range zones {
		got, ok := ParseResetTime(body, instant.In(loc))
		if !ok {
			t.Fatalf("zone %s: 未解析出（不应随机器时区变化）", loc)
		}
		if i == 0 {
			first = got
			continue
		}
		if !got.Equal(first) {
			t.Errorf("zone %s: got=%s，与 UTC 下结果 %s 不一致",
				loc, got.UTC().Format(time.RFC3339), first.UTC().Format(time.RFC3339))
		}
	}
	// 且必须是该瞬时之后的未来时刻：CST 13:25:47 == UTC 05:25:47 > 02:00
	if want := time.Date(2026, 9, 15, 5, 25, 47, 0, time.UTC); !first.Equal(want) {
		t.Errorf("结果=%s want=%s", first.UTC().Format(time.RFC3339), want.Format(time.RFC3339))
	}
}

func TestIsModelRateLimited(t *testing.T) {
	cases := []struct {
		name string
		body string
		want bool
	}{
		{"真实样本 code 6004", `{"code":"6004","msg":"您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。"}`, true},
		{"数字型 code", `{"code":6004,"msg":"x"}`, true},
		{"仅文案兜底", `{"code":"9999","msg":"您也可以切换其他模型继续使用。"}`, true},
		{"账号级限流不应误判", `{"code":"1001","msg":"请求过于频繁，请稍后重试"}`, false},
		{"空体", ``, false},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			if got := IsModelRateLimited(c.body); got != c.want {
				t.Errorf("IsModelRateLimited=%v want %v", got, c.want)
			}
		})
	}
}

// Classify 必须把"429 + 模型限流"判为 ErrModelRate，把普通 429 仍判为 ErrSoftRate。
func TestClassifyModelRate(t *testing.T) {
	model429 := `{"code":"6004","msg":"您的使用量已超出频率限制，将在 2026-09-15 13:25:47 UTC+8 重置，您也可以切换其他模型继续使用。"}`
	if got := Classify(429, model429); got != ErrModelRate {
		t.Errorf("429+6004 → %v want ErrModelRate", got)
	}
	if got := Classify(429, `{"code":"1001","msg":"too many requests"}`); got != ErrSoftRate {
		t.Errorf("普通 429 → %v want ErrSoftRate", got)
	}
	if got := Classify(429, ``); got != ErrSoftRate {
		t.Errorf("空体 429 → %v want ErrSoftRate", got)
	}
	// 模型限流判定必须只在 429 下生效：同样是 6004 文案但 5xx 不该误判。
	if got := Classify(500, model429); got != ErrServer {
		t.Errorf("500+6004文案 → %v want ErrServer（模型限流只在 429 下判定）", got)
	}
}
