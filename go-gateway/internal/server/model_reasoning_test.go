package server

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
)

// ---------------------------------------------------------------------------
// 回归：/v1/models 必须下发模型思考等级（reasoning effort）
//
// 实测缺陷（2026-09-16）：上游 /v3/config 的 data.models[].reasoning 同时给了
//   effort（默认档，string）与 supportedEfforts（允许档，[]string），
// 网关两处都读到了，但 supportedEfforts 只喂给出站降级缓存、effort 更是解析后
// 全树零消费方 —— 客户端在 /v1/models 里看不到任何档位信息。
//
// 后果是「想让模型多思考却调不动」：用户只能靠猜档位名，猜错就被
// normalizeReasoningEffort 悄悄改写（降级，或在「支持档全部高于请求档」时被
// floor 抬升），界面上表现为「我明明调了 max 却没生效」。
//
// 与「图片能力」那个缺陷同构，因此这里沿用同一套下发策略：
//   - 上游未声明 → **不写**任何键（宁缺勿错，见 decision：静态兜底表不带档位）；
//   - 上游声明了 → 一次下发多种拼写（各客户端解析器读法不统一）。
// ---------------------------------------------------------------------------

// v3WithReasoning 贴近真实的 /v3/config：三种模型形态。
//
//	reasoning-full    有 supportedEfforts + effort（常态）
//	reasoning-efforts 只有 supportedEfforts，没有 effort（默认档未声明）
//	reasoning-none    整条无 reasoning 字段（固定档模型）
const v3WithReasoning = `{"code":0,"data":{
	"agents":[{"name":"cli","models":["reasoning-full","reasoning-efforts","reasoning-none"]}],
	"models":[
		{"id":"reasoning-full","maxInputTokens":400000,"maxOutputTokens":128000,
		 "reasoning":{"effort":"high","supportedEfforts":["low","medium","high","xhigh","max"]}},
		{"id":"reasoning-efforts","maxInputTokens":1000000,"maxOutputTokens":48000,
		 "reasoning":{"supportedEfforts":["low","high","max"]}},
		{"id":"reasoning-none","maxInputTokens":300000,"maxOutputTokens":32000}
	]}}`

// reasoningEntry 取某模型条目，失败即终止。
func reasoningEntry(t *testing.T, id string) map[string]any {
	t.Helper()
	entry := entryByID(listModels(t, imageCapableHandler(t, v3WithReasoning)), id)
	if entry == nil {
		t.Fatalf("列表里应有 %s", id)
	}
	return entry
}

// reasoningKeys 所有可能被下发的档位键，用于断言「一个都不该出现」。
var reasoningKeys = []string{
	"supported_efforts", "supportedEfforts", "reasoning_efforts",
	"reasoningEfforts", "reasoning", "default_effort", "defaultEffort",
	"default_reasoning_effort",
}

// TestModelsExposeSupportedEfforts 上游声明的档位要下发，且值/顺序与上游一致。
func TestModelsExposeSupportedEfforts(t *testing.T) {
	entry := reasoningEntry(t, "reasoning-full")
	want := []string{"low", "medium", "high", "xhigh", "max"}

	// 主拼写 + 容错拼写 + 嵌套，值必须都等于上游真值。
	for _, key := range []string{"supported_efforts", "supportedEfforts", "reasoning_efforts", "reasoningEfforts"} {
		got, ok := entry[key].([]any)
		if !ok {
			t.Errorf("%s 应为数组，实际 %v（键存在=%v）", key, entry[key], ok)
			continue
		}
		if len(got) != len(want) {
			t.Errorf("%s 应有 %d 档，实际 %v", key, len(want), got)
			continue
		}
		for i, w := range want {
			if s, _ := got[i].(string); s != w {
				t.Errorf("%s[%d] 应为 %q，实际 %v（顺序也要保持上游原样）", key, i, w, got[i])
			}
		}
	}

	// OpenRouter 风格嵌套。
	nested, ok := entry["reasoning"].(map[string]any)
	if !ok {
		t.Fatalf("应有 reasoning 对象（OpenRouter 风格），实际 %v", entry["reasoning"])
	}
	if got, _ := nested["supported_efforts"].([]any); len(got) != len(want) {
		t.Errorf("reasoning.supported_efforts 应有 %d 档，实际 %v", len(want), nested["supported_efforts"])
	}
}

