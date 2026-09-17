import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { confirm, open } from "@tauri-apps/plugin-dialog";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import * as zoomMath from "./zoom";
import type { Box } from "./zoom";

// ---------------------------------------------------------------------------
// 类型：与 Rust 侧命令的返回值一一对应
// ---------------------------------------------------------------------------

interface ScanSummary {
  scanned: number;
  unchanged: number;
  inserted: number;
  updated: number;
  removed: number;
  failed: number;
  pairs: number;
  orphanRaw: number;
  orphanJpg: number;
  exifFailed: number;
  elapsedMs: number;
}

interface LibraryStats {
  files: number;
  pairs: number;
  orphanRaw: number;
  orphanJpg: number;
  exifFailed: number;
  cameras: number;
  dbPath: string;
}

interface ScanProgress {
  phase: "walking" | "parsing" | "writing" | "stats";
  done: number;
  total: number;
}

interface PairCard {
  id: number;
  pairKey: string;
  path: string;
  fileKind: string;
  pairState: "both" | "rawOnly" | "jpgOnly";
  takenAtText: string | null;
  dayKey: string | null;
  cameraModel: string | null;
  cameraSerial: string | null;
  lens: string | null;
  focalLen: number | null;
  aperture: number | null;
  shutter: string | null;
  iso: number | null;
  fileSize: number;
  decodePath: string | null;
  /** 选片结果。落库落在 pair_key 上，所以 NEF 和 JPG 永远同进同退。 */
  decision: Decision;
  /** 0–5 */
  stars: number;
  /** 画面分析。null＝还没分析过（后台分析是扫描之后才跑的） */
  sharpness: number | null;
  overexposed: number | null;
  underexposed: number | null;
}

interface CameraBody {
  serial: string;
  model: string;
  offsetSeconds: number;
  photos: number;
}

interface AnalyzeSummary {
  analyzed: number;
  failed: number;
  remaining: number;
  elapsedMs: number;
}

type Decision = "none" | "keep" | "reject";

interface PairPage {
  items: PairCard[];
  total: number;
}

/** 文件夹范围选择弹窗里用的目录节点。 */
interface DirNode {
  path: string;
  name: string;
  depth: number;
  hasChildren: boolean;
}

interface Facet {
  key: string;
  label: string;
  count: number;
}

interface LibraryFacets {
  total: number;
  pairStates: Facet[];
  cameras: Facet[];
  days: Facet[];
  daysTruncated: boolean;
  decisions: Facet[];
  stars: Facet[];
  /** 画面质量：blur / over / under。后端还没分析过时三项计数都是 0。 */
  quality: Facet[];
  /** 镜头：有几种列几种；ISO 与焦段是固定档位，计数为 0 也会出现。 */
  lenses: Facet[];
  focals: Facet[];
  isos: Facet[];
}

interface ThumbPayload {
  id: number;
  size: number;
  dataUrl: string;
  route: string;
  sourceWidth: number;
  sourceHeight: number;
  fromCache: boolean;
}

/** 缓存占用。缩略图随时能重建，索引重建要重扫，选片标记是唯一不能重建的东西。 */
interface CacheStats {
  thumbsFiles: number;
  thumbsBytes: number;
  thumbsDir: string;
  dbBytes: number;
  dbPath: string;
  photos: number;
  decisions: number;
}

interface ExportSummary {
  dest: string;
  manifest: string;
  photos: number;
  files: number;
  copied: number;
  skipped: number;
  failed: number;
  bytes: number;
  elapsedMs: number;
}

/** 连拍分组里的一张。 */
interface SimilarMember {
  id: number;
  pairKey: string;
  name: string;
  timeText: string | null;
  decision: string;
  stars: number;
}

/** 一次连拍聚类出的一组（>=2 张才成组）。 */
interface SimilarGroup {
  key: string;
  size: number;
  spanSecs: number;
  startText: string | null;
  members: SimilarMember[];
}

interface ExportProgress {
  phase: "listing" | "copying" | "manifest";
  done: number;
  total: number;
}

interface FilterState {
  pairState: string;
  cameraSerial: string | null;
  day: string | null;
  /** all / none / keep / reject / marked */
  decision: string;
  /** null＝不筛。注意和 0（只要没打星的）是两码事。 */
  stars: number | null;
  /** 画面质量：null / blur / over / under */
  quality: string | null;
  /** 镜头型号，null＝不筛 */
  lens: string | null;
  /** 焦段分档：null / wide / normal / tele / super */
  focal: string | null;
  /** ISO 分档：null / low / mid / high / veryHigh */
  iso: string | null;
  search: string;
  sort: string;
}

const NONE_KEY = "__none__";

const ROUTE_LABEL: Record<string, string> = {
  "embedded-jpeg": "内嵌预览",
  "file-jpeg": "原文件 JPEG",
  "image-decode": "位图直接解码",
  "raw-decode": "RAW 完整解码",
  placeholder: "占位图（没能解出画面）",
  cached: "缓存",
};

const PAIR_LABEL: Record<PairCard["pairState"], string> = {
  both: "NEF + JPG",
  rawOnly: "仅 NEF",
  jpgOnly: "仅 JPG",
};

/** 配对不完整时才显示的徽标文案。 */
const BROKEN_LABEL: Record<string, string> = {
  rawOnly: "缺 JPG",
  jpgOnly: "缺 NEF",
};

// ---------------------------------------------------------------------------
// DOM
// ---------------------------------------------------------------------------

const $ = <T extends HTMLElement>(sel: string) => document.querySelector(sel) as T;

const elRootChip = $<HTMLElement>("#root-chip");
const elRootPath = $<HTMLElement>("#root-path");
const elRootMeta = $<HTMLElement>("#root-meta");
const elTheme = $<HTMLButtonElement>("#btn-theme");
const elPick = $<HTMLButtonElement>("#btn-pick");
const elRescan = $<HTMLButtonElement>("#btn-rescan");
const elCache = $<HTMLButtonElement>("#btn-cache");

const elFacetsDecision = $<HTMLElement>("#facets-decision");
const elFacetsStars = $<HTMLElement>("#facets-stars");
const elFacetsPair = $<HTMLElement>("#facets-pair");
const elFacetsQuality = $<HTMLElement>("#facets-quality");
const elFacetsLens = $<HTMLElement>("#facets-lens");
const elFacetsFocal = $<HTMLElement>("#facets-focal");
const elFacetsIso = $<HTMLElement>("#facets-iso");
const elBtnTimeOffset = $<HTMLButtonElement>("#btn-time-offset");
const elOffsetModal = $<HTMLElement>("#offset-modal");
const elOffsetList = $<HTMLElement>("#offset-list");
const elOffsetEmpty = $<HTMLElement>("#offset-empty");
const elOffsetCancel = $<HTMLButtonElement>("#offset-cancel");
const elGroupQuality = $<HTMLElement>("#group-quality");
const elGroupLens = $<HTMLElement>("#group-lens");
const elGroupParams = $<HTMLElement>("#group-params");
const elGroupIso = $<HTMLElement>("#group-iso");
const elQualityNote = $<HTMLElement>("#quality-note");
const elFacetsDays = $<HTMLElement>("#facets-days");
const elFacetsCameras = $<HTMLElement>("#facets-cameras");
const elDaysNote = $<HTMLElement>("#days-note");

const elSearch = $<HTMLInputElement>("#search");const elSearchClear = $<HTMLButtonElement>("#search-clear");
const elSort = $<HTMLSelectElement>("#sort");
const elDensity = $<HTMLElement>("#density");
const elClear = $<HTMLButtonElement>("#btn-clear");
const elCount = $<HTMLElement>("#count");
const elGrid = $<HTMLElement>("#grid");
const elProgress = $<HTMLElement>("#progress");
const elProgressFill = $<HTMLElement>("#progress-fill");
const elProgressText = $<HTMLElement>("#progress-text");
const elHint = $<HTMLElement>("#hint");
const elCacheInfo = $<HTMLElement>("#cache-info");
const elBanner = $<HTMLElement>("#banner");
const elBannerText = $<HTMLElement>("#banner-text");
const elBannerClose = $<HTMLButtonElement>("#banner-close");

const elCullbar = $<HTMLElement>("#cullbar");
const elCullInfo = $<HTMLElement>("#cullbar-info");
const elBtnKeep = $<HTMLButtonElement>("#btn-keep");
const elBtnReject = $<HTMLButtonElement>("#btn-reject");
const elBtnUnmark = $<HTMLButtonElement>("#btn-unmark");
const elStars = $<HTMLElement>("#cullbar-stars");
const elBtnSelectAll = $<HTMLButtonElement>("#btn-select-all");
const elBtnInvert = $<HTMLButtonElement>("#btn-invert");
const elBtnSelectNone = $<HTMLButtonElement>("#btn-select-none");
const elBtnExport = $<HTMLButtonElement>("#btn-export");

const elLoupe = $<HTMLElement>("#loupe");
const elLoupeImg = $<HTMLImageElement>("#loupe-img");
const elLoupeName = $<HTMLElement>("#loupe-name");
const elLoupeExif = $<HTMLElement>("#loupe-exif");
const elLoupePos = $<HTMLElement>("#loupe-pos");
const elLoupeClose = $<HTMLButtonElement>("#loupe-close");
const elLoupePrev = $<HTMLButtonElement>("#loupe-prev");
const elLoupeNext = $<HTMLButtonElement>("#loupe-next");
const elLoupeKeep = $<HTMLButtonElement>("#loupe-keep");
const elLoupeReject = $<HTMLButtonElement>("#loupe-reject");
const elLoupeUnmark = $<HTMLButtonElement>("#loupe-unmark");
const elLoupeMark = $<HTMLElement>("#loupe-mark");
const elLoupeStars = $<HTMLElement>("#loupe-stars");
const elLoupeZoomLabel = $<HTMLButtonElement>("#loupe-zoom-reset");
const elLoupeZoomIn = $<HTMLButtonElement>("#loupe-zoom-in");
const elLoupeZoomOut = $<HTMLButtonElement>("#loupe-zoom-out");
const elLoupeZoomActual = $<HTMLButtonElement>("#loupe-zoom-actual");

const elExportModal = $<HTMLElement>("#export-modal");
const elExportScope = $<HTMLElement>("#export-scope");
const elExportMode = $<HTMLElement>("#export-mode");
const elExportFiles = $<HTMLElement>("#export-files");
const elExportTemplate = $<HTMLInputElement>("#export-template");
const elExportCancel = $<HTMLButtonElement>("#export-cancel");
const elExportConfirm = $<HTMLButtonElement>("#export-confirm");
const elExportReveal = $<HTMLButtonElement>("#export-reveal");
const elExportProgress = $<HTMLElement>("#export-progress");
const elExportFill = $<HTMLElement>("#export-fill");
const elExportProgressText = $<HTMLElement>("#export-progress-text");

// ── 文件夹范围选择与分组 ───────────────────────────────────────────────
const elBtnClearLib = $<HTMLButtonElement>("#btn-clear-lib");
const elBtnUndo = $<HTMLButtonElement>("#btn-undo");
const elScopeModal = $<HTMLElement>("#scope-modal");
const elScopeRoot = $<HTMLElement>("#scope-root");
const elScopeList = $<HTMLElement>("#scope-list");
const elScopeAll = $<HTMLButtonElement>("#scope-all");
const elScopeNone = $<HTMLButtonElement>("#scope-none");
const elScopeOk = $<HTMLButtonElement>("#scope-ok");
const elScopeCancel = $<HTMLButtonElement>("#scope-cancel");
const elExportResult = $<HTMLElement>("#export-result");
const elNoteKeep = $<HTMLElement>("#note-keep");
const elNoteReject = $<HTMLElement>("#note-reject");
const elNoteMarked = $<HTMLElement>("#note-marked");
const elNoteAll = $<HTMLElement>("#note-all");
const elNoteSelected = $<HTMLElement>("#note-selected");
const elChoiceSelected = $<HTMLElement>("#choice-selected");
const elNoteThumbs = $<HTMLElement>("#note-thumbs");

const elCacheModal = $<HTMLElement>("#cache-modal");
const elCacheThumbs = $<HTMLElement>("#cache-size-thumbs");
const elCacheDb = $<HTMLElement>("#cache-size-db");
const elCacheDecisions = $<HTMLElement>("#cache-size-decisions");
const elCacheDir = $<HTMLElement>("#cache-dir");
const elCacheScope = $<HTMLElement>("#cache-scope");
const elCacheCancel = $<HTMLButtonElement>("#cache-cancel");
const elCacheClear = $<HTMLButtonElement>("#cache-clear");

const elBtnSimilar = $<HTMLButtonElement>("#btn-similar");
const elSimilarModal = $<HTMLElement>("#similar-modal");
const elSimilarList = $<HTMLElement>("#similar-list");
const elSimilarGap = $<HTMLElement>("#similar-gap");
const elSimilarClose = $<HTMLButtonElement>("#similar-close");
const elSimilarEmpty = $<HTMLElement>("#similar-empty");
const elSimilarNote = $<HTMLElement>("#similar-note");

const elCompareModal = $<HTMLElement>("#compare-modal");
const elCompareGrid = $<HTMLElement>("#compare-grid");
const elCompareDone = $<HTMLButtonElement>("#compare-done");

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

const ROOT_KEY = "sp:root";
const THEME_KEY = "sp:theme";
const DEN_KEY = "sp:density";
const GROUP_KEY = "sp:group";

const PAGE_SIZE = 120;

let rootPath: string | null = null;
let scanning = false;
/** 画面分析（清晰度 / 曝光）的后台任务，同一时刻只跑一个。 */
let analyzing = false;
/** 最近一次拉到的分面数据。分析完要报「多少张糊了」，从这里读现成的，不再多问一次。 */
let lastFacets: LibraryFacets | null = null;

/** 上次扫描时勾选的文件夹范围；重新扫描沿用同一范围，不必每次重选。 */
let lastScopeDirs: string[] | null = null;
/** 分组依据。左侧筛选栏的每一维都能当分组，再加上「所在文件夹」。
 *  值与 index.html 里 #group-mode 的 option value 一一对应。 */
type GroupMode =
  | "none"
  | "folder"
  | "decision"
  | "stars"
  | "pairState"
  | "day"
  | "camera"
  | "quality"
  | "lens"
  | "focal"
  | "iso";

/** 从 localStorage 读分组方式，兼容旧版只存 0/1 的开关值。 */
function loadGroupMode(): GroupMode {
  const v = localStorage.getItem(GROUP_KEY);
  if (v === "1") return "folder";
  if (v === "0" || v === null) return "none";
  if (
    ["folder", "decision", "stars", "pairState", "day", "camera", "quality", "lens", "focal", "iso"].includes(v)
  ) {
    return v as GroupMode;
  }
  return "none";
}

let groupMode: GroupMode = loadGroupMode();
/** 分组模式下，组 key → 该组 DOM 与计数。 */
let groupMap: Map<string, { el: HTMLElement; body: HTMLElement; count: number }> | null = null;

let items: PairCard[] = [];
let total = 0;
let noMore = false;
let loadingPage = false;

// ---- 撤销栈 ----
//
// 记的是「改动之前是什么样」，不是「做了什么操作」——回退就是把旧值写回去，
// 这样批量标记、连拍一次性淘汰这种改了一堆的也能一步退回来。
// decision / stars 为 null 表示这一项当时没动过，撤销时也别碰它。
interface UndoItem {
  id: number;
  decision: Decision | null;
  stars: number | null;
}
interface UndoEntry {
  label: string;
  before: UndoItem[];
}
/** 最多记这么多步。再早的操作用户也不会想退回去，留着只是占内存。 */
const UNDO_LIMIT = 50;
const undoStack: UndoEntry[] = [];

/** 每次筛选条件变化就自增，用来丢弃过期请求的结果（防止旧页码插到新列表里）。 */
let renderToken = 0;

const filter: FilterState = {
  pairState: "all",
  cameraSerial: null,
  day: null,
  decision: "all",
  stars: null,
  quality: null,
  lens: null,
  focal: null,
  iso: null,
  search: "",
  sort: "takenDesc",
};

/** 月份展开/折叠是用户明确选择过的，跨重新渲染要保住。 */
const monthChoice = new Map<string, boolean>();

