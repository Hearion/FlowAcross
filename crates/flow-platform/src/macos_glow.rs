//! macOS：屏幕边缘浮光。
//!
//! 一个贴着屏幕边的透明窗口：不激活应用、不成为键盘焦点、鼠标事件穿透、盖在全屏应用和菜单栏之上，
//! 里面是两层 Core Animation 渐变（形状和节奏见 [`crate::glow`]）。窗口只建一次，之后复用；
//! 动画播完后收起，不在屏幕上留一个看不见的窗口。只能在主线程使用。

use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use flow_core::geometry::Edge;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSScreen, NSScreenSaverWindowLevel, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{NSArray, NSNumber, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{
    CABasicAnimation, CAGradientLayer, CAKeyframeAnimation, CALayer, CAMediaTiming,
    CAMediaTimingFunction, CATransaction, kCAGradientLayerRadial, kCAMediaTimingFunctionEaseOut,
};

use crate::glow;

struct Overlay {
    window: Retained<NSWindow>,
    /// 两层渐变的容器，整体淡入淡出。
    glow: Retained<CALayer>,
    /// 整条边上的浅光带。
    band: Retained<CAGradientLayer>,
    /// 进入点处更亮的一团。
    spot: Retained<CAGradientLayer>,
}

thread_local! {
    static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) };
    /// 每次显示加一；延迟收起时对不上就说明又显示了新的一次，不收。
    static GENERATION: Cell<usize> = const { Cell::new(0) };
}

