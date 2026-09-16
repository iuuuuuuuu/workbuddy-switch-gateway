// Package server 暴露 OpenAI 兼容 HTTP 接口，内部驱动 pool 挑号 + upstream 转发。
package server

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"time"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/pool"
	"workbuddy2api/internal/session"
	"workbuddy2api/internal/upstream"
	"workbuddy2api/internal/usage"
)

// Config handler 依赖。
type Config struct {
	Pool      *pool.Pool
	Upstream  *upstream.Client
	APIKey    string // 空 = 不鉴权
	MaxRotate int    // 单请求最多换号次数，默认 3
	// Session 会话粘性路由器（可选；nil = 关闭粘性，纯 Pick 轮换）。
	Session *session.Router
	// StickyCount 返回当前粘性会话绑定数（供 /status）；nil 时报告 0。
	StickyCount func() int
	// RedisMode 观测字段（"upstash" / "noop"），供 /status 透出。
	RedisMode    string
	SoftCooldown time.Duration // 429 冷却，默认 60s
	RefreshSkew  time.Duration // token 提前刷新窗口，默认 10m
	// Usage Token 用量统计器（可选；nil = 不统计，/usage 返回 enabled=false）。
	Usage *usage.Stats
	// AllowedModel 「单一模型」锁定：非空时**只放行这一个模型**，其余一律拒绝。
	//
	// 用于「单一模型 + 积分轮转」模式：轮转的语义是「把这个账号的某个模型额度
	// 烧干净再换下一个账号」，因此必须锁定模型 —— 否则客户端换个模型就能绕过
	// 轮转策略，账号选择与额度消耗都会变得不可预期。
	//
	// 空串 = 不限制（默认，向后兼容）。大小写不敏感比较。
	AllowedModel string
}

// ServiceName 网关身份标识。经 /healthz 响应体 service 字段与 X-Service 头同时透出：
// 宿主（如 workbuddy-switch 托管网关子进程）探测同端口的旧服务/其他服务时，对方即使
// 返回 2xx 也不带本标识，宿主据此可识别"假成功"。
const ServiceName = "workbuddy2api"

// Handler 主路由。
type Handler struct {
	cfg Config
	mux *http.ServeMux
}

// NewHandler 构建 handler。
func NewHandler(cfg Config) *Handler {
	if cfg.MaxRotate <= 0 {
		cfg.MaxRotate = 3
	}
	if cfg.SoftCooldown <= 0 {
		cfg.SoftCooldown = 60 * time.Second
	}
	if cfg.RefreshSkew <= 0 {
		cfg.RefreshSkew = 10 * time.Minute
	}
	h := &Handler{cfg: cfg, mux: http.NewServeMux()}
	h.mux.HandleFunc("POST /v1/chat/completions", h.withAuth(h.chatCompletions))
	h.mux.HandleFunc("POST /v1/responses", h.withAuth(h.responses))
	h.mux.HandleFunc("POST /responses", h.withAuth(h.responses))
	h.mux.HandleFunc("POST /v1/messages", h.withAuth(h.messages))
	h.mux.HandleFunc("POST /messages", h.withAuth(h.messages))
	h.mux.HandleFunc("GET /v1/models", h.withAuth(h.models))
	h.mux.HandleFunc("GET /status", h.withAuth(h.status))
	h.mux.HandleFunc("GET /usage", h.withAuth(h.usageReport))
	h.mux.HandleFunc("GET /healthz", h.healthz)
	return h
}

func (h *Handler) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	h.mux.ServeHTTP(w, r)
}

// withAuth 校验客户端凭据。
//
// 同时接受两种头部形态，覆盖不同客户端的认证习惯：
//   - Authorization: Bearer <key> —— OpenAI SDK、Claude Code 的 ANTHROPIC_AUTH_TOKEN、
//     Claude Desktop 3P（inferenceGatewayAuthScheme=bearer）
//   - x-api-key: <key>            —— Anthropic SDK、Claude Code 的 ANTHROPIC_API_KEY
func (h *Handler) withAuth(next http.HandlerFunc) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		if !h.authorized(r) {
			writeOpenAIError(w, http.StatusUnauthorized, "invalid_api_key", "missing or invalid API key")
			return
		}
		next(w, r)
	}
}

