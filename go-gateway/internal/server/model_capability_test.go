package server

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"workbuddy2api/internal/auth"
)

// ---------------------------------------------------------------------------
// 回归：/v1/models 必须下发模型能力（图片输入）
//
// 实测缺陷（2026-09-16）：上游 /v3/config 的 data.models[] 每条都带
// supportsImages（bool）与 disabledMultimodal，但网关两处都丢了：
//   - upstream.ModelInfo 没这个字段，FetchModels 的匿名结构体也没解；
//   - modelList() 只写 id/object/created/owned_by/context_length/max_output_tokens。
//
// 后果是「有多模态模型但客户端发不出图片」：能读该字段的客户端（OpenClaw
// 的 Codex/Copilot/HuggingFace/OpenRouter/Vercel/LM Studio 解析器）拿不到任何
// 能力信号，只能按纯文本处理。构建期完全看不出来，只在用户拖图片时才失败。
//
// 字段名不统一是实测结论（各客户端读的拼写都不同），因此一次下发多种拼写；
// 详见 modelCapabilityFields 的注释与 TestModelCapabilityFieldsCoversKnownClientSpellings。
// ---------------------------------------------------------------------------

// imageCapableHandler 构造一个「动态拉取成功且模型带能力」的 handler。
//
// 复用同包已有的 newFakeUpstream，不另造一套上游模拟。
func imageCapableHandler(t *testing.T, body string) *Handler {
	t.Helper()
	resetModelsCache()
	p := testPoolWith(&auth.Auth{
		UID:             "cn-1",
		AccessToken:     "t",
		Domain:          "copilot.tencent.com",
		SoonestExpireAt: 1 << 40,
	})
	return NewHandler(Config{
		Pool:     p,
		Upstream: newFakeUpstream(t, func(string) (int, string, bool) { return http.StatusOK, body, false }),
	})
}

