import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { invoke } from "@tauri-apps/api/core";
import { domToPng } from "modern-screenshot";
import {
  Dashboard, PeriodReport, ModelStat, ToolStat, UsageLimit, Theme, TH,
  fetchDashboard, fmtInt, fmtTokens, pct,
} from "./data";
import {
  TokenGlyph, Segmented, BarChart, Sparkline, CostDonut, BarList, Heatmap,
} from "./charts";

// The four windows the panel can show. "5H" is the rolling block subscription
// plans meter (report_five_hour); the rest are calendar windows.
type Period = "5H" | "Day" | "Week" | "Month";
const PERIODS: readonly Period[] = ["5H", "Day", "Week", "Month"];

// Count up to `target`. Restarts from 0 whenever `resetKey` changes (popover
// open / period switch); on a live value change it eases from the current
// value to the new one instead of snapping back to 0.
function useCountUp(target: number, resetKey: string, active: boolean, duration = 850): number {
  const [val, setVal] = useState(0);
  const valRef = useRef(0);
  const keyRef = useRef<string | null>(null);
  const rafRef = useRef(0);
  // useLayoutEffect so the reset-to-0 is committed *before* the browser paints
  // (otherwise the old/final value flashes for a frame before counting up).
  useLayoutEffect(() => {
    cancelAnimationFrame(rafRef.current);
    const set = (v: number) => { valRef.current = v; setVal(v); };
    // while the popover is hidden, hold at 0 so the next open starts clean
    if (!active) { keyRef.current = null; set(0); return; }
    const reset = keyRef.current !== resetKey;
    keyRef.current = resetKey;
    // open / period switch → start from 0 (paint it now); live update → ease
    // from the current value to the new one.
    let from = valRef.current;
    if (reset) { from = 0; set(0); }
    const start = performance.now();
    const ease = (t: number) => 1 - Math.pow(1 - t, 3); // easeOutCubic
    const tick = (now: number) => {
      const p = Math.min(1, (now - start) / duration);
      set(from + (target - from) * ease(p));
      if (p < 1) rafRef.current = requestAnimationFrame(tick);
    };
    rafRef.current = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(rafRef.current);
  }, [resetKey, target, active, duration]);
  return val;
}

function Delta({ v, theme }: { v: number; theme: Theme }) {
  const up = v >= 0;
  // Usage/cost going up is "bad" → red; going down is "good" → green.
  const col = up ? "#e0795f" : theme.accent;
  return (
    <span style={{ font: `600 10px ${theme.mono}`, color: col, display: "inline-flex", alignItems: "center", gap: 2,
      padding: "1.5px 5px", borderRadius: 5, background: up ? "rgba(224,121,95,0.16)" : "rgba(39,176,110,0.14)" }}>
      {up ? "▲" : "▼"}{Math.abs(Math.round(v))}%
    </span>
  );
}

// Round each value's share to 1 decimal (%) via largest-remainder apportionment,
// so the displayed percentages sum to exactly 100.0% (plain rounding wouldn't).
function sharePcts(values: number[]): number[] {
  const total = values.reduce((s, v) => s + v, 0);
  if (total <= 0) return values.map(() => 0);
  const UNITS = 1000; // work in 0.1% units; target is 100.0%
  const raw = values.map((v) => (v / total) * UNITS);
  const units = raw.map(Math.floor);
  const left = Math.round(UNITS - units.reduce((s, f) => s + f, 0));
  raw
    .map((r, i) => ({ i, frac: r - Math.floor(r) }))
    .sort((a, b) => b.frac - a.frac)
    .slice(0, left)
    .forEach(({ i }) => (units[i] += 1));
  return units.map((u) => u / 10);
}

function ModelRow({ m, max, theme, share }: { m: ModelStat; max: number; theme: Theme; share: number }) {
  // 1-decimal share; whole numbers drop the ".0" (100% not 100.0%).
  const pctStr = share % 1 === 0 ? share.toFixed(0) : share.toFixed(1);
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 9, padding: "5px 0" }}>
      <span style={{ width: 7, height: 7, borderRadius: 2, background: m.color, flex: "0 0 auto" }} />
      <div style={{ minWidth: 0, flex: "0 0 118px" }}>
        <div style={{ font: `500 11.5px ${theme.ui}`, color: theme.text, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{m.name}</div>
      </div>
      <div style={{ flex: 1, height: 5, borderRadius: 3, background: theme.gridLine, overflow: "hidden" }}>
        <div style={{ width: `${(m.tokens / max) * 100}%`, height: "100%", background: m.color, borderRadius: 3 }} />
      </div>
      <span style={{ font: `500 10.5px ${theme.mono}`, color: theme.dim, flex: "0 0 auto", width: 42, textAlign: "right" }}>{fmtTokens(m.tokens)}</span>
      <span style={{ font: `600 10.5px ${theme.mono}`, color: theme.text, flex: "0 0 auto", width: 40, textAlign: "right" }}>{pctStr}%</span>
    </div>
  );
}

