import { useEffect, useMemo, useState } from "react";
import {
  Activity,
  AlertTriangle,
  CheckCircle2,
  ChevronLeft,
  ChevronRight,
  ListFilter,
  Loader2,
  RefreshCw,
  Search,
  SearchX,
} from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import * as api from "@/lib/api";
import type {
  GatewayRequestRecord,
  GatewayRequestsResult,
  GatewayUsageGroup,
  GatewayUsageResult,
} from "@/lib/types";
import { cn } from "@/lib/utils";
import { useAccountsStore } from "@/stores/accounts";

type RangeKey = "today" | "7d" | "30d" | "all";

const RANGE_OPTIONS: { key: RangeKey; label: string; days?: number }[] = [
  { key: "today", label: "今日", days: 1 },
  { key: "7d", label: "近 7 天", days: 7 },
  { key: "30d", label: "近 30 天", days: 30 },
  { key: "all", label: "全部" },
];

const exact = new Intl.NumberFormat("en-US");

function compact(value: number): string {
  const abs = Math.abs(value);
  if (abs >= 1000000000) return (value / 1000000000).toFixed(1) + "B";
  if (abs >= 1000000) return (value / 1000000).toFixed(1) + "M";
  if (abs >= 1000) return (value / 1000).toFixed(1) + "K";
  return exact.format(value);
}

function maxOf(values: number[]): number {
  return values.reduce((m, v) => Math.max(m, v), 1);
}

function relative(deltaMs: number): string {
  const sec = Math.max(0, Math.floor(deltaMs / 1000));
  if (sec < 5) return "刚刚";
  if (sec < 60) return sec + " 秒前";
  const min = Math.floor(sec / 60);
  if (min < 60) return min + " 分钟前";
  const hour = Math.floor(min / 60);
  if (hour < 24) return hour + " 小时前";
  return Math.floor(hour / 24) + " 天前";
}

function labelOf(a: { uid: string | null; nickname?: string | null; note?: string | null }): string {
  return a.note?.trim() || a.nickname?.trim() || (a.uid ?? "").slice(0, 8);
}

// ---------------------------------------------------------------------------
// 请求明细：列格式化 + 客户端筛选分页
// ---------------------------------------------------------------------------

/** 每页条数候选；50 起步是因为 12 列表格再小就失去了「扫一眼」的意义。 */
const PAGE_SIZE_OPTIONS = [50, 100, 200] as const;
/** 明细一次拉取上限：与网关侧 limit 语义对齐，再大也不适合放在一张表里看。 */
const REQUEST_FETCH_LIMIT = 1000;

/** 结果筛选值：`all` 之外即字面量，便于直接当 key 用。 */
type ResultFilter = "all" | "success" | "failure";