// authorized 判断请求是否携带了正确的网关密钥；未配置密钥时一律放行。
func (h *Handler) authorized(r *http.Request) bool {
	if h.cfg.APIKey == "" {
		return true
	}
	if authz := r.Header.Get("Authorization"); strings.HasPrefix(authz, "Bearer ") {
		return strings.TrimPrefix(authz, "Bearer ") == h.cfg.APIKey
	}
	return r.Header.Get("x-api-key") == h.cfg.APIKey
}

func (h *Handler) healthz(w http.ResponseWriter, r *http.Request) {
	total, healthy, _, _, _ := h.cfg.Pool.CountsDetailed()
	// 用 ServableNow 判定：healthy>0 但全占满在途时 chat 会 503，探活必须同口径，
	// 否则负载均衡器会把流量持续打进无法受理的实例。
	status := http.StatusOK
	if !h.cfg.Pool.ServableNow() {
		status = http.StatusServiceUnavailable
	}
	// 恒无鉴权（负载均衡/编排探活只需 2xx/503 语义），身份靠 service 字段 + X-Service 头双保险。
	w.Header().Set("X-Service", ServiceName)
	writeJSON(w, status, map[string]any{
		"healthy": healthy,
		"total":   total,
		"service": ServiceName,
	})
}

func (h *Handler) status(w http.ResponseWriter, r *http.Request) {
	total, healthy, cooling, disabled, inFlightFull := h.cfg.Pool.CountsDetailed()
	sticky := 0
	if h.cfg.StickyCount != nil {
		sticky = h.cfg.StickyCount()
	}
	redisMode := h.cfg.RedisMode
	if redisMode == "" {
		redisMode = "noop"
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"accounts":        h.cfg.Pool.List(),
		"total":           total,
		"healthy":         healthy,
		"cooling":         cooling,
		"disabled":        disabled,
		"in_flight_full":  inFlightFull,
		"sticky_sessions": sticky,
		"redis_mode":      redisMode,
	})
}

// usageReport 返回网关累计 Token 用量（GET /usage?days=N，days 省略或 0 = 全部）。
//
// 数据来源是网关自己记录的每次成功请求的上游 usage，与本地客户端日志统计相互独立。
// 未装配统计器时返回 enabled=false，让宿主能区分「网关没开统计」与「统计为空」。
func (h *Handler) usageReport(w http.ResponseWriter, r *http.Request) {
	if h.cfg.Usage == nil {
		writeJSON(w, http.StatusOK, map[string]any{
			"enabled":     false,
			"generatedAt": time.Now().UnixMilli(),
		})
		return
	}
	days := 0
	if raw := r.URL.Query().Get("days"); raw != "" {
		if n, err := strconv.Atoi(raw); err == nil && n > 0 {
			days = n
		}
	}
	snapshot := h.cfg.Usage.Snapshot(days)
	snapshot["enabled"] = true
	writeJSON(w, http.StatusOK, snapshot)
}

// recordUsage 把一次请求采集到的完整用量写入统计；无计量或未装配统计时跳过。
// 只统计成功请求（上游返回了可用 usage 的请求），失败请求不计入。
func (h *Handler) recordUsage(s *chatStat) {
	if h.cfg.Usage == nil || !s.hasCounters {
		return
	}
	h.cfg.Usage.Record(s.uid, s.model, s.counters)
}

// withImageCapability 给静态表条目补上能力字段。
//
// 静态表取自 /v3/config 的 agents[cli].models，实测（2026-09-16，国服 16 个
// cli 模型 + 国际版 21 个模型池）该清单下**全部**模型 supportsImages=true，
// 因此统一标注为支持图片。
//
// 只影响「上游不可达、回退静态表」时的结果：动态拉取成功时用上游真值。
// 不标注的话，回退期间客户端会把所有模型当纯文本 —— 图片能力整个消失，
// 而这恰恰是最难排查的一类问题（网关看起来完全正常）。
func withImageCapability(entries []map[string]any) []map[string]any {
	yes := true
	for _, m := range entries {
		for k, v := range modelCapabilityFields(&yes) {
			if _, exists := m[k]; !exists {
				m[k] = v
			}
		}
	}
	return entries
}