unsafe extern "C" {
    /// `dispatch_get_main_queue()` 在 C 里是取这个全局变量地址的宏。
    static _dispatch_main_q: c_void;
    fn dispatch_time(when: u64, delta: i64) -> u64;
    fn dispatch_after_f(
        when: u64,
        queue: *const c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

/// 一组不透明度不同的品牌蓝，给渐变图层用。
fn colors(alphas: &[f64]) -> Retained<NSArray> {
    let (r, g, b) = glow::COLOR;
    let list: Vec<_> = alphas
        .iter()
        .map(|a| {
            NSColor::colorWithSRGBRed_green_blue_alpha(
                r as f64 / 255.0,
                g as f64 / 255.0,
                b as f64 / 255.0,
                *a,
            )
            .CGColor()
        })
        .collect();
    let objects: Vec<&AnyObject> = list
        .iter()
        // SAFETY: CGColor 是 Core Foundation 对象，可以当作 Objective-C 对象放进 NSArray
        // （CAGradientLayer.colors 要的就是 CGColor 数组）；`list` 在数组建好之前一直有效。
        .map(|c| unsafe { &*(Retained::as_ptr(c) as *const AnyObject) })
        .collect();
    NSArray::from_slice(&objects)
}

fn numbers(values: &[f64]) -> Retained<NSArray<NSNumber>> {
    let list: Vec<_> = values.iter().map(|v| NSNumber::new_f64(*v)).collect();
    NSArray::from_retained_slice(&list)
}

fn build(mtm: MainThreadMarker, frame: NSRect) -> Option<Overlay> {
    // SAFETY: 在主线程创建无边框窗口；下面关掉"关闭时释放"，由 Retained 管理生命周期。
    let window = unsafe {
        let w = NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        );
        w.setReleasedWhenClosed(false);
        w
    };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setHasShadow(false);
    window.setIgnoresMouseEvents(true);
    // 盖在全屏应用、菜单栏和程序坞之上。
    window.setLevel(NSScreenSaverWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    let view = window.contentView()?;
    view.setWantsLayer(true);
    let root = view.layer()?;
    let glow = CALayer::new();
    // 平时完全透明，只有动画期间可见。
    glow.setOpacity(0.0);
    let band = CAGradientLayer::new();
    let spot = CAGradientLayer::new();
    // SAFETY: Core Animation 导出的常量。
    spot.setType(unsafe { kCAGradientLayerRadial });
    glow.addSublayer(&band);
    glow.addSublayer(&spot);
    root.addSublayer(&glow);
    Some(Overlay {
        window,
        glow,
        band,
        spot,
    })
}

extern "C" fn hide(context: *mut c_void) {
    // 在主队列上执行，也就是主线程。
    if GENERATION.with(Cell::get) != context as usize {
        return;
    }
    OVERLAY.with(|o| {
        if let Some(o) = o.borrow().as_ref() {
            o.window.orderOut(None);
        }
    });
}

/// 在 `display_local_id` 这块屏幕的 `edge` 边上闪一道浮光；`along` 是进入点在这条边上的位置
/// （0..1，左右边从上往下，上下边从左往右）。不在主线程或找不到这块屏幕时什么也不做。
pub fn show(display_local_id: &str, edge: Edge, along: f64) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Ok(id) = display_local_id.parse::<u32>() else {
        return;
    };
    let key = NSString::from_str("NSScreenNumber");
    let Some(screen) = NSScreen::screens(mtm).iter().find(|s| {
        s.deviceDescription()
            .objectForKey(&key)
            .and_then(|o| o.downcast::<NSNumber>().ok())
            .is_some_and(|n| n.unsignedIntValue() == id)
    }) else {
        return;
    };
    // AppKit 的屏幕坐标原点在主屏左下角，y 向上。
    let f = screen.frame();
    let depth = glow::DEPTH.min(f.size.width).min(f.size.height);
    let vertical = edge.is_vertical();
    let frame = match edge {
        Edge::Left => NSRect::new(f.origin, NSSize::new(depth, f.size.height)),
        Edge::Right => NSRect::new(
            NSPoint::new(f.origin.x + f.size.width - depth, f.origin.y),
            NSSize::new(depth, f.size.height),
        ),
        Edge::Top => NSRect::new(
            NSPoint::new(f.origin.x, f.origin.y + f.size.height - depth),
            NSSize::new(f.size.width, depth),
        ),
        Edge::Bottom => NSRect::new(f.origin, NSSize::new(f.size.width, depth)),
    };
    let size = frame.size;
    let length = if vertical { size.height } else { size.width };
    let along = along.clamp(0.0, 1.0);
    let radius = glow::spot_radius(1.0, length);

    OVERLAY.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = build(mtm, frame);
        }
        let Some(o) = slot.as_ref() else {
            return;
        };
        o.window.setFrame_display(frame, false);

        // 图层坐标原点在左下角。光带从贴着屏幕边的一侧向里淡出；那团光的中心就在屏幕边上的进入点，
        // 图层有一半伸到窗口外面，被窗口裁掉。
        let unit = |x: f64, y: f64| NSPoint::new(x, y);
        let (start, end, center) = match edge {
            Edge::Left => (
                unit(0.0, 0.5),
                unit(1.0, 0.5),
                NSPoint::new(0.0, (1.0 - along) * length),
            ),
            Edge::Right => (
                unit(1.0, 0.5),
                unit(0.0, 0.5),
                NSPoint::new(depth, (1.0 - along) * length),
            ),
            Edge::Top => (
                unit(0.5, 1.0),
                unit(0.5, 0.0),
                NSPoint::new(along * length, depth),
            ),
            Edge::Bottom => (
                unit(0.5, 0.0),
                unit(0.5, 1.0),
                NSPoint::new(along * length, 0.0),
            ),
        };
        let spot_size = if vertical {
            NSSize::new(depth * 2.0, radius * 2.0)
        } else {
            NSSize::new(radius * 2.0, depth * 2.0)
        };

        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let bounds = NSRect::new(NSPoint::new(0.0, 0.0), size);
        o.glow.setFrame(bounds);
        o.band.setFrame(bounds);
        o.band.setStartPoint(start);
        o.band.setEndPoint(end);
        // SAFETY: 数组里都是 CGColor。
        unsafe {
            o.band.setColors(Some(&colors(&[
                glow::BAND_ALPHA,
                glow::BAND_ALPHA * 0.3,
                0.0,
            ])));
        }
        o.band.setLocations(Some(&numbers(&[0.0, 0.3, 1.0])));
        o.spot.setFrame(NSRect::new(
            NSPoint::new(
                center.x - spot_size.width / 2.0,
                center.y - spot_size.height / 2.0,
            ),
            spot_size,
        ));
        // 径向渐变：从图层中心到四边，正好是一个贴着图层边的椭圆。
        o.spot.setStartPoint(unit(0.5, 0.5));
        o.spot.setEndPoint(unit(1.0, 1.0));
        // SAFETY: 同上。
        unsafe {
            o.spot.setColors(Some(&colors(&[
                glow::SPOT_ALPHA,
                glow::SPOT_ALPHA * 0.35,
                0.0,
            ])));
        }
        o.spot.setLocations(Some(&numbers(&[0.0, 0.45, 1.0])));
        o.glow.removeAllAnimations();
        o.spot.removeAllAnimations();
        CATransaction::commit();

        // 整体：很快亮起，停一下，慢慢淡出。播完后回到图层本身的不透明度 0。
        let fade = CAKeyframeAnimation::animationWithKeyPath(Some(&NSString::from_str("opacity")));
        let (times, values): (Vec<f64>, Vec<f64>) = glow::FADE.iter().copied().unzip();
        // SAFETY: 不透明度的关键帧是数字。
        unsafe {
            let list: Vec<_> = values.iter().map(|v| NSNumber::new_f64(*v)).collect();
            let objects: Vec<&AnyObject> = list.iter().map(|n| n.as_ref()).collect();
            fade.setValues(Some(&NSArray::from_slice(&objects)));
        }
        fade.setKeyTimes(Some(&numbers(&times)));
        fade.setDuration(glow::DURATION);
        o.glow
            .addAnimation_forKey(&fade, Some(&NSString::from_str("fade")));

        // 那团光沿着边向两侧散开。系统打开了「减少动态效果」时只淡入淡出。
        if !NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion() {
            let path = if vertical {
                "transform.scale.y"
            } else {
                "transform.scale.x"
            };
            let spread = CABasicAnimation::animationWithKeyPath(Some(&NSString::from_str(path)));
            // SAFETY: 缩放的起止值是数字；时间函数名是 Core Animation 导出的常量。
            unsafe {
                spread.setFromValue(Some(&NSNumber::new_f64(glow::SPREAD_FROM)));
                spread.setToValue(Some(&NSNumber::new_f64(glow::SPREAD_TO)));
                spread.setTimingFunction(Some(&CAMediaTimingFunction::functionWithName(
                    kCAMediaTimingFunctionEaseOut,
                )));
            }
            spread.setDuration(glow::DURATION);
            o.spot
                .addAnimation_forKey(&spread, Some(&NSString::from_str("spread")));
        }

        // 只排到最前面，不激活流界、不抢键盘焦点。
        o.window.orderFrontRegardless();
    });

    let generation = GENERATION.with(|g| {
        g.set(g.get().wrapping_add(1));
        g.get()
    });
    // 播完后稍等一下再收起窗口。
    let delay_ns = ((glow::DURATION + 0.1) * 1e9) as i64;
    // SAFETY: 主队列是进程级的全局对象；上下文只是一个数字，不是指针。
    unsafe {
        dispatch_after_f(
            dispatch_time(0, delay_ns),
            &raw const _dispatch_main_q,
            generation as *mut c_void,
            hide,
        );
    }
}
