import { useEffect, useState } from "react";
import { AlertTriangle, Loader2, RefreshCw } from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import * as api from "@/lib/api";
import { displayText } from "@/lib/utils";
import type {
  AccountMeta,
  LocalAccountFreshness,
  LocalImportCandidate,
  LocalScanResult,
} from "@/lib/types";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 导入完成后回调（参数为本次结果计数）。 */
  onImported?: (result: { imported: number; added: number; updated: number }) => void;
}

/** 候选账号展示名（与账号卡片一致）。 */
function candidateLabel(meta: AccountMeta): string {
  return displayText(meta.nickname) ?? displayText(meta.email) ?? displayText(meta.uid) ?? meta.id;
}

/** 只展示到分钟，避免把毫秒时间戳直接抛给用户。 */
function formatWhen(ms: number): string {
  if (!ms) return "";
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** 凭证可用性对应的徽标样式：过期用警示色，其余保持中性。 */
function freshnessVariant(freshness: LocalAccountFreshness) {
  return freshness === "expired" ? ("warning" as const) : ("outline" as const);
}

/** 来源徽标：当前登录态最值得关注，用 primary 强调。 */
function sourceVariant(source: LocalImportCandidate["source"]) {
  return source === "current" ? ("default" as const) : ("secondary" as const);
}

/**
 * 从本机导入账号弹框：扫描本机全部登录态 → 勾选 → 批量并入账号库。
 *
 * 与旧「导入本机账号」按钮的区别：除两个固定认证文件（当前登录态）外，
 * 还会扫出客户端留存的历史登录快照与本工具切换前的备份，因此能一次
 * 找回历史上登录过的多个账号。同一账号的多份文件已在后端按凭证新旧去重。
 */
export function ImportLocalDialog({ open, onOpenChange, onImported }: Props) {
  const [scan, setScan] = useState<LocalScanResult | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [scanning, setScanning] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  /** 拉取扫描结果；默认只勾选「值得导入」的候选。 */
  async function refresh() {
    setScanning(true);
    setError("");
    try {
      const res = await api.scanLocalAccounts();
      setScan(res);
      // 默认勾选：未入库且凭证未完全过期的候选（已入库的不必重复导入）。
      setSelected(
        new Set(
          res.candidates
            .filter((c) => c.freshness !== "expired" && !c.alreadyImported)
            .map((c) => c.path),
        ),
      );
    } catch (e) {
      setScan(null);
      setSelected(new Set());
      setError(api.asError(e));
    } finally {
      setScanning(false);
    }
  }

  useEffect(() => {
    if (open) {
      setScan(null);
      setSelected(new Set());
      setBusy(false);
      setError("");
      void refresh();
    }
    // refresh 依赖 api 与 state setter，均为稳定引用，无需列入依赖。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const candidates = scan?.candidates ?? [];
  const allSelected = candidates.length > 0 && selected.size === candidates.length;

  function toggleAll() {
    setSelected(allSelected ? new Set() : new Set(candidates.map((c) => c.path)));
  }

  function toggle(path: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }

  async function doImport() {
    if (busy || scanning || selected.size === 0) return;
    setBusy(true);
    setError("");
    try {
      const res = await api.importLocalSelected([...selected]);
      onImported?.({ imported: res.imported, added: res.added, updated: res.updated });
      onOpenChange(false);
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="min-w-0 max-w-2xl overflow-x-hidden">
        <DialogHeader>
          <DialogTitle>从本机导入账号</DialogTitle>
          <DialogDescription>
            扫描本机当前登录态、客户端历史快照与切换备份，勾选要导入的账号。
          </DialogDescription>
        </DialogHeader>

        {scanning && (
          <div className="flex items-center gap-2 py-6 text-sm text-muted-foreground">
            <Loader2 className="animate-spin" /> 正在扫描本机登录态…
          </div>
        )}

        {!scanning && scan && (
          <>
            <div className="rounded-md border bg-muted/30 px-3 py-2 text-xs leading-5 text-muted-foreground">
              扫描到 {scan.filesScanned} 份认证文件，去重后得到 {scan.total} 个账号
              {scan.usable < scan.total && <>（其中 {scan.usable} 个凭证仍可用）</>}。
              <span className="mt-1 block break-all font-mono text-[11px] opacity-80">
                {scan.authDir}
              </span>
              <span className="block break-all font-mono text-[11px] opacity-80">
                {scan.backupDir}
              </span>
            </div>

            {candidates.length === 0 ? (
              <p className="py-4 text-center text-sm text-muted-foreground">
                未在本机发现任何 WorkBuddy 登录记录。请先在客户端登录，或改用 OAuth 登录添加。
              </p>
            ) : (
              <>
                <div className="flex items-center justify-between text-sm">
                  <span className="text-muted-foreground">
                    共 {candidates.length} 个账号，已选 {selected.size} 个
                  </span>
                  <div className="flex items-center gap-3">
                    <button
                      type="button"
                      className="text-primary hover:underline"
                      onClick={toggleAll}
                    >
                      {allSelected ? "取消全选" : "全选"}
                    </button>
                    <button
                      type="button"
                      className="flex items-center gap-1 text-muted-foreground hover:underline"
                      onClick={() => void refresh()}
                    >
                      <RefreshCw className="size-3" />
                      重新扫描
                    </button>
                  </div>
                </div>

                <div className="max-h-72 space-y-1 overflow-y-auto pr-1">
                  {candidates.map((c) => (
                    <label
                      key={c.path}
                      className="flex cursor-pointer items-start gap-3 rounded-md border px-3 py-2 hover:bg-accent/50"
                    >
                      <input
                        type="checkbox"
                        className="mt-1 size-4 accent-primary"
                        checked={selected.has(c.path)}
                        onChange={() => toggle(c.path)}
                      />
                      <span className="min-w-0 flex-1 space-y-1">
                        <span className="flex flex-wrap items-center gap-1.5">
                          <span className="truncate text-sm font-medium">
                            {candidateLabel(c.meta)}
                          </span>
                          <Badge variant="secondary" className="shrink-0">
                            {c.meta.region ?? "未知区域"}
                          </Badge>
                          <Badge variant={sourceVariant(c.source)} className="shrink-0">
                            {c.sourceLabel}
                          </Badge>
                          <Badge variant={freshnessVariant(c.freshness)} className="shrink-0">
                            {c.freshnessLabel}
                          </Badge>
                          {c.alreadyImported && (
                            <Badge variant="outline" className="shrink-0">
                              {c.updatesStored ? "将更新" : "已在账号库"}
                            </Badge>
                          )}
                        </span>
                        <span className="flex flex-wrap items-center gap-x-2 text-xs text-muted-foreground">
                          <span>{formatWhen(c.modifiedAt)}</span>
                          {c.duplicateCount > 1 && (
                            <span>另有 {c.duplicateCount - 1} 份旧快照（已按凭证新旧取此份）</span>
                          )}
                        </span>
                      </span>
                    </label>
                  ))}
                </div>
              </>
            )}
          </>
        )}

        {!scanning && scan && scan.usable === 0 && candidates.length > 0 && (
          <Alert variant="warning">
            <AlertTriangle />
            <AlertTitle>凭证均已过期</AlertTitle>
            <AlertDescription>
              本机发现的账号 refresh token 都已过期，导入后无法保活，需在 WorkBuddy
              客户端重新登录再导入。
            </AlertDescription>
          </Alert>
        )}

        {error && (
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
            取消
          </Button>
          <Button onClick={doImport} disabled={busy || scanning || selected.size === 0}>
            {busy ? <Loader2 className="animate-spin" /> : null}
            {busy ? "导入中…" : `导入勾选账号（${selected.size}）`}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