// 静态 CN 模型表（api-reference §5，动态接口失败时的回退）。
var staticModels = withImageCapability([]map[string]any{
	{"id": "glm-5.2", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "glm-5.1", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "glm-5v-turbo", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "kimi-k2.7", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "minimax-m3", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "hy3", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "hy3-preview", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "hy3-preview-agent", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "deepseek-v4-pro", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
	{"id": "deepseek-v4-flash", "object": "model", "created": 1753600000, "owned_by": "workbuddy", "context_length": 131072},
})

// staticModelsIntl 国际版静态模型表（动态接口失败时的回退）。
//
// 取自 /v3/config 的 data.agents[name=="cli"].models（实测 2026-09-15），
// 即客户端选模型时真正看到的清单，另加 hy4-preview（见下）。
//
// 历史：此前该表抄自本地缓存 acc-product-config-v3.json，其中
//   - gpt-5.3-codex 属于 CodeBuddy 产品清单，不在 WorkBuddy 的 cli 清单里；
//   - 缺 kimi-k2.8-preview、hy4-preview-f。
// 现已按 /v3/config 校正。
//
// 关于 hy4-preview：它不在 cli 清单里，但**实测可用**（HTTP 200 正常出流），
// 且出现在 /v3/config 的 data.models 与 productFeaturesConfig.ModelTrialBanner 中
// （作为 hy4-preview-f 的试用目标模型）。保留它，避免用户手动指定时报「模型不存在」。
//
// 注意与国服的差异（这也是客户端选模型时最易踩的坑）：
//
//	国服   deepseek-v4-flash   / glm-5.2 / kimi-k2.7 / minimax-m3
//	国际版 deepseek-v4.1-flash / glm-5.3 / kimi-k3   / gpt-5.6-* / gemini-3.5-flash
var staticModelsIntl = withImageCapability([]map[string]any{
	{"id": "default-model", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 200000},
	{"id": "fast-model", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 200000},
	{"id": "balanced-model", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 256000},
	{"id": "primary-model", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 272000},
	{"id": "deep-model", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 200000},
	{"id": "hy4-preview-f", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 300000},
	{"id": "hy4-preview", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 200000},
	{"id": "hy3", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 192000},
	{"id": "deepseek-v4.1-flash", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 300000},
	{"id": "gpt-6-astra", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 400000},
	{"id": "gpt-5.6-sol", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 1000000},
	{"id": "gpt-5.6-terra", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 1000000},
	{"id": "gpt-5.6-luna", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 1000000},
	{"id": "gpt-5.5", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 1000000},
	{"id": "gpt-5.4", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 272000},
	{"id": "gemini-3.5-flash", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 1000000},
	{"id": "glm-5.3", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 1000000},
	{"id": "glm-5.2", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 1000000},
	{"id": "kimi-k3", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 1000000},
	{"id": "kimi-k2.8-preview", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 300000},
	{"id": "kimi-k2.6", "object": "model", "created": 1753600000, "owned_by": "workbuddy-intl", "context_length": 256000},
})

// staticModelsAll 合并两个区域的模型（按 id 去重，国服优先）。
//
// /v1/models 没有账号上下文，因此返回并集：客户端据此得知全部可用名称。
// 具体某个名称能否用，取决于实际选中的账号属于哪个区域 ——
// 不匹配时上游会返回 code=11102 model service info not found，提示清晰。
var staticModelsAll = func() []map[string]any {
	seen := map[string]bool{}
	out := make([]map[string]any, 0, len(staticModels)+len(staticModelsIntl))
	for _, m := range append(append([]map[string]any{}, staticModels...), staticModelsIntl...) {
		id, _ := m["id"].(string)
		if id == "" || seen[id] {
			continue
		}
		seen[id] = true
		out = append(out, m)
	}
	return out
}()

// dynamicModelsCache 动态模型缓存。
var dynamicModelsCache struct {
	sync.RWMutex
	ids      []upstream.ModelInfo
	fetched  time.Time // 最近一次成功拉取时间
	lastFail time.Time // 最近一次拉取失败时间（负缓存）
}

const (
	dynamicModelsTTL        = time.Hour
	modelsFetchFailCooldown = 5 * time.Minute
)

// models 返回模型列表：优先动态（缓存 1h），失败回退静态表。
func (h *Handler) models(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{
		"object": "list",
		"data":   h.modelList(),
	})
}

