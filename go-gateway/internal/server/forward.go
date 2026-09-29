package server

// forward.go 抽出「选号 → 轮转 → 转发上游 chat/completions」的核心流程，
// 供三种协议入口共用：
//
//   - POST /v1/chat/completions  原生 OpenAI Chat Completions（本文件直通）
//   - POST /v1/responses         OpenAI Responses API（Codex / ChatGPT 系）
//   - POST /v1/messages          Anthropic Messages API（Claude Code / Claude Desktop）
//
// 设计：后两者在进入本流程之前把请求体转换成 OpenAI Chat 形态，
// 拿到上游的 chat 响应（非流式 map 或原始 SSE 流）后，各自再转回目标协议。
// 这样账号池、粘性会话、熔断冷却、积分冷却、统计日志全部只有一份实现，
// 协议适配层只负责「形状转换」，不碰任何调度状态。

import (
	"encoding/json"
	"errors"
	"io"
	"log"
	"net/http"
	"regexp"
	"strings"

	"workbuddy2api/internal/auth"
	"workbuddy2api/internal/session"
	"workbuddy2api/internal/upstream"
)

// modelLockedError 「单一模型」模式拒绝了非目标模型的请求。
//
// 单独成型（而不是拼一个字符串）是为了让调用方能识别它并回以 400 +
// 明确的错误码，而不是当成「账号不可用」的 503 —— 后者会误导用户去查账号。
type modelLockedError struct {
	requested string // 客户端请求的模型（可能为空，表示请求体未带 model）
	allowed   string // 当前锁定的模型
}

func (e *modelLockedError) Error() string {
	got := e.requested
	if got == "" {
		got = "(未指定)"
	}
	return "当前为「单一模型」模式，只允许调用 " + e.allowed + "；收到的是 " + got +
		"。请在客户端把模型改为 " + e.allowed + "，或切换网关的工作模式。"
}

// chatResult 一次成功的上游调用结果。
//
// 二者互斥：
//   - Stream != nil  → 流式：调用方负责 Close，并按目标协议解析/转换 SSE。
//   - Response != nil → 非流式：上游 SSE 已被 Aggregate 成 OpenAI chat.completion。
type chatResult struct {
	UID      string
	Model    string
	Stream   io.ReadCloser
	Response map[string]any
}