// TestModelsExposeDefaultEffort 上游给的默认档要与档位清单一并下发。
//
// 这是原先的「死字段」：reasoning.effort 被解析进匿名结构体后无人读取。
func TestModelsExposeDefaultEffort(t *testing.T) {
	entry := reasoningEntry(t, "reasoning-full")

	for _, key := range []string{"default_effort", "defaultEffort", "default_reasoning_effort"} {
		if got, _ := entry[key].(string); got != "high" {
			t.Errorf("%s 应为 \"high\"（上游 reasoning.effort），实际 %v", key, entry[key])
		}
	}
	nested, ok := entry["reasoning"].(map[string]any)
	if !ok {
		t.Fatalf("应有 reasoning 对象，实际 %v", entry["reasoning"])
	}
	if got, _ := nested["default_effort"].(string); got != "high" {
		t.Errorf("reasoning.default_effort 应为 \"high\"，实际 %v", nested["default_effort"])
	}
}

// TestModelsOmitDefaultEffortWhenUnset 上游没给默认档时**不写**该键。
//
// 不能拿 efforts[0] 之类的猜测顶上：网关并不知道默认档，编一个会误导客户端。
func TestModelsOmitDefaultEffortWhenUnset(t *testing.T) {
	entry := reasoningEntry(t, "reasoning-efforts")

	// 档位清单照常下发。
	if got, _ := entry["supported_efforts"].([]any); len(got) != 3 {
		t.Fatalf("supported_efforts 应有 3 档，实际 %v", entry["supported_efforts"])
	}
	for _, key := range []string{"default_effort", "defaultEffort", "default_reasoning_effort"} {
		if v, exists := entry[key]; exists {
			t.Errorf("上游未声明默认档时不应下发 %s（猜测值会误导客户端），实际 %v", key, v)
		}
	}
	nested, _ := entry["reasoning"].(map[string]any)
	if v, exists := nested["default_effort"]; exists {
		t.Errorf("上游未声明默认档时不应下发 reasoning.default_effort，实际 %v", v)
	}
}

// TestModelsOmitReasoningWhenUnsupported 固定档模型（无 reasoning 字段）一个档位键都不写。
//
// 写空数组是错的：客户端会当成「有该字段但没档位」，与「未声明」语义不同。
func TestModelsOmitReasoningWhenUnsupported(t *testing.T) {
	entry := reasoningEntry(t, "reasoning-none")

	for _, key := range reasoningKeys {
		if v, exists := entry[key]; exists {
			t.Errorf("上游未声明 reasoning 时不应下发 %s（空数组会被当成「有字段无档位」），实际 %v", key, v)
		}
	}
}

// TestStaticModelsFallbackCarriesNoReasoning 上游不可达回退静态表时不带档位。
//
// 静态表没有档位真值，网关不编造：谎报的档位要么被上游降级、要么被忽略，
// 用户看到的是「调了没生效」。宁可不下发，让客户端按自己的默认策略处理。
func TestStaticModelsFallbackCarriesNoReasoning(t *testing.T) {
	resetModelsCache()
	p := testPoolWith(&auth.Auth{
		UID:             "cn-1",
		AccessToken:     "t",
		SoonestExpireAt: 1 << 40,
	})
	h := NewHandler(Config{Pool: p, Upstream: modelsFailUpstream(t), MaxRotate: 1})

	list := listModels(t, h)
	if len(list) == 0 {
		t.Fatal("回退静态表后列表不应为空")
	}
	for _, entry := range list {
		id, _ := entry["id"].(string)
		for _, key := range reasoningKeys {
			if v, exists := entry[key]; exists {
				t.Errorf("回退静态表时 %s 不应带 %s（静态表无档位真值），实际 %v", id, key, v)
			}
		}
	}
}