/**
 * 选中的照片（photos.id）。用 Set 是因为「选中」的判定在每个键盘事件里
 * 都要发生，数组的 includes 在几千张时会开始拖手感。
 *
 * 选中和标记是两件事，刻意分开：选中说的是「我接下来要动的范围」，
 * 标记说的是「我对这张照片的决定」。混成一个字段就没法表达
 * 「把这一批全部标成保留」——那时候你既想保留它们，又想让它们继续被选中。
 */
const selection = new Set<number>();

/** 按 id 找卡片数据。标记时要就地改数据再重画，不能重新发一次查询。 */
const itemById = new Map<number, PairCard>();

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

const baseName = (p: string) => p.split(/[\\/]/).pop() ?? p;

const stem = (p: string) => {
  const b = baseName(p);
  const i = b.lastIndexOf(".");
  return i > 0 ? b.slice(0, i) : b;
};

const pad2 = (n: number) => String(n).padStart(2, "0");

function fmtExposure(c: PairCard): string {
  const parts: string[] = [];
  if (c.focalLen) parts.push(`${Math.round(c.focalLen)}mm`);
  if (c.aperture) parts.push(`f/${c.aperture.toFixed(1)}`);
  if (c.shutter) parts.push(c.shutter);
  if (c.iso) parts.push(`ISO ${c.iso}`);
  return parts.join(" · ");
}

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(0)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function setHint(text: string, tone: "ok" | "warn" | "error" = "ok") {
  elHint.textContent = text;
  elHint.classList.toggle("is-warn", tone === "warn");
  elHint.classList.toggle("is-error", tone === "error");
}

const WEEKDAYS = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];

/** `2026-09-16` → `{ text: "09-16", weekday: "周三" }`（跨年时才带年份）。 */
function dayParts(key: string): { text: string; weekday: string } {
  const [y, m, d] = key.split("-").map(Number);
  if (!y || !m || !d) return { text: key, weekday: "" };
  const weekday = WEEKDAYS[new Date(Date.UTC(y, m - 1, d)).getUTCDay()] ?? "";
  const thisYear = new Date().getFullYear();
  const text = y === thisYear ? `${pad2(m)}-${pad2(d)}` : `${y}-${pad2(m)}-${pad2(d)}`;
  return { text, weekday };
}

function monthLabel(ym: string): string {
  const [y, m] = ym.split("-").map(Number);
  if (!y || !m) return "无日期";
  const thisYear = new Date().getFullYear();
  return y === thisYear ? `${m} 月` : `${y} 年 ${m} 月`;
}

// ---------------------------------------------------------------------------
// 主题
// ---------------------------------------------------------------------------

function applyTheme(theme: string) {
  document.documentElement.dataset.theme = theme === "light" ? "light" : "dark";
}

function savedTheme(): string {
  const stored = localStorage.getItem(THEME_KEY);
  if (stored === "light" || stored === "dark") return stored;
  // 摄影工具默认深色：照片在黑底上更准，眼睛也更省力
  return "dark";
}

elTheme.addEventListener("click", () => {
  const next = document.documentElement.dataset.theme === "light" ? "dark" : "light";
  applyTheme(next);
  localStorage.setItem(THEME_KEY, next);
});

elBannerClose.addEventListener("click", () => {
  elBanner.hidden = true;
});

/** 启动异常（比如数据库打不开）时，用一条醒目的横幅说清楚，而不是静默失败。 */
async function showStartupError() {
  try {
    const msg = await invoke<string | null>("startup_status");
    if (!msg) return;
    elBannerText.textContent = msg;
    elBanner.hidden = false;
    setHint("数据库不可用，本次运行不会保存任何索引。", "error");
  } catch {
    /* 拿不到就算了，不影响主流程 */
  }
}

// ---------------------------------------------------------------------------
// 缩略图：4 路并发 + LRU
//
// 两个约束决定了这里的写法：
// 1. 一次几千张，绝不能一上来就全要 —— 只有进入视口的卡片才发请求。
// 2. 每张缩略图都要把原图解码一次（NEF 的预览往往是几千万像素），
//    并发太高内存会炸，所以排一个 4 路的队。
// ---------------------------------------------------------------------------

const MAX_CONCURRENT = 4;
/** 网格缩略图的缓存上限。512px 的 JPEG 约 30KB，900 张 ≈ 27MB。 */
const GRID_CACHE_MAX = 900;
/** 大图缓存单独一份、上限很低：1600px 一张就 170KB 上下，不能按网格的规模留。 */
const LOUPE_CACHE_MAX = 40;
/** 放大到 100% 用的高清档，一张几 MB，只留最近看过的两三张。 */
const PREVIEW_CACHE_MAX = 3;

const gridCache = new Map<number, ThumbPayload>();
const loupeCache = new Map<number, ThumbPayload>();
const previewCache = new Map<number, ThumbPayload>();
const pending = new Map<string, Promise<ThumbPayload>>();
let running = 0;
const queue: Array<() => void> = [];

function pump() {
  while (running < MAX_CONCURRENT && queue.length > 0) {
    const job = queue.shift()!;
    running += 1;
    job();
  }
}

/** 插入即「最新使用」，从而让 Map 的插入顺序天然成为 LRU 顺序。 */
function bump(cache: Map<number, ThumbPayload>, t: ThumbPayload) {
  cache.delete(t.id);
  cache.set(t.id, t);
}

function store(cache: Map<number, ThumbPayload>, max: number, t: ThumbPayload) {
  bump(cache, t);
  while (cache.size > max) {
    const oldest = cache.keys().next().value;
    if (oldest === undefined) break;
    cache.delete(oldest);
  }
}

/** 缩略图档位。4096 只在放大到 100% 时按需请求，常规浏览不会碰。 */
type ThumbSize = 512 | 1600 | 4096;

/** 同一个 id + 尺寸只会真正请求一次；重复请求复用同一个 Promise。 */
function loadThumb(id: number, size: ThumbSize): Promise<ThumbPayload> {
  const cache = size === 4096 ? previewCache : size === 1600 ? loupeCache : gridCache;
  const hit = cache.get(id);
  if (hit) {
    bump(cache, hit);
    return Promise.resolve(hit);
  }

  const key = `${id}:${size}`;
  const inflight = pending.get(key);
  if (inflight) return inflight;

  const p = new Promise<ThumbPayload>((resolve, reject) => {
    queue.push(() => {
      invoke<ThumbPayload>("photo_thumbnail", { id, size })
        .then((r) => {
          // 后端可能按「顺带把 micro 也建了」的口径返回更大的尺寸，按实际尺寸归档
          if (r.size >= 4096) store(previewCache, PREVIEW_CACHE_MAX, r);
          else if (r.size >= 1600) store(loupeCache, LOUPE_CACHE_MAX, r);
          else store(gridCache, GRID_CACHE_MAX, r);
          resolve(r);
        })
        .catch(reject)
        .finally(() => {
          pending.delete(key);
          running -= 1;
          pump();
        });
    });
    pump();
  });

  pending.set(key, p);
  return p;
}

// ---------------------------------------------------------------------------
// 卡片
// ---------------------------------------------------------------------------

// ---- 画面质量 ----
//
// 阈值和后端 analyze.rs 里的是一对，改一边就要改另一边，
// 否则会出现「侧栏说 15 张、筛出来 12 张」这种对不上的情况。
const BLUR_THRESHOLD = 30;
const OVEREXPOSED_THRESHOLD = 0.02;
const UNDEREXPOSED_THRESHOLD = 0.25;

/** 这张照片有没有机器能看出来的硬伤。没有就返回 null——不给人添标签。 */
function qualityIssue(c: PairCard): string | null {
  if (is(c.sharpness) && (c.sharpness as number) < BLUR_THRESHOLD) return "糊";
  if (is(c.overexposed) && (c.overexposed as number) >= OVEREXPOSED_THRESHOLD) return "过曝";
  if (is(c.underexposed) && (c.underexposed as number) >= UNDEREXPOSED_THRESHOLD) return "欠曝";
  return null;
}

/** 徽标的悬停说明：把具体数值摆出来，阈值准不准一眼能判断。 */
function qualityDetail(c: PairCard): string {
  const bits: string[] = [];
  if (is(c.sharpness)) bits.push(`清晰度 ${Math.round(c.sharpness as number)}（低于 ${BLUR_THRESHOLD} 判为糊）`);
  if (is(c.overexposed)) bits.push(`高光溢出 ${((c.overexposed as number) * 100).toFixed(1)}%`);
  if (is(c.underexposed)) bits.push(`暗部死黑 ${((c.underexposed as number) * 100).toFixed(1)}%`);
  return bits.join(" · ");
}

/** 分析值可能是 null（还没分析过），收窄一下类型让后面好写。 */
function is(v: number | null | undefined): boolean {
  return v !== null && v !== undefined;
}

function cardEl(c: PairCard): HTMLElement {
  const card = document.createElement("article");
  card.className = "card";
  card.dataset.id = String(c.id);
  card.dataset.state = "idle";
  card.tabIndex = 0;
  card.title = c.path;

  const ph = document.createElement("span");
  ph.className = "shot-placeholder";

  const img = document.createElement("img");
  img.className = "shot-img";
  img.alt = stem(c.path);
  img.decoding = "async";
  img.hidden = true;

  // 左上角的保留/淘汰圆勾。内容与显隐都由 paintCard 决定——
  // 标记是会在原地反复变化的状态，不适合在建 DOM 时一次定死。
  const mark = document.createElement("span");
  mark.className = "card-mark";

  const overlay = document.createElement("div");
  overlay.className = "card-overlay";

  const foot = document.createElement("div");
  foot.className = "card-foot";

  const name = document.createElement("div");
  name.className = "card-name";
  name.textContent = stem(c.path);
  foot.appendChild(name);

  if (c.takenAtText) {
    const time = document.createElement("span");
    time.className = "card-time";
    time.textContent = c.takenAtText.slice(11);
    foot.appendChild(time);
  }

  const stars = document.createElement("span");
  stars.className = "card-stars";
  foot.appendChild(stars);

  const sub = document.createElement("div");
  sub.className = "card-sub";
  const bits = [fmtExposure(c)].filter(Boolean);
  if (bits.length > 0) {
    const span = document.createElement("span");
    span.textContent = bits.join(" · ");
    sub.appendChild(span);
  }
  overlay.append(foot, sub);

  card.append(ph, img, mark, overlay);

  // 徽标只给「配对不完整」的照片——正常照片不该被任何标签打扰
  const broken = BROKEN_LABEL[c.pairState];
  if (broken) {
    const badge = document.createElement("span");
    badge.className = "card-badge";
    badge.textContent = broken;
    card.appendChild(badge);
  }

  // 分析出问题才标。阈值与后端 analyze.rs 保持一致，别各改各的。
  const issue = qualityIssue(c);
  if (issue) {
    const badge = document.createElement("span");
    badge.className = "card-badge card-badge--quality";
    badge.textContent = issue;
    badge.title = qualityDetail(c);
    card.appendChild(badge);
  }

  paintCard(card, c);
  return card;
}

/**
 * 把一张卡片的选片状态刷到 DOM 上。
 *
 * 标记之后只调这个，不重新渲染卡片：重新渲染会丢掉已经解码好的缩略图，
 * 每次按 P 都要等图重新出来一遍，手感就废了。
 */
function paintCard(card: HTMLElement, c: PairCard) {
  card.dataset.decision = c.decision;
  card.dataset.sel = selection.has(c.id) ? "1" : "0";

  const mark = card.querySelector<HTMLElement>(".card-mark");
  if (mark) {
    mark.textContent = c.decision === "keep" ? "✓" : c.decision === "reject" ? "✕" : "";
  }

  const stars = card.querySelector<HTMLElement>(".card-stars");
  if (stars) {
    stars.textContent = c.stars > 0 ? "★".repeat(c.stars) : "";
  }
}

async function hydrate(card: HTMLElement) {
  const id = Number(card.dataset.id);
  if (!id) return;
  if (card.dataset.state === "loading" || card.dataset.state === "ready") return;

  const img = card.querySelector<HTMLImageElement>(".shot-img");
  const ph = card.querySelector<HTMLElement>(".shot-placeholder");
  if (!img || !ph) return;

  card.dataset.state = "loading";
  try {
    const t = await loadThumb(id, 512);
    // 期间可能被滚远了（状态被改回 idle），那就不要白占内存
    if (card.dataset.state !== "loading") return;
    img.src = t.dataUrl;
    img.hidden = false;
    ph.hidden = true;
    ph.classList.remove("shot-placeholder--error");
    if (t.sourceWidth) {
      img.title = `${t.sourceWidth}×${t.sourceHeight} 像素 · 来源：${
        ROUTE_LABEL[t.route] ?? t.route
      }`;
    }
    card.dataset.state = "ready";
  } catch (e) {
    if (card.dataset.state !== "loading") return;
    card.dataset.state = "error";
    ph.hidden = false;
    ph.classList.add("shot-placeholder--error");
    ph.textContent = "预览提取失败";
    ph.title = String(e);
  }
}

/** 把已经解码好的位图还给系统（数据 URL 仍留在 JS 缓存里，回来时秒恢复）。 */
function unloadThumb(card: HTMLElement) {
  if (card.dataset.state !== "ready") return;
  const img = card.querySelector<HTMLImageElement>(".shot-img");
  const ph = card.querySelector<HTMLElement>(".shot-placeholder");
  if (!img || !ph) return;
  img.removeAttribute("src");
  img.hidden = true;
  ph.hidden = false;
  card.dataset.state = "idle";
}

// 近处：进入视口就取图（提前一屏，滚动时不会看到空白）
const nearObserver = new IntersectionObserver(
  (entries) => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      nearObserver.unobserve(entry.target);
      void hydrate(entry.target as HTMLElement);
    }
  },
  { root: elGrid, rootMargin: "800px 0px" }
);

// 远处：离得足够远的卡片释放位图。三千张 DOM 里若每张都挂着 512px 位图，
// 光解码后的像素就是好几个 GB。
const farObserver = new IntersectionObserver(
  (entries) => {
    for (const entry of entries) {
      if (entry.isIntersecting) continue;
      unloadThumb(entry.target as HTMLElement);
    }
  },
  { root: elGrid, rootMargin: "2400px 0px" }
);

// ---------------------------------------------------------------------------
// 网格渲染与分页
// ---------------------------------------------------------------------------

function observeCard(card: HTMLElement) {
  nearObserver.observe(card);
  farObserver.observe(card);
}

/**
 * 当前视图的目录范围。
 *
 * 换文件夹时旧文件夹的照片**故意留在库里**不删（标记是按 pair_key 存的，留着
 * 回头再选同一个文件夹时标记会自己回来），所以每次查询都得带上范围，否则换完
 * 文件夹会看到上一个文件夹的照片还挂在网格里。
 *
 * 勾过子目录就用勾选结果，没勾过就是整个选中的文件夹；「清空」之后没有文件夹，
 * 返回 null —— 那种情况视图本来就该是空的（见 loadMore）。
 */
function currentRoots(): string[] | null {
  if (lastScopeDirs && lastScopeDirs.length > 0) return lastScopeDirs;
  return rootPath ? [rootPath] : null;
}

function currentFilterPayload() {
  return {
    pairState: filter.pairState,
    cameraSerial: filter.cameraSerial,
    day: filter.day,
    // 「全部」在后端是不筛，用一个空值表达最清楚
    decision: filter.decision === "all" ? null : filter.decision,
    stars: filter.stars,
    quality: filter.quality,
    lens: filter.lens,
    focal: filter.focal,
    iso: filter.iso,
    search: filter.search,
    sort: filter.sort,
    roots: currentRoots(),
  };
}

/** 有没有任何一个筛选条件在生效。决定「清除筛选」按钮显不显示。 */
function isFiltered(): boolean {
  return (
    filter.pairState !== "all" ||
    filter.cameraSerial !== null ||
    filter.day !== null ||
    filter.decision !== "all" ||
    filter.stars !== null ||
    filter.quality !== null ||
    filter.lens !== null ||
    filter.focal !== null ||
    filter.iso !== null ||
    filter.search.trim() !== ""
  );
}

/**
 * 当前筛选里有没有依赖「选片状态」的条件。
 *
 * 有依赖时，标记会让照片从当前视图里消失（这正是「只看未标记」时想要的效果——
 * 一屏一屏地清空）。没有依赖时就不动它，免得照片毫无理由地跳走。
 */
function filterTracksMarks(): boolean {
  return filter.decision !== "all" || filter.stars !== null;
}