// modelCapabilityFields 生成模型能力字段（图片输入等）。
//
// 为什么一次下发**多种拼写**：客户端读的字段名各不相同，且都只在各自的
// provider 专用解析器里读，没有统一约定（实测 2026-09-16，见各客户端源码）：
//
//	OpenClaw   OpenAI Codex  → input_modalities / inputModalities
//	OpenClaw   Copilot       → capabilities.supports.vision
//	OpenClaw   HuggingFace   → architecture.input_modalities
//	OpenClaw   OpenRouter    → architecture.modality（"text+image->text"）
//	OpenClaw   Vercel AI GW  → tags 含 "vision"
//	OpenClaw   LM Studio     → capabilities.vision
//	ZCode      /v1/models    → 只读 id / supported_formats（不读能力字段）
//	DSH        /v1/models    → 只读 id/name/context/maxTokens（不读能力字段）
//
// 多写几种是安全的：所有已知解析器都只取自己认识的键，遇到多余键不会报错
// （OpenClaw 的 Copilot 解析器只额外要求 object=="model"，本函数已保证）。
// 这样 OpenClaw 等能读该字段的客户端可直接受益，其余客户端行为不变。
//
// supportsImages 为 nil（上游未声明）时**不下发**任何能力字段：宁可不写，
// 也不要谎报成纯文本 —— 后者会让本可用的图片能力被客户端主动关掉。
func modelCapabilityFields(supportsImages *bool) map[string]any {
	if supportsImages == nil {
		return nil
	}
	if !*supportsImages {
		// 显式不支持：明确告知，避免客户端按「默认支持」处理。
		return map[string]any{
			"supportsImages": false,
			"capabilities":   map[string]any{"vision": false, "supports": map[string]any{"vision": false}},
		}
	}
	return map[string]any{
		"supportsImages": true,
		// OpenClaw OpenAI Codex：接受 "image"/"vision" 两种写法。
		"input_modalities": []string{"text", "image"},
		"inputModalities":  []string{"text", "image"},
		// OpenClaw Copilot / LM Studio。
		"capabilities": map[string]any{
			"vision":   true,
			"supports": map[string]any{"vision": true},
		},
		// OpenClaw HuggingFace / OpenRouter。
		"architecture": map[string]any{
			"input_modalities": []string{"text", "image"},
			"modality":         "text+image->text",
		},
		// OpenClaw Vercel AI Gateway。
		"tags": []string{"vision"},
		// ZCode 自身配置用的词汇（对 /v1/models 无消费方，但无副作用且便于人读）。
		"modalities": map[string]any{
			"input":  []string{"text", "image"},
			"output": []string{"text"},
		},
	}
}

// modelList 动态获取模型列表并包装成 OpenAI 格式（含 context_length）。
func (h *Handler) modelList() []map[string]any {
	if infos := h.fetchDynamicModels(); len(infos) > 0 {
		out := make([]map[string]any, 0, len(infos)+len(staticModelsIntl))
		seen := make(map[string]bool, len(infos)+len(staticModelsIntl))
		for _, mi := range infos {
			entry := map[string]any{
				"id":                mi.ID,
				"object":            "model",
				"created":           1753600000,
				"owned_by":          "workbuddy",
				"context_length":    mi.ContextWindow,
				"max_output_tokens": mi.MaxTokens,
			}
			if mi.ContextWindow == 0 {
				entry["context_length"] = 131072 // 兜底
			}
			for k, v := range modelCapabilityFields(mi.SupportsImages) {
				entry[k] = v
			}
			seen[mi.ID] = true
			out = append(out, entry)
		}
		// 动态列表只来自「被抽中的那个账号」所在区域（通常是国服），
		// 另一个区域的模型名不会出现在里面。不补的话，混合账号池下客户端
		// 看不到国际版独有模型（如 hy4-preview），也就无法主动选用。
		// 注意：国际版的拉取接口已改用 /v3/config（两区域都可用），
		// 这里保留静态表补齐是为了覆盖「抽到国服账号」这一情况，属兜底。
		for _, m := range staticModelsIntl {
			if id, _ := m["id"].(string); id != "" && !seen[id] {
				seen[id] = true
				out = append(out, m)
			}
		}
		return out
	}
	return staticModelsAll
}