// forwardChat 执行「选号 → token 刷新 → 转发 → 失败换号」的完整轮转。
//
// 参数：
//   - body：已转换成 OpenAI Chat 形态的请求体（原始字节，发往上游前由
//     upstream.Client 再做一次 PrepareBody：强制 stream、归一化 role/tool_choice）。
//   - stream：调用方是否要求流式。上游恒为流式，非流式时本函数读完后 Aggregate。
//   - sessKey：会话粘性键；空串表示不做粘性绑定。
//
// 返回：
//   - result：成功时非 nil。
//   - status/lastErr：失败时给出应回给客户端的 HTTP 状态与最后一处错误，
//     调用方据此生成对应协议的错误体。
//
// 失败语义与原有 chatCompletions 完全一致：传输层错误只换号不喂熔断，
// 业务错误按 Classify 结果施加冷却/禁用/熔断。
func (h *Handler) forwardChat(body []byte, stream bool, sessKey string) (*chatResult, int, error) {
	tried := map[string]bool{}
	var lastErr error
	var lastUID string
	lastStatus := http.StatusServiceUnavailable
	// lastKind/lastBody 记录最后一次上游失败的分类与原始响应体，
	// 用于在全部账号失败时给客户端一句**可读**的原因（见函数末尾的 FriendlyMessage）。
	var lastKind upstream.ErrKind
	var lastBody string
	// lastTransportErr 非 nil 表示最后一次失败是传输层（无上游响应体）。
	var lastTransportErr error

	var stickyUID string
	if sessKey != "" && h.cfg.Session != nil {
		if uid, ok := h.cfg.Session.Resolve(sessKey); ok {
			stickyUID = uid
		}
	}

	// 在途租约：成功选中即占名额，函数出口统一释放（成功即转移给调用方持有）。
	var heldUID string
	var handedOff bool
	defer func() {
		if heldUID != "" && !handedOff {
			h.cfg.Pool.Release(heldUID)
		}
	}()
	releaseHeld := func() {
		if heldUID != "" {
			h.cfg.Pool.Release(heldUID)
			heldUID = ""
		}
	}
	fail := func(uid string) {
		releaseHeld()
		if stickyUID != "" && uid == stickyUID && h.cfg.Session != nil {
			h.cfg.Session.Unbind(sessKey)
			stickyUID = ""
		}
	}

	// 请求的目标模型：用于「模型级限流」的选号过滤与冷却记账。
	// 取不到时为空串，各环节自动退化为原有行为（不做模型过滤）。
	model := modelOf(body)

	// 「单一模型」锁定：非空时只放行该模型。
	//
	// 为什么在选号之前就拒绝（而不是换个模型重试）：轮转模式的语义是
	// 「把这个账号的指定模型额度烧干净再换号」，模型是策略的一部分。
	// 若允许其他模型通过，客户端换个模型就能绕过轮转与额度控制，
	// 也让「当前烧的是哪个模型」变得不可预期 —— 因此明确拒绝并说明原因，
	// 比静默改写模型（用户以为在用 A、实际用了 B）更安全。
	if allowed := h.cfg.AllowedModel; allowed != "" && !strings.EqualFold(model, allowed) {
		// 返回非 nil 的 result：调用方会在错误分支里读 result.UID 记日志，
		// 返回 nil 会 panic。UID 留空即可（本次没有选中任何账号）。
		return &chatResult{Model: model}, http.StatusBadRequest,
			&modelLockedError{requested: model, allowed: allowed}
	}

	for i := 0; i < h.cfg.MaxRotate; i++ {
		var acct *auth.Auth
		if stickyUID != "" {
			acct = h.cfg.Pool.PickByUIDForModel(stickyUID, model)
			if acct == nil {
				if h.cfg.Session != nil {
					h.cfg.Session.Unbind(sessKey)
				}
				stickyUID = ""
			}
		}
		if acct == nil {
			acct = h.cfg.Pool.PickForModel(model, tried)
		}
		if acct == nil {
			lastStatus = http.StatusServiceUnavailable
			break
		}
		tried[acct.UID] = true
		lastUID = acct.UID

		if !h.cfg.Pool.Acquire(acct.UID) {
			if stickyUID != "" && acct.UID == stickyUID && h.cfg.Session != nil {
				h.cfg.Session.Unbind(sessKey)
				stickyUID = ""
			}
			continue
		}
		heldUID = acct.UID

		// token 临近过期 → 先 refresh（失败冷却换号）
		if acct.NeedsRefresh(h.cfg.RefreshSkew) {
			if err := h.cfg.Upstream.RefreshToken(acct); err != nil {
				lastErr = err
				var ue *upstream.Error
				// 末尾的可读文案读的是 lastKind/lastBody/lastTransportErr，三者必须
				// 与 lastErr 同步更新 —— 否则会沿用**上一个账号**留下的分类，例如把
				// 「刷新失败」报成「额度已耗尽」，把用户引向错误的排查方向。
				if errors.As(err, &ue) {
					lastKind, lastTransportErr = ue.Kind, nil
				} else {
					// 非 upstream.Error 的失败基本都是传输层（超时 / 连接被拒）
					lastKind, lastTransportErr = upstream.ErrNone, err
				}
				lastBody = ""
				if errors.As(err, &ue) && ue.Kind == upstream.ErrSessionDead {
					h.cfg.Pool.Disable(acct.UID, "refresh session dead")
				} else {
					h.cfg.Pool.NoteError(acct.UID)
				}
				fail(acct.UID)
				continue
			}
			if err := acct.SaveAtomic(); err != nil {
				log.Printf("chat refresh uid=%s: save auth failed: %v", acct.UID, err)
			}
		}

		rc, status, respBody, terr := h.cfg.Upstream.ChatStream(acct, body)
		if terr != nil {
			lastStatus = http.StatusServiceUnavailable
			lastErr = terr
			// 传输层失败（超时/连接被拒/DNS）没有上游业务体，Classify 不适用；
			// 单独标记，使末尾的错误文案也能给出可读原因而不是原始 Go 报错
			// （实测：原始文案会把 tcp 四元组与 wsarecv 细节直接抛给客户端）。
			lastKind = upstream.ErrNone
			lastBody = ""
			lastTransportErr = terr
			fail(acct.UID)
			continue
		}
		if status >= 400 {
			lastStatus = status
			kind := upstream.Classify(status, string(respBody))
			lastKind = kind
			lastBody = string(respBody)
			lastTransportErr = nil
			lastErr = &upstream.Error{Kind: kind, Status: status, Msg: string(respBody)}

			// 上下文超长是**请求侧**错误：换号无用（同一请求体发给任何账号都同样失败），
			// 继续轮转只会把整个请求体对着每个账号重传一遍（实测 1.12M token × 3），
			// 最后还被伪装成「账号全部不可用」，把排查方向引向账号故障。
			//
			// 立即以真实状态返回，交由客户端精简上下文后重试。
			// 不罚账号也不换号：账号状态完全不动（applyErrorPolicy 对请求侧错误
			// 本就只换号不罚，这里连换号都省掉）。
			//
			// 上下文超长与安全审查属于**请求侧**错误：换号无用（同一请求体发给任何账号都同样失败），
			// 立即以真实状态返回，交由客户端调整后重试。
			// 不罚账号也不换号：账号状态完全不动。
			if kind == upstream.ErrContextTooLong {
				uid := acct.UID
				releaseHeld()
				return &chatResult{UID: uid}, status, &forwardFailure{
					Kind:    FailureContextTooLong,
					Status:  status,
					Message: upstream.ContextTooLongMessage(string(respBody)),
				}
			}
			if kind == upstream.ErrContentSafety {
				uid := acct.UID
				releaseHeld()
				return &chatResult{UID: uid}, http.StatusBadRequest, &forwardFailure{
					Kind:    FailureContentSafety,
					Status:  http.StatusBadRequest,
					Message: upstream.ContentSafetyMessage(string(respBody)),
				}
			}

			h.applyErrorPolicy(acct.UID, model, kind, string(respBody))
			fail(acct.UID)
			continue
		}

		h.cfg.Pool.NoteSuccess(acct.UID)
		// 粘性跟随最终成功号。
		if sessKey != "" && h.cfg.Session != nil {
			h.cfg.Session.Bind(sessKey, acct.UID)
		}

		uid := acct.UID
		handedOff = true // 租约移交调用方，由其读完/关闭后释放

		if stream {
			return &chatResult{UID: uid, Model: modelOf(body), Stream: rc}, status, nil
		}

		resp, err := upstream.Aggregate(rc)
		rc.Close()
		h.cfg.Pool.Release(uid)
		handedOff = false
		heldUID = ""
		if err != nil {
			return nil, http.StatusBadGateway, err
		}
		return &chatResult{UID: uid, Model: modelOf(body), Response: resp}, http.StatusOK, nil
	}

	msg := "all accounts unavailable (cooling/disabled)"
	if lastErr != nil {
		// 客户端可读性：优先用提炼后的原因（如「账号额度已耗尽…」），
		// 拿不到再用原始文案 —— 原始文案含整段上游 JSON 或 tcp 底层细节，又长又难懂。
		if friendly := upstream.FriendlyMessage(lastKind, lastStatus, lastBody); friendly != "" {
			msg = friendly
		} else if lastTransportErr != nil {
			msg = "无法连接上游（网络超时 / 连接被拒）：请检查本机网络或代理设置后重试"
		} else {
			msg += ": " + sanitizeUpstreamErrorText(lastErr.Error())
		}
	}
	// 失败时也带上最后尝试过的账号，请求日志据此仍能显示 uid（与原实现一致）。
	return &chatResult{UID: lastUID}, lastStatus, errors.New(msg)
}

