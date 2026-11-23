// ---------------------------------------------------------------------------
// 快捷键注册表与持久化
//
// 早期所有键位都写死在 main.ts 的 keydown 里，别人没法按自己的习惯改。
// 现在把「动作 → 键位」抽成一张表：动作在这里登记，键位存在 localStorage，
// 运行时查表派发。改键只动这张表和用户数据，不动每个动作的处理函数。
//
// 覆盖范围（按需求只放开「核心选片键」）：选片标记、星级、色标、网格/大图导航、
// 缩放、搜索、侧栏、列数等。窗口级固定键（⌘A / ⌘Z / ⇧⌘Z / ⌘, / F / ⇧F 全屏复位）
// 不在此表内，仍写死在 main.ts，避免和浏览器/系统快捷键打架。
// ---------------------------------------------------------------------------

/** 快捷键生效的场景。网格（loupe 关闭）与大图（loupe 打开）是两套上下文，
 * 同一场景内键位不能撞，不同场景（如网格里的色标 - 与大图里的缩小 -）可以共存。 */
export type Ctx = "grid" | "loupe";

export interface ActionDef {
  id: string;
  /** 中文动作名，弹窗里直接展示。 */
  label: string;
  /** 生效场景。both = 网格与大图都生效（选片类动作基本都是）。 */
  ctx: Ctx | "both";
  /** 默认键（规范化字符串，见 normKey）。 */
  def: string;
}

// 想新增一个可改键的动作，只要在这里登记一行即可，main.ts 里的派发会自动认识它。
// 注意 ctx=both 的动作会同时出现在两个场景里，键位共享——改一处两边一起变。
export const ACTIONS: ActionDef[] = [
  // —— 选片：网格与大图通用 ——
  { id: "keep", label: "保留", ctx: "both", def: "p" },
  { id: "reject", label: "淘汰", ctx: "both", def: "x" },
  { id: "unmark", label: "清除标记", ctx: "both", def: "u" },
  { id: "star0", label: "清除星级", ctx: "both", def: "0" },
  { id: "star1", label: "1 星", ctx: "both", def: "1" },
  { id: "star2", label: "2 星", ctx: "both", def: "2" },
  { id: "star3", label: "3 星", ctx: "both", def: "3" },
  { id: "star4", label: "4 星", ctx: "both", def: "4" },
  { id: "star5", label: "5 星", ctx: "both", def: "5" },
  { id: "colorRed", label: "红标", ctx: "both", def: "6" },
  { id: "colorYellow", label: "黄标", ctx: "both", def: "7" },
  { id: "colorGreen", label: "绿标", ctx: "both", def: "8" },
  { id: "colorBlue", label: "蓝标", ctx: "both", def: "9" },
  { id: "colorPurple", label: "紫标", ctx: "both", def: "-" },

  // —— 网格导航 ——
  { id: "moveLeft", label: "左移选中", ctx: "grid", def: "arrowleft" },
  { id: "moveRight", label: "右移选中", ctx: "grid", def: "arrowright" },
  { id: "moveUp", label: "上移选中", ctx: "grid", def: "arrowup" },
  { id: "moveDown", label: "下移选中", ctx: "grid", def: "arrowdown" },
  { id: "openLoupe", label: "打开大图", ctx: "grid", def: "space" },
  { id: "searchFocus", label: "聚焦搜索", ctx: "grid", def: "/" },
  { id: "clearSel", label: "取消选中", ctx: "grid", def: "escape" },
  { id: "toggleSidebar", label: "开关侧栏", ctx: "grid", def: "\\" },
  { id: "colMinus", label: "减少列数", ctx: "grid", def: "[" },
  { id: "colPlus", label: "增加列数", ctx: "grid", def: "]" },

  // —— 大图浏览 ——
  { id: "loupePrev", label: "上一张", ctx: "loupe", def: "arrowleft" },
  { id: "loupeNext", label: "下一张", ctx: "loupe", def: "arrowright" },
  { id: "zoomIn", label: "放大", ctx: "loupe", def: "=" },
  { id: "zoomOut", label: "缩小", ctx: "loupe", def: "_" },
  { id: "zoomActual", label: "1:1 像素", ctx: "loupe", def: "a" },
  { id: "toggleAf", label: "对焦框", ctx: "loupe", def: "o" },
  { id: "toggleDetail", label: "详情面板", ctx: "loupe", def: "i" },
  { id: "histogram", label: "直方图", ctx: "loupe", def: "h" },
  { id: "rotateCw", label: "顺时针旋转 90°", ctx: "loupe", def: "r" },
  { id: "closeLoupe", label: "关闭大图", ctx: "loupe", def: "escape" },
];

const STORE_KEY = "sp:shortcuts";