// fetchDynamicModels 从池中任一健康账号拉模型列表（含 contextWindow/maxTokens），缓存 1h。
// 拉取失败记录时间戳进入 5min 负缓存，冷却期内直接用静态表，避免反复打上游。
func (h *Handler) fetchDynamicModels() []upstream.ModelInfo {
	dynamicModelsCache.RLock()
	if len(dynamicModelsCache.ids) > 0 && time.Since(dynamicModelsCache.fetched) < dynamicModelsTTL {
		out := dynamicModelsCache.ids
		dynamicModelsCache.RUnlock()
		return out
	}
	// 失败负缓存：冷却期内不再请求上游。
	if !dynamicModelsCache.lastFail.IsZero() && time.Since(dynamicModelsCache.lastFail) < modelsFetchFailCooldown {
		dynamicModelsCache.RUnlock()
		return nil
	}
	dynamicModelsCache.RUnlock()

	acct := h.pickModelsProbeAccount()
	if acct == nil {
		return nil
	}
	infos, err := h.cfg.Upstream.FetchModels(acct)
	if err != nil || len(infos) == 0 {
		// **不喂熔断器**：/models 是「能力探测」接口（拿 contextWindow / efforts），
		// 它的失败不代表该账号不能聊天 —— 实测国际版账号的
		// /console/enterprises/personal/models 恒返回 500，而同账号的 chat 完全正常。
		//
		// 曾经这里调 NoteError(acct.UID)，导致：国际版账号恰好占据最早到期档位
		// （分层选号优先选它们）→ 每次客户端启动探测模型都记一次失败 → 累计 3 次
		// 触发 30 分钟熔断 → 界面上表现为「这几个国际版账号莫名被熔断」。
		//
		// 防重复请求由下面的 lastFail 负缓存负责，无需惩罚账号。
		dynamicModelsCache.Lock()
		dynamicModelsCache.lastFail = time.Now()
		dynamicModelsCache.Unlock()
		return nil
	}
	dynamicModelsCache.Lock()
	dynamicModelsCache.ids = infos
	dynamicModelsCache.fetched = time.Now()
	dynamicModelsCache.lastFail = time.Time{} // 成功则清空负缓存
	dynamicModelsCache.Unlock()
	return infos
}

// pickModelsProbeAccount 选一个用于探测 /models 的账号。
//
// 优先非国际版：国际版该端点恒 500（实测 5/5），选它只会浪费一次请求并让
// 动态模型列表永远拉不到（只能退回静态表）。
//
// 为什么不用 Pool.Pick()：探测是**只读能力发现**，不需要遵循分层/轮转选号策略 ——
// 那些策略的目的是「把流量导向最该用的账号」，而这里只需要一个能用的账号。
// 用 Pick() 反而会固定选中「最早到期档位」（可能整档都是国际版）。
//
// 全是国际版时仍返回其中一个（而非 nil）：万一上游修好了该端点，可自愈。
func (h *Handler) pickModelsProbeAccount() *auth.Auth {
	var intlFallback *auth.Auth
	for _, uid := range h.cfg.Pool.AvailableUIDs() {
		a := h.cfg.Pool.AuthByUID(uid)
		if a == nil {
			continue
		}
		if upstream.IsIntl(a) {
			if intlFallback == nil {
				intlFallback = a
			}
			continue
		}
		return a
	}
	return intlFallback
}

