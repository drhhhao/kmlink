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
//!
//! 实现补充：
//! - 落点往本机内侧挪 1 像素，免得沿边上下移动时立刻又跨过去。
//! - 被控时又收到对方 Enter（双方同时跨屏）：本机放手，避免互相驱动。
//! - 看门狗发现钩子丢了且正在驱动：先回到本机（光标恢复可见），再重装钩子。

use crate::config::Config;
use crate::geom::{Desk, Layout, Point, Side, Step, VirtualCursor};
use crate::hub::{Hub, Out, ToDesktop, ToInput, WM_APP_WAKE};
use crate::proto::{Button, INJECT_MARK, Msg};
use crossbeam_channel::Receiver;
use std::cell::{Cell, RefCell};
use std::sync::Arc;

// ───────────────────────── 状态机（纯逻辑，可测试） ─────────────────────────

/// 状态机要求外界做的事（钩子回调在释放状态借用之后执行）。
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// hub.send
    Send(Msg),
    /// hub.to_net(Out::Move)
    Move { epoch: u32, seq: u32, x: i32, y: i32 },
    /// SetCursorPos
    SetCursor(Point),
    HideCursor,
    ShowCursor,
    /// hub.set_status(driving)
    Driving(bool),
    /// hub.set_status(paused)
    Paused(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Swallow,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MouseKind {
    Move,
    Button(Button, bool),
    Wheel { delta: i32, horizontal: bool },
}

#[derive(Clone, Copy, Debug)]
pub struct MouseEv {
    pub kind: MouseKind,
    pub pt: Point,
    /// 事件时间（毫秒，GetTickCount 同一时基）
    pub time: u32,
    /// 别的程序注入的（不是我们自己的）
    pub injected: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct KeyEv {
    pub vk: u32,
    pub scan: u16,
    pub ext: bool,
    pub down: bool,
    pub time: u32,
    pub injected: bool,
    /// AltGr 产生的假 LCtrl（scanCode 0x21D）：不转发
    pub fake: bool,
}

/// 读取此刻本机所有虚拟键是否按着（GetAsyncKeyState）。只在跨屏那一刻调用。
pub type KeyProbe = Box<dyn Fn() -> [bool; 256]>;

const MOUSE_VKS: [usize; 5] = [0x01, 0x02, 0x04, 0x05, 0x06];
const VK_PACKET: u32 = 0xE7;
const VK_PAUSE: u32 = 0x13;
/// 停放后这段时间内，离停放点太远的移动是停放前的旧坐标。
const STALE_MS: i32 = 50;
const STALE_DIST: i64 = 300;
const YIELD_MS: i32 = 300;

fn button_index(b: Button) -> usize {
    match b {
        Button::Left => 0,
        Button::Right => 1,
        Button::Middle => 2,
        Button::X1 => 3,
        Button::X2 => 4,
    }
}

/// 驱动对方期间的状态。
struct Drive {
    vc: VirtualCursor,
    exit: Point,
    park: Point,
    park_time: u32,
    /// 跨屏时本机按着的键：抬起放行给本机，不转发
    local_held: [bool; 256],
    /// 已转发给对方、还没抬起的键/按钮
    remote_keys: [bool; 256],
    remote_buttons: [bool; 5],
}

pub struct Kvm {
    mine: Desk,
    peer: Option<Desk>,
    layout: Layout,
    speed: f64,
    hotkey: u32,
    paused: bool,
    driven: bool,
    epoch: u32,
    seq: u32,
    drive: Option<Drive>,
    /// 本机没见过按下的键/按钮：之后的抬起要吞掉
    swallow_keys: [bool; 256],
    swallow_buttons: [bool; 5],
    /// 热键按着（防止自动重复反复切换）
    hotkey_held: bool,
    last_yield: Option<u32>,
    probe: KeyProbe,
}

impl Kvm {
    pub fn new(mine: Desk, cfg: &Config, epoch: u32, probe: KeyProbe) -> Self {
        let mut k = Kvm {
            mine,
            peer: None,
            layout: Layout::default(),
            speed: 1.0,
            hotkey: 0,
            paused: false,
            driven: false,
            epoch,
            seq: 0,
            drive: None,
            swallow_keys: [false; 256],
            swallow_buttons: [false; 5],
            hotkey_held: false,
            last_yield: None,
            probe,
        };
        k.apply_config(cfg);
        k
    }

    #[cfg(test)]
    pub fn driving(&self) -> bool {
        self.drive.is_some()
    }

    fn apply_config(&mut self, cfg: &Config) {
        self.layout = cfg.layout.layout;
        let s = cfg.input.speed;
        self.speed = if s.is_finite() && s > 0.0 { s.min(20.0) } else { 1.0 };
        self.hotkey = cfg.input.hotkey_vk & 0xFF;
    }

    fn maybe_yield(&mut self, time: u32, injected: bool, out: &mut Vec<Action>) {
        if !self.driven || injected {
            return;
        }
        let due = match self.last_yield {
            None => true,
            Some(t) => (time.wrapping_sub(t) as i32) >= YIELD_MS || (time.wrapping_sub(t) as i32) < 0,
        };
        if due {
            self.last_yield = Some(time);
            out.push(Action::Send(Msg::Yield));
        }
    }

    // ── 鼠标 ──

    pub fn mouse(&mut self, ev: MouseEv, out: &mut Vec<Action>) -> Verdict {
        if self.drive.is_some() {
            return self.drive_mouse(ev, out);
        }
        self.maybe_yield(ev.time, ev.injected, out);
        match ev.kind {
            MouseKind::Button(b, down) => {
                let i = button_index(b);
                if self.swallow_buttons[i] {
                    self.swallow_buttons[i] = false;
                    // 按下：本机重新看到了完整的一次按键，放行
                    return if down { Verdict::Pass } else { Verdict::Swallow };
                }
                Verdict::Pass
            }
            MouseKind::Wheel { .. } => Verdict::Pass,
            MouseKind::Move => {
                if self.try_cross(ev.pt, ev.time, out) {
                    Verdict::Swallow
                } else {
                    Verdict::Pass
                }
            }
        }
    }

    fn try_cross(&mut self, pt: Point, time: u32, out: &mut Vec<Action>) -> bool {
        if self.paused || self.driven {
            return false;
        }
        let Some(peer) = self.peer.as_ref() else {
            return false;
        };
        if peer.is_empty() || self.mine.is_empty() {
            return false;
        }
        // 钩子里的坐标可能越出屏幕：先钳回桌面
        let p = self.mine.clamp(pt);
        let Some(entry) = self.layout.exit_to_peer(&self.mine, peer, p) else {
            return false;
        };
        let mut held = (self.probe)();
        if MOUSE_VKS.iter().any(|&v| held[v]) {
            return false; // 拖动中不跨屏
        }
        for v in MOUSE_VKS {
            held[v] = false;
        }
        self.epoch = self.epoch.wrapping_add(1);
        self.seq = 0;
        let park = self.mine.park_point();
        self.drive = Some(Drive {
            vc: VirtualCursor::new(entry),
            exit: p,
            park,
            park_time: time,
            local_held: held,
            remote_keys: [false; 256],
            remote_buttons: [false; 5],
        });
        out.push(Action::Send(Msg::Enter { epoch: self.epoch, x: entry.x, y: entry.y }));
        out.push(Action::HideCursor);
        out.push(Action::SetCursor(park));
        out.push(Action::Driving(true));
        true
    }

    fn drive_mouse(&mut self, ev: MouseEv, out: &mut Vec<Action>) -> Verdict {
        let epoch = self.epoch;
        let Some(d) = self.drive.as_mut() else {
            return Verdict::Pass;
        };
        match ev.kind {
            MouseKind::Move => {
                let dx = ev.pt.x - d.park.x;
                let dy = ev.pt.y - d.park.y;
                let dist2 = (dx as i64) * (dx as i64) + (dy as i64) * (dy as i64);
                if (ev.time.wrapping_sub(d.park_time) as i32) < STALE_MS && dist2 > STALE_DIST * STALE_DIST {
                    return Verdict::Swallow; // 停放前的旧坐标
                }
                if dx == 0 && dy == 0 {
                    return Verdict::Swallow;
                }
                let Some(peer) = self.peer.as_ref() else {
                    let exit = d.exit;
                    self.go_home(exit, out);
                    return Verdict::Swallow;
                };
                let before = d.vc.pos;
                match d.vc.step(dx, dy, self.speed, &self.layout, &self.mine, peer) {
                    Step::Moved(p) => {
                        if p != before {
                            self.seq = self.seq.wrapping_add(1);
                            out.push(Action::Move { epoch, seq: self.seq, x: p.x, y: p.y });
                        }
                    }
                    Step::Return(p) => self.go_home(p, out),
                }
            }
            MouseKind::Button(b, down) => {
                let i = button_index(b);
                d.remote_buttons[i] = down;
                self.swallow_buttons[i] = false;
                let p = d.vc.pos;
                out.push(Action::Send(Msg::Button { epoch, x: p.x, y: p.y, button: b, down }));
            }
            MouseKind::Wheel { delta, horizontal } => {
                if delta != 0 {
                    let p = d.vc.pos;
                    out.push(Action::Send(Msg::Wheel { epoch, x: p.x, y: p.y, delta, horizontal }));
                }
            }
        }
        Verdict::Swallow
    }

    // ── 键盘 ──

    pub fn key(&mut self, ev: KeyEv, out: &mut Vec<Action>) -> Verdict {
        let vk = (ev.vk & 0xFF) as usize;
        let is_hotkey = self.hotkey != 0 && (ev.vk & 0xFF) == self.hotkey;
        let epoch = self.epoch;
        if let Some(d) = self.drive.as_mut() {
            if d.local_held[vk] {
                // 跨屏前就按着的键：抬起还给本机；自动重复吞掉
                if ev.down {
                    return Verdict::Swallow;
                }
                d.local_held[vk] = false;
                return Verdict::Pass;
            }
            if is_hotkey {
                if ev.down {
                    self.hotkey_held = true;
                    let exit = d.exit;
                    self.go_home(exit, out);
                } else {
                    self.hotkey_held = false;
                }
                return Verdict::Swallow;
            }
            if ev.fake || ev.vk == VK_PACKET {
                return Verdict::Swallow;
            }
            d.remote_keys[vk] = ev.down;
            self.swallow_keys[vk] = false;
            out.push(Action::Send(Msg::Key { epoch, scan: ev.scan, ext: ev.ext, vk: ev.vk as u16, down: ev.down }));
            return Verdict::Swallow;
        }

        self.maybe_yield(ev.time, ev.injected, out);
        if is_hotkey {
            if ev.down {
                if !self.hotkey_held {
                    self.paused = !self.paused;
                    out.push(Action::Paused(self.paused));
                }
                self.hotkey_held = true;
            } else {
                self.hotkey_held = false;
            }
            return Verdict::Swallow;
        }
        if self.swallow_keys[vk] {
            self.swallow_keys[vk] = false;
            return if ev.down { Verdict::Pass } else { Verdict::Swallow };
        }
        Verdict::Pass
    }

    // ── 回到本机 ──

    fn go_home(&mut self, land: Point, out: &mut Vec<Action>) {
        let Some(d) = self.drive.take() else {
            return;
        };
        for (s, r) in self.swallow_keys.iter_mut().zip(d.remote_keys.iter()) {
            *s |= *r;
        }
        for (s, r) in self.swallow_buttons.iter_mut().zip(d.remote_buttons.iter()) {
            *s |= *r;
        }
        let land = self.inward(land);
        out.push(Action::Send(Msg::Leave { epoch: self.epoch }));
        out.push(Action::SetCursor(land));
        out.push(Action::ShowCursor);
        out.push(Action::Driving(false));
    }

    /// 落点钳进本机桌面，再往内侧挪 1 像素（若还在屏幕上）。
    fn inward(&self, p: Point) -> Point {
        let p = self.mine.clamp(p);
        let q = match self.layout.side {
            Side::Right => Point::new(p.x - 1, p.y),
            Side::Left => Point::new(p.x + 1, p.y),
            Side::Bottom => Point::new(p.x, p.y - 1),
            Side::Top => Point::new(p.x, p.y + 1),
        };
        if self.mine.contains(q) { q } else { p }
    }

    fn exit_point(&self) -> Option<Point> {
        self.drive.as_ref().map(|d| d.exit)
    }

    fn go_home_exit(&mut self, out: &mut Vec<Action>) {
        if let Some(e) = self.exit_point() {
            self.go_home(e, out);
        }
    }

    // ── 来自其他线程的消息 ──

    pub fn msg(&mut self, m: ToInput, now: u32, out: &mut Vec<Action>) {
        match m {
            ToInput::PeerUp { name: _, desk } => {
                if desk.is_empty() {
                    self.go_home_exit(out);
                } else if let Some(d) = self.drive.as_mut() {
                    d.vc.pos = desk.clamp(d.vc.pos);
                }
                self.peer = Some(desk);
            }
            ToInput::PeerDown => {
                self.go_home_exit(out);
                self.peer = None;
            }
            ToInput::LocalDesk(desk) => {
                self.mine = desk;
                let park = self.mine.park_point();
                let exit = self.exit_point().map(|e| self.mine.clamp(e));
                if let (Some(d), Some(exit)) = (self.drive.as_mut(), exit) {
                    d.park = park;
                    d.park_time = now;
                    d.exit = exit;
                    out.push(Action::SetCursor(park));
                }
            }
            ToInput::Config(cfg) => self.apply_config(&cfg),
            ToInput::Yield => self.go_home_exit(out),
            ToInput::Driven(b) => {
                self.driven = b;
                self.last_yield = None;
                if b {
                    self.go_home_exit(out); // 双方同时跨屏：本机放手
                }
            }
            ToInput::Pause(p) => {
                self.paused = p;
                if p {
                    self.go_home_exit(out);
                }
                out.push(Action::Paused(p));
            }
            ToInput::Quit => self.go_home_exit(out),
        }
    }

    /// 看门狗发现钩子被系统摘掉：正在驱动就先回到本机。
    pub fn hooks_lost(&mut self, out: &mut Vec<Action>) {
        self.go_home_exit(out);
    }
}

/// 钩子给的扫描码：0 时用 MapVirtualKey 的结果（低字节 = 扫描码，高字节 0xE0/0xE1 = 扩展键）。
/// Pause 键的扫描码和 NumLock 冲突，改用 vk 注入（scan = 0）。
fn scan_of(vk: u32, scan_code: u32, ext_flag: bool, mapped: impl FnOnce() -> u32) -> (u16, bool) {
    if vk == VK_PAUSE {
        return (0, false);
    }
    if scan_code != 0 {
        return ((scan_code & 0xFF) as u16, ext_flag);
    }
    let m = mapped();
    let hi = m & 0xFF00;
    ((m & 0xFF) as u16, ext_flag || hi == 0xE000 || hi == 0xE100)
}

// ───────────────────────── Win32：钩子与消息循环 ─────────────────────────

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetLastInputInfo, LASTINPUTINFO, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, KillTimer, LLKHF_EXTENDED, LLKHF_INJECTED,
    LLKHF_UP, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW, PostQuitMessage, SetCursorPos, SetTimer,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_TIMER, WM_USER,
    WM_XBUTTONDOWN, WM_XBUTTONUP,
};

thread_local! {
    static ST: RefCell<Option<Kvm>> = const { RefCell::new(None) };
    static HUB: RefCell<Option<Arc<Hub>>> = const { RefCell::new(None) };
    /// 最后一次钩子回调的 GetTickCount（看门狗用）。
    static LAST_HOOK: Cell<u32> = const { Cell::new(0) };
}

const WATCHDOG_MS: u32 = 2000;

fn tick() -> u32 {
    unsafe { GetTickCount() }
}

/// 在状态机上跑一步，释放借用后再执行它要求的动作。借不到（重入）就放行。
fn dispatch(f: impl FnOnce(&mut Kvm, &mut Vec<Action>) -> Verdict) -> Verdict {
    let mut out = Vec::new();
    let v = ST.with(|st| match st.try_borrow_mut() {
        Ok(mut g) => match g.as_mut() {
            Some(k) => f(k, &mut out),
            None => Verdict::Pass,
        },
        Err(_) => Verdict::Pass,
    });
    if !out.is_empty() {
        execute(out);
    }
    v
}

fn execute(actions: Vec<Action>) {
    HUB.with(|h| {
        let h = h.borrow();
        let Some(hub) = h.as_ref() else {
            return;
        };
        for a in actions {
            match a {
                Action::Send(m) => hub.send(m),
                Action::Move { epoch, seq, x, y } => hub.to_net(Out::Move { epoch, seq, x, y }),
                Action::SetCursor(p) => unsafe {
                    let _ = SetCursorPos(p.x, p.y);
                },
                Action::HideCursor => hub.to_desktop(ToDesktop::HideCursor),
                Action::ShowCursor => hub.to_desktop(ToDesktop::ShowCursor),
                Action::Driving(b) => hub.set_status(|s| s.driving = b),
                Action::Paused(b) => hub.set_status(|s| s.paused = b),
            }
        }
    });
}

fn mouse_event(msg: u32, ms: &MSLLHOOKSTRUCT) -> Option<MouseEv> {
    let hi = (ms.mouseData >> 16) as u16;
    let kind = match msg {
        WM_MOUSEMOVE => MouseKind::Move,
        WM_LBUTTONDOWN => MouseKind::Button(Button::Left, true),
        WM_LBUTTONUP => MouseKind::Button(Button::Left, false),
        WM_RBUTTONDOWN => MouseKind::Button(Button::Right, true),
        WM_RBUTTONUP => MouseKind::Button(Button::Right, false),
        WM_MBUTTONDOWN => MouseKind::Button(Button::Middle, true),
        WM_MBUTTONUP => MouseKind::Button(Button::Middle, false),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let b = match hi {
                1 => Button::X1,
                2 => Button::X2,
                _ => return None,
            };
            MouseKind::Button(b, msg == WM_XBUTTONDOWN)
        }
        WM_MOUSEWHEEL => MouseKind::Wheel { delta: hi as i16 as i32, horizontal: false },
        WM_MOUSEHWHEEL => MouseKind::Wheel { delta: hi as i16 as i32, horizontal: true },
        _ => return None,
    };
    Some(MouseEv {
        kind,
        pt: Point::new(ms.pt.x, ms.pt.y),
        time: ms.time,
        injected: ms.flags & LLMHF_INJECTED != 0,
    })
}