/**
 * 重新铺一遍网格。
 *
 * keepView 用于「扫描进行中的渐进刷新」：那种刷新一秒来好几次，如果每次都把滚动
 * 位置弹回顶部、把选中清掉，用户等于一边扫一边被抢走鼠标，渐进显示就没意义了。
 * 所以这个模式下先把滚动位置和选中项记下来，铺完再原样放回去。
 */
async function reload(opts?: { keepView?: boolean }) {
  renderToken += 1;
  const token = renderToken;

  const keepView = opts?.keepView === true;
  const keepScroll = keepView ? elGrid.scrollTop : 0;
  const keepSelection = keepView ? Array.from(selection) : null;

  items = [];
  total = 0;
  noMore = false;
  itemById.clear();
  selection.clear();
  nearObserver.disconnect();
  farObserver.disconnect();
  elGrid.innerHTML = "";
  elGrid.classList.toggle("is-grouped", groupMode !== "none");
  groupMap = groupMode !== "none" ? new Map() : null;
  elGrid.scrollTop = 0;
  updateCount();
  updateCullInfo();

  await loadMore(token);

  // 期间又发起了一次刷新（或页面被清掉）就别再动手了，交给后发起的那次收尾
  if (token !== renderToken) return;

  if (!keepView) return;

  if (keepSelection && keepSelection.length > 0) {
    for (const id of keepSelection) {
      if (itemById.has(id)) selection.add(id);
    }
    // 选中态是画在卡片上的，卡片重建过，得照着新的 selection 再刷一遍
    for (const card of elGrid.querySelectorAll<HTMLElement>(".card[data-id]")) {
      const id = Number(card.dataset.id);
      card.dataset.sel = selection.has(id) ? "1" : "0";
    }
  }
  elGrid.scrollTop = keepScroll;
  updateCount();
  updateCullInfo();
}

async function loadMore(token: number) {
  if (loadingPage || noMore || token !== renderToken) return;
  loadingPage = true;

  // 没选文件夹时视图就是空的：库里可能还留着上次文件夹的照片（标记要留着复用），
  // 不带范围去查会把它们全捞出来，看起来就像「清空」没生效。
  if (!rootPath) {
    total = 0;
    noMore = true;
    updateCount();
    renderEmpty();
    loadingPage = false;
    return;
  }

  try {
    const page = await invoke<PairPage>("list_pairs", {
      filter: currentFilterPayload(),
      limit: PAGE_SIZE,
      offset: items.length,
    });
    if (token !== renderToken) return;

    total = page.total;

    if (page.items.length === 0) {
      noMore = true;
    } else {
      items.push(...page.items);
      appendCards(page.items);
    }
    if (items.length >= total) noMore = true;

    updateCount();
    if (items.length === 0) renderEmpty();

    // 首屏没填满时继续取，直到出现滚动条或取完
    if (!noMore && elGrid.scrollHeight <= elGrid.clientHeight + 200) {
      loadingPage = false;
      return loadMore(token);
    }
  } catch (e) {
    setHint(`读取图库失败：${String(e)}`, "error");
    noMore = true;
  } finally {
    loadingPage = false;
  }
}

function appendCards(list: PairCard[]) {
  if (groupMode !== "none") {
    appendGrouped(list);
    return;
  }
  const frag = document.createDocumentFragment();
  const cards: HTMLElement[] = [];
  for (const c of list) {
    itemById.set(c.id, c);
    const card = cardEl(c);
    frag.appendChild(card);
    cards.push(card);
  }
  elGrid.appendChild(frag);
  // 必须先入 DOM 再 observe，否则 IntersectionObserver 拿不到位置
  for (const card of cards) observeCard(card);
}

/** 照片所在目录（父文件夹的绝对路径）。分组时一组对应一个这样的路径。 */
function dirOf(path: string): string {
  const i = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return i < 0 ? "" : path.slice(0, i);
}

/** 路径最后一段（文件名或目录名）。 */
function baseOf(path: string): string {
  const i = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return i < 0 ? path : path.slice(i + 1);
}

/** 分组标题：优先显示目录名，拿不到名字时退回到完整路径。 */
function folderLabel(path: string): string {
  const dir = dirOf(path);
  return baseOf(dir) || dir || "(根目录)";
}

/** 分组模式下，拿到或创建一个分组容器（标题 + 卡片网格）。
 *  key 是稳定标识（同组必同 key），label 是标题文字，title 是悬停提示（可省）。 */
function ensureGroup(key: string, label: string, title?: string): { el: HTMLElement; body: HTMLElement; count: number } {
  let g = groupMap!.get(key);
  if (g) return g;
  const el = document.createElement("section");
  el.className = "group";
  const head = document.createElement("div");
  head.className = "group-head";
  const caret = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  caret.setAttribute("viewBox", "0 0 16 16");
  caret.setAttribute("class", "group-caret");
  caret.innerHTML =
    '<path d="M4 6l4 4 4-4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>';
  const name = document.createElement("span");
  name.className = "group-name";
  name.textContent = label;
  name.title = title ?? label;
  const count = document.createElement("span");
  count.className = "group-count";
  count.textContent = "0";
  head.append(caret, name, count);
  const body = document.createElement("div");
  body.className = "group-grid";
  if (elGrid.dataset.density) body.dataset.density = elGrid.dataset.density;
  head.addEventListener("click", () => el.classList.toggle("is-collapsed"));
  el.append(head, body);
  elGrid.appendChild(el);
  g = { el, body, count: 0 };
  groupMap!.set(key, g);
  return g;
}

// ── 分组依据：一张照片 → 哪一组 ──────────────────────────────────────────
//
// 档位划分要和侧栏筛选（后端 bucket_facets）保持一致：
// 焦段 24/70/200、ISO 400/1600/6400，都是「下界含、上界不含」。
// 两处分叉的话，用户会遇到「按这个分了组、按同一条件却筛不全」的怪事。

const FOCAL_BUCKETS: Array<{ lo: number; hi: number; key: string; label: string }> = [
  { lo: 0, hi: 24, key: "wide", label: "24 以下" },
  { lo: 24, hi: 70, key: "normal", label: "24–70" },
  { lo: 70, hi: 200, key: "tele", label: "70–200" },
  { lo: 200, hi: Infinity, key: "super", label: "200 以上" },
];

const ISO_BUCKETS: Array<{ lo: number; hi: number; key: string; label: string }> = [
  { lo: 0, hi: 400, key: "low", label: "400 以下" },
  { lo: 400, hi: 1600, key: "mid", label: "400–1600" },
  { lo: 1600, hi: 6400, key: "high", label: "1600–6400" },
  { lo: 6400, hi: Infinity, key: "veryHigh", label: "6400 以上" },
];

function bucketOf(v: number | null, buckets: typeof FOCAL_BUCKETS): { key: string; label: string } | null {
  if (v === null || !Number.isFinite(v)) return null;
  return buckets.find((b) => v >= b.lo && v < b.hi) ?? null;
}

/** 画面质量归档：优先级 跑焦 > 过曝 > 欠曝 > 正常；还没分析过的归「未分析」。
 *  三个阈值与 src-tauri/src/analyze.rs 保持一致（改动要两边同步）。 */
function qualityOf(c: PairCard): { key: string; label: string } {
  if (c.sharpness === null && c.overexposed === null && c.underexposed === null) {
    return { key: "pending", label: "未分析" };
  }
  if (c.sharpness !== null && c.sharpness < 25) return { key: "blur", label: "疑似跑焦" };
  if (c.overexposed !== null && c.overexposed >= 0.02) return { key: "over", label: "高光溢出" };
  if (c.underexposed !== null && c.underexposed >= 0.25) return { key: "under", label: "暗部死黑" };
  return { key: "ok", label: "正常" };
}

/** 按当前 groupMode 算出一张照片的组。返回 null 表示这种模式不该分组（none）。 */
function groupKeyOf(c: PairCard, mode: GroupMode): { key: string; label: string; title?: string } | null {
  switch (mode) {
    case "none":
      return null;
    case "folder": {
      const dir = dirOf(c.path);
      return { key: dir || "(根目录)", label: folderLabel(dir), title: dir };
    }
    case "decision":
      return { key: c.decision, label: DECISION_LABEL[c.decision] };
    case "stars":
      return c.stars > 0 ? { key: String(c.stars), label: `${c.stars} 星` } : { key: "0", label: "未评分" };
    case "pairState":
      return c.pairState === "both"
        ? { key: "both", label: "NEF + JPG" }
        : c.pairState === "rawOnly"
          ? { key: "rawOnly", label: "仅 NEF" }
          : { key: "jpgOnly", label: "仅 JPG" };
    case "day":
      return c.dayKey ? { key: c.dayKey, label: c.dayKey } : { key: "?", label: "未知日期" };
    case "camera":
      return c.cameraModel ? { key: c.cameraModel, label: c.cameraModel } : { key: "?", label: "未知机身" };
    case "quality":
      return qualityOf(c);
    case "lens":
      return c.lens ? { key: c.lens, label: c.lens } : { key: "?", label: "未知镜头" };
    case "focal": {
      const b = bucketOf(c.focalLen, FOCAL_BUCKETS);
      return b ? { key: b.key, label: b.label } : { key: "?", label: "未知焦段" };
    }
    case "iso": {
      const b = bucketOf(c.iso, ISO_BUCKETS);
      return b ? { key: b.key, label: b.label } : { key: "?", label: "未知 ISO" };
    }
  }
}

function appendGrouped(list: PairCard[]) {
  for (const c of list) {
    itemById.set(c.id, c);
    const g0 = groupKeyOf(c, groupMode) ?? { key: "?", label: "未分组" };
    const g = ensureGroup(g0.key, g0.label, g0.title);
    const card = cardEl(c);
    g.body.appendChild(card);
    g.count += 1;
    (g.el.querySelector(".group-count") as HTMLElement).textContent = String(g.count);
    observeCard(card);
  }
}

function updateCount() {
  elClear.hidden = !isFiltered();
  // 图库空的时候，一整条选片操作栏只是噪声——它要操作的东西还不存在
  elCullbar.hidden = total === 0 && items.length === 0;

  // 按钮的可用状态取决于「有没有选中」「库里有东西」，两件事都在总数变化时才会变
  updateCullInfo();

  if (total === 0 && items.length === 0) {
    elCount.textContent = "";
    return;
  }
  const parts = [`${total.toLocaleString()} 张`];
  if (items.length < total) parts.push(`已加载 ${items.length.toLocaleString()}`);
  if (loadingPage && items.length > 0) parts.push("读取中…");
  elCount.textContent = parts.join(" · ");
}

function renderEmpty() {
  elGrid.innerHTML = "";

  const filtered = isFiltered();

  const box = document.createElement("div");
  box.className = "empty";

  if (total === 0 && !filtered) {
    // 图库是空的：给一个明确的下一步
    box.innerHTML = `
      <svg class="empty-icon" viewBox="0 0 48 48" aria-hidden="true">
        <path d="M4 12a3 3 0 0 1 3-3h9l3.5 4H41a3 3 0 0 1 3 3v20a3 3 0 0 1-3 3H7a3 3 0 0 1-3-3V12Z"
              fill="none" stroke="currentColor" stroke-width="2.2" stroke-linejoin="round"/>
        <circle cx="17" cy="24" r="4" fill="none" stroke="currentColor" stroke-width="2.2"/>
        <path d="M26 30l6-6 8 8" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/>
      </svg>`;
    const title = document.createElement("div");
    title.className = "empty-title";
    title.textContent = "还没有照片";
    const text = document.createElement("div");
    text.className = "empty-text";
    text.textContent =
      "选择一次拍摄的文件夹，扫描和缩略图都会自动完成，不用再点任何按钮。NEF 和 JPG 会按「一次快门」自动配对，所以卡片数就是你实际拍的张数。选完之后按 P 保留、X 淘汰，← → 翻页。";
    const btn = document.createElement("button");
    btn.className = "btn btn-primary";
    btn.textContent = "选择文件夹";
    btn.addEventListener("click", () => void pickFolder());
    box.append(title, text, btn);
  } else {
    const title = document.createElement("div");
    title.className = "empty-title";
    title.textContent = "没有符合条件的照片";
    const text = document.createElement("div");
    text.className = "empty-text";
    text.textContent = "当前筛选条件下没有结果。放宽一个条件再试试。";
    box.append(title, text);
  }

  elGrid.appendChild(box);
}

elGrid.addEventListener(
  "scroll",
  () => {
    if (elGrid.scrollHeight - elGrid.scrollTop - elGrid.clientHeight < 1400) {
      void loadMore(renderToken);
    }
  },
  { passive: true }
);

// ---------------------------------------------------------------------------
// 筛选栏
// ---------------------------------------------------------------------------

function facetButton(opts: {
  facet: string;
  key: string;
  label: string;
  count: number;
  active: boolean;
  warn?: boolean;
  extra?: HTMLElement;
}): HTMLElement {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "facet";
  b.dataset.facet = opts.facet;
  b.dataset.key = opts.key;
  if (opts.active) b.classList.add("is-active");
  b.setAttribute("aria-pressed", String(opts.active));

  if (opts.warn) {
    const dot = document.createElement("span");
    dot.className = "facet-dot";
    b.appendChild(dot);
  }

  const label = document.createElement("span");
  label.className = "facet-label";
  label.textContent = opts.label;
  label.title = opts.label;
  b.appendChild(label);

  if (opts.extra) b.appendChild(opts.extra);

  const count = document.createElement("span");
  count.className = "facet-count";
  count.textContent = opts.count.toLocaleString();
  b.appendChild(count);

  return b;
}

function renderPairFacets(f: LibraryFacets) {
  elFacetsPair.innerHTML = "";
  for (const it of f.pairStates) {
    elFacetsPair.appendChild(
      facetButton({
        facet: "pairState",
        key: it.key,
        label: it.label,
        count: it.count,
        active: filter.pairState === it.key,
        warn: it.key === "orphan" && it.count > 0,
      })
    );
  }
}

/**
 * 画面质量。三档都是「可能有问题」，所以只在这三档里有一档非零时才整组出现——
 * 一张问题都没有的时候摆一排 0 纯属噪音。
 */
function renderQualityFacets(f: LibraryFacets) {
  elFacetsQuality.innerHTML = "";
  const any = f.quality.some((it) => it.count > 0);
  elGroupQuality.hidden = !any;
  if (!any) {
    elQualityNote.textContent = "";
    return;
  }
  for (const it of f.quality) {
    elFacetsQuality.appendChild(
      facetButton({
        facet: "quality",
        key: it.key,
        label: it.label,
        count: it.count,
        active: filter.quality === it.key,
      })
    );
  }
  elQualityNote.textContent = "自动判断";
}

/**
 * 焦段 / ISO：档位是固定的，所以整组里只要有任何一档有照片就显示，
 * 计数为 0 的档也留着——位置固定，眼睛不用重新找。
 */
function renderBucketFacets(host: HTMLElement, group: HTMLElement, items: Facet[], facet: string, current: string | null) {
  host.innerHTML = "";
  const any = items.some((it) => it.count > 0);
  group.hidden = !any;
  if (!any) return;
  for (const it of items) {
    host.appendChild(
      facetButton({ facet, key: it.key, label: it.label, count: it.count, active: current === it.key })
    );
  }
}

function renderLensFacets(f: LibraryFacets) {
  elFacetsLens.innerHTML = "";
  // 只有一种镜头（或全都没读到）时这一栏没有信息量，反而占地方
  const useful = f.lenses.filter((it) => it.count > 0).length > 1;
  elGroupLens.hidden = !useful;
  if (!useful) return;
  for (const it of f.lenses) {
    elFacetsLens.appendChild(
      facetButton({
        facet: "lens",
        key: it.key,
        label: it.label,
        count: it.count,
        active: filter.lens === it.key,
      })
    );
  }
}

function renderDecisionFacets(f: LibraryFacets) {
  elFacetsDecision.innerHTML = "";
  for (const it of f.decisions) {
    elFacetsDecision.appendChild(
      facetButton({
        facet: "decision",
        key: it.key,
        label: it.label,
        count: it.count,
        active: filter.decision === it.key,
      })
    );
  }
}

function renderStarFacets(f: LibraryFacets) {
  elFacetsStars.innerHTML = "";
  for (const it of f.stars) {
    elFacetsStars.appendChild(
      facetButton({
        facet: "stars",
        key: it.key,
        label: it.label,
        count: it.count,
        active: filter.stars === Number(it.key),
      })
    );
  }
}