/** 时间列：`MM/DD HH:mm:ss`（跨年也够用，12 列表格不宜再宽）。 */
function formatRequestTime(ts: number): string {
  const d = new Date(ts);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getMonth() + 1)}/${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

/** 区域文案：未知区域显示 `—`，避免表格里出现空白单元格。 */
function regionLabel(region: string): string {
  if (region === "cn") return "国内";
  if (region === "intl") return "国际版";
  return "—";
}

/** 首字延迟：0 表示非流式或未记录，显示 `—` 而不是 `0 ms`。 */
function formatTtfb(ms: number): string {
  return ms > 0 ? exact.format(Math.round(ms)) + " ms" : "—";
}

/** 总耗时：秒级改用秒，否则毫秒更直观。 */
function formatDuration(ms: number): string {
  if (ms >= 1000) return (ms / 1000).toFixed(2) + " s";
  return exact.format(Math.round(ms)) + " ms";
}

/** 缓存率：null（无输入）显示 `—`；只保留 1 位小数，避免表格里数字过长。 */
function formatPercent(rate: number | null): string {
  return rate == null ? "—" : (rate * 100).toFixed(1) + "%";
}

/** 账号列：优先昵称 / 备注，映射不到时退化成 uid 前 8 位 + `…`（与「按账号」一致）。 */
function accountLabel(record: GatewayRequestRecord, nickname: Map<string, string>): string {
  const label = nickname.get(record.uid);
  if (label) return label;
  return record.uid ? record.uid.slice(0, 8) + "…" : "—";
}

/** 模型列副行：`区域 · 入口 · 流式/非流式`。 */
function requestMeta(record: GatewayRequestRecord): string {
  return [regionLabel(record.region), record.entry || "—", record.stream ? "流式" : "非流式"].join(" · ");
}

/** 表头 / 单元格共用的对齐类：文本左对齐、数字右对齐，表头保持同一套。 */
const TH_TEXT = "px-3 py-2.5 font-medium";
const TH_NUM = "px-3 py-2.5 text-right font-medium";
const TD_TEXT = "px-3 py-2.5";
const TD_NUM = "px-3 py-2.5 text-right tabular-nums";

function BarRow({ label, value, max, meta }: { label: string; value: number; max: number; meta?: string }) {
  const percent = max > 0 ? Math.max(3, Math.round((value / max) * 100)) : 0;
  return (
    <div className="space-y-1.5 px-4 py-2 sm:px-5">
      <div className="flex items-baseline justify-between gap-3 text-xs">
        <span className="min-w-0 truncate">{label}</span>
        <span className="shrink-0 tabular-nums text-muted-foreground">{compact(value)}</span>
      </div>
      <div className="h-1.5 overflow-hidden rounded-full bg-muted">
        <div className="h-full rounded-full bg-primary/70" style={{ width: percent + "%" }} />
      </div>
      {meta ? <div className="truncate text-[11px] text-muted-foreground">{meta}</div> : null}
    </div>
  );
}

function Stat({ label, value, hint }: { label: string; value: string; hint?: string }) {
  return (
    <div className="min-w-0 rounded-lg border border-border/60 px-3 py-2">
      <div className="text-[11px] text-muted-foreground">{label}</div>
      <div className="mt-0.5 truncate text-[15px] font-medium tabular-nums">{value}</div>
      {hint ? <div className="mt-0.5 truncate text-[11px] text-muted-foreground">{hint}</div> : null}
    </div>
  );
}

function Hint({ icon, children }: { icon: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-center py-6">
      <div className="flex items-center gap-2 text-xs text-muted-foreground">
        {icon}
        {children}
      </div>
    </div>
  );
}

/** 明细表单行。12 列里 9 列是数字，统一 tabular-nums 才能竖向对齐着看。 */
function RequestRow({ record, nickname }: { record: GatewayRequestRecord; nickname: Map<string, string> }) {
  const ok = record.status >= 200 && record.status < 300;
  const account = accountLabel(record, nickname);
  return (
    <tr className="border-t border-border/50 align-top transition-colors hover:bg-muted/40">
      <td className={cn(TD_TEXT, "whitespace-nowrap tabular-nums text-muted-foreground")}>
        {formatRequestTime(record.ts)}
      </td>
      <td className={cn(TD_TEXT, "max-w-[190px]")}>
        <div className="truncate" title={record.model}>
          {record.model || "—"}
        </div>
        <div className="mt-0.5 truncate text-[10px] text-muted-foreground">{requestMeta(record)}</div>
      </td>
      <td className={cn(TD_TEXT, "max-w-[140px] truncate")} title={record.uid}>
        {account}
      </td>
      <td className={TD_NUM}>{compact(record.input)}</td>
      <td className={TD_NUM}>{compact(record.output)}</td>
      <td className={TD_NUM}>{compact(record.cacheRead)}</td>
      <td className={cn(TD_NUM, "text-muted-foreground")}>{formatPercent(record.cacheHitRate)}</td>
      <td className={cn(TD_NUM, "font-medium")}>{compact(record.total)}</td>
      <td className={TD_NUM}>{record.tps > 0 ? record.tps.toFixed(1) + " t/s" : "—"}</td>
      <td className={TD_NUM}>{formatTtfb(record.ttfbMs)}</td>
      <td className={TD_NUM}>{formatDuration(record.totalMs)}</td>
      <td className={cn(TD_TEXT, "whitespace-nowrap")}>
        {ok ? (
          <Badge variant="success">成功</Badge>
        ) : (
          <span className="flex items-center gap-1.5">
            <Badge variant="destructive">失败</Badge>
            <span className="text-[10px] tabular-nums text-muted-foreground">{record.status}</span>
          </span>
        )}
      </td>
    </tr>
  );
}

/** Radix Select 不接受空字符串值，用一个不会与真实模型名冲突的哨兵表示「全部」。 */
const ALL_FILTER = "__all__";

/**
 * 「请求明细」卡片：顶部筛选条 + 12 列高密度表格 + 底部分页条。
 *
 * 数据由父组件一次性取回（最多 1000 条），筛选与分页都在客户端完成：
 * 网关侧只支持「按天数 + 条数」两个维度，逐条翻页再请求会白跑很多次网络。
 */
function RequestsDetailCard({
  data,
  loading,
  nickname,
}: {
  data: GatewayRequestsResult | null;
  loading: boolean;
  nickname: Map<string, string>;
}) {
  const [model, setModel] = useState(ALL_FILTER);
  const [result, setResult] = useState<ResultFilter>("all");
  const [keyword, setKeyword] = useState("");
  const [pageSize, setPageSize] = useState<number>(PAGE_SIZE_OPTIONS[0]);
  const [page, setPage] = useState(1);

  const snapshot = data?.usage ?? null;
  const records = useMemo(() => snapshot?.requests ?? [], [snapshot]);

  // 模型候选取自当前这批数据本身：下拉里不会出现选了却一行都匹配不到的模型。
  const modelOptions = useMemo(() => {
    const set = new Set<string>();
    for (const record of records) if (record.model) set.add(record.model);
    return [...set].sort((a, b) => a.localeCompare(b));
  }, [records]);

  const filtered = useMemo(() => {
    const needle = keyword.trim().toLowerCase();
    return records.filter((record) => {
      if (model !== ALL_FILTER && record.model !== model) return false;
      const ok = record.status >= 200 && record.status < 300;
      if (result === "success" && !ok) return false;
      if (result === "failure" && ok) return false;
      if (!needle) return true;
      // 关键字同时匹配模型名 / uid / 账号昵称：三种定位方式都是排查时的常用入口。
      const label = nickname.get(record.uid) ?? "";
      return (
        record.model.toLowerCase().includes(needle) ||
        record.uid.toLowerCase().includes(needle) ||
        label.toLowerCase().includes(needle)
      );
    });
  }, [records, model, result, keyword, nickname]);

  const pageCount = Math.max(1, Math.ceil(filtered.length / pageSize));
  // 刷新后条数可能变少，这里钳制页码而不是再开一个 effect，避免闪一帧空表。
  const currentPage = Math.min(page, pageCount);
  const start = (currentPage - 1) * pageSize;
  const pageRows = filtered.slice(start, start + pageSize);
  const firstShown = filtered.length === 0 ? 0 : start + 1;
  const lastShown = start + pageRows.length;

  if (loading && !data) {
    return <Hint icon={<Loader2 className="size-3.5 animate-spin" />}>正在读取网关请求明细…</Hint>;
  }
  if (!data || !data.running) {
    return (
      <Hint icon={<CheckCircle2 className="size-3.5" />}>
        网关未运行，启动后这里会展示逐条请求明细
      </Hint>
    );
  }
  if (!data.reachable) {
    return (
      <Hint icon={<Activity className="size-3.5" />}>
        网关已启动但暂时无法读取请求明细{data.error ? "：" + data.error : ""}
      </Hint>
    );
  }
  if (snapshot && snapshot.enabled === false) {
    return (
      <Hint icon={<AlertTriangle className="size-3.5" />}>
        当前网关可执行文件不支持用量统计，请更新网关后重试
      </Hint>
    );
  }
  if (!snapshot) {
    return (
      <Hint icon={<AlertTriangle className="size-3.5" />}>
        无法读取网关请求明细{data.error ? "：" + data.error : ""}
      </Hint>
    );
  }
  if (records.length === 0) {
    return <Hint icon={<SearchX className="size-3.5" />}>该范围内暂无请求明细</Hint>;
  }

  const truncated = snapshot.total > snapshot.returned;
  const summary = truncated
    ? "网关仅返回最近 " + exact.format(snapshot.returned) + " / " + exact.format(snapshot.total) + " 条"
    : "范围内共 " + exact.format(snapshot.total) + " 条请求";

  return (
    <>
      <div className="mx-4 flex min-w-0 flex-wrap items-center justify-between gap-3 border-b border-border/50 py-2.5 sm:mx-5">
        <div className="flex min-w-0 items-center gap-1.5 text-[11px] text-muted-foreground">
          <ListFilter className="size-3.5 shrink-0" />
          <span className="truncate" title={summary}>
            {summary}
            {filtered.length !== records.length ? " · 已筛选 " + exact.format(filtered.length) + " 条" : ""}
            {loading ? " · 更新中…" : ""}
          </span>
        </div>
        <div className="flex min-w-0 flex-wrap items-center gap-1.5">
          <Select
            value={model}
            onValueChange={(value) => {
              setModel(value);
              setPage(1);
            }}
          >
            <SelectTrigger size="sm" className="h-7 w-[150px] text-xs" aria-label="按模型筛选">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={ALL_FILTER}>全部模型</SelectItem>
              {modelOptions.map((name) => (
                <SelectItem key={name} value={name}>
                  {name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>

          <Select
            value={result}
            onValueChange={(value) => {
              setResult(value === "success" || value === "failure" ? value : "all");
              setPage(1);
            }}
          >
            <SelectTrigger size="sm" className="h-7 w-[110px] text-xs" aria-label="按结果筛选">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="all">全部结果</SelectItem>
              <SelectItem value="success">仅成功</SelectItem>
              <SelectItem value="failure">仅失败</SelectItem>
            </SelectContent>
          </Select>

          <div className="relative">
            <Search className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input
              className="h-7 w-[200px] pl-7 text-xs"
              placeholder="搜索模型 / 账号 / uid"
              value={keyword}
              onChange={(event) => {
                setKeyword(event.target.value);
                setPage(1);
              }}
              aria-label="搜索请求明细"
            />
          </div>

          <Select
            value={String(pageSize)}
            onValueChange={(value) => {
              setPageSize(Number(value));
              setPage(1);
            }}
          >
            <SelectTrigger size="sm" className="h-7 w-[110px] text-xs" aria-label="每页条数">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {PAGE_SIZE_OPTIONS.map((size) => (
                <SelectItem key={size} value={String(size)}>
                  每页 {size} 条
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      </div>

      {/* 12 列在窄窗口下必然放不下：外层 overflow-auto 同时负责横向滚动与表头吸顶。 */}
      {filtered.length === 0 ? (
        <Hint icon={<SearchX className="size-3.5" />}>没有符合筛选条件的请求明细，试着放宽条件</Hint>
      ) : (
        <div className="min-w-0 max-h-[600px] overflow-auto">
          <table className="w-full min-w-[1120px] text-left text-[11px]">
            <thead className="sticky top-0 z-10 bg-muted/95 text-muted-foreground">
              <tr>
                <th className={TH_TEXT}>时间</th>
                <th className={TH_TEXT}>模型</th>
                <th className={TH_TEXT}>账号</th>
                <th className={TH_NUM}>输入</th>
                <th className={TH_NUM}>输出</th>
                <th className={TH_NUM}>缓存</th>
                <th className={TH_NUM}>缓存率</th>
                <th className={TH_NUM}>总计</th>
                <th className={TH_NUM}>速度</th>
                <th className={TH_NUM}>首字</th>
                <th className={TH_NUM}>耗时</th>
                <th className={TH_TEXT}>结果</th>
              </tr>
            </thead>
            <tbody>
              {pageRows.map((record) => (
                <RequestRow key={record.seq} record={record} nickname={nickname} />
              ))}
            </tbody>
          </table>
        </div>
      )}

      <div className="mx-4 flex min-w-0 flex-wrap items-center justify-between gap-x-3 gap-y-1.5 border-t border-border/50 py-2.5 text-[11px] text-muted-foreground sm:mx-5">
        <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
          <span className="tabular-nums">共 {exact.format(filtered.length)} 条记录</span>
          <span className="tabular-nums">
            显示第 {exact.format(firstShown)} - {exact.format(lastShown)} 条
          </span>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <Button
            variant="outline"
            size="sm"
            className="h-7 px-2 text-xs"
            disabled={currentPage <= 1}
            onClick={() => setPage(currentPage - 1)}
          >
            <ChevronLeft className="size-3.5" />
            上一页
          </Button>
          <span className="px-1 tabular-nums">
            第 {currentPage} / {pageCount} 页
          </span>
          <Button
            variant="outline"
            size="sm"
            className="h-7 px-2 text-xs"
            disabled={currentPage >= pageCount}
            onClick={() => setPage(currentPage + 1)}
          >
            下一页
            <ChevronRight className="size-3.5" />
          </Button>
        </div>
      </div>
    </>
  );
}

export default function UsageRecordsPage() {
  const accounts = useAccountsStore((s) => s.accounts);
  const [range, setRange] = useState<RangeKey>("today");
  const [data, setData] = useState<GatewayUsageResult | null>(null);
  const [detailData, setDetailData] = useState<GatewayRequestsResult | null>(null);
  const [loading, setLoading] = useState(true);
  const [detailLoading, setDetailLoading] = useState(true);
  const [nonce, setNonce] = useState(0);
  const [updatedAt, setUpdatedAt] = useState<number | null>(null);
  const [nowTick, setNowTick] = useState(() => Date.now());

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setDetailLoading(true);
    const days = RANGE_OPTIONS.find((o) => o.key === range)?.days;
    // 聚合与明细并行请求、各自落地：任一方失败都不该让另一块卡片变成空白。
    api
      .getGatewayUsage(days)
      .then((res) => {
        if (cancelled) return;
        setData(res);
        if (res.usage) setUpdatedAt(Date.now());
      })
      .catch((e) => {
        if (cancelled) return;
        setData({ running: false, reachable: false, usage: null, error: api.asError(e) });
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    api
      .getGatewayUsageRequests(days, REQUEST_FETCH_LIMIT)
      .then((res) => {
        if (cancelled) return;
        setDetailData(res);
      })
      .catch((e) => {
        if (cancelled) return;
        setDetailData({ running: false, reachable: false, usage: null, error: api.asError(e) });
      })
      .finally(() => {
        if (!cancelled) setDetailLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [range, nonce]);

  useEffect(() => {
    const t = window.setInterval(() => setNowTick(Date.now()), 1000);
    return () => window.clearInterval(t);
  }, []);

  const nickname = useMemo(() => {
    const m = new Map<string, string>();
    for (const a of accounts) if (a.uid) m.set(a.uid, labelOf(a));
    return m;
  }, [accounts]);

  const snapshot = data?.usage ?? null;
  const summary = snapshot?.summary ?? null;
  const models = snapshot?.models ?? [];
  const accountRows = snapshot?.accounts ?? [];
  const daily = snapshot?.daily ?? [];
  const maxModel = maxOf(models.map((m) => m.total));
  const maxAccount = maxOf(accountRows.map((a) => a.total));
  const maxDaily = maxOf(daily.map((d) => d.total));

  return (
    <div className="mx-auto w-full max-w-[1180px] min-w-0 px-4 py-6 sm:px-8 sm:py-9">
      <header className="mb-10 flex min-w-0 flex-wrap items-start justify-between gap-4 sm:mb-12">
        <div className="min-w-0">
          <h1 className="text-[28px] font-semibold tracking-tight">使用记录</h1>
          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
            经网关成功请求的上游用量，按模型 / 账号 / 日期聚合（网关重启后保留）。
          </p>
        </div>
        <Button
          className="shrink-0"
          variant="outline"
          size="sm"
          onClick={() => setNonce((n) => n + 1)}
          disabled={loading}
        >
          {loading ? <Loader2 className="animate-spin" /> : <RefreshCw />}
          刷新用量
        </Button>
      </header>

      <section className="min-w-0 space-y-2.5" aria-labelledby="usage-records-title">
        <div className="px-1">
          <h2 id="usage-records-title" className="text-[13px] font-medium leading-5">
            Token 用量
          </h2>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <div className="mx-4 flex min-w-0 flex-wrap items-center justify-between gap-3 border-b border-border/50 py-2.5 sm:mx-5">
            <div className="min-w-0">
              <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5">
                <span className="text-[13px]">统计范围</span>
                {updatedAt ? (
                  <span
                    className="text-[11px] tabular-nums text-muted-foreground"
                    title={"上次更新：" + new Date(updatedAt).toLocaleString("zh-CN")}
                  >
                    上次更新 {relative(nowTick - updatedAt)}
                    {loading ? " · 更新中…" : ""}
                  </span>
                ) : null}
              </div>
              <div className="mt-0.5 text-[11px] tabular-nums text-muted-foreground">
                {summary
                  ? "共 " + exact.format(summary.records) + " 次调用 · 合计 " + exact.format(summary.total) + " tokens"
                  : "等待网关数据"}
              </div>
            </div>
            <div className="flex shrink-0 items-center gap-1.5">
              {RANGE_OPTIONS.map((o) => (
                <Button
                  key={o.key}
                  variant={range === o.key ? "default" : "outline"}
                  size="sm"
                  className="h-7 px-2 text-xs"
                  onClick={() => setRange(o.key)}
                >
                  {o.label}
                </Button>
              ))}
              <Button
                variant="ghost"
                size="icon"
                className="size-7"
                onClick={() => setNonce((n) => n + 1)}
                disabled={loading}
                aria-label="刷新用量"
              >
                <RefreshCw className={cn("size-3.5", loading && "animate-spin")} />
              </Button>
            </div>
          </div>

          {loading && !snapshot ? (
            <Hint icon={<Loader2 className="size-3.5 animate-spin" />}>正在读取网关用量…</Hint>
          ) : (data && !data.running) || !data ? (
            <Hint icon={<CheckCircle2 className="size-3.5" />}>
              网关未运行，启动后这里会展示经网关请求的 Token 用量
            </Hint>
          ) : !data.reachable ? (
            <Hint icon={<Activity className="size-3.5" />}>
              网关已启动但暂时无法读取用量{data.error ? "：" + data.error : ""}
            </Hint>
          ) : snapshot && snapshot.enabled === false ? (
            <Hint icon={<AlertTriangle className="size-3.5" />}>
              当前网关可执行文件不支持用量统计，请更新网关后重试
            </Hint>
          ) : snapshot && summary ? (
            <>
              <div className="mx-4 grid grid-cols-2 gap-2 py-3 sm:mx-5 sm:grid-cols-4">
                <Stat label="总 Token" value={compact(summary.total)} hint={exact.format(summary.total)} />
                <Stat
                  label="输入"
                  value={compact(summary.input)}
                  hint={
                    summary.cacheHitRate != null
                      ? "缓存命中率 " + (summary.cacheHitRate * 100).toFixed(1) + "%"
                      : "无缓存读取数据"
                  }
                />
                <Stat label="输出" value={compact(summary.output)} hint={"缓存写入 " + compact(summary.cacheWrite)} />
                <Stat label="调用次数" value={exact.format(summary.records)} hint="成功请求" />
              </div>

              <div className="grid gap-4 border-t border-border/50 pb-2 pt-3 sm:grid-cols-2">
                <div className="min-w-0">
                  <div className="px-4 text-[12px] font-medium text-muted-foreground sm:px-5">
                    按模型
                    {models.length > 0 ? (
                      <span className="ml-1.5 font-normal text-muted-foreground/70">共 {models.length} 个</span>
                    ) : null}
                  </div>
                  <div className="mt-1 max-h-72 overflow-y-auto">
                    {models.length > 0 ? (
                      models.map((m: GatewayUsageGroup) => (
                        <BarRow
                          key={m.key}
                          label={m.key}
                          value={m.total}
                          max={maxModel}
                          meta={
                            exact.format(m.records) +
                            " 次调用 · 输入 " +
                            compact(m.input) +
                            " / 输出 " +
                            compact(m.output)
                          }
                        />
                      ))
                    ) : (
                      <div className="px-4 py-2 text-xs text-muted-foreground sm:px-5">该范围内暂无数据</div>
                    )}
                  </div>
                </div>
                <div className="min-w-0">
                  <div className="px-4 text-[12px] font-medium text-muted-foreground sm:px-5">
                    按账号
                    {accountRows.length > 0 ? (
                      <span className="ml-1.5 font-normal text-muted-foreground/70">共 {accountRows.length} 个</span>
                    ) : null}
                  </div>
                  <div className="mt-1 max-h-72 overflow-y-auto">
                    {accountRows.length > 0 ? (
                      accountRows.map((a: GatewayUsageGroup) => (
                        <BarRow
                          key={a.key}
                          label={nickname.get(a.key) ?? a.key.slice(0, 8) + "…"}
                          value={a.total}
                          max={maxAccount}
                          meta={exact.format(a.records) + " 次调用 · " + a.key.slice(0, 8)}
                        />
                      ))
                    ) : (
                      <div className="px-4 py-2 text-xs text-muted-foreground sm:px-5">该范围内暂无数据</div>
                    )}
                  </div>
                </div>
              </div>

              {daily.length > 0 ? (
                <div className="border-t border-border/50 px-4 pb-3 pt-3 sm:px-5">
                  <div className="text-[12px] font-medium text-muted-foreground">每日用量</div>
                  <div className="mt-2 flex h-16 items-end gap-1">
                    {daily.slice(-30).map((d) => (
                      <div
                        key={d.key}
                        className="flex h-full flex-1 items-end"
                        title={d.key + " · " + exact.format(d.total) + " tokens · " + d.records + " 次调用"}
                      >
                        <div
                          className="w-full rounded-t-[3px] bg-primary/60 transition-colors hover:bg-primary"
                          style={{ height: Math.max(4, Math.round((d.total / maxDaily) * 100)) + "%" }}
                        />
                      </div>
                    ))}
                  </div>
                  <div className="mt-1 flex justify-between text-[10px] text-muted-foreground">
                    <span>{daily[Math.max(0, daily.length - 30)]?.key}</span>
                    <span>{daily[daily.length - 1]?.key}</span>
                  </div>
                </div>
              ) : null}
            </>
          ) : (
            <Hint icon={<AlertTriangle className="size-3.5" />}>
              无法读取网关用量{data?.error ? "：" + data.error : ""}
            </Hint>
          )}
        </Card>
      </section>

      <section className="min-w-0 space-y-2.5" aria-labelledby="usage-requests-title">
        <div className="px-1">
          <h2 id="usage-requests-title" className="text-[13px] font-medium leading-5">
            请求明细
          </h2>
        </div>
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <RequestsDetailCard data={detailData} loading={detailLoading} nickname={nickname} />
        </Card>
      </section>
    </div>
  );
}