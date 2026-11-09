import { useLayoutEffect, useMemo, useRef, useState, type PointerEvent } from "react";
import type { DeviceView, Display, Layout } from "./types";
import { CursorIcon, OsIcon, hueOf, resolution } from "./ui";

interface Props {
  layout: Layout;
  devices: DeviceView[];
  selected: string | null;
  onSelect: (device: string | null) => void;
  /** 拖动结束。位移与吸附距离的单位都是布局空间逻辑点。 */
  onDrop: (device: string, dx: number, dy: number, snap: number) => void;
  /** 底部预留的像素高度（给空状态提示等浮层留位置，屏幕不会被盖住）。 */
  reserveBottom?: number;
}

interface Drag {
  device: string;
  pointerId: number;
  startX: number;
  startY: number;
  dx: number;
  dy: number;
}

/** 两台不同设备的屏幕相贴的一段边：鼠标可以从这里跨过去。布局空间坐标。 */
interface Link {
  key: string;
  vertical: boolean;
  x: number;
  y: number;
  length: number;
}

const PADDING = 40;
/** 屏幕之间留出的缝（像素，每边一半），发光的接缝就画在缝里。 */
const GAP = 16;
/** 接缝的粗细（像素），与 styles.css 的 .link 一致。 */
const SEAM = 6;
const SNAP_PX = 16;
const MAX_SCALE = 0.24;
/** 与 flow-core 的 ADJACENCY_TOLERANCE 保持一致。 */
const TOUCH = 1;

function bounds(displays: Display[]) {
  const xs = displays.flatMap((d) => [d.rect.x, d.rect.x + d.rect.width]);
  const ys = displays.flatMap((d) => [d.rect.y, d.rect.y + d.rect.height]);
  return {
    x: Math.min(...xs),
    y: Math.min(...ys),
    w: Math.max(...xs) - Math.min(...xs),
    h: Math.max(...ys) - Math.min(...ys),
  };
}

function overlap(a0: number, a1: number, b0: number, b1: number) {
  return [Math.max(a0, b0), Math.min(a1, b1)] as const;
}

function findLinks(displays: Display[]): Link[] {
  const out: Link[] = [];
  for (let i = 0; i < displays.length; i++) {
    for (let j = i + 1; j < displays.length; j++) {
      const a = displays[i].rect;
      const b = displays[j].rect;
      if (displays[i].id.device === displays[j].id.device) continue;
      const key = `${i}-${j}`;
      for (const [l, r] of [
        [a, b],
        [b, a],
      ]) {
        if (Math.abs(l.x + l.width - r.x) <= TOUCH) {
          const [y0, y1] = overlap(l.y, l.y + l.height, r.y, r.y + r.height);
          if (y1 > y0) out.push({ key: `${key}v${out.length}`, vertical: true, x: r.x, y: y0, length: y1 - y0 });
        }
        if (Math.abs(l.y + l.height - r.y) <= TOUCH) {
          const [x0, x1] = overlap(l.x, l.x + l.width, r.x, r.x + r.width);
          if (x1 > x0) out.push({ key: `${key}h${out.length}`, vertical: false, x: x0, y: r.y, length: x1 - x0 });
        }
      }
    }
  }
  return out;
}

/** 每台设备选一块"主"屏幕来显示设备名：面积最大的那块。 */
function leadDisplays(displays: Display[]): Set<Display> {
  const best = new Map<string, Display>();
  for (const d of displays) {
    const cur = best.get(d.id.device);
    if (!cur || d.rect.width * d.rect.height > cur.rect.width * cur.rect.height) best.set(d.id.device, d);
  }
  return new Set(best.values());
}

const STATUS_CHIP: Record<DeviceView["status"], string> = {
  local: "本机",
  connected: "在线",
  connecting: "连接中",
  offline: "离线",
};