fn key_event(kb: &KBDLLHOOKSTRUCT) -> KeyEv {
    let flags = kb.flags;
    let ext_flag = flags.0 & LLKHF_EXTENDED.0 != 0;
    let (scan, ext) = scan_of(kb.vkCode, kb.scanCode, ext_flag, || unsafe {
        MapVirtualKeyW(kb.vkCode, MAPVK_VK_TO_VSC_EX)
    });
    KeyEv {
        vk: kb.vkCode,
        scan,
        ext,
        down: flags.0 & LLKHF_UP.0 == 0,
        time: kb.time,
        injected: flags.0 & LLKHF_INJECTED.0 != 0,
        fake: kb.scanCode == 0x21D,
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 && lp.0 != 0 {
        LAST_HOOK.with(|c| c.set(tick()));
        let ms = unsafe { &*(lp.0 as *const MSLLHOOKSTRUCT) };
        if ms.dwExtraInfo != INJECT_MARK {
            if let Some(ev) = mouse_event(wp.0 as u32, ms) {
                if dispatch(|k, out| k.mouse(ev, out)) == Verdict::Swallow {
                    return LRESULT(1);
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

unsafe extern "system" fn key_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 && lp.0 != 0 {
        LAST_HOOK.with(|c| c.set(tick()));
        let kb = unsafe { &*(lp.0 as *const KBDLLHOOKSTRUCT) };
        if kb.dwExtraInfo != INJECT_MARK {
            let ev = key_event(kb);
            if dispatch(|k, out| k.key(ev, out)) == Verdict::Swallow {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

fn probe_keys() -> [bool; 256] {
    let mut a = [false; 256];
    for (vk, held) in a.iter_mut().enumerate().skip(1) {
        *held = unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0;
    }
    a
}

#[derive(Default)]
struct Hooks {
    mouse: Option<HHOOK>,
    key: Option<HHOOK>,
}

impl Hooks {
    fn install(&mut self) {
        self.uninstall();
        unsafe {
            let hinst = GetModuleHandleW(None).ok().map(|m| HINSTANCE(m.0));
            match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst, 0) {
                Ok(h) => self.mouse = Some(h),
                Err(e) => crate::log!("安装鼠标钩子失败：{e}"),
            }
            match SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_proc), hinst, 0) {
                Ok(h) => self.key = Some(h),
                Err(e) => crate::log!("安装键盘钩子失败：{e}"),
            }
        }
        LAST_HOOK.with(|c| c.set(tick()));
    }

    fn uninstall(&mut self) {
        unsafe {
            if let Some(h) = self.mouse.take() {
                let _ = UnhookWindowsHookEx(h);
            }
            if let Some(h) = self.key.take() {
                let _ = UnhookWindowsHookEx(h);
            }
        }
    }
}

/// 在当前（主）线程运行，直到收到 ToInput::Quit。
pub fn run(hub: Arc<Hub>, rx: Receiver<ToInput>) {
    let mut msg = MSG::default();
    unsafe {
        // 先建好线程消息队列，PostThreadMessage 才能送达
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        if let Err(e) = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) {
            crate::log!("设置线程优先级失败：{e}");
        }
    }

    let cfg = hub.cfg();
    let mine = crate::sys::monitors();
    crate::log!("本机显示器：{:?}", mine.monitors);
    // epoch 从时间起步，重启后也不会和上一次重复
    let epoch = (crate::config::now_ms() / 1000) as u32;
    ST.with(|s| *s.borrow_mut() = Some(Kvm::new(mine, &cfg, epoch, Box::new(probe_keys))));
    HUB.with(|h| *h.borrow_mut() = Some(hub.clone()));

    let mut hooks = Hooks::default();
    hooks.install();
    let timer = unsafe { SetTimer(None, 0, WATCHDOG_MS, None) };
    if timer == 0 {
        crate::log!("看门狗定时器创建失败");
    }
    hub.set_input_tid(unsafe { GetCurrentThreadId() });

    let mut lost_streak: u32 = 0;
    let mut quitting = false;
    loop {
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 == 0 || r.0 == -1 {
            break;
        }
        match msg.message {
            WM_APP_WAKE => {}
            WM_TIMER => {
                if watchdog_tripped() {
                    lost_streak += 1;
                    if lost_streak == 1 || lost_streak % 30 == 0 {
                        crate::log!("钩子似乎被系统摘掉了（第 {lost_streak} 次），重装");
                    }
                    dispatch(|k, out| {
                        k.hooks_lost(out);
                        Verdict::Pass
                    });
                    hooks.install();
                } else {
                    lost_streak = 0;
                }
            }
            _ => unsafe {
                DispatchMessageW(&msg);
            },
        }
        if !quitting {
            quitting = drain(&rx);
            if quitting {
                hooks.uninstall();
                unsafe { PostQuitMessage(0) };
            }
        }
    }

    hooks.uninstall();
    if timer != 0 {
        unsafe {
            let _ = KillTimer(None, timer);
        }
    }
    ST.with(|s| s.borrow_mut().take());
    HUB.with(|h| h.borrow_mut().take());
}

/// 处理所有待处理的 ToInput；收到 Quit 返回 true。
fn drain(rx: &Receiver<ToInput>) -> bool {
    while let Ok(m) = rx.try_recv() {
        let quit = matches!(m, ToInput::Quit);
        if let ToInput::LocalDesk(d) = &m {
            crate::log!("本机显示器变化：{:?}", d.monitors);
        }
        let now = tick();
        dispatch(|k, out| {
            k.msg(m, now, out);
            Verdict::Pass
        });
        if quit {
            return true;
        }
    }
    false
}

/// 有输入发生、但钩子 1 秒以上没被调用：钩子丢了。
fn watchdog_tripped() -> bool {
    let mut lii = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if !unsafe { GetLastInputInfo(&mut lii) }.as_bool() {
        return false;
    }
    let last_hook = LAST_HOOK.with(|c| c.get());
    (lii.dwTime.wrapping_sub(last_hook) as i32) > 1000
}

// ───────────────────────── 测试 ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Rect;
    use std::rc::Rc;

    const E0: u32 = 100;
    const PARK: Point = Point::new(960, 540);

    struct T {
        k: Kvm,
        held: Rc<RefCell<[bool; 256]>>,
        t: u32,
    }

    fn desk() -> Desk {
        Desk::new(vec![Rect::new(0, 0, 1920, 1080)])
    }

    fn setup() -> T {
        let held = Rc::new(RefCell::new([false; 256]));
        let h2 = held.clone();
        let cfg = Config::default(); // 对方在右侧，热键 Scroll Lock
        let mut k = Kvm::new(desk(), &cfg, E0, Box::new(move || *h2.borrow()));
        let mut out = vec![];
        k.msg(ToInput::PeerUp { name: "b".into(), desk: desk() }, 0, &mut out);
        assert!(out.is_empty());
        T { k, held, t: 1000 }
    }

    impl T {
        fn mv(&mut self, x: i32, y: i32) -> (Verdict, Vec<Action>) {
            self.t += 8;
            self.mouse(MouseKind::Move, Point::new(x, y), false)
        }
        fn mouse(&mut self, kind: MouseKind, pt: Point, injected: bool) -> (Verdict, Vec<Action>) {
            let mut out = vec![];
            let v = self.k.mouse(MouseEv { kind, pt, time: self.t, injected }, &mut out);
            (v, out)
        }
        fn rel(&mut self, dx: i32, dy: i32) -> (Verdict, Vec<Action>) {
            self.t += 100; // 远离停放时间，避开旧坐标过滤
            self.mouse(MouseKind::Move, Point::new(PARK.x + dx, PARK.y + dy), false)
        }
        fn key(&mut self, vk: u32, down: bool) -> (Verdict, Vec<Action>) {
            self.t += 8;
            let mut out = vec![];
            let ev = KeyEv { vk, scan: 0x10, ext: false, down, time: self.t, injected: false, fake: false };
            let v = self.k.key(ev, &mut out);
            (v, out)
        }
        fn msg(&mut self, m: ToInput) -> Vec<Action> {
            let mut out = vec![];
            self.k.msg(m, self.t, &mut out);
            out
        }
        fn cross(&mut self) {
            let (v, out) = self.mv(1919, 500);
            assert_eq!(v, Verdict::Swallow);
            assert_eq!(
                out,
                vec![
                    Action::Send(Msg::Enter { epoch: self.k.epoch, x: 0, y: 500 }),
                    Action::HideCursor,
                    Action::SetCursor(PARK),
                    Action::Driving(true),
                ]
            );
            assert!(self.k.driving());
        }
    }

    fn home_actions(epoch: u32, land: Point) -> Vec<Action> {
        vec![Action::Send(Msg::Leave { epoch }), Action::SetCursor(land), Action::ShowCursor, Action::Driving(false)]
    }

    #[test]
    fn cross_right_edge_enters_and_parks() {
        let mut t = setup();
        assert_eq!(t.mv(1000, 500), (Verdict::Pass, vec![]));
        t.cross();
        assert_eq!(t.k.epoch, E0 + 1);
    }

    #[test]
    fn cross_with_unclamped_point() {
        let mut t = setup();
        let (v, out) = t.mv(1925, 500);
        assert_eq!(v, Verdict::Swallow);
        assert_eq!(out[0], Action::Send(Msg::Enter { epoch: E0 + 1, x: 0, y: 500 }));
    }

    #[test]
    fn moves_send_increasing_seq() {
        let mut t = setup();
        t.cross();
        let e = t.k.epoch;
        assert_eq!(t.rel(10, 0), (Verdict::Swallow, vec![Action::Move { epoch: e, seq: 1, x: 10, y: 500 }]));
        assert_eq!(t.rel(5, 3), (Verdict::Swallow, vec![Action::Move { epoch: e, seq: 2, x: 15, y: 503 }]));
        // 停放点本身（零位移）不发
        assert_eq!(t.rel(0, 0), (Verdict::Swallow, vec![]));
        // 下一次跨屏：seq 重新开始
        t.rel(-100, 0);
        assert!(!t.k.driving());
        t.cross();
        assert_eq!(t.rel(1, 0), (Verdict::Swallow, vec![Action::Move { epoch: e + 1, seq: 1, x: 1, y: 500 }]));
    }

    #[test]
    fn return_via_virtual_cursor() {
        let mut t = setup();
        t.cross();
        let e = t.k.epoch;
        t.rel(3, 0); // (3, 500)
        let (v, out) = t.rel(-10, 4);
        assert_eq!(v, Verdict::Swallow);
        // 回到 (1919, 504)，往内挪一像素
        assert_eq!(out, home_actions(e, Point::new(1918, 504)));
        assert!(!t.k.driving());
        // 回来后，本机移动放行
        assert_eq!(t.mv(1900, 504), (Verdict::Pass, vec![]));
    }

    #[test]
    fn keys_held_at_crossing_pass_on_key_up() {
        let mut t = setup();
        t.held.borrow_mut()[0xA0] = true; // LShift
        t.cross();
        // 自动重复：吞掉，不转发
        assert_eq!(t.key(0xA0, true), (Verdict::Swallow, vec![]));
        // 抬起：放行给本机，不转发
        assert_eq!(t.key(0xA0, false), (Verdict::Pass, vec![]));
        // 之后再按：正常转发
        let e = t.k.epoch;
        assert_eq!(
            t.key(0xA0, true),
            (Verdict::Swallow, vec![Action::Send(Msg::Key { epoch: e, scan: 0x10, ext: false, vk: 0xA0, down: true })])
        );
    }

    #[test]
    fn forwarded_keys_held_at_return_swallow_local_up() {
        let mut t = setup();
        t.cross();
        let e = t.k.epoch;
        let (v, out) = t.key(0x41, true);
        assert_eq!(v, Verdict::Swallow);
        assert_eq!(out, vec![Action::Send(Msg::Key { epoch: e, scan: 0x10, ext: false, vk: 0x41, down: true })]);
        t.key(0x42, true);
        t.key(0x42, false); // B 已经抬起
        t.rel(-100, 0); // 回到本机
        assert!(!t.k.driving());
        assert_eq!(t.key(0x41, false), (Verdict::Swallow, vec![]));
        assert_eq!(t.key(0x42, false), (Verdict::Pass, vec![]));
        // 只吞一次
        assert_eq!(t.key(0x41, true), (Verdict::Pass, vec![]));
        assert_eq!(t.key(0x41, false), (Verdict::Pass, vec![]));
    }

    #[test]
    fn forwarded_buttons_held_at_return_swallow_local_up() {
        let mut t = setup();
        t.cross();
        let e = t.k.epoch;
        t.rel(50, 0);
        let (v, out) = t.mouse(MouseKind::Button(Button::Left, true), PARK, false);
        assert_eq!(v, Verdict::Swallow);
        assert_eq!(out, vec![Action::Send(Msg::Button { epoch: e, x: 50, y: 500, button: Button::Left, down: true })]);
        let (_, out) = t.mouse(MouseKind::Wheel { delta: -120, horizontal: false }, PARK, false);
        assert_eq!(out, vec![Action::Send(Msg::Wheel { epoch: e, x: 50, y: 500, delta: -120, horizontal: false })]);
        t.key(0x91, true); // 热键回家
        assert!(!t.k.driving());
        assert_eq!(t.mouse(MouseKind::Button(Button::Left, false), PARK, false), (Verdict::Swallow, vec![]));
        assert_eq!(t.mouse(MouseKind::Button(Button::Left, true), PARK, false), (Verdict::Pass, vec![]));
    }

    #[test]
    fn hotkey_at_home_toggles_pause() {
        let mut t = setup();
        assert_eq!(t.key(0x91, true), (Verdict::Swallow, vec![Action::Paused(true)]));
        assert_eq!(t.key(0x91, true), (Verdict::Swallow, vec![])); // 自动重复不再切换
        assert_eq!(t.key(0x91, false), (Verdict::Swallow, vec![]));
        // 暂停时不跨屏
        assert_eq!(t.mv(1919, 500), (Verdict::Pass, vec![]));
        assert_eq!(t.key(0x91, true), (Verdict::Swallow, vec![Action::Paused(false)]));
        t.key(0x91, false);
        t.cross();
    }

    #[test]
    fn hotkey_while_driving_returns_to_exit() {
        let mut t = setup();
        t.mv(1919, 300);
        assert!(t.k.driving());
        let e = t.k.epoch;
        t.rel(400, 200);
        let (v, out) = t.key(0x91, true);
        assert_eq!(v, Verdict::Swallow);
        assert_eq!(out, home_actions(e, Point::new(1918, 300)));
        // 热键的自动重复和抬起：吞掉，也不切换暂停
        assert_eq!(t.key(0x91, true), (Verdict::Swallow, vec![]));
        assert_eq!(t.key(0x91, false), (Verdict::Swallow, vec![]));
        assert!(!t.k.paused);
    }

    #[test]
    fn stale_events_after_parking_dropped() {
        let mut t = setup();
        t.cross();
        let e = t.k.epoch;
        // 停放后 10ms，还在边上的旧坐标：丢弃
        t.t += 2;
        assert_eq!(t.mouse(MouseKind::Move, Point::new(1919, 500), false), (Verdict::Swallow, vec![]));
        // 停放点附近的小移动照常
        assert_eq!(
            t.mouse(MouseKind::Move, Point::new(PARK.x + 4, PARK.y), false),
            (Verdict::Swallow, vec![Action::Move { epoch: e, seq: 1, x: 4, y: 500 }])
        );
        // 过了 50ms 后，大幅移动也照常
        t.t += 60;
        let (_, out) = t.mouse(MouseKind::Move, Point::new(PARK.x + 400, PARK.y), false);
        assert_eq!(out, vec![Action::Move { epoch: e, seq: 2, x: 404, y: 500 }]);
    }

    #[test]
    fn peer_down_while_driving_returns_home() {
        let mut t = setup();
        t.cross();
        let e = t.k.epoch;
        t.rel(100, 100);
        assert_eq!(t.msg(ToInput::PeerDown), home_actions(e, Point::new(1918, 500)));
        assert!(!t.k.driving());
        // 没有对方：不再跨屏
        assert_eq!(t.mv(1919, 500), (Verdict::Pass, vec![]));
    }

    #[test]
    fn yield_pause_and_quit_return_home() {
        for m in [ToInput::Yield, ToInput::Pause(true), ToInput::Quit, ToInput::Driven(true)] {
            let mut t = setup();
            t.cross();
            let out = t.msg(m);
            assert_eq!(out[0], Action::Send(Msg::Leave { epoch: t.k.epoch }));
            assert!(!t.k.driving());
        }
        // 不在驱动时 Yield 什么都不做
        let mut t = setup();
        assert!(t.msg(ToInput::Yield).is_empty());
    }

    #[test]
    fn no_crossing_while_button_held() {
        let mut t = setup();
        t.held.borrow_mut()[0x01] = true;
        assert_eq!(t.mv(1919, 500), (Verdict::Pass, vec![]));
        t.held.borrow_mut()[0x01] = false;
        t.cross();
    }

    #[test]
    fn no_crossing_when_paused_no_peer_or_empty_desk() {
        let mut t = setup();
        t.msg(ToInput::Pause(true));
        assert_eq!(t.mv(1919, 500), (Verdict::Pass, vec![]));
        t.msg(ToInput::Pause(false));

        t.msg(ToInput::PeerDown);
        assert_eq!(t.mv(1919, 500), (Verdict::Pass, vec![]));

        t.msg(ToInput::PeerUp { name: "b".into(), desk: Desk::default() });
        assert_eq!(t.mv(1919, 500), (Verdict::Pass, vec![]));

        t.msg(ToInput::PeerUp { name: "b".into(), desk: desk() });
        t.cross();
    }

    #[test]
    fn driven_sends_yield_and_does_not_cross() {
        let mut t = setup();
        t.msg(ToInput::Driven(true));
        assert_eq!(t.mv(1919, 500), (Verdict::Pass, vec![Action::Send(Msg::Yield)]));
        // 300ms 内不重复
        assert_eq!(t.mv(1919, 501), (Verdict::Pass, vec![]));
        // 别的程序注入的输入不算
        t.t += 400;
        assert_eq!(t.mouse(MouseKind::Move, Point::new(10, 10), true), (Verdict::Pass, vec![]));
        assert_eq!(t.key(0x41, true), (Verdict::Pass, vec![Action::Send(Msg::Yield)]));
        t.msg(ToInput::Driven(false));
        t.cross();
    }

    #[test]
    fn fake_ctrl_and_packet_not_forwarded() {
        let mut t = setup();
        t.cross();
        let mut out = vec![];
        let ev = KeyEv { vk: 0xA2, scan: 0x1D, ext: false, down: true, time: 5000, injected: false, fake: true };
        assert_eq!(t.k.key(ev, &mut out), Verdict::Swallow);
        assert_eq!(t.key(VK_PACKET, true), (Verdict::Swallow, vec![]));
        assert!(out.is_empty());
        // 在本机时假 LCtrl 放行
        t.msg(ToInput::Yield);
        assert_eq!(t.k.key(ev, &mut out), Verdict::Pass);
    }

    #[test]
    fn local_desk_change_while_driving_reparks() {
        let mut t = setup();
        t.cross();
        let d2 = Desk::new(vec![Rect::new(0, 0, 2560, 1440)]);
        assert_eq!(t.msg(ToInput::LocalDesk(d2)), vec![Action::SetCursor(Point::new(1280, 720))]);
    }

    #[test]
    fn scan_code_mapping() {
        assert_eq!(scan_of(0x41, 0x1E, false, || unreachable!()), (0x1E, false));
        assert_eq!(scan_of(0x41, 0x21D, false, || unreachable!()), (0x1D, false));
        assert_eq!(scan_of(0x2E, 0, false, || 0xE053), (0x53, true));
        assert_eq!(scan_of(0x41, 0, false, || 0x1E), (0x1E, false));
        assert_eq!(scan_of(VK_PAUSE, 0x45, false, || unreachable!()), (0, false));
    }
}