// listModels 调一次 /v1/models，返回解析后的 data 数组。
func listModels(t *testing.T, h *Handler) []map[string]any {
	t.Helper()
	req := httptest.NewRequest(http.MethodGet, "/v1/models", nil)
	rec := httptest.NewRecorder()
	h.mux.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("/v1/models 应返回 200，实际 %d", rec.Code)
	}
	var payload struct {
		Data []map[string]any `json:"data"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &payload); err != nil {
		t.Fatalf("解析响应失败: %v（原文 %s）", err, rec.Body.String())
	}
	return payload.Data
}

// entryByID 在列表里按 id 找条目。
func entryByID(list []map[string]any, id string) map[string]any {
	for _, m := range list {
		if got, _ := m["id"].(string); got == id {
			return m
		}
	}
	return nil
}

// v3WithCapabilities 贴近真实的 /v3/config：一个支持图片、一个明确不支持、
// 一个字段缺失（补全类模型），以及一个账号级多模态被禁用的。
const v3WithCapabilities = `{"code":0,"data":{
	"agents":[{"name":"cli","models":["vision-ok","vision-off","vision-unknown","vision-disabled-mm"]}],
	"models":[
		{"id":"vision-ok","maxInputTokens":1000000,"maxOutputTokens":128000,"supportsImages":true},
		{"id":"vision-off","maxInputTokens":1000000,"maxOutputTokens":128000,"supportsImages":false},
		{"id":"vision-unknown","maxInputTokens":200000,"maxOutputTokens":24000},
		{"id":"vision-disabled-mm","maxInputTokens":1000000,"maxOutputTokens":64000,"supportsImages":true,"disabledMultimodal":true}
	]}}`

// TestModelsExposeSupportsImages 支持图片的模型要下发 supportsImages=true。
func TestModelsExposeSupportsImages(t *testing.T) {
	h := imageCapableHandler(t, v3WithCapabilities)
	entry := entryByID(listModels(t, h), "vision-ok")
	if entry == nil {
		t.Fatal("列表里应有 vision-ok")
	}
	if got, ok := entry["supportsImages"].(bool); !ok || !got {
		t.Errorf("vision-ok 应下发 supportsImages=true，实际 %v（键存在=%v）",
			entry["supportsImages"], ok)
	}
}

// TestModelsExposeImageModalitySpellings 一次下发多种拼写，覆盖各客户端。
//
// 实测各客户端读的字段名都不同，且没有统一约定；多写几种是安全的
// （解析器只取自己认识的键，多余键不会报错）。这里逐条锁住已知拼写，
// 避免以后有人"清理重复字段"时把某个客户端的支持删掉。
func TestModelsExposeImageModalitySpellings(t *testing.T) {
	h := imageCapableHandler(t, v3WithCapabilities)
	entry := entryByID(listModels(t, h), "vision-ok")
	if entry == nil {
		t.Fatal("列表里应有 vision-ok")
	}

	// OpenClaw OpenAI Codex：input_modalities / inputModalities
	for _, key := range []string{"input_modalities", "inputModalities"} {
		mods, ok := entry[key].([]any)
		if !ok || !containsAnyStr(mods, "image") {
			t.Errorf("%s 应含 \"image\"（OpenClaw Codex 解析器），实际 %v", key, entry[key])
		}
	}

	// OpenClaw Copilot：capabilities.supports.vision；LM Studio：capabilities.vision
	caps, ok := entry["capabilities"].(map[string]any)
	if !ok {
		t.Fatalf("应有 capabilities 对象（OpenClaw Copilot/LM Studio），实际 %v", entry["capabilities"])
	}
	if v, _ := caps["vision"].(bool); !v {
		t.Errorf("capabilities.vision 应为 true（OpenClaw LM Studio），实际 %v", caps["vision"])
	}
	supports, _ := caps["supports"].(map[string]any)
	if v, _ := supports["vision"].(bool); !v {
		t.Errorf("capabilities.supports.vision 应为 true（OpenClaw Copilot），实际 %v", caps["supports"])
	}

	// OpenClaw HuggingFace：architecture.input_modalities；OpenRouter：architecture.modality
	arch, ok := entry["architecture"].(map[string]any)
	if !ok {
		t.Fatalf("应有 architecture 对象（OpenClaw HuggingFace/OpenRouter），实际 %v", entry["architecture"])
	}
	if mods, _ := arch["input_modalities"].([]any); !containsAnyStr(mods, "image") {
		t.Errorf("architecture.input_modalities 应含 \"image\"，实际 %v", arch["input_modalities"])
	}
	if mod, _ := arch["modality"].(string); !strings.Contains(mod, "image") {
		t.Errorf("architecture.modality 应含 \"image\"（形如 text+image->text），实际 %v", arch["modality"])
	}

	// OpenClaw Vercel AI Gateway：tags 含 "vision"
	if tags, _ := entry["tags"].([]any); !containsAnyStr(tags, "vision") {
		t.Errorf("tags 应含 \"vision\"（OpenClaw Vercel AI Gateway），实际 %v", entry["tags"])
	}
}

// TestModelsOmitCapabilityWhenUnsupported 明确不支持时下发 false，而不是省略。
//
// 省略会让客户端回退到自己的默认（常按「支持」处理），发出必然失败的请求；
// 明确 false 才能让客户端主动隐藏/降级图片入口。
func TestModelsOmitCapabilityWhenUnsupported(t *testing.T) {
	h := imageCapableHandler(t, v3WithCapabilities)
	entry := entryByID(listModels(t, h), "vision-off")
	if entry == nil {
		t.Fatal("列表里应有 vision-off")
	}
	got, ok := entry["supportsImages"].(bool)
	if !ok {
		t.Fatalf("vision-off 应下发 supportsImages=false（省略会让客户端按默认处理），实际 %v", entry["supportsImages"])
	}
	if got {
		t.Error("vision-off 的 supportsImages 应为 false")
	}
	caps, _ := entry["capabilities"].(map[string]any)
	if v, _ := caps["vision"].(bool); v {
		t.Error("vision-off 的 capabilities.vision 应为 false")
	}
}

// TestModelsOmitCapabilityWhenUnknown 上游没声明时**不写**能力字段。
//
// 三态：nil = 未声明。谎报成 false 会把本可用的图片能力关掉，
// 这比不下发更糟 —— 客户端本来可以按自己的默认策略处理。
func TestModelsOmitCapabilityWhenUnknown(t *testing.T) {
	h := imageCapableHandler(t, v3WithCapabilities)
	entry := entryByID(listModels(t, h), "vision-unknown")
	if entry == nil {
		t.Fatal("列表里应有 vision-unknown")
	}
	for _, key := range []string{"supportsImages", "input_modalities", "inputModalities", "capabilities", "architecture", "tags"} {
		if v, exists := entry[key]; exists {
			t.Errorf("上游未声明能力时不应下发 %s（谎报成纯文本会关掉可用能力），实际 %v", key, v)
		}
	}
}

// TestModelsDisabledMultimodalForcesUnsupported 账号级多模态开关优先于模型能力。
//
// 上游用 disabledMultimodal 表达「该账号不能发图片」，与模型自身能力无关。
// 此时宣称支持会让客户端发出必然失败的请求。
func TestModelsDisabledMultimodalForcesUnsupported(t *testing.T) {
	h := imageCapableHandler(t, v3WithCapabilities)
	entry := entryByID(listModels(t, h), "vision-disabled-mm")
	if entry == nil {
		t.Fatal("列表里应有 vision-disabled-mm")
	}
	got, ok := entry["supportsImages"].(bool)
	if !ok {
		t.Fatalf("disabledMultimodal=true 时应明确下发 supportsImages=false，实际 %v", entry["supportsImages"])
	}
	if got {
		t.Error("disabledMultimodal=true 时即便 supportsImages=true 也必须降级为 false")
	}
}

// TestStaticModelsFallbackCarriesImageCapability 回退静态表时也要带能力字段。
//
// 上游不可达时若静态表没有能力字段，图片能力会整个消失 —— 而网关看起来
// 完全正常（返回 200 + 完整模型列表），是最难排查的一类问题。
func TestStaticModelsFallbackCarriesImageCapability(t *testing.T) {
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
		if got, ok := entry["supportsImages"].(bool); !ok || !got {
			t.Errorf("回退静态表时 %s 应带 supportsImages=true（实测该清单全部支持图片），实际 %v", id, entry["supportsImages"])
		}
	}
}

// TestStaticModelsIntlFallbackSupplementsCapability 动态列表补齐国际版静态条目时也要带能力。
//
// 混合账号池下会走到这条路径（动态只来自被抽中的那个区域）。
func TestStaticModelsIntlFallbackSupplementsCapability(t *testing.T) {
	h := imageCapableHandler(t, v3WithCapabilities)
	list := listModels(t, h)
	// 动态列表只有 4 个模型，国际版静态表应被补齐进来
	entry := entryByID(list, "gpt-6-astra")
	if entry == nil {
		t.Fatal("动态列表应补齐国际版静态条目 gpt-6-astra")
	}
	if got, ok := entry["supportsImages"].(bool); !ok || !got {
		t.Errorf("补齐的国际版条目应带 supportsImages=true，实际 %v", entry["supportsImages"])
	}
}

// containsAnyStr 判断 []any 里是否含某字符串。
func containsAnyStr(xs []any, want string) bool {
	for _, x := range xs {
		if s, _ := x.(string); s == want {
			return true
		}
	}
	return false
}