// One source CLI's share of the period (Claude Code / Codex / opencode /
// Oh My Pi). Same row geometry as ModelRow so the two lists line up.
function ToolRow({ s, max, theme, share }: { s: ToolStat; max: number; theme: Theme; share: number }) {
  const pctStr = share % 1 === 0 ? share.toFixed(0) : share.toFixed(1);
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 9, padding: "5px 0" }}>
      <div style={{ minWidth: 0, flex: "0 0 118px" }}>
        <div style={{ font: `500 11.5px ${theme.ui}`, color: theme.text, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>{s.label}</div>
        <div style={{ font: `500 9px ${theme.mono}`, color: theme.faint, marginTop: 1, whiteSpace: "nowrap" }}>
          {fmtInt(s.requests)} req · {fmtInt(s.sessions)} sess
        </div>
      </div>
      <div style={{ flex: 1, height: 5, borderRadius: 3, background: theme.gridLine, overflow: "hidden" }}>
        <div style={{ width: `${(s.tokens / max) * 100}%`, height: "100%", background: theme.accentSoft, borderRadius: 3 }} />
      </div>
      <span style={{ font: `500 10.5px ${theme.mono}`, color: theme.dim, flex: "0 0 auto", width: 42, textAlign: "right" }}>{fmtTokens(s.tokens)}</span>
      <span style={{ font: `600 10.5px ${theme.mono}`, color: theme.text, flex: "0 0 auto", width: 40, textAlign: "right" }}>{pctStr}%</span>
    </div>
  );
}

// Time until a window resets, e.g. "1h 12m" / "4d 3h". Empty when unknown.
function fmtReset(ms: number): string {
  if (!ms) return "";
  const left = ms - Date.now();
  if (left <= 0) return "resetting";
  const mins = Math.floor(left / 60000);
  const hours = Math.floor(mins / 60);
  const days = Math.floor(hours / 24);
  if (days >= 1) return `${days}d ${hours % 24}h`;
  if (hours >= 1) return `${hours}h ${mins % 60}m`;
  return `${mins}m`;
}

// How old an observation may be before its row stops presenting it as current.
// The provider poll runs every 15 minutes, so this is three missed polls: past
// that the number is a last-known value, not a reading. A window whose reset
// time has already passed is stale by definition — the provider has rolled it
// and we never heard the new fill level. That is exactly what happened when the
// Claude OAuth token expired: the rows kept showing "67% · resets 20:30" for
// hours after that window had already reset.
const LIMIT_STALE_MS = 45 * 60 * 1000;