func (h *Handler) chatCompletions(w http.ResponseWriter, r *http.Request) {
	body, err := readLimitedBody(r)
	if err != nil {
		writeBodyReadError(w, err, openAIBodyCodes, writeOpenAIError)
		return
	}
	var peek struct {
		Stream bool `json:"stream"`
	}
	_ = json.Unmarshal(body, &peek)

	st := newChatStat(time.Now(), body, peek.Stream)
	defer func() {
		st.done()
		h.recordUsage(st)
	}()

	sessKey := ""
	if h.cfg.Session != nil {
		sessKey = session.ExtractKey(body)
	}

	result, status, ferr := h.forwardChat(body, peek.Stream, sessKey)
	if ferr != nil {
		st.status = status
		st.uid = result.UID
		code, msg := openAIFailure(ferr)
		writeOpenAIError(w, status, code, msg)
		return
	}
	st.uid = result.UID

	if result.Stream != nil {
		st.status = http.StatusOK
		stats := newChatStatsReaderSince(result.Stream, st.start)
		_ = upstream.Stream(w, stats)
		st.ttfb = stats.TTFB()
		st.toks, _ = stats.Tokens()
		if counters, ok := stats.Usage(); ok {
			st.setCounters(counters)
		}
		result.Stream.Close()
		h.release(result.UID)
		return
	}

	writeJSON(w, http.StatusOK, result.Response)
	st.status = http.StatusOK
	st.toks = completionTokens(result.Response)
	if u, ok := result.Response["usage"].(map[string]any); ok {
		st.setUsageMap(u)
	}
}

// applyErrorPolicy 按错误分类对账号施加冷却/禁用/熔断策略（最终版状态机）。
// kind 是唯一权威分类（来自 upstream.Classify），此处不再按原始 status 二次判断。
// 仅在 chat 轮转循环内调用：调用方已准备好 lastErr 并打算 continue 换号。
//
// model 是本次请求的目标模型，rawBody 是上游原始响应体 —— 仅 ErrModelRate 需要
// 用到（按 uid+model 记账，并从文案里取重置时间）。
//
// 六条路径，各司其职：
//   - ErrHardCredit → CooldownUntilTomorrow4AM：即时硬冷却到次日 04:00（等签到恢复）。
//   - ErrModelRate → CooldownModel：**模型级**冷却到上游给的重置时间；该账号其他模型不受影响。
//   - ErrSoftRate / ErrNotFound → Cooldown(CoolSoft)：账号级即时软冷却（429/404）。
//   - ErrSessionDead → Disable：session 死亡，永久禁用（需人工重登）。
//   - ErrServer → NoteError：喂单一连续失败计数器 fails + 累计错误 errTotal，
//     达到 breakerThreshold 触发熔断（指数退避）。
//   - 其他（default：ErrClient/ErrNone）→ 只换号不罚（防雪崩），不喂熔断。
//
// 恢复出口：CoolSoft/CoolHard 各自到期自动恢复；模型冷却按各自截止到期；
// 熔断按其指数退避截止到期；成功（NoteSuccess）清 fails/熔断；
// 签到解冻（ReenableIfCredits→reviveCoolingLocked）只清账号级冷却，不动熔断与模型冷却。
func (h *Handler) applyErrorPolicy(uid, model string, kind upstream.ErrKind, rawBody string) {
	switch kind {
	case upstream.ErrHardCredit:
		// 402 + 余额关键词即积分耗尽：同步冷却到次日 04:00（签到任务 09/21 点恢复），
		// 不需要异步核查（冗余）。立即换号。
		h.cfg.Pool.CooldownUntilTomorrow4AM(uid, "余额不足")
	case upstream.ErrModelRate:
		// 模型级限流（429 code=6004）：只冷却该账号的这个模型。
		//
		// 上游文案给出确切重置时刻（"将在 2026-09-15 13:25:47 UTC+8 重置"），
		// 优先用它 —— 比固定 soft_rate 更准，既不会过早重试（继续撞限流），
		// 也不会过晚恢复（白等）。解析不出时回退固定软冷却时长。
		now := time.Now()
		until, parsed := upstream.ParseResetTime(rawBody, now)
		if !parsed {
			until = now.Add(h.cfg.SoftCooldown)
		}
		h.cfg.Pool.CooldownModel(uid, model, until, modelRateReason(model, until, parsed), parsed)
	case upstream.ErrSoftRate:
		h.cfg.Pool.Cooldown(uid, pool.CoolSoft, h.cfg.SoftCooldown, "429 rate limit")
	case upstream.ErrSessionDead:
		h.cfg.Pool.Disable(uid, "12153 session dead")
	case upstream.ErrNotFound:
		// 404 短冷却（软冷却），防雪崩。
		h.cfg.Pool.Cooldown(uid, pool.CoolSoft, h.cfg.SoftCooldown, "upstream 404")
	case upstream.ErrServer:
		// 5xx 上游故障：Classify 已把 ≥500 判为 ErrServer，在此喂熔断计数（不再手写 status>=500）。
		h.cfg.Pool.NoteError(uid)
	default:
		// 其余（ErrClient/ErrNone）：只换号不罚（防雪崩），不喂熔断。
	}
}