export function LayoutMap({ layout, devices, selected, onSelect, onDrop, reserveBottom = 0 }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ w: 800, h: 500 });
  const [drag, setDrag] = useState<Drag | null>(null);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const ro = new ResizeObserver(([e]) => setSize({ w: e.contentRect.width, h: e.contentRect.height }));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // 布局空间 → 画布像素。拖动期间布局不变，比例也不变，不会抖动。
  const view = useMemo(() => {
    if (layout.displays.length === 0) return { s: 1, ox: 0, oy: 0 };
    const b = bounds(layout.displays);
    const h = size.h - reserveBottom;
    const s = Math.min((size.w - PADDING * 2) / b.w, (h - PADDING * 2) / b.h, MAX_SCALE);
    return { s, ox: (size.w - b.w * s) / 2 - b.x * s, oy: (h - b.h * s) / 2 - b.y * s };
  }, [layout, size, reserveBottom]);

  const links = useMemo(() => findLinks(layout.displays), [layout]);
  const leads = useMemo(() => leadDisplays(layout.displays), [layout]);
  const deviceCount = useMemo(() => new Set(layout.displays.map((d) => d.id.device)).size, [layout]);
  const deviceById = new Map(devices.map((d) => [d.id, d]));

  const onPointerDown = (e: PointerEvent, device: string) => {
    e.stopPropagation();
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    setDrag({ device, pointerId: e.pointerId, startX: e.clientX, startY: e.clientY, dx: 0, dy: 0 });
  };
  const onPointerMove = (e: PointerEvent) => {
    if (!drag || e.pointerId !== drag.pointerId) return;
    setDrag({ ...drag, dx: e.clientX - drag.startX, dy: e.clientY - drag.startY });
  };
  const onPointerUp = (e: PointerEvent) => {
    if (!drag || e.pointerId !== drag.pointerId) return;
    const moved = Math.abs(drag.dx) + Math.abs(drag.dy) > 3;
    // 吸附距离按布局图上的像素算（约 16 像素），不受缩放影响；屏幕间显示的缝隙也算进去。
    if (moved) onDrop(drag.device, drag.dx / view.s, drag.dy / view.s, (SNAP_PX + GAP) / view.s);
    else onSelect(drag.device);
    setDrag(null);
  };

  return (
    <div
      className={`map ${drag ? "is-dragging" : ""}`}
      ref={ref}
      onPointerDown={() => onSelect(null)}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={() => setDrag(null)}
    >
      {layout.displays.map((d) => {
        const dev = deviceById.get(d.id.device);
        const dragging = drag?.device === d.id.device;
        const lead = leads.has(d);
        const cls = [
          "screen",
          `hue-${hueOf(devices, d.id.device)}`,
          dev?.status ?? "offline",
          dev?.active ? "active" : "",
          selected === d.id.device ? "selected" : "",
          dragging ? "dragging" : "",
        ].join(" ");
        return (
          <div
            key={`${d.id.device}/${d.id.local}`}
            className={cls}
            style={{
              left: view.ox + d.rect.x * view.s + GAP / 2,
              top: view.oy + d.rect.y * view.s + GAP / 2,
              width: d.rect.width * view.s - GAP,
              height: d.rect.height * view.s - GAP,
              transform: dragging ? `translate(${drag.dx}px, ${drag.dy}px)` : undefined,
            }}
            onPointerDown={(e) => onPointerDown(e, d.id.device)}
            title={`${dev?.name ?? ""}：${d.name}`}
          >
            <div className="screen-glass">
              {lead && dev && (
                <div className="screen-head">
                  <span className="screen-device">
                    <OsIcon os={dev.os} size={13} className="screen-os" />
                    <span className="screen-device-name">{dev.name}</span>
                  </span>
                  {dev.active ? (
                    <span className="screen-chip here">
                      <CursorIcon size={11} />
                      键鼠在这里
                    </span>
                  ) : (
                    <span className={`screen-chip ${dev.status}`}>{STATUS_CHIP[dev.status]}</span>
                  )}
                </div>
              )}
              <div className="screen-mark">{dev && <OsIcon os={dev.os} size={40} />}</div>
              <div className="screen-foot">
                <span className="screen-name">{d.name}</span>
                <span className="screen-meta">{resolution(d, dev?.os ?? "Other")}</span>
              </div>
            </div>
          </div>
        );
      })}

      {!drag &&
        links.map((l) => (
          <div
            key={l.key}
            className={`link ${l.vertical ? "v" : "h"}`}
            style={
              l.vertical
                ? { left: view.ox + l.x * view.s - SEAM / 2, top: view.oy + l.y * view.s + 10, height: Math.max(l.length * view.s - 20, 8) }
                : { top: view.oy + l.y * view.s - SEAM / 2, left: view.ox + l.x * view.s + 10, width: Math.max(l.length * view.s - 20, 8) }
            }
          />
        ))}

      {/* 只有本机时没有接缝可讲；有多台但都没相贴时，告诉用户怎么做 */}
      {deviceCount > 1 && (
        <div className="map-legend">
          {links.length > 0 ? (
            <>
              <span className="legend-link" />
              蓝色接缝处可以跨屏
              <span className="legend-sep" />
              拖动屏幕调整摆放
            </>
          ) : (
            <>
              <span className="legend-link off" />
              不同电脑的屏幕还没有相贴，把它们拖到挨在一起才能跨屏
            </>
          )}
        </div>
      )}
    </div>
  );
}