function renderCameraFacets(f: LibraryFacets) {
  // 两台以上机身才需要时间校正——单机身时这个按钮只是噪音
  const bodies = f.cameras.filter((it) => it.key !== "" && it.count > 0);
  elBtnTimeOffset.hidden = bodies.length < 2;
  elFacetsCameras.innerHTML = "";
  if (f.cameras.length === 0) {
    const none = document.createElement("div");
    none.className = "facet";
    none.style.cursor = "default";
    none.textContent = "—";
    elFacetsCameras.appendChild(none);
    return;
  }

  elFacetsCameras.appendChild(
    facetButton({
      facet: "camera",
      key: "",
      label: "全部机身",
      count: f.total,
      active: filter.cameraSerial === null,
    })
  );

  for (const c of f.cameras) {
    elFacetsCameras.appendChild(
      facetButton({
        facet: "camera",
        key: c.key,
        label: c.label,
        count: c.count,
        active: filter.cameraSerial === c.key,
      })
    );
  }
}

function renderDayFacets(f: LibraryFacets) {
  elFacetsDays.innerHTML = "";
  elDaysNote.textContent = f.daysTruncated ? "（仅最近若干天）" : "";

  elFacetsDays.appendChild(
    facetButton({
      facet: "day",
      key: "",
      label: "全部日期",
      count: f.total,
      active: filter.day === null,
    })
  );

  // 按月份分组：几百天的时候，没有分组会翻到崩溃
  const dated = f.days.filter((d) => d.key !== NONE_KEY);
  const undated = f.days.filter((d) => d.key === NONE_KEY);

  const groups = new Map<string, Facet[]>();
  for (const d of dated) {
    const ym = d.key.slice(0, 7);
    const arr = groups.get(ym);
    if (arr) arr.push(d);
    else groups.set(ym, [d]);
  }

  let monthIndex = 0;
  for (const [ym, list] of groups) {
    // 默认只展开最近两个月，其余折叠——侧栏一屏放不下几百天。
    // 用户手动展开过的月份记在 monthChoice 里，重新渲染时不会被默认值盖掉。
    const collapsed = monthChoice.get(ym) ?? monthIndex >= 2;
    const holdsActive = filter.day !== null && filter.day.startsWith(ym);

    const head = document.createElement("button");
    head.type = "button";
    head.className = "month";
    if (collapsed && !holdsActive) head.classList.add("is-collapsed");
    head.dataset.month = ym;

    const caret = document.createElement("svg");
    caret.setAttribute("viewBox", "0 0 12 12");
    caret.classList.add("month-caret");
    caret.innerHTML = `<path d="m4 2 4 4-4 4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>`;

    const name = document.createElement("span");
    name.textContent = monthLabel(ym);

    const sum = document.createElement("span");
    sum.className = "month-count";
    sum.textContent = list.reduce((a, b) => a + b.count, 0).toLocaleString();

    head.append(caret, name, sum);

    const wrap = document.createElement("div");
    wrap.className = "month-days";

    for (const d of list) {
      const { text, weekday } = dayParts(d.key);
      const wd = document.createElement("span");
      wd.className = "facet-weekday";
      wd.textContent = weekday;
      const btn = facetButton({
        facet: "day",
        key: d.key,
        label: text,
        count: d.count,
        active: filter.day === d.key,
        extra: wd,
      });
      btn.classList.add("facet--day");
      wrap.appendChild(btn);
    }

    elFacetsDays.append(head, wrap);
    monthIndex += 1;
  }

  for (const d of undated) {
    elFacetsDays.appendChild(
      facetButton({
        facet: "day",
        key: NONE_KEY,
        label: "无时间信息",
        count: d.count,
        active: filter.day === NONE_KEY,
        warn: true,
      })
    );
  }
}

elFacetsDays.addEventListener("click", (e) => {
  const month = (e.target as HTMLElement).closest<HTMLElement>(".month");
  if (!month?.dataset.month) return;
  // 读 DOM 上的现状而不是我们以为的状态，展开/折叠永远自洽
  const nowCollapsed = month.classList.contains("is-collapsed");
  monthChoice.set(month.dataset.month, !nowCollapsed);
  month.classList.toggle("is-collapsed", !nowCollapsed);
});

/** 侧栏摆出「图库空」的状态：各分组清掉，只留一句话。
 *  图库查出来是 0 张时用，手动「清空当前文件夹」后也直接用它——
 *  那种时候不该再拿 roots=null 去查后端（那会把历史文件夹的照片全数出来）。 */
function renderFacetsEmpty() {
  for (const host of [
    elFacetsDecision,
    elFacetsStars,
    elFacetsPair,
    elFacetsCameras,
    elFacetsDays,
    elFacetsQuality,
    elFacetsLens,
    elFacetsFocal,
    elFacetsIso,
  ]) {
    host.innerHTML = "";
  }
  elGroupQuality.hidden = true;
  elGroupLens.hidden = true;
  elGroupParams.hidden = true;
  elGroupIso.hidden = true;
  const none = document.createElement("div");
  none.className = "facet facet--none";
  none.textContent = "图库还没有照片";
  elFacetsDecision.appendChild(none);
  elDaysNote.textContent = "";
}

async function loadFacets() {
  const f = await invoke<LibraryFacets>("library_facets", { roots: currentRoots() });
  lastFacets = f;

  if (f.total === 0) {
    // 图库空的时候别摆一排 0，一句话说清就够了
    renderFacetsEmpty();
    return;
  }

  renderDecisionFacets(f);
  renderStarFacets(f);
  renderPairFacets(f);
  renderCameraFacets(f);
  renderDayFacets(f);
  renderQualityFacets(f);
  renderLensFacets(f);
  renderBucketFacets(elFacetsFocal, elGroupParams, f.focals, "focal", filter.focal);
  renderBucketFacets(elFacetsIso, elGroupIso, f.isos, "iso", filter.iso);
}

/**
 * 只刷新侧栏计数，不重新拉列表。
 *
 * 标记之后每次都要更新计数（「未标记」的数字要往下掉），但重新拉一遍
 * 整个列表会让网格闪一下、滚动位置也会丢——按 P 的时候那种顿挫就是它造成的。
 */
async function refreshFacetCounts() {
  try {
    const f = await invoke<LibraryFacets>("library_facets", { roots: currentRoots() });
    if (f.total === 0) return;
    renderDecisionFacets(f);
    renderStarFacets(f);
  } catch {
    /* 计数刷不上不影响标记本身 */
  }
}

// ---------------------------------------------------------------------------
// 筛选交互（事件委托：侧栏里的按钮是重建的，逐个绑会漏）
// ---------------------------------------------------------------------------

document.querySelector(".sidebar")?.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest<HTMLElement>(".facet");
  if (!btn?.dataset.facet || btn.dataset.key === undefined) return;
  if (btn.style.cursor === "default") return;

  const facet = btn.dataset.facet;
  const key = btn.dataset.key;

  if (facet === "pairState") {
    filter.pairState = key;
  } else if (facet === "camera") {
    // 再点一次当前项＝取消这个条件，省得每次都跑去找「全部」
    filter.cameraSerial = key === "" || key === filter.cameraSerial ? null : key;
  } else if (facet === "day") {
    filter.day = key === "" || key === filter.day ? null : key;
  } else if (facet === "decision") {
    // 和上面几维一样的规矩：再点一次当前项就回到「全部」
    filter.decision = key === "all" || key === filter.decision ? "all" : key;
  } else if (facet === "stars") {
    const n = Number(key);
    filter.stars = filter.stars === n ? null : n;
  } else if (facet === "quality") {
    filter.quality = key === filter.quality ? null : key;
  } else if (facet === "lens") {
    filter.lens = key === filter.lens ? null : key;
  } else if (facet === "focal") {
    filter.focal = key === filter.focal ? null : key;
  } else if (facet === "iso") {
    filter.iso = key === filter.iso ? null : key;
  }

  void applyFilterChange();
});

async function applyFilterChange() {
  await loadFacets();
  await reload();
}

/** 把所有筛选条件拨回「全部」，搜索框也清空。只改状态不重查——
 *  查询由调用方决定（按钮点完要 applyFilterChange，清空图库时不用查）。 */
function resetFilter() {
  filter.pairState = "all";
  filter.cameraSerial = null;
  filter.day = null;
  filter.decision = "all";
  filter.stars = null;
  filter.quality = null;
  filter.lens = null;
  filter.focal = null;
  filter.iso = null;
  filter.search = "";
  elSearch.value = "";
  elSearchClear.hidden = true;
}

elClear.addEventListener("click", () => {
  resetFilter();
  void applyFilterChange();
});

elSort.addEventListener("change", () => {
  filter.sort = elSort.value;
  void reload();
});

// 缩略图大小：粗筛时用紧凑一屏看更多，终选时用大图看细节
elDensity.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest<HTMLButtonElement>("button[data-density]");
  if (!btn?.dataset.density) return;
  const density = btn.dataset.density;
  if (density === "normal") delete elGrid.dataset.density;
  else elGrid.dataset.density = density;
  for (const b of elDensity.querySelectorAll("button")) {
    b.classList.toggle("is-active", b === btn);
  }
  // 分组模式下真正的卡片网格在 .group-grid 里，密度得同步过去
  if (groupMap) {
    for (const g of groupMap.values()) {
      if (density === "normal") delete g.body.dataset.density;
      else g.body.dataset.density = density;
    }
  }
  localStorage.setItem(DEN_KEY, density);
});

let searchTimer: number | undefined;
elSearch.addEventListener("input", () => {
  window.clearTimeout(searchTimer);
  elSearchClear.hidden = elSearch.value === "";
  searchTimer = window.setTimeout(() => {
    filter.search = elSearch.value;
    void reload();
  }, 260);
});

elSearchClear.addEventListener("click", () => {
  elSearch.value = "";
  elSearchClear.hidden = true;
  filter.search = "";
  void reload();
});

// ---------------------------------------------------------------------------
// 选片
//
// 整个应用的主循环在这里：选一张 → 按 P 或 X → 自动跳下一张 → 重复。
// 三个细节决定了它好不好用：
// 1. 标记先改画面、再落库（乐观更新）。等一次 IPC 再变色，连按几下就是「黏」。
// 2. 落库失败要把画面退回去并明说——静默失败比慢得多更伤人。
// 3. 标完自动前进。选片是几千次重复动作，每一下省掉的按键都乘以几千。
// ---------------------------------------------------------------------------

/** Shift 范围选择的锚点：上一次单击/切换的那张。 */
let selAnchor: number | null = null;

const DECISION_LABEL: Record<Decision, string> = {
  none: "未标记",
  keep: "保留",
  reject: "淘汰",
};

function cardOf(id: number): HTMLElement | null {
  return elGrid.querySelector<HTMLElement>(`.card[data-id="${id}"]`);
}

/** 把一批 id 重新画一遍。传 id 而不是卡片，是因为要画的往往正是没渲染的那些。 */
function repaint(ids: Iterable<number>) {
  for (const id of ids) {
    const c = itemById.get(id);
    const el = cardOf(id);
    if (c && el) paintCard(el, c);
  }
}

function updateCullInfo() {
  const loupeOpen = !elLoupe.hidden;

  if (loupeOpen) {
    // 大图里信息条已经被照片参数占满，别再加一行
    elCullInfo.textContent = "";
  } else if (selection.size > 0) {
    elCullInfo.textContent = `已选 ${selection.size.toLocaleString()} 张 —— P 保留 · X 淘汰 · U 清除`;
  } else {
    elCullInfo.textContent = "单击选中 · 双击看大图 · 选中后按 P / X";
  }

  const hasTarget = loupeOpen || selection.size > 0;
  elBtnKeep.disabled = !hasTarget;
  elBtnReject.disabled = !hasTarget;
  elBtnUnmark.disabled = !hasTarget;
  for (const b of elStars.querySelectorAll<HTMLButtonElement>("button")) {
    b.disabled = !hasTarget;
    // 只选了一张时，把它的星级点亮到操作栏上——想调整时照着点就行
    const n = Number(b.dataset.stars);
    const sole = selection.size === 1 ? itemById.get([...selection][0]) : undefined;
    b.classList.toggle("is-active", !!sole && n > 0 && sole.stars >= n);
  }
  elBtnSelectNone.disabled = selection.size === 0;
  elBtnInvert.disabled = total === 0;
}

function syncSelection() {
  // 只改已经渲染出来的卡片。没渲染的那些不在 DOM 里，
  // 但它们的数据还在 selection 里，等哪天被渲染出来时 paintCard 会自己补上。
  for (const card of elGrid.querySelectorAll<HTMLElement>(".card")) {
    const id = Number(card.dataset.id);
    card.dataset.sel = selection.has(id) ? "1" : "0";
  }
  updateCullInfo();
}

function selectOnly(id: number) {
  selection.clear();
  selection.add(id);
  selAnchor = id;
  syncSelection();
}

function toggleSelected(id: number) {
  if (selection.has(id)) selection.delete(id);
  else selection.add(id);
  selAnchor = id;
  syncSelection();
}

/** Shift 范围选择：从锚点到目标，按**当前列表顺序**取中间全部。 */
function selectRangeTo(id: number) {
  const to = items.findIndex((c) => c.id === id);
  if (to < 0) return;

  let from = selAnchor === null ? -1 : items.findIndex((c) => c.id === selAnchor);
  if (from < 0) from = to;

  const [lo, hi] = from <= to ? [from, to] : [to, from];
  for (let i = lo; i <= hi; i += 1) selection.add(items[i].id);
  selAnchor = id;
  syncSelection();
}

function clearSelection() {
  selection.clear();
  syncSelection();
}

/** 当前筛选下的**全部** id，由后端算——不能只拿滚出来的那几页。 */
async function allMatchingIds(): Promise<number[]> {
  try {
    return await invoke<number[]>("list_pair_ids", { filter: currentFilterPayload() });
  } catch (e) {
    setHint(`读取照片列表失败：${String(e)}`, "error");
    return [];
  }
}

async function selectAll() {
  const ids = await allMatchingIds();
  if (ids.length === 0) {
    setHint("当前筛选下没有照片可选。", "warn");
    return;
  }
  selection.clear();
  for (const id of ids) selection.add(id);
  selAnchor = ids[0];
  syncSelection();
  setHint(`已选中 ${ids.length.toLocaleString()} 张（当前筛选下的全部）。按一下 P 或 X 一次标记完。`);
}

async function invertSelection() {
  const ids = await allMatchingIds();
  if (ids.length === 0) return;
  for (const id of ids) {
    if (selection.has(id)) selection.delete(id);
    else selection.add(id);
  }
  syncSelection();
}

/**
 * 这一次标记落在谁身上。
 *
 * 大图打开时就是眼前这一张：在大图里，你只可能在判断这一张，
 * 让它去操作背后被选中的那一批是反直觉的，而且看不见后果。
 * 网格里则是当前选中的那一批。
 */
function markTargets(): number[] {
  if (!elLoupe.hidden) {
    const c = items[loupeIndex];
    return c ? [c.id] : [];
  }
  return [...selection];
}

/** 一张卡片在当前筛选下还应该出现吗。只在筛选涉及选片状态时才需要问。 */
function stillMatches(c: PairCard): boolean {
  if (filter.decision === "marked") {
    if (c.decision === "none") return false;
  } else if (filter.decision !== "all" && c.decision !== filter.decision) {
    return false;
  }
  if (filter.stars !== null && c.stars !== filter.stars) return false;
  return true;
}

/**
 * 把标记过的照片从网格里拿掉。
 *
 * 只在筛选依赖选片状态时调用。这时候「标记」的语义就是「处理完了，别再让我看见」——
 * 一屏一屏地清空，是选片里最爽的一环。
 *
 * 用直接摘 DOM 而不是重新查询：重新查询会把滚动位置和已经解码好的缩略图全丢掉，
 * 连按 P 的时候就会一直闪。
 */
function dropFromView(ids: Set<number>): boolean {
  if (ids.size === 0) return false;

  let removed = 0;
  const kept: PairCard[] = [];
  for (const c of items) {
    if (!ids.has(c.id)) {
      kept.push(c);
      continue;
    }
    const el = cardOf(c.id);
    if (el) {
      nearObserver.unobserve(el);
      farObserver.unobserve(el);
      el.remove();
    }
    itemById.delete(c.id);
    selection.delete(c.id);
    removed += 1;
  }

  if (removed === 0) return false;
  items = kept;
  // 被拿掉的一定是「当前筛选下符合条件」的那些，所以总数直接减就行
  total = Math.max(0, total - removed);
  updateCount();
  return true;
}