// modelRateReason 生成模型冷却的展示文案（含模型名与重置时间）。
//
// 前端直接用这条文案，避免两端各自格式化时间导致口径不一致。
func modelRateReason(model string, until time.Time, parsed bool) string {
	name := model
	if name == "" {
		name = "（未知模型）"
	}
	if parsed {
		return fmt.Sprintf("%s 已达频率上限，%s 重置", name, until.In(time.Local).Format("01-02 15:04"))
	}
	return fmt.Sprintf("%s 已达频率上限（未取到重置时间，按软冷却 %s 处理）", name, until.In(time.Local).Format("15:04"))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

// maxRequestBody 请求体上限。
//
// 为什么从 8MB 提到 32MB：长对话（Claude Code / Codex 一轮带上大量文件内容与工具
// 结果）很容易突破 8MB，而**静默截断**会把合法 JSON 切成半截字节透传给上游，
// 上游 json.Decoder 报 `unexpected EOF`，表现为 400 code=11101
// "Unmarshal chat params failed with error: unexpected EOF" —— 客户端只看到
// 「请求参数有误」，完全无法定位到是网关截断（Issue #5 实测）。
const maxRequestBody = 32 << 20

// errBodyTooLarge 请求体超过 maxRequestBody。作为哨兵错误供 handler 回 413，
// 避免把截断后的坏字节继续往下传（那样只能在上游报出难以定位的解析错误）。
var errBodyTooLarge = errors.New("request body too large")

// readLimitedBody 读取请求体，超过 maxRequestBody 时**显式报错**而非静默截断。
//
// 关键差别：LimitReader 读满即返回，调用方无法区分「读完了」与「被截断了」，
// 于是截断体一路流到上游才炸。这里多读 1 字节来判定越界：读回长度 > 上限
// 即说明源还没结束，直接返回 errBodyTooLarge。
func readLimitedBody(r *http.Request) ([]byte, error) {
	body, err := io.ReadAll(io.LimitReader(r.Body, maxRequestBody+1))
	if err != nil {
		return nil, err
	}
	if len(body) > maxRequestBody {
		return nil, errBodyTooLarge
	}
	if len(body) == 0 {
		return nil, errors.New("empty request body")
	}
	return body, nil
}

var nowFunc = time.Now

func jsonUnmarshal(s string, v any) error {
	return json.Unmarshal([]byte(s), v)
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	raw, _ := json.Marshal(v)
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_, _ = w.Write(raw)
}

// openAIFailure 把转发失败翻译成 OpenAI 形状的错误码与消息。
//
// 默认沿用既有契约（no_healthy_account + 503，语义是「账号池暂时不可用，
// 稍后重试」）；只有**请求侧**错误才偏离 —— 那些错误重试多少次都一样，
// 必须让客户端看到真实原因，而不是被误导去等账号恢复。
//
// 两类请求侧错误（两者都是「重试无用、要改请求」）：
//   - 单一模型模式拒绝 → model_not_allowed（见 modelLockedError）
//   - 上下文超长       → context_length_exceeded
func openAIFailure(err error) (code, msg string) {
	if f := failureOf(err); f != nil && f.Kind == FailureContextTooLong {
		return "context_length_exceeded", f.Message
	}
	// 其余交给 errorCodeFor（当前只有 model_not_allowed 与 no_healthy_account），
	// 即 chat/completions 一直以来的行为。
	return errorCodeFor(err), errText(err)
}

// anthropicFailure 同上，Anthropic 词汇表。
//
// 上下文超长在 Anthropic 语义里是 invalid_request_error（其真实文案即
// "prompt is too long: ..."），而非 request_too_large（那是请求**字节数**超限）。
func anthropicFailure(err error) (code, msg string) {
	if f := failureOf(err); f != nil && f.Kind == FailureContextTooLong {
		return "invalid_request_error", f.Message
	}
	if errorCodeFor(err) != "no_healthy_account" {
		return "invalid_request_error", errText(err)
	}
	return "api_error", errText(err)
}

// responsesFailure 同上，Responses API 的上游失败码是 upstream_error。
//
// 单一模型拒绝沿用 #14 为该协议定的 invalid_request_error，**不**把
// errorCodeFor 的 model_not_allowed 直接透出：model_not_allowed 是本网关给
// chat/completions 形状定的码，不属于 Responses 词汇表（见 responsesBodyCodes
// ——该协议用 invalid_request / payload_too_large）。同理 anthropicFailure 保持
// invalid_request_error。三个入口共享的是「谁来判定失败类别」这条映射链，
// 不是同一个码面值；把码面值也一起统一会让各协议的词汇表互相串味。
func responsesFailure(err error) (code, msg string) {
	if f := failureOf(err); f != nil && f.Kind == FailureContextTooLong {
		return "context_length_exceeded", f.Message
	}
	if errorCodeFor(err) != "no_healthy_account" {
		return "invalid_request_error", errText(err)
	}
	return "upstream_error", errText(err)
}

// writeOpenAIError 写出 OpenAI 形状的错误体。
func writeOpenAIError(w http.ResponseWriter, status int, code, msg string) {
	writeJSON(w, status, map[string]any{
		"error": map[string]any{
			"message": msg,
			"type":    "api_error",
			"code":    code,
		},
	})
}

// bodyErrorCodes 各协议在「请求体超限」与「读取失败」两种情形下使用的错误码。
//
// 分开配置是因为三家协议的词汇表不同：OpenAI 用 payload_too_large /
// invalid_request，Anthropic 用 request_too_large / invalid_request_error，
// 客户端按自家词汇表分支处理，混用会让错误提示退化成未知错误。
type bodyErrorCodes struct {
	tooLarge   string
	badRequest string
}

// writeBodyReadError 把 readLimitedBody 的失败翻译成目标协议的错误响应。
//
// 超限返回 413 并给出明确原因：客户端据此知道要缩减历史，而不是收到一个
// "请求参数有误"然后无从下手（静默截断透传时的表现，见 Issue #5）。
// 其余读取错误维持 400 原语义。
func writeBodyReadError(w http.ResponseWriter, err error, codes bodyErrorCodes, write func(http.ResponseWriter, int, string, string)) {
	if errors.Is(err, errBodyTooLarge) {
		write(w, http.StatusRequestEntityTooLarge, codes.tooLarge,
			fmt.Sprintf("request body exceeds %d MB limit; reduce the conversation history or attachment size", maxRequestBody>>20))
		return
	}
	write(w, http.StatusBadRequest, codes.badRequest, "read body: "+err.Error())
}

// openAIBodyCodes OpenAI 系（chat/completions）的错误码。
var openAIBodyCodes = bodyErrorCodes{tooLarge: "payload_too_large", badRequest: "invalid_request"}

// anthropicBodyCodes Anthropic Messages 的错误码。
var anthropicBodyCodes = bodyErrorCodes{tooLarge: "request_too_large", badRequest: "invalid_request_error"}

// responsesBodyCodes OpenAI Responses 的错误码。
var responsesBodyCodes = bodyErrorCodes{tooLarge: "payload_too_large", badRequest: "invalid_request"}

// errorCodeFor 把 forwardChat 的错误映射成面向客户端的错误码。
//
// 区分「模型被单一模型模式拒绝」与「账号都不可用」很重要：前者是**调用方
// 需要改的东西**（换模型或换模式），后者是**服务端状态**。都报
// no_healthy_account 会把用户引向排查账号，而真正的原因在请求里。
func errorCodeFor(err error) string {
	var locked *modelLockedError
	if errors.As(err, &locked) {
		return "model_not_allowed"
	}
	return "no_healthy_account"
}
