// ---------------------------------------------------------------------------
// 大图缩放的数学
//
// 这一层刻意不碰 DOM：缩放最容易出错的从来不是事件绑定，而是「以鼠标为锚点」
// 和「别把图拖出视野」这两处换算。抽成纯数字进出的函数，就能真的测。
//
// 坐标约定（全部是屏幕像素）：
// - box：图片「适应窗口」时的中心与尺寸，即 zoom=1、pan=0 时的样子
// - pan：图片整体被平移了多少；zoom 之后图片中心在 box 中心 + pan
// - 图片上一点（相对 box 中心的偏移 u）在屏幕上的位置 = box.c + pan + u * zoom
// ---------------------------------------------------------------------------

export interface Point {
  x: number;
  y: number;
}

export interface Box {
  cx: number;
  cy: number;
  w: number;
  h: number;
}

export interface Viewport {
  w: number;
  h: number;
}

export function clampZoom(z: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, z));
}

/**
 * 把平移限制在「图边刚好贴住视口边」之内。
 *
 * 图比视口大时，两个边界是 lo < hi，夹在中间即可；图比视口小时两者反过来，
 * 取 min/max 交换一下就退化成「图心最多挪到视口边」——两种情况一个公式。
 */
export function clampPan(pan: Point, zoom: number, box: Box, view: Viewport): Point {
  if (zoom <= 1) return { x: 0, y: 0 };

  const halfW = (box.w * zoom) / 2;
  const halfH = (box.h * zoom) / 2;

  const loX = view.w - box.cx - halfW;
  const hiX = halfW - box.cx;
  const loY = view.h - box.cy - halfH;
  const hiY = halfH - box.cy;

  return {
    x: Math.min(Math.max(pan.x, Math.min(loX, hiX)), Math.max(loX, hiX)),
    y: Math.min(Math.max(pan.y, Math.min(loY, hiY)), Math.max(loY, hiY)),
  };
}

/**
 * 缩放后新的平移量，使得锚点（鼠标所在处）底下的那一点纹丝不动。
 *
 * 推导：锚点对应的图上偏移 u = (anchor - box.c - pan) / zoom，
 * 缩放后仍要落在 anchor 上，于是 pan' = anchor - box.c - u * zoom'。
 */
export function panForZoomAt(
  anchor: Point,
  box: Box,
  pan: Point,
  zoom: number,
  next: number,
): Point {
  const ux = (anchor.x - box.cx - pan.x) / zoom;
  const uy = (anchor.y - box.cy - pan.y) / zoom;
  return {
    x: anchor.x - box.cx - ux * next,
    y: anchor.y - box.cy - uy * next,
  };
}

/**
 * 「一个图像素占一个屏幕像素」的倍率。
 *
 * fitWidth 用布局宽度（offsetWidth），它不受 transform 影响，
 * 所以放大之后再问 1:1 是多少倍，答案还是对的。
 */
export function actualZoom(naturalWidth: number, fitWidth: number, max: number): number {
  if (!naturalWidth || !fitWidth) return 1;
  return clampZoom(naturalWidth / fitWidth, 1, max);
}
