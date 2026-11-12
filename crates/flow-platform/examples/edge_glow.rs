//! 预览屏幕边缘浮光：在主显示器的左、上、右、下四条边上依次各闪一次，不需要第二台电脑。
//!
//! ```text
//! cargo run -p flow-platform --example edge_glow
//! ```

use std::time::Duration;

use flow_core::geometry::Edge;

/// 等一会儿，让动画播完。macOS 的窗口和动画靠主线程的事件循环驱动，所以这里转事件循环而不是睡眠。
fn wait(d: Duration) {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSDate, NSRunLoop};
        NSRunLoop::mainRunLoop()
            .runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(d.as_secs_f64()));
    }
    #[cfg(not(target_os = "macos"))]
    std::thread::sleep(d);
}

fn main() {
    // macOS：先连上窗口服务器，否则窗口显示不出来。
    #[cfg(target_os = "macos")]
    {
        let mtm = objc2::MainThreadMarker::new().expect("要在主线程运行");
        objc2_app_kit::NSApplication::sharedApplication(mtm);
    }
    let displays = flow_platform::display_provider()
        .displays()
        .expect("无法枚举显示器");
    let display = displays
        .iter()
        .find(|d| d.primary)
        .or(displays.first())
        .expect("没有显示器");
    println!("在「{}」上预览浮光", display.name);
    for (edge, along) in [
        (Edge::Left, 0.3),
        (Edge::Top, 0.5),
        (Edge::Right, 0.7),
        (Edge::Bottom, 0.5),
    ] {
        println!("{edge:?}");
        flow_platform::edge_glow(&display.local_id, edge, along);
        wait(Duration::from_millis(1200));
    }
}