function revealCard(id: number) {
  cardOf(id)?.scrollIntoView({ block: "nearest" });
}

/**
 * 网格没填满就继续取下一页。
 *
 * 只在滚动时补页是不够的：「只看未标记」时按几下 P 把当前这屏清空，
 * 网格会变得比窗口还短——这时候不会再有任何滚动事件，
 * 新的照片就永远不出现了，看起来像「没了」。
 */
function fillIfNeeded() {
  if (noMore || loadingPage) return;
  if (elGrid.scrollHeight <= elGrid.clientHeight + 200) {
    void loadMore(renderToken);
  }
}

/**
 * 应用一次标记。`patch` 里没给的项就不动——
 * 这样「只改星级」不会把已经做好的保留/淘汰决定冲掉。
 */
async function applyPatch(patch: { decision?: Decision; stars?: number }) {
  const ids = markTargets();
  if (ids.length === 0) {
    setHint("先单击选中一张照片，或双击打开大图，再按 P / X。", "warn");
    return;
  }

  const inLoupe = !elLoupe.hidden;
  const current = inLoupe ? items[loupeIndex] : undefined;
  const anchorIdx = items.findIndex((c) => c.id === ids[0]);

  // ---- 乐观更新：先把画面改对，再等数据库确认 ----
  const backup: Array<{ c: PairCard; decision: Decision; stars: number }> = [];
  for (const id of ids) {
    const c = itemById.get(id);
    if (!c) continue;
    backup.push({ c, decision: c.decision, stars: c.stars });

    if (patch.decision !== undefined) {
      c.decision = patch.decision;
      // 「清除」要连星级一起清，否则会留下「未标记但有三颗星」这种看不懂的组合
      if (patch.decision === "none") c.stars = 0;
    }
    if (patch.stars !== undefined) c.stars = patch.stars;
  }
  repaint(backup.map((b) => b.c.id));

  try {
    await invoke<number>("apply_decision", {
      ids,
      decision: patch.decision ?? null,
      // 「清除标记」连星级一起清（界面上就是这么显示的），不写库的话重载后星级会冒回来
      stars: patch.stars ?? (patch.decision === "none" ? 0 : null),
    });
  } catch (e) {
    for (const b of backup) {
      b.c.decision = b.decision;
      b.c.stars = b.stars;
    }
    repaint(backup.map((b) => b.c.id));
    setHint(`标记没能保存：${String(e)}`, "error");
    return;
  }

  // 落库成功才进撤销栈：没写进去的操作撤销了也没意义
  pushUndo(
    labelForPatch(patch, ids.length),
    backup.map((b) => ({ id: b.c.id, decision: b.decision, stars: b.stars })),
  );

  // 侧栏计数要立刻跟着动——「未标记」少一张是最直接的进度反馈
  void refreshFacetCounts();

  // ---- 处理掉的照片该不该离开当前视图 ----
  const gone = new Set<number>();
  if (filterTracksMarks()) {
    for (const b of backup) if (!stillMatches(b.c)) gone.add(b.c.id);
  }
  const dropped = dropFromView(gone);
  if (dropped) fillIfNeeded();

  // ---- 自动前进 ----
  //
  // 保留/淘汰＝过片：一个键处理一张，手不离开键盘，这是选片真正的手感所在。
  // 打星＝评价：**不前进**。按完 3 星还停在这张上，觉得该给 5 星就再按 5，
  // 反悔了按 0——评价本来就要反复掂量，跳走了还谈什么调整。
  // 批量标记＝一次批处理：清掉选择，免得下一次误按又把整批重标一遍。
  const advance = patch.decision !== undefined;

  if (inLoupe) {
    if (dropped && current && gone.has(current.id)) {
      // 当前这张被筛选拿掉了，同一个索引现在指的就是下一张
      if (items.length === 0) closeLoupe();
      else void openLoupe(Math.min(loupeIndex, items.length - 1));
      return;
    }
    if (advance) {
      stepLoupe(1);
    } else {
      // 停在原地：刷新大图上的星级显示，按钮点亮状态跟着走
      const c = items[loupeIndex];
      if (c) paintLoupeMark(c);
      setHint(
        c && c.stars > 0
          ? `${"★".repeat(c.stars)} —— 再按 1-5 可调整，按 0 清除`
          : "已清除星级",
      );
    }
    return;
  }

  if (!advance) {
    // 网格里打星同样不挪选中：停在这张上，随时按别的数字调整
    syncSelection();
    if (ids.length === 1) {
      const c = itemById.get(ids[0]);
      if (c) {
        setHint(
          c.stars > 0
            ? `${"★".repeat(c.stars)} —— 再按 1-5 可调整，按 0 清除`
            : "已清除星级",
        );
      }
    }
    return;
  }

  const single = ids.length === 1;
  const next = single ? (dropped ? anchorIdx : anchorIdx + 1) : -1;

  selection.clear();
  if (next >= 0 && next < items.length) {
    selection.add(items[next].id);
    selAnchor = items[next].id;
    revealCard(items[next].id);
  }
  syncSelection();
}

function applyDecision(d: Decision) {
  void applyPatch({ decision: d });
}

function applyStars(n: number) {
  void applyPatch({ stars: n });
}

// ---- 撤销 ----

function labelForPatch(patch: { decision?: Decision; stars?: number }, n: number): string {
  const many = n > 1 ? ` ${n} 张` : "";
  if (patch.decision === "keep") return `保留${many}`;
  if (patch.decision === "reject") return `淘汰${many}`;
  if (patch.decision === "none") return `清除标记${many}`;
  if (patch.stars !== undefined) {
    return patch.stars > 0 ? `${patch.stars} 星${many}` : `清除星级${many}`;
  }
  return `标记${many}`;
}

function pushUndo(label: string, before: UndoItem[]) {
  if (before.length === 0) return;
  undoStack.push({ label, before });
  if (undoStack.length > UNDO_LIMIT) undoStack.shift();
  updateUndoBtn();
}

function clearUndo() {
  undoStack.length = 0;
  updateUndoBtn();
}

function updateUndoBtn() {
  const top = undoStack[undoStack.length - 1];
  elBtnUndo.disabled = !top;
  elBtnUndo.title = top
    ? `撤销：${top.label}（⌘Z / Ctrl+Z）`
    : "没有可撤销的标记操作（⌘Z / Ctrl+Z）";
}

/**
 * 把上一次标记改回原样。
 *
 * 走的是和正向标记同一个 apply_decision，所以侧栏计数、筛选、导出全都跟着回退，
 * 不会出现「界面退了、库里没退」这种对不上的情况。
 */
async function undoLast() {
  const entry = undoStack.pop();
  updateUndoBtn();
  if (!entry) {
    setHint("没有可撤销的标记操作。", "warn");
    return;
  }

  // 旧值相同的归成一批，一次调用写完——批量标记时不至于一张一个来回
  const groups = new Map<string, { ids: number[]; decision: Decision | null; stars: number | null }>();
  for (const it of entry.before) {
    const key = `${it.decision}|${it.stars}`;
    let g = groups.get(key);
    if (!g) {
      g = { ids: [], decision: it.decision, stars: it.stars };
      groups.set(key, g);
    }
    g.ids.push(it.id);
  }

  try {
    for (const g of groups.values()) {
      await invoke<number>("apply_decision", {
        ids: g.ids,
        decision: g.decision,
        stars: g.stars,
      });
    }
  } catch (e) {
    // 没退成就塞回去，别把这一步弄丢
    undoStack.push(entry);
    updateUndoBtn();
    setHint(`撤销失败：${String(e)}`, "error");
    return;
  }

  // 库已经改回去了，但内存里的卡片要跟着退，否则界面还是旧的
  let missing = false;
  for (const it of entry.before) {
    const c = itemById.get(it.id);
    if (!c) {
      missing = true;
      continue;
    }
    if (it.decision !== null) c.decision = it.decision;
    if (it.stars !== null) c.stars = it.stars;
  }
  repaint(entry.before.map((it) => it.id));
  void refreshFacetCounts();

  // 有卡片已经不在当前视图里（被筛选挪走了），重新按库里的状态铺一遍最省事
  if (missing) await reload({ keepView: true });

  if (!elLoupe.hidden) {
    const c = items[loupeIndex];
    if (c) paintLoupeMark(c);
  } else {
    // 网格里把光标放回被撤销的那张：退回来了什么，一眼能看到
    const first = entry.before.find((it) => itemById.has(it.id));
    if (first) {
      selection.clear();
      selection.add(first.id);
      selAnchor = first.id;
      syncSelection();
      revealCard(first.id);
    }
  }

  setHint(`已撤销：${entry.label}`);
}

// ---- 操作栏与网格的事件绑定 ----

elBtnKeep.addEventListener("click", () => applyDecision("keep"));
elBtnReject.addEventListener("click", () => applyDecision("reject"));
elBtnUnmark.addEventListener("click", () => applyDecision("none"));
elLoupeKeep.addEventListener("click", () => applyDecision("keep"));
elLoupeReject.addEventListener("click", () => applyDecision("reject"));
elLoupeUnmark.addEventListener("click", () => applyDecision("none"));

elStars.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest<HTMLButtonElement>("button[data-stars]");
  if (!btn?.dataset.stars) return;
  applyStars(Number(btn.dataset.stars));
});

// 大图里的星按钮：markTargets 在大图打开时就是当前这一张，
// 所以点哪儿改的就是眼前这张——打完不跳走，接着点就能调整。
elLoupeStars.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest<HTMLButtonElement>("button[data-stars]");
  if (!btn?.dataset.stars) return;
  applyStars(Number(btn.dataset.stars));
});

elBtnSelectAll.addEventListener("click", () => void selectAll());
elBtnInvert.addEventListener("click", () => void invertSelection());
elBtnSelectNone.addEventListener("click", clearSelection);

elGrid.addEventListener("click", (e) => {
  const card = (e.target as HTMLElement).closest<HTMLElement>(".card");
  if (!card) return;
  const id = Number(card.dataset.id);
  if (!id) return;

  if (e.metaKey || e.ctrlKey) toggleSelected(id);
  else if (e.shiftKey) selectRangeTo(id);
  else selectOnly(id);
});

// 双击看大图。单击留给「选中」——选片时选中比看用得频繁得多，
// 而看大图另外还有回车、空格两条路。
elGrid.addEventListener("dblclick", (e) => {
  const card = (e.target as HTMLElement).closest<HTMLElement>(".card");
  if (!card) return;
  const idx = items.findIndex((c) => String(c.id) === card.dataset.id);
  if (idx >= 0) void openLoupe(idx);
});

// ---------------------------------------------------------------------------
// 导出
//
// 「选完之后呢？」——这是选片必须回答的问题。答案是一份复制出来的好片，
// 加一份清单。**不动原片**：不删、不移、不改名。淘汰的照片也只是被记下来，
// 真正要删的时候由你自己动手；那张照片只被拍过一次，程序没有资格替你决定。
// ---------------------------------------------------------------------------

let lastExport: ExportSummary | null = null;

function currentChoice(group: HTMLElement): string {
  return group.querySelector<HTMLInputElement>("input:checked")?.value ?? "";
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"]/g, (c) =>
    c === "&" ? "&amp;" : c === "<" ? "&lt;" : c === ">" ? "&gt;" : "&quot;"
  );
}

function showExportResult(html: string) {
  elExportResult.innerHTML = html;
  elExportResult.hidden = false;
}

function showExportProgress(p: ExportProgress) {
  const label =
    p.phase === "copying" ? "正在复制文件" : p.phase === "manifest" ? "正在写清单" : "正在整理";

  if (p.total > 0) {
    elExportFill.classList.remove("is-indeterminate");
    elExportFill.style.width = `${Math.round((p.done / p.total) * 100)}%`;
    elExportProgressText.textContent = `${label} ${p.done.toLocaleString()} / ${p.total.toLocaleString()}`;
  } else {
    elExportFill.classList.add("is-indeterminate");
    elExportProgressText.textContent = label;
  }
}

async function openExportDialog() {
  let facets: LibraryFacets | null = null;
  try {
    facets = await invoke<LibraryFacets>("library_facets", { roots: currentRoots() });
  } catch {
    /* 拿不到计数就把选项留空，不挡住导出本身 */
  }
  const count = (key: string) => facets?.decisions.find((d) => d.key === key)?.count ?? 0;
  const fmt = (n: number) => `${n.toLocaleString()} 张`;

  const keep = count("keep");
  const reject = count("reject");
  elNoteKeep.textContent = fmt(keep);
  elNoteReject.textContent = fmt(reject);
  elNoteMarked.textContent = fmt(keep + reject);
  // 「全部」跟着当前筛选走，所以这里给的是筛选后的总数
  elNoteAll.textContent = total > 0 ? fmt(total) : "";

  // 勾了照片就把「当前选中的」摆在第一项并默认选中——这是最常用的那一条路
  const picked = Array.from(selection).filter((id) => itemById.has(id));
  elChoiceSelected.hidden = picked.length === 0;
  elNoteSelected.textContent = picked.length > 0 ? fmt(picked.length) : "";
  const scopeInputs = elExportScope.querySelectorAll<HTMLInputElement>('input[name="scope"]');
  for (const input of scopeInputs) {
    input.checked =
      picked.length > 0 ? input.value === "selected" : input.value === "keep";
  }

  elExportProgress.hidden = true;
  elExportResult.hidden = true;
  elExportReveal.hidden = true;
  elExportConfirm.textContent = "选择文件夹并导出";
  // 只有图库真的是空的才没得导——「全部」这一项任何时候都能用；
  // 图库空但网格里有选中项（极端情况）也允许导出
  elExportConfirm.disabled = total === 0 && picked.length === 0;
  elExportModal.hidden = false;
}

function closeExportDialog() {
  elExportModal.hidden = true;
}

elBtnExport.addEventListener("click", () => void openExportDialog());
elExportCancel.addEventListener("click", closeExportDialog);
elExportModal.addEventListener("click", (e) => {
  if (e.target === elExportModal) closeExportDialog();
});

elExportReveal.addEventListener("click", () => {
  if (!lastExport) return;
  // 在访达/资源管理器里把清单文件选出来，比只给一个路径有用
  void revealItemInDir(lastExport.manifest).catch(() => {
    setHint(`清单在：${lastExport?.manifest ?? ""}`);
  });
});

elExportConfirm.addEventListener("click", () => {
  void runExport();
});

async function runExport() {
  const scope = currentChoice(elExportScope);
  const mode = currentChoice(elExportMode);
  const files = currentChoice(elExportFiles) || "both";
  const template = elExportTemplate.value.trim() || "{name}";
  if (!scope || !mode) return;

  const dest = await open({ directory: true, multiple: false, title: "导出到哪个文件夹" });
  if (typeof dest !== "string") return;

  elExportConfirm.disabled = true;
  elExportProgress.hidden = false;
  elExportResult.hidden = true;
  elExportReveal.hidden = true;
  elExportFill.classList.add("is-indeterminate");
  elExportFill.style.width = "0%";
  elExportProgressText.textContent = "正在整理…";

  // 「当前选中的」是显式的一串 id，不掺筛选条件——勾了什么就导出什么。
  // 其余范围沿用原来的口径：日期、机身、搜索这些条件继续生效，
  // 「把这一天的保留都导出」是最常用的组合。scope 为 "all" 时后端认不出这个
  // 取值，于是不按选片状态筛，正好是我们要的意思。
  const onlyIds = scope === "selected" ? Array.from(selection).filter((id) => itemById.has(id)) : null;
  const filter =
    onlyIds && onlyIds.length > 0
      ? { ...currentFilterPayload(), decision: null, ids: onlyIds }
      : { ...currentFilterPayload(), decision: scope };

  try {
    const summary = await invoke<ExportSummary>("export_selection", {
      filter,
      dest,
      mode,
      scope: files,
      template,
    });
    lastExport = summary;

    const lines: string[] = [];
    if (mode === "copy") {
      lines.push(
        `复制了 <strong>${summary.copied.toLocaleString()}</strong> 个文件（${fmtBytes(summary.bytes)}），` +
          `涉及 <strong>${summary.photos.toLocaleString()}</strong> 张照片`
      );
      if (summary.skipped > 0) {
        lines.push(
          `跳过 <strong>${summary.skipped.toLocaleString()}</strong> 个：目标文件夹里已经有同样大小的同名文件`
        );
      }
      if (summary.failed > 0) {
        lines.push(`<strong>${summary.failed.toLocaleString()}</strong> 个复制失败，原片还在原处`);
      }
    } else {
      lines.push(`清单里共 <strong>${summary.photos.toLocaleString()}</strong> 张照片，没有复制文件`);
    }
    lines.push(`清单：<span class="result-path">${escapeHtml(summary.manifest)}</span>`);
    showExportResult(lines.join("<br>"));
    elExportReveal.hidden = false;
    setHint(`导出完成（用时 ${(summary.elapsedMs / 1000).toFixed(1)} 秒）。原片一张没动。`);
  } catch (e) {
    showExportResult(`导出失败：${escapeHtml(String(e))}`);
  } finally {
    elExportProgress.hidden = true;
    elExportFill.classList.remove("is-indeterminate");
    elExportConfirm.disabled = false;
    elExportConfirm.textContent = "再导出一次";
  }
}

