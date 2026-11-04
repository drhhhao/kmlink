//! desktop 线程：一个隐藏的顶层窗口（WS_EX_TOOLWINDOW，不显示）+ 消息循环。负责：
//!
//! 1. 剪贴板（clip.rs）：AddClipboardFormatListener；WM_CLIPBOARDUPDATE 时若 GetClipboardSequenceNumber()
//!    等于我们自己最后一次写入后的序号 → 忽略；否则 100ms 防抖（SetTimer）后读取：
//!    文字（≤ max_text_kb）→ 与上次发出/收到的文字相同则不发 → hub.send(Msg::ClipText)；
//!    图片（cfg.clipboard.images 且没有文字时；CF_DIBV5 优先，其次 CF_DIB；≤ max_image_mb）→
//!    在另一个工作线程里 miniz_oxide 压缩（level 1）后 hub.to_net(Out::ClipImage{..})（压缩不能在本线程做，会卡消息循环）。
//!    没有连接或 cfg.clipboard.enabled = false 时什么都不做。
//!    收到 ToDesktop::ClipText / ClipImage → 写入剪贴板（clip.rs），记下写入后的序号。
//! 2. 托盘（tray-icon crate）：图标在代码里生成（32x32 RGBA：两个错开的圆角方块，连接时青色、断开时灰色）。
//!    提示文字：“kmlink · 已连接 <对方名字>” / “kmlink · 等待连接”。菜单：
//!      状态行（禁用项）、“屏幕排布与连接设置…”（启动 `<当前exe> settings`）、“暂停共享”（勾选项，hub.to_input(Pause)）、
//!      “开机自启”（勾选项，写/删 HKCU\Software\Microsoft\Windows\CurrentVersion\Run 的 "kmlink" 值，并存 cfg.autostart）、
//!      “打开配置文件夹”（ShellExecuteW explorer config::dir()）、“退出”（hub.to_input(Quit)，然后结束本线程）。
//!    菜单事件用 tray_icon::menu::MenuEvent::set_event_handler 转成本线程消息处理。
//! 3. 显示器变化：WM_DISPLAYCHANGE（和 WM_SETTINGCHANGE 的 SPI_SETWORKAREA）→ 500ms 防抖后 sys::monitors()，
//!    hub.to_input(LocalDesk) 并 hub.send(Msg::Hello{..}) 让对方更新。
//! 4. 配置文件变化：每 1 秒看 config.toml 的修改时间；变了就 Config::load()，若成功：
//!    写回 hub.cfg，hub.to_input(Config(..))；若排布或 rev 变了 → hub.send(Msg::Layout{.., rev})。
//! 5. ToDesktop::HideCursor / ShowCursor → cursor::hide / show。Status → 刷新托盘提示与图标。Quit → 退出循环。
//! 6. first_run = true 时（刚生成了配置）：启动后直接打开设置窗口（`<当前exe> settings`）。

use crate::hub::{Hub, ToDesktop};
use crossbeam_channel::Receiver;
use std::sync::Arc;

pub fn spawn(hub: Arc<Hub>, rx: Receiver<ToDesktop>, first_run: bool) {
    let _ = (hub, rx, first_run);
    todo!("desktop::spawn")
}