/** id → 规范化键。缺省时回退到注册表里的默认键。 */
let bindings: Record<string, string> = load();

function load(): Record<string, string> {
  try {
    const raw = localStorage.getItem(STORE_KEY);
    if (!raw) return {};
    const obj = JSON.parse(raw);
    return obj && typeof obj === "object" ? (obj as Record<string, string>) : {};
  } catch {
    return {};
  }
}

function save() {
  try {
    localStorage.setItem(STORE_KEY, JSON.stringify(bindings));
  } catch {
    /* 隐私模式 / 配额满了：改键这次运行有效，刷新后回到默认，不致命 */
  }
}

/** 注册表里的默认键（规范化字符串）。 */
export function defaultOf(id: string): string {
  return ACTIONS.find((a) => a.id === id)?.def ?? "";
}

/** 一个动作当前的键位（规范化字符串）。 */
export function bindingOf(id: string): string {
  return bindings[id] || defaultOf(id);
}

/** 写入一个新键位并落盘。 */
export function setBinding(id: string, norm: string) {
  bindings[id] = norm;
  save();
}

/** 全部恢复默认（清空本地覆盖）。 */
export function resetBindings() {
  bindings = {};
  try {
    localStorage.removeItem(STORE_KEY);
  } catch {
    /* 同上 */
  }
}

const MOD_SYMBOL: Record<string, string> = {
  meta: "⌘",
  ctrl: "⌃",
  alt: "⌥",
  shift: "⇧",
};

/** 把 KeyboardEvent 压成稳定的规范化键串，例如 "shift+f" / "meta+a" / "arrowleft" / "-"。 */
export function normKey(e: KeyboardEvent): string {
  const mods: string[] = [];
  if (e.metaKey) mods.push("meta");
  if (e.ctrlKey) mods.push("ctrl");
  if (e.altKey) mods.push("alt");

  const k = keyPart(e);
  // 字母与命名键（Arrow* 等）把 shift 当成独立修饰符记；数字和符号（= + - / 等）
  // shift 已经编码进字符本身（"+"、"_"），不要再重复记，否则同一物理键会出两个串。
  const shiftable = /^[a-z]$/.test(k) || k.length > 1;
  if (e.shiftKey && shiftable) mods.push("shift");

  return [...mods, k].join("+");
}

function keyPart(e: KeyboardEvent): string {
  const k = e.key;
  if (k === " ") return "space";
  if (k === "Enter") return "space"; // 回车与空格同义：都算「打开大图」
  if (k === "+") return "="; // + 与 = 同键（shift+=），统一记成 =
  // 注意：_ 与 - 不归并——v0.5.0 里「-」是紫标、「_」(shift+减号) 是缩小，
  // 两个键有意不同，归并会让紫标和缩放在大图里撞键。
  if (k.length === 1) {
    // 单字符：字母统一用小写（大小写由 shift 段表达），数字/符号保留原样
    if (/^[a-z]$/i.test(k)) return k.toLowerCase();
    return k;
  }
  // 命名键 ArrowLeft → arrowleft、Escape…
  return k.toLowerCase();
}

/** 把规范化键串变成给人看的文案：⌘A、⇧F、←、Esc、空格。 */
export function prettyKey(norm: string): string {
  const parts = norm.split("+");
  const k = parts.pop() ?? "";
  const sym = parts.map((m) => MOD_SYMBOL[m] ?? m).join("");
  return sym + prettyKeyPart(k);
}

const ARROW_GLYPH: Record<string, string> = {
  arrowleft: "←",
  arrowright: "→",
  arrowup: "↑",
  arrowdown: "↓",
};

function prettyKeyPart(k: string): string {
  if (k === "space") return "空格";
  if (k === "enter") return "⏎";
  if (k === "escape") return "Esc";
  if (ARROW_GLYPH[k]) return ARROW_GLYPH[k];
  if (k.length === 1) return k.toUpperCase();
  return k;
}

/** 同一场景里两个动作绑了同一个键 → 这些动作的 id 都进集合。
 *  跨场景同名不冲突（网格里的色标 - 与大图里的缩小 - 可以并存）。 */
export function conflictingIds(): Set<string> {
  const bad = new Set<string>();
  const groups = new Map<string, string[]>();
  for (const a of ACTIONS) {
    const key = bindingOf(a.id);
    const ctxs: Ctx[] = a.ctx === "both" ? ["grid", "loupe"] : [a.ctx];
    for (const c of ctxs) {
      const gk = `${c}|${key}`;
      const arr = groups.get(gk);
      if (arr) arr.push(a.id);
      else groups.set(gk, [a.id]);
    }
  }
  for (const arr of groups.values()) {
    if (arr.length > 1) arr.forEach((id) => bad.add(id));
  }
  return bad;
}