// ---------------------------------------------------------------------------
// 连拍分组 + 对比选一张
//
// 拍连拍时一次快门连按好几张，挑一张最好的就行。这里按拍摄时间把相近的
// 聚成一组，点一组进去并排看，点一张 = 留它、同组其它自动淘汰。
// 标记走和卡片、灯箱同一套 apply_decision，所以侧栏计数、筛选都跟着动。
// ---------------------------------------------------------------------------

function currentGap(): number {
  const v = currentChoice(elSimilarGap);
  const n = Number(v);
  return Number.isFinite(n) && n > 0 ? n : 3;
}

async function openSimilarDialog() {
  if (total === 0) {
    setHint("先选择一个装有 NEF / JPG 的文件夹，再来看连拍分组。", "warn");
    return;
  }
  elSimilarList.replaceChildren();
  elSimilarEmpty.hidden = true;
  elSimilarNote.textContent = "正在按拍摄时间聚类…";
  elSimilarModal.hidden = false;
  await refreshSimilarGroups();
}

async function refreshSimilarGroups() {
  try {
    const groups = await invoke<SimilarGroup[]>("similar_groups", {
      filter: currentFilterPayload(),
      gapSecs: currentGap(),
    });
    renderSimilarGroups(groups);
  } catch (e) {
    elSimilarNote.textContent = `连拍分组失败：${String(e)}`;
    elSimilarList.replaceChildren();
  }
}

function renderSimilarGroups(groups: SimilarGroup[]) {
  elSimilarList.replaceChildren();
  if (groups.length === 0) {
    elSimilarEmpty.hidden = false;
    elSimilarNote.textContent = "";
    return;
  }

  let totalPhotos = 0;
  let totalKept = 0;
  for (const g of groups) {
    totalPhotos += g.size;
    totalKept += g.members.filter((m) => m.decision === "keep").length;
  }
  elSimilarNote.textContent =
    `${groups.length} 组 · 涉及 ${totalPhotos} 张` +
    (totalKept > 0 ? ` · 已挑出 ${totalKept} 张` : "");

  for (const g of groups) {
    const kept = g.members.filter((m) => m.decision === "keep").length;
    const card = document.createElement("div");
    card.className = "similar-card";

    const head = document.createElement("div");
    head.className = "similar-head";
    const span = g.spanSecs > 0 ? ` · 跨度 ${g.spanSecs}s` : "";
    const keptText = kept > 0 ? ` · 已挑 ${kept}` : "";
    head.innerHTML =
      `<span class="similar-time">${escapeHtml(g.startText ?? "时间未知")}</span>` +
      `<span class="similar-count">${g.size} 张${span}${keptText}</span>`;
    card.appendChild(head);

    const strip = document.createElement("div");
    strip.className = "similar-strip";
    for (const m of g.members) {
      const cell = document.createElement("button");
      cell.type = "button";
      cell.className = "similar-cell" + (m.decision === "keep" ? " is-kept" : "");
      cell.title = `${m.name}\n${m.timeText ?? ""}`;
      const img = document.createElement("img");
      img.alt = m.name;
      img.loading = "lazy";
      cell.appendChild(img);
      // 缩略图按 id 取，命中前端缓存就不重复请求
      void loadThumb(m.id, 512).then((p) => {
        img.src = p.dataUrl;
      });
      strip.appendChild(cell);
    }
    card.appendChild(strip);

    const actions = document.createElement("div");
    actions.className = "similar-actions";
    const compareBtn = document.createElement("button");
    compareBtn.type = "button";
    compareBtn.className = "btn btn-primary";
    compareBtn.textContent = "对比选一张";
    compareBtn.addEventListener("click", () => openCompare(g));
    actions.appendChild(compareBtn);
    card.appendChild(actions);

    elSimilarList.appendChild(card);
  }
}

let compareGroup: SimilarGroup | null = null;

function openCompare(group: SimilarGroup) {
  compareGroup = group;
  elCompareGrid.replaceChildren();
  elCompareModal.hidden = false;

  group.members.forEach((m, i) => {
    const tile = document.createElement("button");
    tile.type = "button";
    tile.className = "compare-tile" + (m.decision === "keep" ? " is-kept" : "");
    tile.dataset.id = String(m.id);

    const img = document.createElement("img");
    img.alt = m.name;
    tile.appendChild(img);
    void loadThumb(m.id, 1600).then((p) => {
      img.src = p.dataUrl;
    });

    const num = document.createElement("span");
    num.className = "compare-num";
    num.textContent = String(i + 1);
    tile.appendChild(num);

    const name = document.createElement("span");
    name.className = "compare-name";
    name.textContent = m.name;
    tile.appendChild(name);

    if (m.decision === "keep") {
      const badge = document.createElement("span");
      badge.className = "compare-badge";
      badge.textContent = "已选";
      tile.appendChild(badge);
    }

    tile.addEventListener("click", () => pickInGroup(group, m.id));
    elCompareGrid.appendChild(tile);
  });
}

/** 选一张：它标 keep，同组其它一律 reject。已经是 keep 的那张再点一次 = 取消（全清回 none）。 */
async function pickInGroup(group: SimilarGroup, pickId: number) {
  const already = group.members.find((m) => m.id === pickId)?.decision === "keep";
  const patch: { id: number; decision: string }[] = group.members.map((m) => ({
    id: m.id,
    decision: already ? "none" : m.id === pickId ? "keep" : "reject",
  }));

  // 星级没动过，撤销时也不该去碰它，所以记 null
  const before: UndoItem[] = group.members.map((m) => ({
    id: m.id,
    decision: m.decision as Decision,
    stars: null,
  }));

  // 先本地乐观更新，避免一张张闪
  for (const p of patch) {
    const cell = elCompareGrid.querySelector<HTMLElement>(`[data-id="${p.id}"]`);
    if (!cell) continue;
    cell.classList.toggle("is-kept", p.decision === "keep");
    const oldBadge = cell.querySelector(".compare-badge");
    if (p.decision === "keep" && !oldBadge) {
      const badge = document.createElement("span");
      badge.className = "compare-badge";
      badge.textContent = "已选";
      cell.appendChild(badge);
    } else if (p.decision !== "keep" && oldBadge) {
      oldBadge.remove();
    }
  }

  try {
    await Promise.all(
      patch.map((p) => invoke<number>("apply_decision", { ids: [p.id], decision: p.decision, stars: null }))
    );
    // 同步回分组数据，连拍列表的「已挑 N」才准
    for (const p of patch) {
      const m = group.members.find((x) => x.id === p.id);
      if (m) m.decision = p.decision;
    }
    pushUndo(already ? "取消连拍选择" : `连拍选优 ${group.size} 张`, before);
    void refreshFacetCounts();
  } catch (e) {
    setHint(`标记没能保存：${String(e)}`, "error");
    void refreshSimilarGroups();
  }
}

function closeSimilarDialog() {
  elSimilarModal.hidden = true;
}

function closeCompareDialog() {
  elCompareModal.hidden = true;
  compareGroup = null;
  // 关掉对比时把连拍列表的「已挑 N」刷新一下（缩略图走前端缓存，开销很小）
  void refreshSimilarGroups();
}

elBtnSimilar.addEventListener("click", () => void openSimilarDialog());
elBtnUndo.addEventListener("click", () => void undoLast());
elSimilarClose.addEventListener("click", closeSimilarDialog);
elSimilarModal.addEventListener("click", (e) => {
  if (e.target === elSimilarModal) closeSimilarDialog();
});
elSimilarGap.addEventListener("change", () => void refreshSimilarGroups());
elCompareDone.addEventListener("click", closeCompareDialog);
elCompareModal.addEventListener("click", (e) => {
  if (e.target === elCompareModal) closeCompareDialog();
});

// 对比视图里 1–9 直接选第几张，Esc 关掉
document.addEventListener("keydown", (e) => {
  if (elCompareModal.hidden) return;
  if (e.key === "Escape") {
    closeCompareDialog();
    return;
  }
  const n = Number(e.key);
  if (Number.isInteger(n) && n >= 1 && n <= 9 && compareGroup) {
    const m = compareGroup.members[n - 1];
    if (m) void pickInGroup(compareGroup, m.id);
  }
});

// ---------------------------------------------------------------------------
// 扫描
// ---------------------------------------------------------------------------

function renderRoot() {
  if (!rootPath) {
    elRootChip.hidden = true;
    elBtnClearLib.hidden = true;
    return;
  }
  elRootChip.hidden = false;
  elBtnClearLib.hidden = false;
  elRootPath.textContent = rootPath;
  elRootPath.title = rootPath;
  elRootPath.parentElement?.setAttribute("title", rootPath);
}

const PHASE_LABEL: Record<ScanProgress["phase"], string> = {
  walking: "正在遍历目录…",
  parsing: "读取元数据",
  writing: "写入索引…",
  stats: "统计配对…",
};

function showProgress(p: ScanProgress) {
  elProgress.hidden = false;
  const label = PHASE_LABEL[p.phase] ?? p.phase;

  if (p.phase === "parsing" && p.total > 0) {
    elProgressFill.classList.remove("is-indeterminate");
    elProgressFill.style.width = `${Math.round((p.done / p.total) * 100)}%`;
    elProgressText.textContent = `${label} ${p.done.toLocaleString()} / ${p.total.toLocaleString()}`;
  } else if (p.phase === "walking") {
    elProgressFill.classList.add("is-indeterminate");
    elProgressText.textContent = label;
  } else {
    elProgressFill.classList.remove("is-indeterminate");
    elProgressFill.style.width = "100%";
    elProgressText.textContent = label;
  }
}

function hideProgress() {
  elProgress.hidden = true;
  elProgressFill.classList.remove("is-indeterminate");
  elProgressFill.style.width = "0%";
}

function summaryText(r: ScanSummary): string {
  const secs = (r.elapsedMs / 1000).toFixed(1);
  if (r.scanned === 0) {
    return `这个文件夹里没找到 NEF / JPG（用时 ${secs} 秒）。`;
  }
  const bits = [
    `识别 ${r.scanned} 个文件`,
    `配对 ${r.pairs} 张照片`,
    `用时 ${secs} 秒`,
  ];
  if (r.inserted) bits.splice(1, 0, `新增 ${r.inserted}`);
  if (r.updated) bits.splice(1, 0, `更新 ${r.updated}`);
  if (r.removed) bits.push(`清理 ${r.removed}`);
  if (r.failed) bits.push(`${r.failed} 个读不了`);
  if (r.exifFailed) bits.push(`${r.exifFailed} 个无 EXIF`);
  if (r.orphanRaw || r.orphanJpg) bits.push(`孤立 ${r.orphanRaw + r.orphanJpg}（左侧可筛）`);
  return bits.join(" · ");
}

// 渐进式显示：扫描进行时，每隔一小段时间把「已经入库的部分」铺到网格上，
// 用户先看到前 N 张，剩下的在后台继续扫。批与批之间后端会释放数据库连接锁，
// 所以前端插进来查询不会卡住扫描。
let progressiveTimer: number | null = null;
function startProgressiveRefresh() {
  if (progressiveTimer !== null) return;
  const tick = async () => {
    if (!scanning) return;
    try {
      // keepView：扫描中用户可能已经在翻、在选了，别把人弹回顶部
      await reload({ keepView: true });
    } catch {
      /* 扫描中途偶发读不到无所谓，下一拍再试 */
    }
    if (scanning) progressiveTimer = window.setTimeout(tick, 400);
  };
  progressiveTimer = window.setTimeout(tick, 350);
}
function stopProgressiveRefresh() {
  if (progressiveTimer !== null) {
    clearTimeout(progressiveTimer);
    progressiveTimer = null;
  }
}

async function startScan(
  path: string,
  opts: { quiet: boolean; includeDirs?: string[] | null },
) {
  if (scanning) return;
  scanning = true;
  lastScopeDirs = opts.includeDirs ?? null;
  elPick.disabled = true;
  elRescan.disabled = true;
  elProgressFill.classList.add("is-indeterminate");
  elProgressText.textContent = "准备中…";
  elProgress.hidden = false;

  startProgressiveRefresh();

  try {
    const r = await invoke<ScanSummary>("scan_folder", {
      path,
      includeDirs: opts.includeDirs ?? null,
    });
    // 扫描已经结束，立刻停下轮询，避免和下面的完整刷新打架
    stopProgressiveRefresh();

    const changed = r.inserted + r.updated + r.removed;
    if (opts.quiet && changed === 0) {
      // 启动时的自动重扫：图库没变化就别重排界面，免得白闪一下
      setHint(`图库已是最新（${r.pairs.toLocaleString()} 张，检查用时 ${(r.elapsedMs / 1000).toFixed(1)} 秒）`);
    } else {
      if (r.pairs === 0 && opts.includeDirs?.length === 0) {
        // 一个子文件夹都没勾、根目录自己这层又没照片：把出路说清楚，别让人对着空网格猜
        setHint(
          "当前文件夹里没有照片（没有进入子文件夹）。点「重扫」重新选择，勾上里面的子文件夹再试。",
          "warn",
        );
      } else {
        setHint(summaryText(r), r.failed || r.exifFailed ? "warn" : "ok");
      }
      await refreshLibrary();
      // 图库铺完再补画面分析：算过的不再算，中断了下次接着来
      void runAnalysis();
    }
  } catch (e) {
    stopProgressiveRefresh();
    const msg = String(e);
    if (msg.includes("目录不存在") || msg.includes("不是文件夹")) {
      setHint(`文件夹访问不到：${path}（外置盘没插？）—— 图库内容仍然可用`, "warn");
    } else {
      setHint(`扫描失败：${msg}`, "error");
    }
  } finally {
    scanning = false;
    elPick.disabled = false;
    elRescan.disabled = !rootPath;
    hideProgress();
    void refreshCacheInfo();
  }
}

/**
 * 扫描之后跑一遍画面分析（清晰度 / 曝光）。
 *
 * 不挂在扫描里：扫描的KPI是快点出图，分析要解码，混在一起就是干等。
 * 这里只算「还没算过的」，所以中断了下次接着来，不用从头开始。
 */
// ── 机身时间校正 ────────────────────────────────────────────────────────
//
// 单机身时这一栏没有意义，所以按钮只在读到两个以上机身时才出现。
async function openOffsetDialog() {
  let bodies: CameraBody[] = [];
  try {
    bodies = await invoke<CameraBody[]>("list_camera_bodies", { roots: currentRoots() });
  } catch {
    setHint("读不到机身信息。", "warn");
    return;
  }

  elOffsetList.innerHTML = "";
  elOffsetEmpty.hidden = bodies.length > 0;

  for (const b of bodies) {
    const row = document.createElement("div");
    row.className = "offset-row";

    const name = document.createElement("div");
    name.className = "offset-name";
    name.textContent = b.serial === NONE_KEY ? "读不到序列号" : b.model;
    name.title = b.serial === NONE_KEY ? "这批照片里没有机身序列号" : b.serial;

    const sub = document.createElement("div");
    sub.className = "offset-sub";
    sub.textContent = `${b.photos.toLocaleString()} 张`;

    const input = document.createElement("input");
    input.className = "input offset-input";
    input.type = "number";
    input.step = "1";
    input.value = String(b.offsetSeconds);
    input.title = "正数＝这台机身时钟慢了，要往后加；负数＝快了，要往回减";

    const apply = document.createElement("button");
    apply.className = "btn btn-ghost btn-sm";
    apply.textContent = "应用";
    apply.addEventListener("click", () => {
      const secs = Math.trunc(Number(input.value));
      if (!Number.isFinite(secs)) {
        setHint("请填一个整数秒数。", "warn");
        return;
      }
      void (async () => {
        try {
          await invoke("set_camera_offset", { serial: b.serial, offsetSeconds: secs });
          // 校正会重算所有照片的排序时间，视图必须整个重铺
          await refreshLibrary();
          setHint(
            secs === 0
              ? "已清除这台机身的时间校正。"
              : `已校正：${b.model} ${secs > 0 ? "+" : ""}${secs} 秒，排序与连拍分组已按新时间重算。`,
          );
        } catch (e) {
          setHint(`校正失败：${String(e)}`, "error");
        }
      })();
    });

    row.append(name, sub, input, apply);
    elOffsetList.appendChild(row);
  }

  elOffsetModal.hidden = false;
}