// TestStaticModelsIntlSupplementCarriesNoReasoning 动态列表补齐的国际版条目不带档位。
//
// 混合账号池下会走到这条路径（动态只来自被抽中的那个区域）。
func TestStaticModelsIntlSupplementCarriesNoReasoning(t *testing.T) {
	// 动态只返回 3 个 reasoning-* 模型，国际版静态表应被补齐进来。
	entry := reasoningEntry(t, "gpt-6-astra")

	for _, key := range reasoningKeys {
		if v, exists := entry[key]; exists {
			t.Errorf("补齐的国际版条目 %s 不应带 %s（静态表无档位真值），实际 %v", "gpt-6-astra", key, v)
		}
	}
}

// TestReasoningFieldsIsolateCacheSlice 下发的切片必须是副本。
//
// ModelInfo.Efforts 来自动态模型缓存且被多个请求共享；若直接把原切片塞进响应，
// 调用方 in-place 修改返回值就会污染缓存（下一个客户端拿到被改过的档位清单）。
func TestReasoningFieldsIsolateCacheSlice(t *testing.T) {
	src := []string{"low", "high", "max"}
	got := modelReasoningFields(src, "high")

	list, ok := got["supported_efforts"].([]string)
	if !ok {
		t.Fatalf("supported_efforts 应为 []string，实际 %T", got["supported_efforts"])
	}
	if len(list) != len(src) {
		t.Fatalf("档位数量应一致，实际 %v", list)
	}

	// 改返回值，源切片不能被带动。
	list[0] = "TAMPERED"
	if src[0] != "low" {
		t.Errorf("返回值与缓存共享底层数组（源被改成 %q）—— 必须复制", src[0])
	}
}

// TestReasoningFieldsEmptyEfforts 空档位列表返回 nil，不下发任何键。
func TestReasoningFieldsEmptyEfforts(t *testing.T) {
	for _, in := range [][]string{nil, {}} {
		if got := modelReasoningFields(in, "high"); got != nil {
			t.Errorf("efforts=%v 时应返回 nil（不下发），实际 %v", in, got)
		}
	}
}

// TestReasoningFieldsBlankDefault 默认档为空白时不写默认档键。
func TestReasoningFieldsBlankDefault(t *testing.T) {
	got := modelReasoningFields([]string{"low", "high"}, "   ")
	if _, exists := got["default_effort"]; exists {
		t.Errorf("默认档为空白时不应下发 default_effort，实际 %v", got["default_effort"])
	}
	if nested, _ := got["reasoning"].(map[string]any); nested != nil {
		if v, exists := nested["default_effort"]; exists {
			t.Errorf("默认档为空白时不应下发 reasoning.default_effort，实际 %v", v)
		}
	}
}

// TestModelsReasoningCoexistsWithImageCapability 两个维度互不干扰。
//
// 思考等级与图片能力是独立调用、独立合并的两组键；锁住它们可以共存 ——
// 若有人把两者合并进同一个 map 或让后一次合并覆盖前一次，图片能力会静默消失。
func TestModelsReasoningCoexistsWithImageCapability(t *testing.T) {
	// 同一模型既声明图片能力、又声明思考档位。
	const body = `{"code":0,"data":{
		"agents":[{"name":"cli","models":["both"]}],
		"models":[{"id":"both","maxInputTokens":400000,"maxOutputTokens":128000,
		 "supportsImages":true,
		 "reasoning":{"effort":"high","supportedEfforts":["low","high"]}}]}}`

	entry := entryByID(listModels(t, imageCapableHandler(t, body)), "both")
	if entry == nil {
		t.Fatal("列表里应有 both")
	}

	if got, ok := entry["supportsImages"].(bool); !ok || !got {
		t.Errorf("图片能力键不应被思考等级挤掉，实际 %v", entry["supportsImages"])
	}
	if got, ok := entry["supported_efforts"].([]any); !ok || len(got) != 2 {
		t.Errorf("思考档位键不应被图片能力挤掉，实际 %v", entry["supported_efforts"])
	}
	// 图片能力的多拼写之一，确认不是只有顶层键活着。
	if tags, _ := entry["tags"].([]any); !containsAnyStr(tags, "vision") {
		t.Errorf("tags 应含 \"vision\"，实际 %v", entry["tags"])
	}
}