// sanitizeUpstreamErrorText 移除错误文本中可能误导客户端（如 DSH 正则判定 AUTH）的 401/403 字样。
func sanitizeUpstreamErrorText(s string) string {
	reHTTP := regexp.MustCompile(`\bhttp\s+(401|403)\b`)
	s = reHTTP.ReplaceAllString(s, "http rejected")
	reNum := regexp.MustCompile(`\b(401|403)\b`)
	return reNum.ReplaceAllString(s, "rejected")
}

// FailureKind 失败类别：决定回给客户端的错误码。
//
// 存在的意义是让**请求侧**错误说实话：旧实现无论什么原因都回
// no_healthy_account + "all accounts unavailable (cooling/disabled)"，
// 把「这次请求太大」伪装成「账号全挂了」，排查时被直接带偏（2026-09-15 现场）。
type FailureKind int

const (
	// FailureUpstream 其余上游失败：沿用既有契约（503 no_healthy_account）。
	// 账号池耗尽的语义由它承载，客户端据此稍后重试。
	FailureUpstream FailureKind = iota
	// FailureContextTooLong 请求上下文超出模型窗口：请求侧错误，换号无用。
	FailureContextTooLong
	// FailureContentSafety 请求内容未通过安全审查：请求侧错误，换号无用。
	FailureContentSafety
)

// forwardFailure 一次需要特殊上报的转发失败。
//
// 只有需要偏离「503 no_healthy_account」默认契约的失败才用它；
// 其余失败仍是普通 error，行为与旧实现完全一致。
type forwardFailure struct {
	Kind    FailureKind
	Status  int    // 回给客户端的 HTTP 状态
	Message string // 面向客户端的错误消息
}

func (e *forwardFailure) Error() string { return e.Message }

// failureOf 取出 *forwardFailure（若有），供各协议入口按类别选错误码。
func failureOf(err error) *forwardFailure {
	var f *forwardFailure
	if errors.As(err, &f) {
		return f
	}
	return nil
}

// errText 供各协议入口取失败文案。
func errText(err error) string {
	if err == nil {
		return "unknown error"
	}
	return err.Error()
}

// release 供调用方在流式转发结束后归还租约。
func (h *Handler) release(uid string) {
	if uid != "" {
		h.cfg.Pool.Release(uid)
	}
}

// modelOf 从 OpenAI Chat 请求体里取 model 字段（仅用于日志与回填响应）。
func modelOf(body []byte) string {
	var probe struct {
		Model string `json:"model"`
	}
	_ = json.Unmarshal(body, &probe)
	return probe.Model
}

// buildSessKey 为没有原生会话字段的协议（如 Anthropic Messages）合成粘性键。
//
// Anthropic 请求体没有 conversation_id，但有 system + 首条 user 消息；
// 用它们的短哈希做键即可让同一会话稳定命中同一账号。
func buildSessKey(seed string) string {
	if seed == "" {
		return ""
	}
	return session.SessionKeyFromSeed(seed)
}