elBtnTimeOffset.addEventListener("click", () => void openOffsetDialog());
elOffsetCancel.addEventListener("click", () => {
  elOffsetModal.hidden = true;
});
elOffsetModal.addEventListener("click", (e) => {
  if (e.target === elOffsetModal) elOffsetModal.hidden = true;
});

async function runAnalysis() {
  if (analyzing || !rootPath) return;
  analyzing = true;
  try {
    const r = await invoke<AnalyzeSummary>("analyze_library", { roots: currentRoots() });
    if (r.analyzed === 0) return;

    await loadFacets();
    const blur = lastFacets?.quality.find((q) => q.key === "blur")?.count ?? 0;
    const over = lastFacets?.quality.find((q) => q.key === "over")?.count ?? 0;
    const under = lastFacets?.quality.find((q) => q.key === "under")?.count ?? 0;
    const bits: string[] = [];
    if (blur > 0) bits.push(`${blur.toLocaleString()} 张可能糊了`);
    if (over > 0) bits.push(`${over.toLocaleString()} 张高光溢出`);
    if (under > 0) bits.push(`${under.toLocaleString()} 张暗部死黑`);
    setHint(
      bits.length > 0
        ? `画面分析完成（${r.analyzed.toLocaleString()} 张）：${bits.join(" · ")}，左侧「画面质量」可单独筛`
        : `画面分析完成（${r.analyzed.toLocaleString()} 张），没发现明显问题`,
    );
  } catch {
    /* 分析失败不影响选片本身，静默跳过 */
  } finally {
    analyzing = false;
  }
}

async function pickFolder() {
  const picked = await open({ directory: true, multiple: false, title: "选择照片文件夹" });
  if (typeof picked !== "string") return;

  const previous = rootPath;
  rootPath = picked;
  localStorage.setItem(ROOT_KEY, picked);
  renderRoot();
  elRescan.disabled = false;
  elRootMeta.textContent = "";

  // 看看有没有子目录——有就先让用户勾选要纳入扫描的范围
  let subdirs: DirNode[] = [];
  try {
    subdirs = await invoke<DirNode[]>("list_subdirs", { path: picked });
  } catch {
    subdirs = [];
  }
  if (subdirs.length > 0) {
    scopePrevRoot = previous;
    openScopeModal(picked, subdirs);
  } else {
    setHint("正在扫描…缩略图会在过程中逐张出现。");
    await startScan(picked, { quiet: false });
  }
}

/** 清空当前文件夹：不删原片、不丢标记、不碰索引库，只是回到「未选文件夹」状态，
 *  等用户再选一个。已经做过的选片标记按 pair_key 落在库里，重扫同目录会回来。 */
function clearLibrary() {
  stopProgressiveRefresh();
  rootPath = null;
  lastScopeDirs = null;
  scopePrevRoot = null;
  localStorage.removeItem(ROOT_KEY);
  items = [];
  total = 0;
  noMore = false;
  itemById.clear();
  selection.clear();
  nearObserver.disconnect();
  farObserver.disconnect();
  elGrid.innerHTML = "";
  elGrid.classList.remove("is-grouped");
  groupMap = null;
  clearUndo();
  lastFacets = null;
  // 筛选条件是跟着「当前文件夹」走的，文件夹都没了就别留着——
  // 留着的话下一个文件夹会被旧条件悄悄过滤掉几张，很难查。
  resetFilter();
  renderRoot();
  renderFacetsEmpty();
  updateCount();
  elRescan.disabled = true;
  elCount.textContent = "";
  setHint("已清空。选择一个装有 NEF / JPG 的文件夹，选完会自动扫描并出图。");
  void refreshCacheInfo();
}

// ── 文件夹范围选择弹窗 ───────────────────────────────────────────────────

let scopePrevRoot: string | null = null;
let scopeTargetPath = "";

function openScopeModal(root: string, dirs: DirNode[]) {
  scopeTargetPath = root;
  elScopeRoot.textContent = `根目录：${root}`;
  elScopeList.innerHTML = "";
  for (const d of dirs) {
    const li = document.createElement("li");
    li.className = "scope-item";
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = true;
    cb.value = d.path;
    const label = document.createElement("span");
    label.className = "scope-item-name";
    label.textContent = "　".repeat(Math.max(0, d.depth - 1)) + d.name + (d.hasChildren ? " ›" : "");
    label.title = d.path;
    li.append(cb, label);
    // 点整行也能勾选 / 取消
    li.addEventListener("click", (e) => {
      if (e.target !== cb) cb.checked = !cb.checked;
    });
    elScopeList.appendChild(li);
  }
  elScopeModal.hidden = false;
}

function closeScopeModal() {
  elScopeModal.hidden = true;
}

function applyScopeSelection(checked: boolean) {
  for (const cb of elScopeList.querySelectorAll<HTMLInputElement>("input[type=checkbox]")) {
    cb.checked = checked;
  }
}

elScopeAll.addEventListener("click", () => applyScopeSelection(true));
elScopeNone.addEventListener("click", () => applyScopeSelection(false));

elScopeCancel.addEventListener("click", () => {
  closeScopeModal();
  // 取消范围选择：退回原来的文件夹，不切换图库
  if (scopePrevRoot !== null) {
    rootPath = scopePrevRoot;
    if (rootPath) localStorage.setItem(ROOT_KEY, rootPath);
    else localStorage.removeItem(ROOT_KEY);
    renderRoot();
    elRescan.disabled = !rootPath;
    scopePrevRoot = null;
  }
  setHint("已取消范围选择，仍是原来的文件夹。");
});

elScopeOk.addEventListener("click", () => {
  const chosen = Array.from(
    elScopeList.querySelectorAll<HTMLInputElement>("input[type=checkbox]"),
  )
    .filter((cb) => cb.checked)
    .map((cb) => cb.value);
  closeScopeModal();
  const root = scopeTargetPath;
  scopePrevRoot = null;
  // 一个都不勾不是错误，意思是「只读这个文件夹自己那一层的照片」
  setHint(
    chosen.length > 0 ? "正在扫描…缩略图会在过程中逐张出现。" : "正在扫描当前文件夹里的照片（不进子文件夹）…",
  );
  void startScan(root, { quiet: false, includeDirs: chosen });
});

// 点遮罩空白处关闭（等同取消）
elScopeModal.addEventListener("click", (e) => {
  if (e.target === elScopeModal) {
    elScopeCancel.click();
  }
});

// ── 分组方式选择 ─────────────────────────────────────────────────────────

const elGroupMode = $<HTMLSelectElement>("#group-mode");

function setGroupMode(mode: GroupMode) {
  groupMode = mode;
  localStorage.setItem(GROUP_KEY, mode);
  // 重新铺一遍当前图库，分组视图立刻生效
  if (rootPath) void reload();
}

elGroupMode.addEventListener("change", () => setGroupMode(elGroupMode.value as GroupMode));
elBtnClearLib.addEventListener("click", () => clearLibrary());

elPick.addEventListener("click", () => void pickFolder());

elRescan.addEventListener("click", () => {
  if (rootPath) void startScan(rootPath, { quiet: false, includeDirs: lastScopeDirs });
});

// ---------------------------------------------------------------------------
// 图库刷新
// ---------------------------------------------------------------------------

async function refreshLibrary() {
  await loadFacets();
  await reload();
  await loadStats();
  void refreshCacheInfo();
}

async function loadStats() {
  try {
    const s = await invoke<LibraryStats>("library_stats");
    elRootMeta.textContent = s.pairs > 0 ? `${s.pairs.toLocaleString()} 张` : "";
    if (!rootPath && s.pairs > 0) {
      setHint(`图库里有 ${s.pairs.toLocaleString()} 张照片（${s.cameras} 台机身）。选一次拍摄的文件夹开始。`);
    }
  } catch {
    /* 统计失败不影响主流程 */
  }
}

/** 最近一次拿到的占用情况。清理前后各取一次，差值就是这次释放的空间。 */
let lastCache: CacheStats | null = null;

async function refreshCacheInfo() {
  try {
    const c = await invoke<CacheStats>("cache_stats");
    lastCache = c;
    renderCacheInfo(c);
  } catch {
    elCacheInfo.textContent = "";
  }
}

// ---------------------------------------------------------------------------
// 缓存清理
//
// 缩略图缓存是唯一会自己长大的东西（看过的每张照片都留 2~3 份 JPEG），
// 也是唯一「删了无所谓」的东西——原片没动，下次浏览重新抠一次预览就回来。
// 所以清理入口必须让人分得清三件事：能放心删的、删了要重扫的、删了就没了的。
// ---------------------------------------------------------------------------

function renderCacheInfo(c: CacheStats) {
  const total = c.thumbsBytes + c.dbBytes;
  elCacheInfo.textContent = total > 0 ? `缓存 ${fmtBytes(total)}` : "";
  elCacheInfo.classList.toggle("is-link", total > 0);
  if (total > 0) {
    elCacheInfo.title =
      `缩略图 ${fmtBytes(c.thumbsBytes)}（${c.thumbsFiles.toLocaleString()} 个文件）\n` +
      `索引库 ${fmtBytes(c.dbBytes)}\n${c.thumbsDir}\n\n点这里清理`;
  } else {
    elCacheInfo.removeAttribute("title");
  }
}

function paintCacheStats(c: CacheStats) {
  elCacheThumbs.textContent =
    c.thumbsFiles > 0
      ? `${fmtBytes(c.thumbsBytes)} · ${c.thumbsFiles.toLocaleString()} 个文件`
      : "空";
  elCacheDb.textContent =
    c.photos > 0
      ? `${fmtBytes(c.dbBytes)} · ${c.photos.toLocaleString()} 张照片`
      : `${fmtBytes(c.dbBytes)} · 无索引`;
  elCacheDecisions.textContent = c.decisions > 0 ? `${c.decisions.toLocaleString()} 张` : "无";
  elCacheDir.textContent = `缓存目录：${c.thumbsDir}`;
  elNoteThumbs.textContent =
    c.thumbsBytes > 0
      ? `释放约 ${fmtBytes(c.thumbsBytes)}；索引和选片标记都在，下次浏览自动重建`
      : "当前没有缩略图缓存";
}

async function openCacheDialog() {
  elCacheModal.hidden = false;
  elCacheClear.disabled = true;
  elCacheThumbs.textContent = "读取中…";
  elCacheDb.textContent = "读取中…";
  elCacheDecisions.textContent = "读取中…";
  elCacheDir.textContent = "";

  try {
    const c = await invoke<CacheStats>("cache_stats");
    lastCache = c;
    paintCacheStats(c);
    elCacheClear.disabled = c.thumbsBytes + c.dbBytes === 0;
  } catch (e) {
    elCacheThumbs.textContent = "读取失败";
    elCacheClear.disabled = true;
    setHint(`读取缓存占用失败：${String(e)}`, "error");
  }
}

function closeCacheDialog() {
  elCacheModal.hidden = true;
}

async function runClearCache() {
  const scope = currentChoice(elCacheScope);
  if (!scope) return;

  if (scope === "all") {
    const n = lastCache?.decisions ?? 0;
    const ok = await confirm(
      n > 0
        ? `会把 ${n.toLocaleString()} 张照片的保留 / 淘汰和星级一起删除，删掉之后找不回来。\n\n原片不受影响。确定继续吗？`
        : "会清空照片索引和全部选片标记（当前没有已标记的照片）。\n\n原片不受影响。确定继续吗？",
      { title: "全部清空", kind: "warning" }
    );
    if (!ok) return;
  }

  const before = (lastCache?.thumbsBytes ?? 0) + (lastCache?.dbBytes ?? 0);
  elCacheClear.disabled = true;

  try {
    const after = await invoke<CacheStats>("clear_cache", { scope });
    lastCache = after;
    paintCacheStats(after);
    renderCacheInfo(after);

    // 前端内存里也缓存着一批 dataUrl，不清的话「占用」看着像没变
    gridCache.clear();
    loupeCache.clear();

    const freed = Math.max(0, before - after.thumbsBytes - after.dbBytes);
    const what = scope === "thumbs" ? "缩略图缓存" : scope === "index" ? "照片索引和缓存" : "全部数据";
    setHint(
      `已清理${what}，释放 ${fmtBytes(freed)}。原片一张没动。` +
        (scope === "thumbs" ? "" : rootPath ? " 点「重新扫描」可重建索引。" : " 选一个文件夹即可重建索引。")
    );

    // 索引没了，界面上还挂着已经不存在的照片——必须重新拉一次。
    // 顺带丢掉撤销栈：重建索引后 photo id 会重新分配，留着旧 id 撤销会落到别的照片上。
    if (scope !== "thumbs") {
      selection.clear();
      clearUndo();
      await refreshLibrary();
    }
  } catch (e) {
    setHint(`清理失败：${String(e)}`, "error");
  } finally {
    elCacheClear.disabled = false;
  }
}

elCache.addEventListener("click", () => void openCacheDialog());
// 页脚那个「缓存 128 MB」也是入口——数字本身就在提醒人去清理
elCacheInfo.addEventListener("click", () => {
  if (elCacheInfo.classList.contains("is-link")) void openCacheDialog();
});
elCacheCancel.addEventListener("click", closeCacheDialog);
elCacheClear.addEventListener("click", () => void runClearCache());
elCacheModal.addEventListener("click", (e) => {
  if (e.target === elCacheModal) closeCacheDialog();
});

// ---------------------------------------------------------------------------
// 大图查看
// ---------------------------------------------------------------------------

let loupeIndex = -1;

// ---------------------------------------------------------------------------
// 大图缩放
//
// 放大不是为了「看得更大」，是为了看清这张有没有对上焦。所以三件事必须同时成立：
// 1. 手不用离开鼠标键盘就能缩放和平移（滚轮 / 双击 / 拖拽 / 按钮 / 快捷键）；
// 2. 放大后要有更多像素可看——1600px 放大两倍就糊了，得按需换高清档；
// 3. 翻页自动回到整图，不会一不小心停在 400% 上翻完整组。
// ---------------------------------------------------------------------------

const ZOOM_MIN = 1;
const ZOOM_MAX = 8;
/** 双击一步到这个倍率：够看细节，又不至于一步飞进去找不着北。 */
const ZOOM_DBL = 2.5;

let zoom = ZOOM_MIN;
let panX = 0;
let panY = 0;
/** 当前显示的这张图自身有多少像素宽，用来算「1:1」到底是几倍。 */
let naturalW = 0;
/** 当前用上的档位。高清档换上去之后就不再重复替换。 */
let loupeSrcSize = 0;

/**
 * 图片「适应窗口」时的中心与尺寸。
 *
 * 注意 `getBoundingClientRect()` 读的是变换后的盒子，所以要把 pan 减掉才是基准值。
 * 调用前提是 rect 与当前 pan 同步——也就是每次改完 pan/zoom 都立刻 apply 过。
 */
function baseBox(): Box {
  const r = elLoupeImg.getBoundingClientRect();
  return {
    cx: r.left + r.width / 2 - panX,
    cy: r.top + r.height / 2 - panY,
    w: r.width / zoom,
    h: r.height / zoom,
  };
}

/** 把平移限制在「图边刚好贴住视口边」之内，别把图拖出视野。 */
function clampPan(b: Box) {
  const next = zoomMath.clampPan(
    { x: panX, y: panY },
    zoom,
    b,
    { w: window.innerWidth, h: window.innerHeight },
  );
  panX = next.x;
  panY = next.y;
}