// TestReasoningFieldsShapeIsStable 锁住一次下发的完整键集合。
//
// 防止以后有人「清理重复字段」时删掉某个客户端的拼写。
func TestReasoningFieldsShapeIsStable(t *testing.T) {
	got := modelReasoningFields([]string{"low", "high"}, "low")

	wantKeys := []string{
		"supported_efforts", "supportedEfforts", "reasoning_efforts",
		"reasoningEfforts", "reasoning", "default_effort", "defaultEffort",
		"default_reasoning_effort",
	}
	for _, k := range wantKeys {
		if _, exists := got[k]; !exists {
			t.Errorf("应下发 %s（缺少会让对应客户端的档位选择失效）", k)
		}
	}
	if len(got) != len(wantKeys) {
		t.Errorf("键集合应为 %d 个，实际 %d: %v", len(wantKeys), len(got), got)
	}
	nested, _ := got["reasoning"].(map[string]any)
	for _, k := range []string{"supported_efforts", "default_effort"} {
		if _, exists := nested[k]; !exists {
			t.Errorf("reasoning.%s 应下发（OpenRouter 风格）", k)
		}
	}
}

// TestModelsEffortsOrderPreserved 档位顺序必须原样保留。
//
// 客户端可能按位置取（例如把首个当最低档），重排会静默改变语义。
func TestModelsEffortsOrderPreserved(t *testing.T) {
	body := `{"code":0,"data":{
		"agents":[{"name":"cli","models":["odd-order"]}],
		"models":[{"id":"odd-order","maxInputTokens":1000,"maxOutputTokens":100,
		 "reasoning":{"supportedEfforts":["max","low","medium"]}}]}}`

	entry := entryByID(listModels(t, imageCapableHandler(t, body)), "odd-order")
	if entry == nil {
		t.Fatal("列表里应有 odd-order")
	}
	got, _ := entry["supported_efforts"].([]any)
	joined := make([]string, 0, len(got))
	for _, v := range got {
		s, _ := v.(string)
		joined = append(joined, s)
	}
	if strings.Join(joined, ",") != "max,low,medium" {
		t.Errorf("档位顺序应与上游一致（max,low,medium），实际 %v", joined)
	}
}

// TestModelsMultipleEntriesReasoningIsolated 多模型条目的档位互不串味。
func TestModelsMultipleEntriesReasoningIsolated(t *testing.T) {
	h := imageCapableHandler(t, v3WithReasoning)
	list := listModels(t, h)

	full := entryByID(list, "reasoning-full")
	effortsOnly := entryByID(list, "reasoning-efforts")
	if full == nil || effortsOnly == nil {
		t.Fatal("两个模型都应在列表里")
	}

	fullEfforts, _ := full["supported_efforts"].([]any)
	onlyEfforts, _ := effortsOnly["supported_efforts"].([]any)
	if len(fullEfforts) != 5 || len(onlyEfforts) != 3 {
		t.Errorf("两个模型的档位清单应各自独立（5 vs 3），实际 %d vs %d",
			len(fullEfforts), len(onlyEfforts))
	}
}

// TestModelsReasoningSurvivesGET 端到端：/v1/models 返回 200 且带档位。
//
// 防止序列化阶段把 map 里的非 string 键或 []string 弄丢。
func TestModelsReasoningSurvivesGET(t *testing.T) {
	h := imageCapableHandler(t, v3WithReasoning)
	req := httptest.NewRequest(http.MethodGet, "/v1/models", nil)
	rec := httptest.NewRecorder()
	h.mux.ServeHTTP(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("/v1/models 应返回 200，实际 %d", rec.Code)
	}
	if !strings.Contains(rec.Body.String(), `"supported_efforts"`) {
		t.Errorf("响应体里应含 supported_efforts（JSON 序列化后），实际 %s", rec.Body.String())
	}
}
