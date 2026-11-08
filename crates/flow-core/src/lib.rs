//! 流界核心逻辑。
//!
//! 这个 crate 不做任何 IO（不碰网络、不碰系统 API），只放可以在任意平台上单测的纯逻辑：
//!
//! - [`geometry`]：点、矩形、边。
//! - [`layout`]：统一布局空间、跨边映射、拖动吸附、布局校验。
//! - [`cursor`]：在布局空间里移动虚拟光标（含缩放换算）。
//! - [`switching`]：跨设备切换判定（修饰键、停留时间、角落死区、锁定）。
//! - [`keys`]：HID 键码、修饰键、按键路由与"释放全部按键"。
//! - [`protocol`]：设备间消息与帧编解码。
//! - [`pairing`]：设备指纹与 6 位配对码。
//! - [`clipboard`]：剪贴板同步防回环。

pub mod clipboard;
pub mod cursor;
pub mod geometry;
pub mod keys;
pub mod layout;
pub mod pairing;
pub mod protocol;
pub mod switching;

pub use geometry::{Edge, Point, Rect};
pub use layout::{DeviceId, Display, DisplayId, EdgeMapping, Layout};