/** 「一个图像素占一个屏幕像素」时的倍率。图片还没加载出来时给 1。 */
function actualZoom(): number {
  // 布局宽度不受 transform 影响，所以放大之后再问 1:1 是多少倍，答案依然对
  return zoomMath.actualZoom(naturalW, elLoupeImg.offsetWidth, ZOOM_MAX);
}

function applyZoom() {
  elLoupeImg.style.transform =
    zoom <= ZOOM_MIN && panX === 0 && panY === 0
      ? ""
      : `translate(${panX}px, ${panY}px) scale(${zoom})`;
  elLoupeZoomLabel.textContent = `${Math.round(zoom * 100)}%`;
  elLoupe.classList.toggle("is-zoomed", zoom > ZOOM_MIN);
  elLoupeZoomActual.classList.toggle("is-active", Math.abs(zoom - actualZoom()) < 0.01);
}

/** 以屏幕上某点为锚点缩放——滚轮、双击都靠它，鼠标底下那一点必须纹丝不动。 */
function zoomAt(mx: number, my: number, next: number) {
  const b = baseBox();
  const target = zoomMath.clampZoom(next, ZOOM_MIN, ZOOM_MAX);
  const p = zoomMath.panForZoomAt({ x: mx, y: my }, b, { x: panX, y: panY }, zoom, target);
  zoom = target;
  panX = p.x;
  panY = p.y;
  clampPan(b);
  applyZoom();
  const c = items[loupeIndex];
  if (c) scheduleDetail(c);
}

/** 以图片当前中心为锚点缩放，给键盘和 ± 按钮用。 */
function zoomBy(factor: number) {
  const r = elLoupeImg.getBoundingClientRect();
  zoomAt(r.left + r.width / 2, r.top + r.height / 2, zoom * factor);
}

function resetZoom() {
  zoom = ZOOM_MIN;
  panX = 0;
  panY = 0;
  applyZoom();
}

// ---- 按需加载高清档 ----
//
// 4096px 那档一张就是几 MB，所以只在真的放大了才去要。1600px 顶着先看着，
// 高清档到了再悄悄换上——倍率和平移都不变，视觉上只是突然变清晰。

let detailTimer: number | undefined;
let detailBusy = false;

function scheduleDetail(c: PairCard) {
  window.clearTimeout(detailTimer);
  if (zoom <= 1.05) return;
  // 停顿一下再要：滚轮连续放大时，中途那些倍率不值得各生成一次大图
  detailTimer = window.setTimeout(() => void loadDetail(c), 220);
}

async function loadDetail(c: PairCard) {
  if (detailBusy || items[loupeIndex]?.id !== c.id) return;
  if (loupeSrcSize >= 4096) return;
  const cached = previewCache.get(c.id);
  if (cached) {
    swapLoupeSrc(cached);
    return;
  }
  detailBusy = true;
  try {
    const t = await loadThumb(c.id, 4096);
    if (items[loupeIndex]?.id !== c.id) return;
    swapLoupeSrc(t);
  } catch {
    // 高清档拿不到就继续用 1600，不打扰正在选片的人
  } finally {
    detailBusy = false;
  }
}

function swapLoupeSrc(t: ThumbPayload) {
  if (t.size <= loupeSrcSize) return;
  // 先解码再替换，避免中间闪一下空白
  const img = new Image();
  img.onload = () => {
    if (items[loupeIndex]?.id !== t.id) return;
    loupeSrcSize = t.size;
    elLoupeImg.src = t.dataUrl;
    naturalW = img.naturalWidth;
    elLoupeImg.title = `${t.sourceWidth}×${t.sourceHeight} 像素 · 来源：${
      ROUTE_LABEL[t.route] ?? t.route
    }`;
    applyZoom();
  };
  img.src = t.dataUrl;
}

// ---- 交互 ----

elLoupeImg.addEventListener("load", () => {
  naturalW = elLoupeImg.naturalWidth;
  applyZoom();
});

elLoupe.addEventListener(
  "wheel",
  (e) => {
    if (elLoupe.hidden) return;
    e.preventDefault();
    // 触控板会连发几十个小 delta，用指数映射，手感才是连续的而不是一格一跳
    zoomAt(e.clientX, e.clientY, zoom * Math.exp(-e.deltaY * 0.0022));
  },
  { passive: false },
);

elLoupeImg.addEventListener("dblclick", (e) => {
  if (zoom > ZOOM_MIN) resetZoom();
  else zoomAt(e.clientX, e.clientY, ZOOM_DBL);
});

let dragging = false;
let dragBase: Box = { cx: 0, cy: 0, w: 0, h: 0 };
let dragFrom = { x: 0, y: 0, panX: 0, panY: 0 };

elLoupeImg.addEventListener("pointerdown", (e) => {
  if (zoom <= ZOOM_MIN) return;
  dragging = true;
  dragBase = baseBox(); // 拖动期间视口不变，基准量一次就够
  dragFrom = { x: e.clientX, y: e.clientY, panX, panY };
  elLoupeImg.setPointerCapture(e.pointerId);
  elLoupe.classList.add("is-grabbing");
  // 这里不能 preventDefault：那会连掉后续的 click/dblclick，「双击缩小」就废了。
  // 原生拖图和选中文字交给 CSS（user-select / -webkit-user-drag）和下面的 dragstart 拦。
});

// 放大后拖着图走，Safari/Chrome 会想把图片本身拖出去
elLoupeImg.addEventListener("dragstart", (e) => e.preventDefault());

elLoupeImg.addEventListener("pointermove", (e) => {
  if (!dragging) return;
  panX = dragFrom.panX + (e.clientX - dragFrom.x);
  panY = dragFrom.panY + (e.clientY - dragFrom.y);
  clampPan(dragBase);
  applyZoom();
});

function endDrag(e: PointerEvent) {
  if (!dragging) return;
  dragging = false;
  elLoupe.classList.remove("is-grabbing");
  if (elLoupeImg.hasPointerCapture(e.pointerId)) elLoupeImg.releasePointerCapture(e.pointerId);
}
elLoupeImg.addEventListener("pointerup", endDrag);
elLoupeImg.addEventListener("pointercancel", endDrag);

elLoupeZoomIn.addEventListener("click", () => zoomBy(1.4));
elLoupeZoomOut.addEventListener("click", () => zoomBy(1 / 1.4));
elLoupeZoomLabel.addEventListener("click", () => resetZoom());
elLoupeZoomActual.addEventListener("click", () => {
  const z = actualZoom();
  if (Math.abs(zoom - z) < 0.01) {
    resetZoom();
    return;
  }
  const r = elLoupeImg.getBoundingClientRect();
  zoomAt(r.left + r.width / 2, r.top + r.height / 2, z);
});

// 窗口一改尺寸，适应窗口的基准就变了，继续保留倍率只会算出错误的平移边界
window.addEventListener("resize", () => {
  if (!elLoupe.hidden) resetZoom();
});

/** 大图底部那行「已保留 · ★★★」，顺便把星按钮的点亮状态刷对。空着就是还没标记。 */
function paintLoupeMark(c: PairCard) {
  const bits: string[] = [];
  if (c.decision !== "none") bits.push(`已${DECISION_LABEL[c.decision]}`);
  if (c.stars > 0) bits.push("★".repeat(c.stars));
  elLoupeMark.textContent = bits.join(" · ");

  // 点亮到当前星级为止的每一颗——点亮的数字本身也是可点的调整入口
  for (const b of elLoupeStars.querySelectorAll<HTMLButtonElement>("button[data-stars]")) {
    const n = Number(b.dataset.stars);
    b.classList.toggle("is-active", n > 0 && c.stars >= n);
  }
}

async function openLoupe(index: number) {
  const c = items[index];
  if (!c) return;
  loupeIndex = index;
  elLoupe.hidden = false;
  // 换一张就是从头看：倍率、平移、高清档标记全部归零
  resetZoom();
  naturalW = 0;
  loupeSrcSize = 0;
  window.clearTimeout(detailTimer);
  elLoupePos.textContent = `${index + 1} / ${items.length}`;

  elLoupeName.textContent = baseName(c.path);
  const exif = [
    PAIR_LABEL[c.pairState],
    c.cameraModel ?? "",
    fmtExposure(c),
    c.takenAtText ?? "",
  ].filter(Boolean);
  elLoupeExif.textContent = exif.join(" · ");
  paintLoupeMark(c);
  updateCullInfo();

  elLoupeImg.removeAttribute("src");
  elLoupeImg.alt = stem(c.path);
  elLoupeImg.classList.add("is-loading");

  try {
    const t = await loadThumb(c.id, 1600);
    if (loupeIndex !== index) return; // 期间已经翻页了
    loupeSrcSize = t.size;
    elLoupeImg.src = t.dataUrl;
    elLoupeImg.title = `${t.sourceWidth}×${t.sourceHeight} 像素 · 来源：${
      ROUTE_LABEL[t.route] ?? t.route
    }`;
  } catch (e) {
    if (loupeIndex !== index) return;
    elLoupeExif.textContent = `预览提取失败：${String(e)}`;
  } finally {
    if (loupeIndex === index) elLoupeImg.classList.remove("is-loading");
  }
}

function closeLoupe() {
  loupeIndex = -1;
  elLoupe.hidden = true;
  elLoupeImg.removeAttribute("src");
  loupeSrcSize = 0;
  window.clearTimeout(detailTimer);
  resetZoom();
  updateCullInfo();
}

function stepLoupe(delta: number) {
  if (loupeIndex < 0) return;
  const next = loupeIndex + delta;
  if (next < 0 || next >= items.length) return;
  void openLoupe(next);
}

elLoupeClose.addEventListener("click", closeLoupe);
elLoupePrev.addEventListener("click", () => stepLoupe(-1));
elLoupeNext.addEventListener("click", () => stepLoupe(1));
elLoupe.addEventListener("click", (e) => {
  if (e.target === elLoupe) closeLoupe();
});

/** 正在输入框里打字时，字母键不该被当成快捷键抢走。 */
function typingInField(e: KeyboardEvent): boolean {
  const t = e.target as HTMLElement | null;
  if (!t) return false;
  return (
    t.tagName === "INPUT" ||
    t.tagName === "TEXTAREA" ||
    t.tagName === "SELECT" ||
    t.isContentEditable
  );
}

/** 焦点在按钮上时，回车/空格是「按这个按钮」，不该被我们抢去开大图。 */
function onWidget(e: KeyboardEvent): boolean {
  const t = e.target as HTMLElement | null;
  return !!t && (t.tagName === "BUTTON" || t.tagName === "A");
}

/** 网格当前是几列。方向键要按视觉上的上下左右移动，就得知道这个。 */
function gridColumns(): number {
  const tracks = getComputedStyle(elGrid).gridTemplateColumns;
  return Math.max(1, tracks.split(" ").filter(Boolean).length);
}

/** 方向键挪选中。没选中任何东西时，从第一张开始。 */
function moveSelection(delta: number) {
  if (items.length === 0) return;
  const current = items.findIndex((c) => selection.has(c.id));
  const next = current < 0 ? 0 : Math.min(items.length - 1, Math.max(0, current + delta));
  selectOnly(items[next].id);
  revealCard(items[next].id);
}

window.addEventListener("keydown", (e) => {
  const meta = e.metaKey || e.ctrlKey;
  const key = e.key;

  // 导出对话框打开时，它才是当前的焦点
  if (!elExportModal.hidden) {
    if (key === "Escape") {
      e.preventDefault();
      closeExportDialog();
    }
    return;
  }

  // 清理对话框同理
  if (!elCacheModal.hidden) {
    if (key === "Escape") {
      e.preventDefault();
      closeCacheDialog();
    }
    return;
  }

  // ⌘A / Ctrl+A 全选。在输入框里就还给浏览器（选中文字）。
  if (meta && key.toLowerCase() === "a") {
    if (typingInField(e)) return;
    e.preventDefault();
    void selectAll();
    return;
  }

  if (typingInField(e)) return;

  // ---- 撤销：⌘Z / Ctrl+Z ----
  // 放在输入框判断之后，输入文字时把 ⌘Z 还给浏览器自己的撤销
  if (meta && !e.shiftKey && key.toLowerCase() === "z") {
    e.preventDefault();
    void undoLast();
    return;
  }

  // ---- 大图 ----
  if (!elLoupe.hidden) {
    if (key === "Escape") {
      e.preventDefault();
      closeLoupe();
      return;
    }
    if (key === "ArrowRight") {
      e.preventDefault();
      stepLoupe(1);
      return;
    }
    if (key === "ArrowLeft") {
      e.preventDefault();
      stepLoupe(-1);
      return;
    }

    // 缩放：+ 放大、- 缩小、F 复位、A 到 1:1。
    // 0 已经被「清除星级」占了，所以复位不用 0。
    const lower = key.toLowerCase();
    if (key === "+" || key === "=") {
      e.preventDefault();
      zoomBy(1.4);
      return;
    }
    if (key === "-" || key === "_") {
      e.preventDefault();
      zoomBy(1 / 1.4);
      return;
    }
    if (lower === "f") {
      e.preventDefault();
      resetZoom();
      return;
    }
    if (lower === "a") {
      e.preventDefault();
      elLoupeZoomActual.click();
      return;
    }

    // 标记完自动跳下一张——这就是选片的手感：一个键处理一张，手不离开键盘
    if (lower === "p") {
      e.preventDefault();
      applyDecision("keep");
      return;
    }
    if (lower === "x") {
      e.preventDefault();
      applyDecision("reject");
      return;
    }
    if (lower === "u") {
      e.preventDefault();
      applyDecision("none");
      return;
    }
    if (/^[0-5]$/.test(key)) {
      e.preventDefault();
      applyStars(Number(key));
    }
    return;
  }

  // ---- 网格 ----
  if (key === "Escape") {
    clearSelection();
    return;
  }

  if (key === "Enter" || key === " ") {
    if (onWidget(e)) return;
    e.preventDefault();
    const idx = items.findIndex((c) => selection.has(c.id));
    if (idx >= 0) void openLoupe(idx);
    else if (items.length > 0) selectOnly(items[0].id);
    return;
  }

  if (key === "ArrowRight" || key === "ArrowLeft" || key === "ArrowDown" || key === "ArrowUp") {
    e.preventDefault();
    if (key === "ArrowRight") moveSelection(1);
    else if (key === "ArrowLeft") moveSelection(-1);
    else if (key === "ArrowDown") moveSelection(gridColumns());
    else moveSelection(-gridColumns());
    return;
  }

  if (key === "/") {
    e.preventDefault();
    elSearch.focus();
    return;
  }

  const lower = key.toLowerCase();
  if (lower === "p") {
    e.preventDefault();
    applyDecision("keep");
    return;
  }
  if (lower === "x") {
    e.preventDefault();
    applyDecision("reject");
    return;
  }
  if (lower === "u") {
    e.preventDefault();
    applyDecision("none");
    return;
  }
  if (/^[0-5]$/.test(key)) {
    e.preventDefault();
    applyStars(Number(key));
  }
});

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

async function boot() {
  applyTheme(savedTheme());

  const savedDensity = localStorage.getItem(DEN_KEY);
  if (savedDensity) {
    if (savedDensity !== "normal") elGrid.dataset.density = savedDensity;
    for (const b of elDensity.querySelectorAll<HTMLButtonElement>("button")) {
      b.classList.toggle("is-active", b.dataset.density === savedDensity);
    }
  }

  // 同步分组方式的初始状态
  elGroupMode.value = groupMode;
  updateUndoBtn();

  await listen<ScanProgress>("scan://progress", (e) => showProgress(e.payload));
  await listen<ExportProgress>("export://progress", (e) => showExportProgress(e.payload));
  await showStartupError();

  updateCullInfo();

  // 恢复上次的文件夹，先把已有索引铺出来（不等待扫描）
  const savedRoot = localStorage.getItem(ROOT_KEY);
  if (savedRoot) {
    rootPath = savedRoot;
    renderRoot();
    elRescan.disabled = false;
    setHint("正在载入图库…");
  }

  await refreshLibrary();

  if (savedRoot) {
    // 图库铺好之后再增量重扫：文件没变的话是纯 stat，秒级完成，
    // 没有这一步就不会发现「刚拷进来的一批新照片」。
    void startScan(savedRoot, { quiet: true });
  } else if (total === 0) {
    setHint("选择一个装有 NEF / JPG 的文件夹，选完会自动扫描并出图。");
  }
}

window.addEventListener("DOMContentLoaded", () => {
  void boot().catch((e) => {
    setHint(`初始化失败：${String(e)}`, "error");
  });
});
