//! 控制端：低级键鼠钩子 + 跨屏状态机。跑在主线程，自己的消息循环（GetMessageW）。
//!
//! 启动：hub.set_input_tid(GetCurrentThreadId())；SetWindowsHookExW(WH_MOUSE_LL / WH_KEYBOARD_LL)；
//! SetThreadPriority(THREAD_PRIORITY_TIME_CRITICAL)；本机显示器 = sys::monitors()。
//! 状态放在 thread_local 的 RefCell 里（钩子回调是 extern "system" fn，没有上下文指针）。
//! 回调里绝不阻塞：不打日志、不做文件/网络 IO（发消息用 hub.to_net / hub.to_desktop，都是无界 channel）。
//! 回调超过 LowLevelHooksTimeout 会被系统摘掉钩子：消息循环每 2 秒用 SetTimer(线程定时器) 检查
//! GetLastInputInfo 比最后一次钩子回调新 1 秒以上就重装钩子（看门狗）。
//!
//! 模式：
//!   Home：本机在用。鼠标移动时（没有按着鼠标键、未暂停、对方已连接、对方显示器已知）：
//!         若 layout.exit_to_peer(本机, 对方, pt) 有入口点 → 开始驱动：
//!           epoch += 1；虚拟光标 = 入口点；记下“出口点”；记下此刻本机按着的键（GetAsyncKeyState），这些键的抬起放行给本机、不转发；
//!           hub.send(Msg::Enter{epoch, x, y})；hub.to_desktop(HideCursor)；SetCursorPos(停放点 = 本机 Desk::park_point)；
//!           记下停放时间；吞掉这次移动。hub.set_status(driving = true)。
//!   Driving：吞掉所有鼠标/键盘事件（我们自己注入的、带 INJECT_MARK 的除外）。
//!         移动：d = pt - 停放点（停放后 50ms 内离停放点超过 300 像素的事件是停放前的旧坐标，丢弃）；
//!               VirtualCursor::step(d, cfg.input.speed) → Moved(p)：hub.to_net(Out::Move{epoch, seq+=1, x, y})
//!                                                      → Return(p)：回到本机（见下），本机光标放到 p。
//!         按钮：hub.send(Msg::Button{epoch, 虚拟光标位置, ..})；记录对方按着的按钮。
//!         滚轮：WM_MOUSEWHEEL / WM_MOUSEHWHEEL，delta = (mouseData >> 16) as i16。
//!         键盘：热键（cfg.input.hotkey_vk）按下 → 回到本机（落点 = 出口点）。其他键 → Msg::Key{scan = scanCode,
//!               ext = flags & LLKHF_EXTENDED, vk, down}；记录对方按着的键。
//!   回到本机：hub.send(Msg::Leave{epoch})；SetCursorPos(落点)；hub.to_desktop(ShowCursor)；
//!         此刻转发给对方、还没抬起的键/按钮 → 以后在本机的抬起事件要吞掉（本机没见过它们的按下）。
//!         hub.set_status(driving = false)。
//!   Home 时热键 → 切换暂停（同托盘“暂停共享”），吞掉。
//!   被控（ToInput::Driven(true)）期间：本机物理输入（不带 INJECT_MARK、不是 LLMHF_INJECTED/LLKHF_INJECTED）→
//!         hub.send(Msg::Yield)（最多每 300ms 一次），事件放行。被控时不检测跨屏。
//! ToInput：PeerUp 更新对方桌面；PeerDown / Pause(true) 时若在驱动 → 回到本机（落点 = 出口点）；
//!          LocalDesk 更新本机桌面；Config 更新排布/速度/热键；Yield → 若在驱动则回到本机；Quit → 卸钩子、
//!          若在驱动先回到本机，然后退出消息循环（PostQuitMessage）返回。

use crate::hub::{Hub, ToInput};
use crossbeam_channel::Receiver;
use std::sync::Arc;

/// 在当前（主）线程运行，直到收到 ToInput::Quit。
pub fn run(hub: Arc<Hub>, rx: Receiver<ToInput>) {
    let _ = (hub, rx);
    todo!("input::run")
}