// One provider-reported plan window. These are current state rather than
// period-scoped, so the row shows in every period tab and is titled with where
// the number came from (live API call vs Oh My Pi's last snapshot).
function PlanLimitRow({ l, theme }: { l: UsageLimit; theme: Theme }) {
  const name = l.provider === "opencode-go" ? "opencode go" : l.provider === "anthropic" ? "Claude" : l.provider;
  // Label minus its provider prefix and "limit" suffix — that is what tells
  // apart two windows of the same length ("7 Day" vs "7 Day (Fable)").
  const title = l.label.replace(/ limit$/i, "").replace(/^(claude|anthropic|opencode go)\s+/i, "") || l.window;
  const now = Date.now();
  const rolled = !!l.resetsAt && l.resetsAt <= now;
  const stale = rolled || (!!l.observedAt && now - l.observedAt > LIMIT_STALE_MS);
  const hot = l.status === "exhausted" || l.status === "rate-limited";
  const warm = hot || l.status === "warning" || l.used >= 80;
  // A stale row keeps its last value but loses the status colour: it must not
  // look like a live reading of the current window.
  const col = stale ? theme.faint : hot ? "#e0795f" : warm ? "#e0a75f" : theme.accent;
  const pctStr = l.used % 1 === 0 ? l.used.toFixed(0) : l.used.toFixed(1);
  const reset = fmtReset(l.resetsAt);
  const seen = l.observedAt
    ? new Date(l.observedAt).toLocaleTimeString("en-US", { hour: "2-digit", minute: "2-digit" })
    : "";
  const sub = stale
    ? `${rolled ? "window reset" : "stale"}${seen ? ` · read ${seen}` : ""}`
    : reset
      ? `resets in ${reset}`
      : l.status;
  return (
    <div
      title={`${l.source === "api" ? "live" : "Oh My Pi snapshot"}${seen ? ` · as of ${seen}` : ""}${stale ? " · not current" : ""}`}
      style={{ display: "flex", alignItems: "center", gap: 9, padding: "5px 0" }}
    >
      <div style={{ minWidth: 0, flex: "0 0 118px" }}>
        <div style={{ font: `500 11.5px ${theme.ui}`, color: stale ? theme.dim : theme.text, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
          {name} · {title}
        </div>
        <div style={{ font: `500 9px ${theme.mono}`, color: theme.faint, marginTop: 1, whiteSpace: "nowrap" }}>
          {sub}
        </div>
      </div>
      <div style={{ flex: 1, height: 5, borderRadius: 3, background: theme.gridLine, overflow: "hidden" }}>
        <div style={{ width: `${Math.min(100, Math.max(0, l.used))}%`, height: "100%", background: col, borderRadius: 3 }} />
      </div>
      <span style={{ font: `600 10.5px ${theme.mono}`, color: col, flex: "0 0 auto", width: 40, textAlign: "right" }}>{pctStr}%</span>
    </div>
  );
}

function MiniStat({ label, value, sub, theme, accent, children }:
  { label: string; value: string; sub?: string; theme: Theme; accent?: string; children?: React.ReactNode }) {
  return (
    <div style={{ background: theme.gridLine, borderRadius: 9, padding: "9px 10px", minWidth: 0 }}>
      <div style={{ font: `500 9.5px ${theme.ui}`, color: theme.dim, letterSpacing: ".04em", textTransform: "uppercase" }}>{label}</div>
      <div style={{ display: "flex", alignItems: "flex-end", justifyContent: "space-between", marginTop: 3, gap: 6 }}>
        <span style={{ font: `600 17px/1 ${theme.mono}`, color: accent || theme.text }}>{value}</span>
        {children}
      </div>
      {sub && <div style={{ font: `500 9px ${theme.mono}`, color: theme.faint, marginTop: 3 }}>{sub}</div>}
    </div>
  );
}

// Cached/Rest legend: full words by default, abbreviated when the row would
// otherwise overflow. Mirrors the split bar above (dark = cached, light = rest).
function SplitLegend({ t, cacheM, restM, cachedPct }:
  { t: Theme; cacheM: number; restM: number; cachedPct: number }) {
  const ref = useRef<HTMLDivElement>(null);
  const [compact, setCompact] = useState(false);
  const key = `${cacheM}|${restM}|${cachedPct}`;
  // reset to full labels whenever the numbers change, then re-measure
  useLayoutEffect(() => { setCompact(false); }, [key]);
  useLayoutEffect(() => {
    const el = ref.current;
    if (el && !compact && el.scrollWidth > el.clientWidth + 1) setCompact(true);
  });
  return (
    <div ref={ref} style={{
      display: "flex", alignItems: "center", gap: 14,
      font: `500 10px ${t.mono}`, color: t.dim, marginBottom: 14, whiteSpace: "nowrap", overflow: "hidden",
    }}>
      <span><span style={{ color: t.accent }}>●</span> {compact ? "Cache" : "Cached"} {fmtTokens(cacheM)}</span>
      <span><span style={{ color: t.accentSoft }}>●</span> New {fmtTokens(restM)}</span>
      <span style={{ color: t.faint }}>{cachedPct}% cached</span>
    </div>
  );
}

const SectionRule = ({ t, m = "12px 0 10px" }: { t: Theme; m?: string }) => (
  <div style={{ height: 1, background: t.gridLine, margin: m }} />
);
const Label = ({ t, children }: { t: Theme; children: React.ReactNode }) => (
  <span style={{ font: `600 10px ${t.ui}`, color: t.dim, letterSpacing: ".05em", textTransform: "uppercase", whiteSpace: "nowrap" }}>{children}</span>
);

function ThemeToggle({ pref, theme, onCycle }: { pref: "dark" | "light" | "system"; theme: Theme; onCycle: () => void }) {
  const t = theme;
  // Single button cycling Dark → Light → System; the icon shows the current mode.
  const label = pref === "system" ? "System" : pref === "dark" ? "Dark" : "Light";
  return (
    <button onClick={onCycle} title={`Theme: ${label} (click to change)`} aria-label={`theme: ${label}`} style={{
      display: "inline-flex", alignItems: "center", justifyContent: "center",
      width: 26, height: 26, borderRadius: 7, cursor: "pointer", padding: 0,
      background: t.segBg, border: `1px solid ${t.segBorder}`, color: t.dim,
    }}>
      {pref === "light" ? (
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke={t.dim} strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
          <circle cx="12" cy="12" r="4.2" />
          <path d="M12 2.5v2.2M12 19.3v2.2M2.5 12h2.2M19.3 12h2.2M5.1 5.1l1.6 1.6M17.3 17.3l1.6 1.6M18.9 5.1l-1.6 1.6M6.7 17.3l-1.6 1.6" />
        </svg>
      ) : pref === "dark" ? (
        <svg width="14" height="14" viewBox="0 0 24 24" fill={t.dim} stroke="none">
          <path d="M21 12.9A9 9 0 1 1 11.1 3a7.2 7.2 0 0 0 9.9 9.9z" />
        </svg>
      ) : (
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke={t.dim} strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
          <rect x="3" y="4.5" width="18" height="12.5" rx="1.6" />
          <path d="M8.5 20.5h7M12 17v3.5" />
        </svg>
      )}
    </button>
  );
}

function ScreenshotButton({ theme, busy, onClick }: { theme: Theme; busy: boolean; onClick: () => void }) {
  const t = theme;
  return (
    <button onClick={onClick} disabled={busy} title="Save screenshot to Desktop" aria-label="save screenshot" style={{
      display: "inline-flex", alignItems: "center", justifyContent: "center",
      width: 26, height: 26, borderRadius: 7, cursor: busy ? "default" : "pointer", padding: 0,
      background: t.segBg, border: `1px solid ${t.segBorder}`, color: t.dim,
    }}>
      {busy ? (
        <svg className="om-spin" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke={t.dim} strokeWidth="2.6" strokeLinecap="round">
          <path d="M12 3a9 9 0 1 0 9 9" />
        </svg>
      ) : (
        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke={t.dim} strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round">
          <path d="M3 8.5A2.5 2.5 0 0 1 5.5 6h1.7l1.1-1.6A1.5 1.5 0 0 1 9.5 4h5a1.5 1.5 0 0 1 1.2.4L16.8 6h1.7A2.5 2.5 0 0 1 21 8.5v8A2.5 2.5 0 0 1 18.5 19h-13A2.5 2.5 0 0 1 3 16.5z" />
          <circle cx="12" cy="12.2" r="3.4" />
        </svg>
      )}
    </button>
  );
}

function Panel({ dash, dark, themePref, onToggleTheme, openGen, active }: { dash: Dashboard; dark: boolean; themePref: "dark" | "light" | "system"; onToggleTheme: () => void; openGen: number; active: boolean }) {
  const t = TH[dark ? "dark" : "light"];
  // "popover" (menu-bar panel, default) or "window" (a normal decorated app
  // window: opaque, square corners, no custom drag region — the native title bar
  // does the moving there). Read once; the tray toggle restarts the app.
  const [windowMode, setWindowMode] = useState<"popover" | "window">("popover");
  useEffect(() => {
    if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window)) return;
    invoke<string>("get_window_mode")
      .then((m) => setWindowMode(m === "window" ? "window" : "popover"))
      .catch(() => {});
  }, []);
  const normalWindow = windowMode === "window";
  // The popover window is transparent and the rounded card paints its own
  // background; a normal window is opaque edge to edge, so nothing of the
  // desktop shows through its square corners or behind short content.
  useEffect(() => {
    document.body.style.background = normalWindow ? t.card : "transparent";
  }, [normalWindow, t.card]);
  // Drag the popover by its body (Windows/Linux only, popover mode only — macOS
  // uses the menu-bar NSPanel and a normal window has a title bar). A real OS
  // window-drag begins only once the pointer moves past a small threshold, so a
  // plain click still clicks through / dismisses and never arms the
  // hide-suppression guard.
  const canDrag = !normalWindow && typeof window !== "undefined" && "__TAURI_INTERNALS__" in window && !navigator.userAgent.includes("Macintosh");
  const dragRef = useRef<{ x: number; y: number } | null>(null);
  // "5H" is the rolling window plan limits are metered in (see report_five_hour);
  // Day/Week/Month stay calendar-based.
  const [period, setPeriod] = useState<Period>("Week");
  const P: PeriodReport = period === "5H" ? dash.fiveHour : period === "Day" ? dash.day : period === "Month" ? dash.month : dash.week;
  const M = P.metrics;
  // animated Total tokens: counts up from 0 on each open / period switch;
  // held at 0 while the popover is hidden so it never flashes the final value.
  // A detached window is never "hidden" in that sense — it stays on screen while
  // unfocused — so it always counts (and eases) instead of dropping to 0.
  const animTotal = useCountUp(M.totalTokens, `${period}:${openGen}`, active || normalWindow);
  // Same unit switch as fmtTokens: >1000M reads better as "3.75B".
  const heroUnit = animTotal >= 1000 ? "B" : "M";
  const heroVal = animTotal >= 1000 ? animTotal / 1000 : animTotal;
  // Split bar = cached portion vs the rest (uncached input + output), as exact
  // width percentages. Width% (not flexGrow + flexBasis:0): in the WebKit webview
  // that combination sizes each segment to roughly its own grow factor — an
  // absolute fraction — instead of the grow-factor *ratio*, so a lopsided split
  // left the gray track showing through. Width% sums to exactly 100%. The dark
  // segment is the cache share (matching the "% cached" label); "rest" is wider
  // than output-alone would be, so a small non-cached share still reads on the
  // pill-shaped bar. Ratios are exact, never floored.
  const splitTot = M.inputTokens + M.cacheTokens + M.outputTokens;
  const cachePct = splitTot > 0 ? (M.cacheTokens / splitTot) * 100 : 0;
  const restPct = splitTot > 0 ? ((M.inputTokens + M.outputTokens) / splitTot) * 100 : 0;
  const models = P.models;
  // By coding agent. The roster comes from the month window, so a period where
  // one agent is idle still shows it (with 0) instead of vanishing — otherwise
  // a quiet week looks like the agent isn't read at all. A single-agent user
  // gets no section.
  const tools = dash.month.tools.length > 1
    ? dash.month.tools.map((m) => P.tools.find((s) => s.name === m.name) ?? { ...m, tokens: 0, cost: 0, requests: 0, sessions: 0 })
    : P.tools;
  const maxT = Math.max(...tools.map((s) => s.tokens), 1e-9);
  const toolShares = sharePcts(tools.map((s) => s.tokens));
  // Hide noise: 0% token-share rows, and $0 entries in the cost donut.
  // Show models whose share is at least 0.1% when rounded to 1 decimal; below
  // that it'd render a meaningless "0.0%" (a negligible token share). Such a
  // model can still appear under Cost if it has a non-zero cost.
  const tokenModels = models.filter(
    (m) => Math.round((m.tokens / (M.totalTokens || 1)) * 1000) / 10 >= 0.1
  );
  const costModels = models.filter((m) => m.cost > 0);
  // models that were used but have no LiteLLM pricing (cost unknown, not $0)
  const unpricedModels = models.filter((m) => !m.priced && m.tokens > 0);
  const maxM = Math.max(...tokenModels.map((m) => m.tokens), 1e-9);
  // Per-row shares that sum to exactly 100.0% (largest-remainder over visible rows).
  const tokenShares = sharePcts(tokenModels.map((m) => m.tokens));
  const trendSub = { "5H": "last 5h", Day: "today 24h", Week: "this week", Month: "this month" }[period];

  // screenshot capture: rasterize the full panel card to a PNG and hand it to
  // the Rust `save_screenshot` command (browser preview falls back to a download).
  const [shotBusy, setShotBusy] = useState(false);
  const [toast, setToast] = useState<{ msg: string; ok: boolean } | null>(null);
  const toastTimer = useRef<number | null>(null);
  const showToast = (msg: string, ok: boolean) => {
    if (toastTimer.current) window.clearTimeout(toastTimer.current);
    setToast({ msg, ok });
    toastTimer.current = window.setTimeout(() => setToast(null), 1800);
  };
  const captureScreenshot = async () => {
    if (shotBusy) return;
    const el = document.querySelector<HTMLElement>(".om-scroll");
    if (!el) { showToast("Nothing to capture", false); return; }
    setShotBusy(true);
    try {
      // explicit width/height = full scrollable content, not just the viewport;
      // filter drops the capture button itself (and its in-flight spinner) so
      // the saved image is a clean dashboard, not a shot of the button.
      const dataUrl = await domToPng(el, {
        scale: 2,
        backgroundColor: dark ? "#1f2226" : "#ffffff",
        width: el.scrollWidth,
        height: el.scrollHeight,
        filter: (n) => !(n instanceof HTMLElement && n.getAttribute("aria-label") === "save screenshot"),
      });
      const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
      if (inTauri) {
        await invoke<string>("save_screenshot", { dataUrl });
        showToast("Saved to Desktop", true);
      } else {
        const a = document.createElement("a");
        a.href = dataUrl;
        a.download = "tokenscope.png";
        document.body.appendChild(a);
        a.click();
        a.remove();
        showToast("Downloaded", true);
      }
    } catch {
      showToast("Screenshot failed", false);
    } finally {
      setShotBusy(false);
    }
  };

  return (
    <div style={{
      width: "100%", height: "100vh", overflow: "hidden", boxSizing: "border-box",
      position: "relative",
      background: normalWindow ? t.card : "transparent", padding: 0,
      fontFamily: t.ui,
    }}>
      <div className="om-scroll"
        onMouseDown={canDrag ? (e) => {
          // Record the press; the real drag only starts once the pointer moves
          // past the threshold (onMouseMove). Skip interactive controls
          // (data-no-drag) and non-left buttons so clicks still register.
          if (e.button !== 0) return;
          if ((e.target as HTMLElement).closest("[data-no-drag]")) return;
          dragRef.current = { x: e.clientX, y: e.clientY };
        } : undefined}
        onMouseMove={canDrag ? (e) => {
          const s = dragRef.current;
          if (!s) return;
          const dx = e.clientX - s.x, dy = e.clientY - s.y;
          if (dx * dx + dy * dy >= 16) { // ~4px → a drag, not a click
            dragRef.current = null;
            invoke("begin_drag").catch(() => {});
          }
        } : undefined}
        onMouseUp={canDrag ? () => { dragRef.current = null; } : undefined}
        style={{
        width: "100%", height: "100%", overflowY: "auto",
        borderRadius: normalWindow ? 0 : 12, background: t.card,
        border: normalWindow ? "none" : `1px solid ${dark ? "rgba(255,255,255,0.10)" : "rgba(0,0,0,0.08)"}`,
        padding: 0, color: t.text, cursor: canDrag ? "grab" : undefined,
      }}>
        {/* sticky header — stays put while the body scrolls */}
        <div style={{
          position: "sticky", top: 0, zIndex: 10,
          display: "flex", alignItems: "center", justifyContent: "space-between",
          padding: "15px 15px 12px",
          background: dark ? "#1f2226" : "#ffffff",
          borderBottom: `1px solid ${t.gridLine}`,
        }}>
          <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
            <TokenGlyph color={t.accent} size={16} />
            <span style={{ font: `600 13px ${t.ui}`, color: t.text, letterSpacing: ".01em" }}>Tokenscope</span>
          </div>
          <div data-no-drag="" style={{ display: "flex", alignItems: "center", gap: 8, cursor: "default" }}>
            <Segmented value={period} items={PERIODS} theme={t} onSelect={setPeriod} />
            <ThemeToggle pref={themePref} theme={t} onCycle={onToggleTheme} />
            <ScreenshotButton theme={t} busy={shotBusy} onClick={captureScreenshot} />
          </div>
        </div>
        {/* scrolling body */}
        <div style={{ padding: "14px 15px 15px" }}>
        {/* hero */}
        <div style={{ display: "flex", alignItems: "flex-end", justifyContent: "space-between", marginBottom: 10 }}>
          <div>
            <div style={{ font: `500 10px ${t.ui}`, color: t.dim, letterSpacing: ".04em", textTransform: "uppercase" }}>Total tokens</div>
            <div style={{ display: "flex", alignItems: "baseline", gap: 8, marginTop: 3 }}>
              <span style={{ font: `600 30px ${t.mono}`, color: t.text, letterSpacing: "-.01em" }}>{heroVal.toFixed(2)}<span style={{ font: `500 15px ${t.mono}`, color: t.dim, marginLeft: 2 }}>{heroUnit}</span></span>
              {Math.round(M.deltaTokens) !== 0 && <Delta v={M.deltaTokens} theme={t} />}
            </div>
          </div>
          <div style={{ textAlign: "right" }}>
            <div style={{ font: `500 10px ${t.ui}`, color: t.dim }}>Est. cost</div>
            <div style={{ font: `600 18px ${t.mono}`, color: t.accent, marginTop: 2 }}>${M.cost.toFixed(2)}</div>
          </div>
        </div>
        {/* cached vs rest (uncached input + output) — 2-colour pill. Dark segment
            is the cache share, matching the "% cached" label below. */}
        <div style={{ display: "flex", height: 7, borderRadius: 4, overflow: "hidden", marginBottom: 5, background: t.gridLine }}>
          {M.totalTokens > 0 && <>
            <div style={{ width: `${cachePct}%`, background: t.accent }} />
            <div style={{ width: `${restPct}%`, background: t.accentSoft }} />
          </>}
        </div>
        <SplitLegend t={t} cacheM={M.cacheTokens} restM={M.inputTokens + M.outputTokens} cachedPct={pct(M.cacheTokens, M.totalTokens)} />
        {/* bar chart */}
        <BarChart data={P.series} theme={t} height={84} />
        <SectionRule t={t} m="14px 0 10px" />
        {/* models */}
        <div style={{ marginBottom: 4 }}><Label t={t}>Tokens by model</Label></div>
        {tokenModels.length === 0 && <div style={{ font: `500 10.5px ${t.mono}`, color: t.faint, padding: "4px 0" }}>No usage in this period</div>}
        {tokenModels.map((m, i) => <ModelRow key={i} m={m} max={maxM} theme={t} share={tokenShares[i]} />)}
        <SectionRule t={t} m="10px 0 10px" />
        {/* by coding agent — Claude Code / Codex / opencode / Oh My Pi */}
        {tools.length > 1 && (
          <>
            <div style={{ marginBottom: 4 }}><Label t={t}>By coding agent</Label></div>
            {tools.map((s, i) => <ToolRow key={s.name} s={s} max={maxT} theme={t} share={toolShares[i]} />)}
            <SectionRule t={t} m="10px 0 10px" />
          </>
        )}
        {/* plan limits — provider-reported windows, current state (not period-scoped) */}
        {dash.limits.length > 0 && (
          <>
            <SectionRule t={t} m="10px 0 10px" />
            <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", marginBottom: 4 }}>
              <Label t={t}>Plan limits</Label>
              <span style={{ font: `500 9.5px ${t.mono}`, color: t.faint, whiteSpace: "nowrap" }}>
                {dash.limits.some((l) => l.source === "api") ? "live" : "snapshot"}
              </span>
            </div>
            {dash.limits.map((l) => (
              <PlanLimitRow key={`${l.provider}:${l.window}:${l.label}`} l={l} theme={t} />
            ))}
          </>
        )}
        {/* cost donut */}
        <div style={{ marginBottom: 8 }}><Label t={t}>Cost by model</Label></div>
        {costModels.length > 0
          ? <CostDonut models={costModels} theme={t} size={100} thickness={15} />
          : <div style={{ font: `500 10.5px ${t.mono}`, color: t.faint }}>—</div>}
        {unpricedModels.length > 0 && (
          <div style={{ marginTop: 9, font: `500 9.5px/1.5 ${t.mono}`, color: t.faint }}>
            {unpricedModels.length} model{unpricedModels.length > 1 ? "s" : ""} without pricing data (cost not counted):{" "}
            <span style={{ color: t.dim }}>{unpricedModels.map((m) => m.name).join(", ")}</span>
          </div>
        )}
        <SectionRule t={t} m="12px 0 12px" />
        {/* footer stats */}
        <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8 }}>
          <MiniStat label="Requests" value={fmtInt(M.requests)} sub={`${M.sessions} sessions`} theme={t}>
            <Sparkline values={P.reqTrend.length ? P.reqTrend : [0, 0]} theme={t} width={52} height={20} accent={t.accent} />
          </MiniStat>
          <MiniStat label="Cost trend" value={`$${M.cost.toFixed(2)}`} sub={trendSub} theme={t} accent={t.accent}>
            <Sparkline values={P.costTrend.length ? P.costTrend : [0, 0]} theme={t} width={52} height={20} accent={t.accent} />
          </MiniStat>
        </div>
        {/* MCP — shown whenever the user has installed MCP servers */}
        {M.servers > 0 && (
          <>
            <SectionRule t={t} />
            <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", marginBottom: 7 }}>
              <Label t={t}>MCP calls</Label>
              <span style={{ font: `500 10px ${t.mono}`, color: t.faint, whiteSpace: "nowrap" }}><span style={{ color: t.text, fontWeight: 600 }}>{fmtInt(M.mcpCalls)}</span> · {M.servers} servers</span>
            </div>
            {P.mcp.length > 0
              ? <BarList key={period} items={P.mcp} theme={t} accent={t.accent} />
              : <div style={{ font: `500 10px ${t.mono}`, color: t.faint, padding: "2px 0" }}>No MCP calls in this period</div>}
          </>
        )}
        {/* Skill — shown whenever the user has installed skills */}
        {M.skills > 0 && (
          <>
            <SectionRule t={t} />
            <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", marginBottom: 7 }}>
              <Label t={t}>Skill calls</Label>
              <span style={{ font: `500 10px ${t.mono}`, color: t.faint, whiteSpace: "nowrap" }}><span style={{ color: t.text, fontWeight: 600 }}>{fmtInt(M.skillCalls)}</span> · {M.skills} skills</span>
            </div>
            {P.skills.length > 0
              ? <BarList key={period} items={P.skills} theme={t} accent={t.accent} />
              : <div style={{ font: `500 10px ${t.mono}`, color: t.faint, padding: "2px 0" }}>No skill calls in this period</div>}
          </>
        )}
        {/* heatmap */}
        <SectionRule t={t} />
        <div style={{ marginBottom: 9 }}><Label t={t}>Daily activity</Label></div>
        <Heatmap days={dash.heatmap} theme={t} accent={t.accent} />
        {/* footer note */}
        <div style={{ marginTop: 12, font: `500 8.5px ${t.mono}`, color: t.faint, textAlign: "center" }}>
          Est. cost via models.dev / LiteLLM · estimate
        </div>
        </div>{/* /scrolling body */}
      </div>
      {toast && (
        <div className="om-toast" style={{
          position: "absolute", top: 58, left: "50%", transform: "translateX(-50%)",
          zIndex: 20, whiteSpace: "nowrap", pointerEvents: "none",
          font: `600 12px ${t.mono}`, color: "#fff",
          background: toast.ok ? t.accent : "#e0795f",
          padding: "7px 13px", borderRadius: 9,
          boxShadow: "0 8px 22px rgba(0,0,0,0.34)",
        }}>
          {toast.msg}
        </div>
      )}
    </div>
  );
}

export default function App() {
  const [dash, setDash] = useState<Dashboard | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [openGen, setOpenGen] = useState(0);
  const [focused, setFocused] = useState(true); // browser preview: always "focused"
  // Theme preference: explicit Dark / Light, or System (follows the OS
  // appearance live on both macOS and Windows via prefers-color-scheme). First
  // run defaults to System.
  const [themePref, setThemePref] = useState<"dark" | "light" | "system">(() => {
    const saved = typeof localStorage !== "undefined" ? localStorage.getItem("tokenscope-theme") : null;
    if (saved === "dark" || saved === "light" || saved === "system") return saved;
    return "system";
  });
  const [systemDark, setSystemDark] = useState<boolean>(
    () => typeof window !== "undefined" && !!window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches
  );
  // Follow the OS appearance live while in System mode (and keep it current for
  // an instant switch back to System).
  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return;
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = (e: MediaQueryListEvent) => setSystemDark(e.matches);
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, []);
  const dark = themePref === "system" ? systemDark : themePref === "dark";
  // Cycle Dark → Light → System on each click; persist the choice.
  const cycleTheme = () =>
    setThemePref((p) => {
      const n = p === "dark" ? "light" : p === "light" ? "system" : "dark";
      // Best-effort: storage can be unavailable (private mode), and the theme
      // still applies for this session either way.
      try { localStorage.setItem("tokenscope-theme", n); } catch { /* ignore */ }
      return n;
    });

  useEffect(() => {
    // Apply fresh data AND clear any stale error: a transient initial-load
    // failure must not pin the error page for the whole session — the next
    // successful fetch (focus refetch or the 30s background push) recovers it.
    const apply = (d: Dashboard) => {
      setDash(d);
      setErr(null);
    };
    // initial load (shows the Loading state only until the first data arrives)
    fetchDashboard().then(apply).catch((e) => setErr(String(e)));

    const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
    if (!inTauri) return;
    // Under StrictMode the effect mounts → cleans up → remounts; the async
    // listen()/onFocusChanged() promises can resolve after the first cleanup,
    // so unregister any late arrival immediately instead of leaking a duplicate.
    let dead = false;
    const unlisten: Array<() => void> = [];
    const track = (u: () => void) => {
      if (dead) u();
      else unlisten.push(u);
    };
    // live updates pushed from the background refresh thread — swaps the data in
    // place (no Loading), so values update without any flicker.
    listen<Dashboard>("dashboard-updated", (e) => apply(e.payload)).then(track);
    // System appearance pushed natively from Rust (macOS). The webview's
    // prefers-color-scheme is unreliable for our hidden, non-activating menu-bar
    // panel, so the native event is the source of truth for System mode there;
    // it fires once at startup (correcting any stale launch value) and on every
    // OS theme change. Harmlessly never fires on Windows, where matchMedia works.
    listen<boolean>("system-theme", (e) => setSystemDark(e.payload)).then(track);
    // refetch the instant the popover gains focus (i.e. is opened)
    getCurrentWindow()
      .onFocusChanged(({ payload: focused }) => {
        setFocused(focused);
        if (focused) {
          setOpenGen((g) => g + 1); // re-run the count-up on each open
          fetchDashboard().then(apply).catch(() => {});
        }
      })
      .then(track);
    return () => {
      dead = true;
      unlisten.forEach((u) => u());
    };
  }, []);

  // Suppress per-property CSS transitions across a theme flip so the panel
  // repaints in the new theme in one step instead of cross-fading each color
  // (see .ts-no-transition in main.tsx). A background light→dark switch lands
  // while the panel is hidden; rAF callbacks don't run while hidden, so the
  // class stays on until the popover is shown — the first painted frame is
  // already the new theme with no transition, then we strip it a couple of
  // frames later so live interactions (e.g. switching the period) animate as
  // before. Skipped on the very first render (no prior frame to cross-fade).
  const firstThemeRun = useRef(true);
  useEffect(() => {
    if (firstThemeRun.current) {
      firstThemeRun.current = false;
      return;
    }
    const el = document.documentElement;
    el.classList.add("ts-no-transition");
    const id = requestAnimationFrame(() =>
      requestAnimationFrame(() => el.classList.remove("ts-no-transition"))
    );
    return () => cancelAnimationFrame(id);
  }, [dark]);

  const t = TH[dark ? "dark" : "light"];
  if (err) {
    return <div style={{ padding: 20, font: `500 12px ${t.mono}`, color: "#e0795f" }}>Failed to load: {err}</div>;
  }
  if (!dash) {
    return (
      <div style={{ height: "100vh", padding: 10, boxSizing: "border-box", background: "transparent" }}>
        <div style={{ height: "100%", borderRadius: 14, background: dark ? "#1f2226" : "#ffffff",
          display: "flex", alignItems: "center", justifyContent: "center",
          font: `500 12px ${t.mono}`, color: t.dim }}>Loading…</div>
      </div>
    );
  }
  return <Panel dash={dash} dark={dark} themePref={themePref} onToggleTheme={cycleTheme} openGen={openGen} active={focused} />;
}
