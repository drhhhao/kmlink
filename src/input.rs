//! 控制端：低级键鼠钩子 + 跨屏状态机。跑在主线程，自己的消息循环（GetMessageW）。
//!
//! 启动：hub.set_input_tid(GetCurrentThreadId())；SetWindowsHookExW(WH_MOUSE_LL / WH_KEYBOARD_LL)；
//! SetThreadPriority(THREAD_PRIORITY_TIME_CRITICAL)；本机显示器 = sys::monitors()；cursor::start_worker()（备份系统光标）。
//! 状态放在 thread_local 的 RefCell 里（钩子回调是 extern "system" fn，没有上下文指针）。
//! 回调里绝不阻塞：不打日志、不做文件/网络 IO、不加锁等待、尽量不分配（动作缓冲区预先分配、复用）。
//! 发消息用 hub.to_net / hub.send（无界 channel）；要拿锁的状态更新和 SendInput 推迟到消息循环。
//! 每个非注入（物理）输入事件把 GetTickCount 写进 LAST_PHYSICAL（`last_physical_input()`，被控端 enter 时读）。
//!
//! 模式：
//!   Home：本机在用。鼠标移动时（没有按着鼠标键、未暂停、未被控、对方已连接、对方显示器已知、
//!         距上次接受对方 Yield 已过 YIELD_COOLDOWN_MS）：
//!         光标钳进桌面（Desk::clamp_from：先钳在当前所在显示器里）；
//!         若 layout.exit_to_peer(本机, 对方, pt) 有入口点：
//!           先看前台是不是管理员窗口（是就提示、不跨），再只查 5 个鼠标键（拖动中不跨），
//!           真要跨屏时才扫一遍全部键（GetAsyncKeyState）。开始驱动：
//!           epoch += 1；虚拟光标 = 入口点；记下“出口点”；记下此刻本机按着的键，这些键的抬起放行给本机、不转发；
//!           hub.send(Msg::Enter{epoch, x, y})；cursor::request_hide(本线程, epoch)；吞掉这次移动。
//!           光标此时还留在出口点（看得见也自然），等光标线程隐藏完发回 WM_APP_HIDDEN(epoch)，
//!           消息循环里才 SetCursorPos(停放点 = 本机 Desk::park_point)——先隐藏再停放，屏幕中间不会闪箭头。
//!           光标线程迟迟没回（PARK_WAIT_MS）就在钩子里直接停放兜底。hub.set_status(driving = true)。
//!   Driving：吞掉所有鼠标/键盘事件（我们自己注入的、带 INJECT_MARK 的除外）。
//!         移动：d = pt - 真实光标位置（GetCursorPos；见文末）；
//!               VirtualCursor::step(d, cfg.input.speed) → Moved(p)：hub.to_net(Out::Move{epoch, seq+=1, x, y})
//!                                                      → Return(p)：回到本机（见下），本机光标放到 p。
//!         按钮：hub.send(Msg::Button{epoch, 虚拟光标位置, ..})；记录对方按着的按钮。
//!         滚轮：WM_MOUSEWHEEL / WM_MOUSEHWHEEL，delta = (mouseData >> 16) as i16。
//!         键盘：热键（cfg.input.hotkey_vk）按下 → 回到本机（落点 = 出口点）。其他键 → Msg::Key{scan = scanCode,
//!               ext = flags & LLKHF_EXTENDED, vk, down}；记录对方按着的键。
//!   回到本机：hub.send(Msg::Leave{epoch})；SetCursorPos(落点)；cursor::request_show()（快速恢复，约 1~2ms）；
//!         此刻转发给对方、还没抬起的键/按钮 → 以后在本机的抬起事件要吞掉（本机没见过它们的按下）；
//!         其中 Alt/Win 若自动重复的按下被放行给本机，它的抬起要防菜单（mask_up）。
//!         hub.set_status(driving = false)。
//!   Home 时热键 → 切换暂停（同托盘“暂停共享”），吞掉。
//!   被控（ToInput::Driven(true)）期间：本机物理输入（不带 INJECT_MARK、不是 LLMHF_INJECTED/LLKHF_INJECTED）→
//!         按键/按钮/滚轮立刻、鼠标累计位移超过 YIELD_MOVE_PX（停顿 YIELD_IDLE_MS 清零）才
//!         hub.send(Msg::Yield{epoch = inject::driven_epoch()})（最多每 YIELD_MS 一次），事件放行。被控时不检测跨屏。
//! ToInput：PeerUp 更新对方桌面；PeerDown / Pause(true) / GoHome 时若在驱动 → 回到本机（落点 = 出口点）；
//!          LocalDesk 更新本机桌面（并让光标备份失效）；Config 更新排布/速度/热键；
//!          Yield(epoch) → 在驱动且 epoch 为 0 或等于当前驱动轮次才回到本机，之后 YIELD_COOLDOWN_MS 内不跨屏
//!          （两边同时有人操作时不来回抢）；Quit → 卸钩子、若在驱动先回到本机，然后退出消息循环返回。
//!
//! 实现补充：
//! - 落点往本机内侧挪 1 像素，免得沿边上下移动时立刻又跨过去。
//! - 被控时又收到对方 Enter（双方同时跨屏）：本机放手，避免互相驱动。
//! - 看门狗发现钩子丢了且正在驱动：先回到本机（光标恢复可见），再重装钩子。
//!
//! 后来补的规则（评审修复）：
//! - 定时器 250ms 一次：查前台是否管理员窗口（UIPI，钩子收不到它的输入）和输入桌面是否 Default
//!   （Ctrl+Alt+Del/UAC/锁屏）。前台是管理员窗口时不跨屏（状态栏提示）；驱动中遇到这两种情况立刻回到本机。
//!   因安全桌面回来的：输入桌面切回 Default 且不在驱动时，再停一次落点并补一次显示光标；桌面切换时清 hotkey_held。
//! - 看门狗：输入桌面不是 Default、或前台是管理员窗口时不查（钩子本来就收不到），并把 LAST_HOOK 刷成现在。
//!   驱动中（已停放）每 250ms 查一次：光标离停放点超过 REPARK_DIST、钩子 HOOK_IDLE_MS 没被调用、
//!   且这之后确有输入（GetLastInputInfo，排除别的程序 SetCursorPos）→ 一次就判定钩子丢了：回到本机并重装。
//!   不在驱动时每秒查一次，连续两次“有输入但钩子 1 秒没动静”才重装。
//! - 驱动中移动量 = pt - 钩子回调时的真实光标位置（GetCursorPos）；光标离停放点超过 50 像素就在钩子里重新停放。
//! - 旧坐标：钩子回调里同步 SetCursorPos 后，排队中的事件坐标是按新位置算的（实测确认），不会有旧坐标；
//!   但停放在钩子回调外做（WM_APP_HIDDEN、显示器变化）时，已算好坐标、正等着回调的事件还是停放前的坐标。
//!   所以只在回调外停放后的 STALE_MS 内、头 STALE_EVENTS 个事件里，丢掉“离停放前位置比离停放点还近”的事件
//!   （按距离比较，快速甩动的真实位移不会被吞）。
//! - 对方还按着鼠标键（拖动中）时不回到本机，虚拟光标贴边停住。
//! - 回到本机或被控结束后，要先看到光标离开过跨屏那条边，才允许再次跨屏（防止来回弹）；
//!   但贴着边继续往外推够 REARM_PUSH_PX、或从回来后第一次移动起过了 REARM_MS，也放行
//!  （刚回来又想马上过去时，不必先往回挪一下）。
//! - 跨屏时就按着的 Alt/Win，若驱动期间转发过东西，它在本机的抬起改为：注入 vk 0xE8 按下抬起 + 该键抬起，
//!   免得本机弹出菜单栏/开始菜单（在消息循环里 SendInput，不在钩子回调里）。
//! - 光标隐藏/恢复走 cursor::request_hide/request_show（专用线程）；状态更新和 SendInput 推迟到消息循环执行。

use crate::config::Config;
use crate::geom::{Desk, Layout, Point, Side, Step, VirtualCursor};
use crate::hub::{Hub, Out, ToInput, WM_APP_WAKE};
use crate::proto::{Button, INJECT_MARK, Msg};
use crossbeam_channel::Receiver;
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

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
    /// cursor::request_hide(本线程, token)：隐藏完成后消息循环收到 WM_APP_HIDDEN(token) → Kvm::hidden
    HideCursor(u32),
    ShowCursor,
    /// hub.send(Msg::Yield{epoch = inject::driven_epoch()})
    Yield,
    /// hub.set_status(driving)
    Driving(bool),
    /// hub.set_status(paused)
    Paused(bool),
    /// on：status.last_error = msg；off：last_error 仍是 msg 时清掉。
    Error { msg: &'static str, on: bool },
    /// 本机 Alt/Win 抬起前先注入 vk 0xE8 按下抬起，防止弹菜单（scan/ext 是原来那次抬起的）。
    Mask { vk: u16, scan: u16, ext: bool },
}

/// 前台是管理员（更高完整性）窗口时的提示。
pub const FG_BLOCKED_MSG: &str = "前台是管理员窗口，无法切换";

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
    /// 回调时的真实光标位置（GetCursorPos；只有移动事件有，取不到为 None）
    pub cur: Option<Point>,
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

/// 查本机某个虚拟键此刻是否按着（GetAsyncKeyState）。只在贴边要跨屏时调用。
pub type KeyProbe = Box<dyn Fn(usize) -> bool>;

const MOUSE_VKS: [usize; 5] = [0x01, 0x02, 0x04, 0x05, 0x06];
const VK_PACKET: u32 = 0xE7;
const VK_PAUSE: u32 = 0x13;
/// 在钩子回调外停放后这段时间内，可能还有停放前算好坐标的事件。
const STALE_MS: i32 = 50;
/// 回调外停放后只对头几个移动事件做旧坐标过滤。
const STALE_EVENTS: u8 = 3;
/// 光标离停放点超过这个距离就重新停放（看门狗也用它）。
const REPARK_DIST: i64 = 50;
/// 跨屏后等光标线程隐藏完再停放；等这么久还没回就直接停放。
const PARK_WAIT_MS: i32 = 40;
/// 被控时发 Yield 的最小间隔。
const YIELD_MS: i32 = 300;
/// 被控时本机鼠标累计位移超过这么多像素才发 Yield（手碰一下桌子不算）。
const YIELD_MOVE_PX: i64 = 8;
/// 被控时鼠标停这么久，累计位移清零。
const YIELD_IDLE_MS: i32 = 500;
/// 回到本机后光标一直贴着跨屏那条边：从回来后第一次移动起过了这么久，再往外推就放行。
const REARM_MS: i32 = 300;
/// 回到本机后贴着边往外推的累计像素（原始位移）超过这么多，就放行（手抖一两个像素不会弹回去）。
const REARM_PUSH_PX: i64 = 20;
/// 接受对方 Yield 回到本机后，这么久之内不跨屏。
const YIELD_COOLDOWN_MS: i32 = 1500;
/// 驱动中钩子这么久没被调用（而光标被挪走了、也有输入）：钩子丢了。
const HOOK_IDLE_MS: u32 = 250;
/// 最后一次输入比最后一次钩子回调晚这么多才算“钩子之后还有输入”（容掉 GetTickCount 的粒度）。
const INPUT_AFTER_HOOK_MS: i32 = 50;
/// 本机抬起时要防菜单的键：Alt、LAlt、RAlt、LWin、RWin。
const MASK_VKS: [usize; 5] = [0x12, 0xA4, 0xA5, 0x5B, 0x5C];
/// 不在任何显示器上的桌面：传给 VirtualCursor::step 当“本机”，就不会回到本机（拖动中贴边停住）。
static NO_DESK: Desk = Desk { monitors: Vec::new() };

fn dist2(a: Point, b: Point) -> i64 {
    let dx = a.x as i64 - b.x as i64;
    let dy = a.y as i64 - b.y as i64;
    dx * dx + dy * dy
}

/// 两个时间点（GetTickCount）的差，后者在前时为负。
fn since(now: u32, then: u32) -> i32 {
    now.wrapping_sub(then) as i32
}

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
    /// 已停放（跨屏后等光标隐藏完才停放，之前光标还在出口点）
    parked: bool,
    /// 跨屏 / 最近一次停放的时间
    park_time: u32,
    /// 最近一次停放前光标所在的位置（判断旧坐标）
    before: Point,
    /// 还要做旧坐标过滤的事件数
    fresh: u8,
    /// 跨屏时本机按着的键：抬起放行给本机，不转发
    local_held: [bool; 256],
    /// 已转发给对方、还没抬起的键/按钮
    remote_keys: [bool; 256],
    remote_buttons: [bool; 5],
    /// 这次驱动转发过按键/按钮/滚轮（决定本机 Alt/Win 抬起要不要防菜单）
    forwarded: bool,
}

impl Drive {
    /// 停放。outside = 在钩子回调外停放：之后排队中的事件可能还是停放前的坐标，要做旧坐标过滤。
    fn park_now(&mut self, before: Point, time: u32, outside: bool, out: &mut Vec<Action>) {
        self.parked = true;
        self.park_time = time;
        self.before = before;
        self.fresh = if outside { STALE_EVENTS } else { 0 };
        out.push(Action::SetCursor(self.park));
    }
}

pub struct Kvm {
    /// 最近一次顶在跨屏边上没过去的原因（"" = 没被拒）；refuse_why 是 try_cross_at 的工作变量。
    pub refused: &'static str,
    refuse_why: &'static str,
    /// 诊断（KMLINK_TRACE=1）：跨屏后前 32 个移动事件 (time, pt.x, pt.y, cur.x, cur.y, parked, vc.x)。
    pub trace: [(u32, i32, i32, i32, i32, i32, i32); 32],
    pub trace_n: usize,
    /// 最近一次回到本机是哪行代码触发的、当时对方光标在哪（定时器里写日志，钩子里不写）。
    pub home_note: Option<(&'static std::panic::Location<'static>, Point)>,
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
    /// 被控期间本机鼠标的累计位移（像素）和最后一次移动的时间
    yield_acc: i64,
    yield_acc_at: Option<u32>,
    /// 最近一次接受对方 Yield（回到本机）的时间：之后 YIELD_COOLDOWN_MS 内不跨屏
    yielded_at: Option<u32>,
    probe: KeyProbe,
    /// 跨屏已就绪：回到本机/被控结束后，要先看到光标离开过跨屏那条边
    armed: bool,
    /// 没就绪时：回来后第一次移动的时间、贴边往外推的累计像素
    disarmed_at: Option<u32>,
    push: i64,
    /// 前台是管理员窗口（定时器刷新的缓存）
    fg_blocked: bool,
    /// 已经提示过 FG_BLOCKED_MSG
    warned: bool,
    /// 驱动结束时仍按着的 Alt/Win：本机抬起时要防菜单
    mask_up: [bool; 256],
    /// 输入桌面是 Default（上次 poll 的结果）
    desk_ok: bool,
    /// 因切到安全桌面回到本机时的落点：切回 Default 后再停一次、补一次显示光标
    desk_restore: Option<Point>,
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
            yield_acc: 0,
            yield_acc_at: None,
            yielded_at: None,
            home_note: None,
            refused: "",
            refuse_why: "",
            trace: [(0, 0, 0, 0, 0, 0, 0); 32],
            trace_n: 0,
            probe,
            armed: true,
            disarmed_at: None,
            push: 0,
            fg_blocked: false,
            warned: false,
            mask_up: [false; 256],
            desk_ok: true,
            desk_restore: None,
        };
        k.apply_config(cfg);
        k
    }

    #[cfg(test)]
    pub fn driving(&self) -> bool {
        self.drive.is_some()
    }

    /// 正在驱动且已停放（驱动中看门狗只在这时查）。
    pub fn parked(&self) -> bool {
        self.drive.as_ref().is_some_and(|d| d.parked)
    }

    fn apply_config(&mut self, cfg: &Config) {
        self.layout = cfg.layout.layout;
        let s = cfg.input.speed;
        self.speed = if s.is_finite() && s > 0.0 { s.min(20.0) } else { 1.0 };
        self.hotkey = cfg.input.hotkey_vk & 0xFF;
    }

    // ── 被控时请对方放手 ──

    /// 发 Yield（最多每 YIELD_MS 一次）。
    fn yield_now(&mut self, time: u32, out: &mut Vec<Action>) {
        let due = match self.last_yield {
            None => true,
            Some(t) => {
                let dt = since(time, t);
                !(0..YIELD_MS).contains(&dt)
            }
        };
        if due {
            self.last_yield = Some(time);
            self.yield_acc = 0;
            out.push(Action::Yield);
        }
    }

    /// 被控时本机鼠标移动了 dist 像素：累计超过 YIELD_MOVE_PX 才发 Yield。
    fn yield_move(&mut self, time: u32, dist: i64, out: &mut Vec<Action>) {
        if self.yield_acc_at.is_some_and(|t| !(0..=YIELD_IDLE_MS).contains(&since(time, t))) {
            self.yield_acc = 0;
        }
        self.yield_acc_at = Some(time);
        self.yield_acc = self.yield_acc.saturating_add(dist);
        if self.yield_acc > YIELD_MOVE_PX {
            self.yield_now(time, out);
        }
    }

    // ── 鼠标 ──

    pub fn mouse(&mut self, ev: MouseEv, out: &mut Vec<Action>) -> Verdict {
        if self.drive.is_some() {
            return self.drive_mouse(ev, out);
        }
        if self.driven && !ev.injected {
            match ev.kind {
                MouseKind::Move => {
                    let c = ev.cur.unwrap_or(ev.pt);
                    let d = (ev.pt.x as i64 - c.x as i64).abs() + (ev.pt.y as i64 - c.y as i64).abs();
                    self.yield_move(ev.time, d, out);
                }
                _ => self.yield_now(ev.time, out),
            }
        }
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
                if self.try_cross(ev.pt, ev.cur, ev.time, out) {
                    Verdict::Swallow
                } else {
                    Verdict::Pass
                }
            }
        }
    }

    fn try_cross(&mut self, pt: Point, cur: Option<Point>, time: u32, out: &mut Vec<Action>) -> bool {
        // 钩子里的坐标可能越出屏幕：先钳回桌面（钳在当前所在的显示器里）
        let c = cur.unwrap_or(pt);
        let p = self.mine.clamp_from(c, pt);
        // 这次移动往跨屏那条边外推了多少（原始位移；光标已顶在边上时钩子坐标会越出屏幕）
        let outward = match self.layout.side {
            Side::Left => c.x as i64 - pt.x as i64,
            Side::Right => pt.x as i64 - c.x as i64,
            Side::Top => c.y as i64 - pt.y as i64,
            Side::Bottom => pt.y as i64 - c.y as i64,
        };
        let r = self.try_cross_at(p, outward.max(0), time, out);
        // 顶在跨屏那条边上却没过去：记下原因（定时器里原因变化时写一行日志，钩子里不写）
        self.refused = if !r && self.mine.on_outer_edge(self.layout.side, p) { self.refuse_why } else { "" };
        r
    }

    fn try_cross_at(&mut self, p: Point, outward: i64, time: u32, out: &mut Vec<Action>) -> bool {
        self.refuse_why = "";
        if !self.armed {
            // 回来后先要离开过跨屏那条边；或者贴着边继续往外推够了 / 过了一会儿
            if !self.mine.on_outer_edge(self.layout.side, p) {
                self.armed = true;
                return false;
            }
            let t0 = *self.disarmed_at.get_or_insert(time);
            self.push = self.push.saturating_add(outward);
            let waited = !(0..REARM_MS).contains(&since(time, t0));
            if !(waited && outward > 0) && self.push < REARM_PUSH_PX {
                self.refuse_why = "刚回到本机，先离开这条边再过去";
                return false;
            }
            self.armed = true;
        }
        if self.paused || self.driven {
            self.refuse_why = if self.paused { "已暂停共享" } else { "本机正被对方控制" };
            return false;
        }
        if let Some(t) = self.yielded_at {
            if (0..YIELD_COOLDOWN_MS).contains(&since(time, t)) {
                self.refuse_why = "对方刚请本机放手，1.5 秒内不过去";
                return false; // 对方刚请本机放手：先别再过去抢
            }
            self.yielded_at = None;
        }
        let Some(peer) = self.peer.as_ref() else {
            self.refuse_why = "还没连上对方";
            return false;
        };
        if peer.is_empty() || self.mine.is_empty() {
            self.refuse_why = "不知道对方的显示器";
            return false;
        }
        let Some(entry) = self.layout.exit_to_peer(&self.mine, peer, p) else {
            return false;
        };
        if self.fg_blocked {
            // 前台是管理员窗口：钩子收不到它的按键，跨过去键盘会落在本机
            self.refuse_why = "前台是管理员窗口";
            self.warn(out);
            return false;
        }
        if MOUSE_VKS.iter().any(|&v| (self.probe)(v)) {
            self.refuse_why = "鼠标键按着（拖动中）";
            return false; // 拖动中不跨屏
        }
        let mut held = [false; 256];
        for (v, h) in held.iter_mut().enumerate().skip(1) {
            if !MOUSE_VKS.contains(&v) {
                *h = (self.probe)(v);
            }
        }
        self.epoch = self.epoch.wrapping_add(1);
        self.seq = 0;
        self.desk_restore = None;
        let park = self.mine.park_point();
        // 上次驱动留下的 Alt/Win 防菜单标记并入这次
        let forwarded = MASK_VKS.iter().any(|&v| self.mask_up[v] && held[v]);
        self.mask_up = [false; 256];
        self.drive = Some(Drive {
            vc: VirtualCursor::new(entry),
            exit: p,
            park,
            parked: false,
            park_time: time,
            before: p,
            fresh: 0,
            local_held: held,
            remote_keys: [false; 256],
            remote_buttons: [false; 5],
            forwarded,
        });
        out.push(Action::Send(Msg::Enter { epoch: self.epoch, x: entry.x, y: entry.y }));
        // 先隐藏、隐藏完再停放（见 hidden）
        out.push(Action::HideCursor(self.epoch));
        self.trace_n = 0;
        out.push(Action::Driving(true));
        true
    }

    /// 光标线程隐藏完成（消息循环里调用；cur = 此刻真实光标位置）：停放到屏幕中间。
    pub fn hidden(&mut self, token: u32, cur: Option<Point>, now: u32, out: &mut Vec<Action>) {
        if token != self.epoch {
            return;
        }
        let Some(d) = self.drive.as_mut() else {
            return;
        };
        if d.parked {
            return;
        }
        let before = cur.unwrap_or(d.exit);
        d.park_now(before, now, true, out);
    }

    fn drive_mouse(&mut self, ev: MouseEv, out: &mut Vec<Action>) -> Verdict {
        let epoch = self.epoch;
        let Some(d) = self.drive.as_mut() else {
            return Verdict::Pass;
        };
        match ev.kind {
            MouseKind::Move => {
                // 吞掉的事件不会移动光标：位移 = pt - 此刻真实光标位置
                let cur = ev.cur.unwrap_or(if d.parked { d.park } else { d.exit });
                let since_park = since(ev.time, d.park_time);
                if d.fresh > 0 {
                    d.fresh -= 1;
                    // 回调外停放前就算好坐标的事件：离停放前的位置比离停放点还近
                    if since_park < STALE_MS && dist2(ev.pt, d.before) < dist2(ev.pt, d.park) {
                        return Verdict::Swallow;
                    }
                }
                if !d.parked {
                    // 光标线程迟迟没隐藏完：直接停放（可能闪一下，但不能一直停在边上）
                    if since_park >= PARK_WAIT_MS {
                        d.park_now(cur, ev.time, false, out);
                    }
                } else if dist2(cur, d.park) > REPARK_DIST * REPARK_DIST && since_park >= STALE_MS {
                    // 光标被别的程序挪走了（SetCursorPos、对齐默认按钮、ClipCursor）：重新停放
                    d.park_now(cur, ev.time, false, out);
                }
                let dx = ev.pt.x.saturating_sub(cur.x);
                let dy = ev.pt.y.saturating_sub(cur.y);
                if TRACE.load(Ordering::Relaxed) && self.trace_n < self.trace.len() {
                    self.trace[self.trace_n] = (ev.time, ev.pt.x, ev.pt.y, cur.x, cur.y, d.parked as i32, d.vc.pos.x);
                    self.trace_n += 1;
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
                // 对方还按着鼠标键（拖动中）：不回到本机，贴边停住
                let mine = if d.remote_buttons.iter().any(|b| *b) { &NO_DESK } else { &self.mine };
                match d.vc.step(dx, dy, self.speed, &self.layout, mine, peer) {
                    Step::Moved(p) => {
                        if p != before {
                            self.seq = self.seq.wrapping_add(1);
                            out.push(Action::Move { epoch, seq: self.seq, x: p.x, y: p.y });
                        }
                    }
                    Step::Return(p) => {
                        self.go_home(p, out);
                    }
                }
            }
            MouseKind::Button(b, down) => {
                let i = button_index(b);
                d.remote_buttons[i] = down;
                d.forwarded = true;
                self.swallow_buttons[i] = false;
                let p = d.vc.pos;
                out.push(Action::Send(Msg::Button { epoch, x: p.x, y: p.y, button: b, down }));
            }
            MouseKind::Wheel { delta, horizontal } => {
                if delta != 0 {
                    d.forwarded = true;
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
                if d.forwarded && MASK_VKS.contains(&vk) {
                    // 本机只看到 Alt/Win 按下又抬起会弹菜单：改成先注入掩码键再抬起
                    out.push(mask(&ev));
                    return Verdict::Swallow;
                }
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
            d.forwarded = true;
            self.swallow_keys[vk] = false;
            out.push(Action::Send(Msg::Key { epoch, scan: ev.scan, ext: ev.ext, vk: ev.vk as u16, down: ev.down }));
            return Verdict::Swallow;
        }

        if self.driven && !ev.injected {
            self.yield_now(ev.time, out);
        }
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
        // 驱动结束时还按着的 Alt/Win：自动重复的按下照常放行，抬起时防菜单
        if self.mask_up[vk] && !ev.down {
            self.mask_up[vk] = false;
            out.push(mask(&ev));
            return Verdict::Swallow;
        }
        if self.swallow_keys[vk] {
            self.swallow_keys[vk] = false;
            if ev.down {
                // 转发给对方的键回家后自动重复的按下：本机重新看到了按下，放行。
                // 若是 Alt/Win，本机之后只会看到它按下又抬起：抬起时防菜单
                if MASK_VKS.contains(&vk) {
                    self.mask_up[vk] = true;
                }
                return Verdict::Pass;
            }
            return Verdict::Swallow;
        }
        Verdict::Pass
    }

    // ── 回到本机 ──

    /// 回到本机，返回实际落点（不在驱动时 None）。
    #[track_caller]
    fn go_home(&mut self, land: Point, out: &mut Vec<Action>) -> Option<Point> {
        let d = self.drive.take()?;
        self.home_note = Some((std::panic::Location::caller(), d.vc.pos));
        for (s, r) in self.swallow_keys.iter_mut().zip(d.remote_keys.iter()) {
            *s |= *r;
        }
        for (s, r) in self.swallow_buttons.iter_mut().zip(d.remote_buttons.iter()) {
            *s |= *r;
        }
        if d.forwarded {
            for v in MASK_VKS {
                self.mask_up[v] = d.local_held[v];
            }
        }
        // 先离开跨屏那条边才能再跨（防止立刻弹回去）
        self.disarm();
        let land = self.inward(land);
        out.push(Action::Send(Msg::Leave { epoch: self.epoch }));
        out.push(Action::SetCursor(land));
        out.push(Action::ShowCursor);
        out.push(Action::Driving(false));
        Some(land)
    }

    /// 先离开跨屏那条边（或贴边推够了 / 过了 REARM_MS）才能再跨。
    fn disarm(&mut self) {
        self.armed = false;
        self.disarmed_at = None;
        self.push = 0;
    }

    /// 落点钳进本机桌面，再往内侧挪 1 像素（若还在屏幕上）。
    fn inward(&self, p: Point) -> Point {
        let p = self.mine.clamp(p);
        let q = match self.layout.side {
            Side::Right => Point::new(p.x.saturating_sub(1), p.y),
            Side::Left => Point::new(p.x.saturating_add(1), p.y),
            Side::Bottom => Point::new(p.x, p.y.saturating_sub(1)),
            Side::Top => Point::new(p.x, p.y.saturating_add(1)),
        };
        if self.mine.contains(q) { q } else { p }
    }

    fn exit_point(&self) -> Option<Point> {
        self.drive.as_ref().map(|d| d.exit)
    }

    #[track_caller]
    fn go_home_exit(&mut self, out: &mut Vec<Action>) -> Option<Point> {
        let e = self.exit_point()?;
        self.go_home(e, out)
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
                    let before = if d.parked { d.park } else { d.exit };
                    d.park = park;
                    d.exit = exit;
                    d.park_now(before, now, true, out);
                }
            }
            ToInput::Config(cfg) => self.apply_config(&cfg),
            ToInput::Yield(epoch) => {
                // 只认当前这一轮（0 = 对方不确定，总是接受）；旧轮次晚到的 Yield 不能把新一轮踢回来
                if self.drive.is_some() && (epoch == 0 || epoch == self.epoch) {
                    self.go_home_exit(out);
                    self.yielded_at = Some(now);
                }
            }
            ToInput::GoHome => {
                self.go_home_exit(out);
            }
            ToInput::Driven(b) => {
                self.driven = b;
                self.last_yield = None;
                self.yield_acc = 0;
                self.yield_acc_at = None;
                if b {
                    self.go_home_exit(out); // 双方同时跨屏：本机放手
                } else {
                    // 对方放手时本机光标可能正停在共用的那条边上：先离开过边才跨
                    self.disarm();
                }
            }
            ToInput::Pause(p) => {
                self.paused = p;
                if p {
                    self.go_home_exit(out);
                }
                out.push(Action::Paused(p));
            }
            ToInput::Quit => {
                self.go_home_exit(out);
            }
        }
    }

    /// 看门狗发现钩子被系统摘掉：正在驱动就先回到本机。
    pub fn hooks_lost(&mut self, out: &mut Vec<Action>) {
        self.go_home_exit(out);
    }

    /// 驱动中看门狗（定时器每 250ms）：cur = 真实光标位置；hook_idle = 距最后一次钩子回调的毫秒数；
    /// input_after_hook = 最后一次钩子回调之后还有过输入（GetLastInputInfo）。
    /// 光标被真实输入挪离了停放点、钩子却没动静 → 钩子丢了：回到本机，返回 true（调用方重装钩子）。
    pub fn hooks_check(&mut self, cur: Option<Point>, hook_idle: u32, input_after_hook: bool, out: &mut Vec<Action>) -> bool {
        if self.fg_blocked {
            return false;
        }
        let (Some(d), Some(c)) = (self.drive.as_ref(), cur) else {
            return false;
        };
        if !d.parked || dist2(c, d.park) <= REPARK_DIST * REPARK_DIST || hook_idle <= HOOK_IDLE_MS || !input_after_hook {
            return false;
        }
        self.go_home_exit(out);
        true
    }

    /// 定时器（250ms）：fg_blocked = 前台是管理员窗口；desk_ok = 输入桌面是 Default。
    /// 驱动中任一不满足就立刻回到本机（Leave 让对方抬起所有键）。
    pub fn poll(&mut self, fg_blocked: bool, desk_ok: bool, out: &mut Vec<Action>) {
        self.fg_blocked = fg_blocked;
        if self.drive.is_some() && (!desk_ok || fg_blocked) {
            let land = self.go_home_exit(out);
            if desk_ok {
                self.warn(out);
            } else {
                // 安全桌面上 SetCursorPos/恢复光标可能没生效：切回来后再做一次
                self.desk_restore = land;
            }
        }
        if desk_ok != self.desk_ok {
            self.desk_ok = desk_ok;
            // 安全桌面上钩子收不到热键的抬起
            self.hotkey_held = false;
            if desk_ok && self.drive.is_none() {
                if let Some(p) = self.desk_restore.take() {
                    out.push(Action::SetCursor(p));
                    out.push(Action::ShowCursor);
                }
            }
        }
        if !fg_blocked && self.warned {
            self.warned = false;
            out.push(Action::Error { msg: FG_BLOCKED_MSG, on: false });
        }
    }

    fn warn(&mut self, out: &mut Vec<Action>) {
        if !self.warned {
            self.warned = true;
            out.push(Action::Error { msg: FG_BLOCKED_MSG, on: true });
        }
    }
}

/// 本机 Alt/Win 的抬起改成“掩码键 + 抬起”。
fn mask(ev: &KeyEv) -> Action {
    Action::Mask { vk: (ev.vk & 0xFF) as u16, scan: ev.scan, ext: ev.ext }
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

use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetLastInputInfo, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, LASTINPUTINFO, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, KillTimer, LLKHF_EXTENDED,
    LLKHF_INJECTED, LLKHF_UP, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW, PostQuitMessage,
    PostThreadMessageW, SetCursorPos, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WM_APP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_TIMER, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

/// 看门狗探针（注入 1 像素移动 + 一个空键 vk 0xE8 的抬起，两个钩子认出来就吞掉）：
/// dwExtraInfo 标记、鼠标/键盘钩子是否各自看到了。两个都看到了才算钩子都还在。
const PROBE_MARK: usize = 0x4B4D_5052;
static PROBE_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static KEY_PROBE_SEEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 本机最后一次物理输入（钩子看到的、不是注入的）的 GetTickCount；0 = 还没有。
static LAST_PHYSICAL: AtomicU32 = AtomicU32::new(0);

/// 本机最后一次物理键鼠输入的时间（GetTickCount 时基）。任意线程可读。
#[cfg_attr(test, allow(dead_code))]
pub fn last_physical_input() -> Option<u32> {
    match LAST_PHYSICAL.load(Ordering::Relaxed) {
        0 => None,
        t => Some(t),
    }
}

fn note_physical() {
    LAST_PHYSICAL.store(tick().max(1), Ordering::Relaxed);
}

thread_local! {
    static ST: RefCell<Option<Kvm>> = const { RefCell::new(None) };
    static HUB: RefCell<Option<Arc<Hub>>> = const { RefCell::new(None) };
    /// 本线程 id（光标线程隐藏完发 WM_APP_HIDDEN 回来）。
    static TID: Cell<u32> = const { Cell::new(0) };
    /// 最后一次钩子回调的 GetTickCount（看门狗用）。
    static LAST_HOOK: Cell<u32> = const { Cell::new(0) };
    /// 状态机输出的动作（复用，钩子回调里不分配）。
    static OUT: RefCell<Vec<Action>> = const { RefCell::new(Vec::new()) };
    /// 推迟到消息循环执行的动作。
    static DEFER: RefCell<Vec<Action>> = const { RefCell::new(Vec::new()) };
}

/// 自己发给自己：执行 DEFER 里的动作。
const WM_APP_DEFER: u32 = WM_APP + 2;
/// 未分配的虚拟键，用来打断 Alt/Win 单按（同 AutoHotkey 的 vkE8）。
const VK_MASK: u16 = 0xE8;
/// 定时器周期：前台/桌面检查、驱动中看门狗；不在驱动时看门狗每 WATCHDOG_TICKS 次查一次。
const TICK_MS: u32 = 250;
const WATCHDOG_TICKS: u32 = 4;

fn tick() -> u32 {
    unsafe { GetTickCount() }
}

/// 在状态机上跑一步，释放借用后再执行它要求的动作。借不到（重入）就放行。
fn dispatch(f: impl FnOnce(&mut Kvm, &mut Vec<Action>) -> Verdict) -> Verdict {
    OUT.with(|o| match o.try_borrow_mut() {
        Ok(mut out) => dispatch_into(&mut out, f),
        // 重入（不应发生）：临时缓冲区
        Err(_) => dispatch_into(&mut Vec::new(), f),
    })
}

fn dispatch_into(out: &mut Vec<Action>, f: impl FnOnce(&mut Kvm, &mut Vec<Action>) -> Verdict) -> Verdict {
    let v = ST.with(|st| match st.try_borrow_mut() {
        Ok(mut g) => match g.as_mut() {
            Some(k) => f(k, out),
            None => Verdict::Pass,
        },
        Err(_) => Verdict::Pass,
    });
    if !out.is_empty() {
        execute(out);
    }
    v
}

/// 钩子回调里可能执行：只做不阻塞的事（channel、SetCursorPos）。执行完清空 actions（保留容量）。
/// 要拿锁的状态更新和 SendInput 推迟到消息循环（WM_APP_DEFER）。
fn execute(actions: &mut Vec<Action>) {
    let mut deferred = false;
    HUB.with(|h| {
        let h = h.borrow();
        let Some(hub) = h.as_ref() else {
            actions.clear();
            return;
        };
        for a in actions.drain(..) {
            match a {
                Action::Send(m) => hub.send(m),
                Action::Move { epoch, seq, x, y } => hub.to_net(Out::Move { epoch, seq, x, y }),
                Action::SetCursor(p) => unsafe {
                    let _ = SetCursorPos(p.x, p.y);
                },
                Action::HideCursor(token) => crate::cursor::request_hide(TID.with(|t| t.get()), token),
                Action::ShowCursor => crate::cursor::request_show(),
                Action::Yield => hub.send(Msg::Yield { epoch: crate::inject::driven_epoch() }),
                a @ (Action::Driving(_) | Action::Paused(_) | Action::Error { .. } | Action::Mask { .. }) => {
                    DEFER.with(|d| d.borrow_mut().push(a));
                    deferred = true;
                }
            }
        }
    });
    if deferred {
        unsafe {
            let _ = PostThreadMessageW(GetCurrentThreadId(), WM_APP_DEFER, WPARAM(0), LPARAM(0));
        }
    }
}

/// 消息循环里执行推迟的动作（DEFER 保留容量，钩子回调里 push 不分配）。
fn run_deferred(hub: &Hub) {
    let list: Vec<Action> = DEFER.with(|d| d.borrow_mut().drain(..).collect());
    for a in list {
        match a {
            Action::Driving(b) => hub.set_status(|s| s.driving = b),
            Action::Paused(b) => hub.set_status(|s| s.paused = b),
            Action::Error { msg, on } => hub.set_status(|s| {
                if on {
                    s.last_error = msg.to_string();
                } else if s.last_error == msg {
                    s.last_error.clear();
                }
            }),
            Action::Mask { vk, scan, ext } => send_mask(vk, scan, ext),
            _ => {}
        }
    }
}

/// 注入 [vk 0xE8 按下, 0xE8 抬起, vk 抬起]（INJECT_MARK，本机钩子不处理），顺序固定：先掩码后抬起。
fn send_mask(vk: u16, scan: u16, ext: bool) {
    let key = |vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: INJECT_MARK },
        },
    };
    let up_flags = if ext { KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY } else { KEYEVENTF_KEYUP };
    let v = [key(VK_MASK, 0, KEYBD_EVENT_FLAGS(0)), key(VK_MASK, 0, KEYEVENTF_KEYUP), key(vk, scan, up_flags)];
    unsafe {
        let _ = SendInput(&v, std::mem::size_of::<INPUT>() as i32);
    }
}

fn cursor_pos() -> Option<Point> {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p) }.ok().map(|_| Point::new(p.x, p.y))
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
    // 移动事件：此刻真实光标位置（回调时还没被这次事件移动）
    let cur = if kind == MouseKind::Move { cursor_pos() } else { None };
    Some(MouseEv {
        kind,
        pt: Point::new(ms.pt.x, ms.pt.y),
        cur,
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
        if ms.dwExtraInfo == PROBE_MARK {
            PROBE_SEEN.store(true, Ordering::Relaxed);
            return LRESULT(1);
        }
        if ms.dwExtraInfo != INJECT_MARK {
            if ms.flags & LLMHF_INJECTED == 0 {
                note_physical();
            }
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
        if kb.dwExtraInfo == PROBE_MARK {
            KEY_PROBE_SEEN.store(true, Ordering::Relaxed);
            return LRESULT(1);
        }
        if kb.dwExtraInfo != INJECT_MARK {
            let ev = key_event(kb);
            if !ev.injected {
                note_physical();
            }
            if dispatch(|k, out| k.key(ev, out)) == Verdict::Swallow {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

fn probe_key(vk: usize) -> bool {
    (unsafe { GetAsyncKeyState(vk as i32) } as u16) & 0x8000 != 0
}

#[derive(Default)]
struct Hooks {
    mouse: Option<HHOOK>,
    key: Option<HHOOK>,
}

impl Hooks {
    fn install(&mut self) {
        self.uninstall();
        // 看门狗拿“最后一次输入”和“最后一次钩子回调”比：刚装好时算作刚回调过，
        // 否则启动后（或重装后）第一次检查会拿装钩子之前的输入误判成钩子丢了
        LAST_HOOK.with(|c| c.set(tick()));
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
/// 诊断开关：环境变量 KMLINK_TRACE=1。
pub static TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn run(hub: Arc<Hub>, rx: Receiver<ToInput>) {
    TRACE.store(std::env::var_os("KMLINK_TRACE").is_some_and(|v| v == "1"), Ordering::Relaxed);
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
    ST.with(|s| *s.borrow_mut() = Some(Kvm::new(mine, &cfg, epoch, Box::new(probe_key))));
    HUB.with(|h| *h.borrow_mut() = Some(hub.clone()));
    TID.with(|t| t.set(unsafe { GetCurrentThreadId() }));
    // 钩子回调里 push 不再分配
    OUT.with(|o| o.borrow_mut().reserve(16));
    DEFER.with(|d| d.borrow_mut().reserve(16));

    crate::cursor::start_worker();
    let mut hooks = Hooks::default();
    hooks.install();
    let timer = unsafe { SetTimer(None, 0, TICK_MS, None) };
    if timer == 0 {
        crate::log!("看门狗定时器创建失败");
    }
    hub.set_input_tid(unsafe { GetCurrentThreadId() });

    let mut ticks: u32 = 0;
    let mut trips: u32 = 0;
    let mut lost_streak: u32 = 0;
    let mut false_alarms: u32 = 0;
    let mut last_refused: &'static str = "";
    let mut desk_was_ok = true;
    let mut quitting = false;
    loop {
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 == 0 || r.0 == -1 {
            break;
        }
        match msg.message {
            WM_APP_WAKE => {}
            WM_APP_DEFER => run_deferred(&hub),
            crate::cursor::WM_APP_HIDDEN => {
                let token = msg.wParam.0 as u32;
                let cur = cursor_pos();
                let now = tick();
                dispatch(|k, out| {
                    k.hidden(token, cur, now, out);
                    Verdict::Pass
                });
            }
            WM_TIMER => {
                ticks = ticks.wrapping_add(1);
                let desk_ok = crate::elev::input_desktop_default();
                if desk_ok != desk_was_ok {
                    desk_was_ok = desk_ok;
                    crate::log!("输入桌面{}", if desk_ok { "回到 Default" } else { "切走了（安全桌面/锁屏）" });
                }
                let blocked = desk_ok && crate::elev::foreground_blocked();
                dispatch(|k, out| {
                    k.poll(blocked, desk_ok, out);
                    if k.refused != last_refused {
                        if !k.refused.is_empty() {
                            crate::log!("顶在边上没跨过去：{}", k.refused);
                        }
                        last_refused = k.refused;
                    }
                    if let Some((loc, p)) = k.home_note.take() {
                        crate::log!("回到本机（input.rs:{}，对方光标 {},{}）", loc.line(), p.x, p.y);
                        if TRACE.load(Ordering::Relaxed) {
                            for t in &k.trace[..k.trace_n] {
                                crate::log!("  trace t={} pt={},{} cur={},{} parked={} vc.x={}", t.0, t.1, t.2, t.3, t.4, t.5, t.6);
                            }
                            k.trace_n = 0;
                        }
                    }
                    Verdict::Pass
                });
                if !desk_ok || blocked {
                    // 安全桌面 / 前台是管理员窗口：钩子本来就收不到输入，不算，之后也不把这段输入算进去
                    trips = 0;
                    LAST_HOOK.with(|c| c.set(tick()));
                } else if drive_watchdog() {
                    trips = 0;
                    lost_streak += 1;
                    if lost_streak == 1 || lost_streak % 30 == 0 {
                        crate::log!("驱动中钩子没反应、光标被挪离停放点：钩子被系统摘掉了（第 {lost_streak} 次），回到本机并重装");
                    }
                    hooks.install();
                } else if ticks % WATCHDOG_TICKS == 0 {
                    // 连续两次异常才算钩子丢了（避开空闲后第一次输入的竞态）
                    trips = if watchdog_tripped() { trips + 1 } else { 0 };
                    if trips >= 2 && hook_alive() {
                        // 钩子其实还在：那段输入来自钩子看不到的来源（触摸/笔/别的设备），不重装
                        trips = 0;
                        false_alarms += 1;
                        if false_alarms == 1 || false_alarms % 30 == 0 {
                            crate::log!("看门狗误报（第 {false_alarms} 次）：钩子还在，有钩子看不到的输入");
                        }
                        LAST_HOOK.with(|c| c.set(tick()));
                    } else if trips >= 2 {
                        trips = 0;
                        lost_streak += 1;
                        if lost_streak == 1 || lost_streak % 30 == 0 {
                            crate::log!("钩子似乎被系统摘掉了（第 {lost_streak} 次），重装");
                        }
                        dispatch(|k, out| {
                            k.hooks_lost(out);
                            Verdict::Pass
                        });
                        hooks.install();
                    } else if trips == 0 {
                        lost_streak = 0;
                    }
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
    run_deferred(&hub);
    // 日志是后台线程写的：退出前尽量写完
    crate::log::flush(300);
    ST.with(|s| s.borrow_mut().take());
    HUB.with(|h| h.borrow_mut().take());
}

/// 处理所有待处理的 ToInput；收到 Quit 返回 true。
fn drain(rx: &Receiver<ToInput>) -> bool {
    while let Ok(m) = rx.try_recv() {
        let quit = matches!(m, ToInput::Quit);
        if let ToInput::LocalDesk(d) = &m {
            crate::log!("本机显示器变化：{:?}", d.monitors);
            // DPI/显示器变了：光标备份可能不再合适，下次恢复重新加载
            crate::cursor::invalidate();
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

/// 先让排队中的钩子调用跑完（PeekMessage 会处理发给本线程的钩子回调）。
fn flush_hook_calls() {
    let mut m = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut m, None, 0, 0, PM_NOREMOVE);
    }
}

/// 最后一次输入的时间（GetLastInputInfo）。
fn last_input_time() -> Option<u32> {
    let mut lii = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe { GetLastInputInfo(&mut lii) }.as_bool().then_some(lii.dwTime)
}

/// 驱动中（已停放）的看门狗：钩子丢了就回到本机，返回 true（调用方重装）。
fn drive_watchdog() -> bool {
    let parked = ST.with(|s| s.try_borrow().ok().is_some_and(|g| g.as_ref().is_some_and(|k| k.parked())));
    if !parked {
        return false;
    }
    flush_hook_calls();
    let cur = cursor_pos();
    let last_hook = LAST_HOOK.with(|c| c.get());
    let idle = tick().wrapping_sub(last_hook);
    let input_after = last_input_time().is_some_and(|t| since(t, last_hook) > INPUT_AFTER_HOOK_MS);
    let mut lost = false;
    dispatch(|k, out| {
        lost = k.hooks_check(cur, idle, input_after, out);
        Verdict::Pass
    });
    lost
}

/// 注入探针（1 像素移动 + 空键 vk 0xE8 的抬起），看两个钩子还收不收得到（收到就吞掉，光标不动、没有按键）。
/// 两个都收到才返回 true。最多等 100ms。钩子真丢了时漏到系统的只是 1 像素移动和一个没人用的键的抬起。
fn hook_alive() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT};
    PROBE_SEEN.store(false, Ordering::Relaxed);
    KEY_PROBE_SEEN.store(false, Ordering::Relaxed);
    let m = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx: 1, dy: 0, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: PROBE_MARK },
        },
    };
    let k = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(VK_MASK), wScan: 0, dwFlags: KEYEVENTF_KEYUP, time: 0, dwExtraInfo: PROBE_MARK },
        },
    };
    if unsafe { SendInput(&[m, k], std::mem::size_of::<INPUT>() as i32) } < 2 {
        return false;
    }
    let t0 = tick();
    loop {
        flush_hook_calls();
        if PROBE_SEEN.load(Ordering::Relaxed) && KEY_PROBE_SEEN.load(Ordering::Relaxed) {
            return true;
        }
        if tick().wrapping_sub(t0) > 100 {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// 有输入发生、但钩子 1 秒以上没被调用：钩子丢了。
fn watchdog_tripped() -> bool {
    flush_hook_calls();
    let Some(t) = last_input_time() else {
        return false;
    };
    let last_hook = LAST_HOOK.with(|c| c.get());
    since(t, last_hook) > 1000
}

// ───────────────────────── 测试 ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Rect;
    use std::rc::Rc;

    const E0: u32 = 100;
    const PARK: Point = Point::new(960, 540);
    const EDGE: Point = Point::new(1919, 500);

    struct T {
        k: Kvm,
        held: Rc<RefCell<[bool; 256]>>,
        /// probe 被调用的次数
        probes: Rc<Cell<u32>>,
        t: u32,
    }

    fn desk() -> Desk {
        Desk::new(vec![Rect::new(0, 0, 1920, 1080)])
    }

    fn setup() -> T {
        let held = Rc::new(RefCell::new([false; 256]));
        let probes = Rc::new(Cell::new(0));
        let (h2, p2) = (held.clone(), probes.clone());
        let cfg = Config::default(); // 对方在右侧，热键 Scroll Lock
        let probe = move |vk: usize| {
            p2.set(p2.get() + 1);
            h2.borrow()[vk]
        };
        let mut k = Kvm::new(desk(), &cfg, E0, Box::new(probe));
        let mut out = vec![];
        k.msg(ToInput::PeerUp { name: "b".into(), desk: desk() }, 0, &mut out);
        assert!(out.is_empty());
        T { k, held, probes, t: 1000 }
    }

    impl T {
        fn mv(&mut self, x: i32, y: i32) -> (Verdict, Vec<Action>) {
            self.t += 8;
            self.mouse(MouseKind::Move, Point::new(x, y), false)
        }
        fn mouse(&mut self, kind: MouseKind, pt: Point, injected: bool) -> (Verdict, Vec<Action>) {
            self.mouse_cur(kind, pt, PARK, injected)
        }
        /// cur：回调时的真实光标位置（默认在停放点）
        fn mouse_cur(&mut self, kind: MouseKind, pt: Point, cur: Point, injected: bool) -> (Verdict, Vec<Action>) {
            let mut out = vec![];
            let v = self.k.mouse(MouseEv { kind, pt, cur: Some(cur), time: self.t, injected }, &mut out);
            (v, out)
        }
        fn poll(&mut self, blocked: bool, desk_ok: bool) -> Vec<Action> {
            let mut out = vec![];
            self.k.poll(blocked, desk_ok, &mut out);
            out
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
        fn hidden(&mut self, token: u32, cur: Point) -> Vec<Action> {
            self.t += 1;
            let mut out = vec![];
            self.k.hidden(token, Some(cur), self.t, &mut out);
            out
        }
        fn hooks_check(&mut self, cur: Point, idle: u32, input_after: bool) -> (bool, Vec<Action>) {
            let mut out = vec![];
            let lost = self.k.hooks_check(Some(cur), idle, input_after, &mut out);
            (lost, out)
        }
        /// 跨到右边（还没停放）。
        fn cross_unparked(&mut self) {
            // 先离开边缘（回来/被控结束后要重新就绪）
            assert_eq!(self.mv(1000, 500).0, Verdict::Pass);
            let (v, out) = self.mv(EDGE.x, EDGE.y);
            assert_eq!(v, Verdict::Swallow);
            let e = self.k.epoch;
            assert_eq!(
                out,
                vec![Action::Send(Msg::Enter { epoch: e, x: 0, y: 500 }), Action::HideCursor(e), Action::Driving(true)]
            );
            assert!(self.k.driving());
            assert!(!self.k.parked());
        }
        /// 跨到右边，光标线程隐藏完后停放。
        fn cross(&mut self) {
            self.cross_unparked();
            let e = self.k.epoch;
            assert_eq!(self.hidden(e, EDGE), vec![Action::SetCursor(PARK)]);
            assert!(self.k.parked());
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
    fn crossing_hides_first_then_parks() {
        let mut t = setup();
        t.cross_unparked();
        let e = t.k.epoch;
        // 还没隐藏完：光标仍在出口点，位移按真实光标位置算，照常转发
        let (v, out) = t.mouse_cur(MouseKind::Move, Point::new(1925, 500), EDGE, false);
        assert_eq!((v, out), (Verdict::Swallow, vec![Action::Move { epoch: e, seq: 1, x: 6, y: 500 }]));
        // 别的轮次的通知不管
        assert!(t.hidden(e.wrapping_sub(1), EDGE).is_empty());
        // 隐藏完：停放，只停一次
        assert_eq!(t.hidden(e, EDGE), vec![Action::SetCursor(PARK)]);
        assert!(t.hidden(e, EDGE).is_empty());
        // 停放前算好坐标的事件（离出口点更近）：丢弃
        t.t += 2;
        assert_eq!(t.mouse(MouseKind::Move, Point::new(1921, 500), false), (Verdict::Swallow, vec![]));
        // 快速甩动：位移很大但离停放点更近，照常转发（旧逻辑会吞掉）
        let (_, out) = t.mouse(MouseKind::Move, Point::new(PARK.x + 400, PARK.y), false);
        assert_eq!(out, vec![Action::Move { epoch: e, seq: 2, x: 406, y: 500 }]);
        // 停放点附近的小移动照常
        let (_, out) = t.mouse(MouseKind::Move, Point::new(PARK.x + 4, PARK.y), false);
        assert_eq!(out, vec![Action::Move { epoch: e, seq: 3, x: 410, y: 500 }]);
        // 回家后再收到通知：不停放
        t.key(0x91, true);
        assert!(t.hidden(e, EDGE).is_empty());
    }

    #[test]
    fn park_fallback_when_hide_is_slow() {
        let mut t = setup();
        t.cross_unparked();
        let e = t.k.epoch;
        t.t += 50;
        let (_, out) = t.mouse_cur(MouseKind::Move, Point::new(1922, 500), EDGE, false);
        assert_eq!(out, vec![Action::SetCursor(PARK), Action::Move { epoch: e, seq: 1, x: 3, y: 500 }]);
        assert!(t.k.parked());
        // 钩子里同步停放，不做旧坐标过滤
        t.t += 1;
        let (_, out) = t.mouse(MouseKind::Move, Point::new(PARK.x + 2, PARK.y), false);
        assert_eq!(out, vec![Action::Move { epoch: e, seq: 2, x: 5, y: 500 }]);
        // 迟到的通知：已经停放了，不再停
        assert!(t.hidden(e, PARK).is_empty());
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
    fn forwarded_alt_autorepeat_at_home_is_masked() {
        let mut t = setup();
        t.cross();
        t.key(0xA4, true); // LAlt 转发给对方
        t.rel(-100, 0); // Alt 还按着就回到本机
        assert!(!t.k.driving());
        // 自动重复的按下放行给本机；之后的抬起改为掩码键 + 抬起
        assert_eq!(t.key(0xA4, true), (Verdict::Pass, vec![]));
        assert_eq!(t.key(0xA4, false), (Verdict::Swallow, vec![Action::Mask { vk: 0xA4, scan: 0x10, ext: false }]));
        // 普通键不需要
        t.cross();
        t.key(0x41, true);
        t.rel(-100, 0);
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
    fn yield_go_home_pause_and_quit_return_home() {
        for m in [ToInput::Yield(0), ToInput::GoHome, ToInput::Pause(true), ToInput::Quit, ToInput::Driven(true)] {
            let mut t = setup();
            t.cross();
            let out = t.msg(m);
            assert_eq!(out[0], Action::Send(Msg::Leave { epoch: t.k.epoch }));
            assert!(!t.k.driving());
        }
        // 不在驱动时 Yield / GoHome 什么都不做
        let mut t = setup();
        assert!(t.msg(ToInput::Yield(0)).is_empty());
        assert!(t.msg(ToInput::GoHome).is_empty());
    }

    #[test]
    fn yield_only_for_current_epoch() {
        let mut t = setup();
        t.cross();
        let e = t.k.epoch;
        // 上一轮晚到的 Yield：不理
        assert!(t.msg(ToInput::Yield(e - 1)).is_empty());
        assert!(t.msg(ToInput::Yield(e + 1)).is_empty());
        assert!(t.k.driving());
        assert_eq!(t.msg(ToInput::Yield(e)), home_actions(e, Point::new(1918, 500)));
    }

    #[test]
    fn no_crossing_soon_after_yield() {
        let mut t = setup();
        t.cross();
        t.msg(ToInput::Yield(0));
        assert!(!t.k.driving());
        // 1.5 秒内：离开边再回来也不跨
        assert_eq!(t.mv(1000, 500), (Verdict::Pass, vec![]));
        assert_eq!(t.mv(1919, 500), (Verdict::Pass, vec![]));
        t.t += 1400;
        assert_eq!(t.mv(1919, 501), (Verdict::Pass, vec![]));
        // 过了 1.5 秒：可以
        t.t += 100;
        t.cross();
        // 热键/虚拟光标回来不算 Yield：立刻可以再跨
        t.key(0x91, true);
        t.key(0x91, false);
        t.cross();
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
    fn refused_crossing_probes_few_keys() {
        let mut t = setup();
        // 不在边上：一个键都不查
        t.mv(1000, 500);
        assert_eq!(t.probes.get(), 0);
        // 前台是管理员窗口：先拒绝，不查键
        t.poll(true, true);
        t.mv(1919, 500);
        assert_eq!(t.probes.get(), 0);
        t.poll(false, true);
        // 拖动中：只查鼠标键
        t.held.borrow_mut()[0x02] = true;
        t.mv(1919, 501);
        assert!(t.probes.get() <= 5, "{}", t.probes.get());
        t.held.borrow_mut()[0x02] = false;
        // 真要跨屏：扫全部键（每个键查一次）
        t.probes.set(0);
        t.mv(1919, 502);
        assert!(t.k.driving());
        assert_eq!(t.probes.get(), 255);
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
    fn driven_yields_on_real_activity_only() {
        let mut t = setup();
        t.msg(ToInput::Driven(true));
        // 被控时不跨屏；小抖动（累计 ≤ 8 像素）不发 Yield
        let (v, out) = t.mouse_cur(MouseKind::Move, Point::new(1919, 500), Point::new(1916, 500), false);
        assert_eq!((v, out), (Verdict::Pass, vec![]));
        t.t += 8;
        assert_eq!(t.mouse_cur(MouseKind::Move, Point::new(1918, 503), Point::new(1919, 500), false).1, vec![]);
        // 累计超过 8 像素：发 Yield
        t.t += 8;
        assert_eq!(t.mouse_cur(MouseKind::Move, Point::new(1915, 503), Point::new(1918, 503), false).1, vec![Action::Yield]);
        // 300ms 内不重复
        t.t += 8;
        assert_eq!(t.mouse(MouseKind::Button(Button::Left, true), Point::new(10, 10), false).1, vec![]);
        // 按钮、滚轮、按键：立刻
        t.t += 400;
        assert_eq!(t.mouse(MouseKind::Wheel { delta: 120, horizontal: false }, PARK, false).1, vec![Action::Yield]);
        t.t += 400;
        assert_eq!(t.key(0x41, true), (Verdict::Pass, vec![Action::Yield]));
        // 别的程序注入的输入不算
        t.t += 400;
        assert_eq!(t.mouse(MouseKind::Button(Button::Left, true), PARK, true), (Verdict::Pass, vec![]));
        assert_eq!(t.mouse_cur(MouseKind::Move, Point::new(10, 10), Point::new(500, 500), true).1, vec![]);
        // 慢慢挪：停顿超过 500ms 累计清零
        for _ in 0..4 {
            t.t += 600;
            assert_eq!(t.mouse_cur(MouseKind::Move, Point::new(105, 100), Point::new(100, 100), false).1, vec![]);
        }
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
        t.msg(ToInput::Yield(0));
        assert_eq!(t.k.key(ev, &mut out), Verdict::Pass);
    }

    #[test]
    fn local_desk_change_while_driving_reparks() {
        let mut t = setup();
        t.cross();
        let d2 = Desk::new(vec![Rect::new(0, 0, 2560, 1440)]);
        assert_eq!(t.msg(ToInput::LocalDesk(d2)), vec![Action::SetCursor(Point::new(1280, 720))]);
        // 回调外停放：之后紧跟着的、还是旧停放点坐标的事件丢掉
        t.t += 1;
        assert_eq!(t.mouse_cur(MouseKind::Move, Point::new(962, 540), Point::new(1280, 720), false).1, vec![]);
    }

    #[test]
    fn scan_code_mapping() {
        assert_eq!(scan_of(0x41, 0x1E, false, || unreachable!()), (0x1E, false));
        assert_eq!(scan_of(0x41, 0x21D, false, || unreachable!()), (0x1D, false));
        assert_eq!(scan_of(0x2E, 0, false, || 0xE053), (0x53, true));
        assert_eq!(scan_of(0x41, 0, false, || 0x1E), (0x1E, false));
        assert_eq!(scan_of(VK_PAUSE, 0x45, false, || unreachable!()), (0, false));
    }

    #[test]
    fn no_rearm_until_cursor_leaves_edge() {
        let mut t = setup();
        t.cross();
        t.rel(3, 0);
        t.rel(-10, 0); // 回到本机
        assert!(!t.k.driving());
        // 还贴在边上（沿边移动、往外只推一点）：不跨
        t.t += 8;
        assert_eq!(t.mouse_cur(MouseKind::Move, Point::new(1919, 500), Point::new(1918, 500), false), (Verdict::Pass, vec![]));
        t.t += 8;
        assert_eq!(t.mouse_cur(MouseKind::Move, Point::new(1920, 520), Point::new(1919, 519), false), (Verdict::Pass, vec![]));
        // 离开过边以后才跨
        t.t += 8;
        assert_eq!(t.mouse_cur(MouseKind::Move, Point::new(1918, 520), Point::new(1919, 520), false), (Verdict::Pass, vec![]));
        t.t += 8;
        let (v, _) = t.mouse_cur(MouseKind::Move, Point::new(1920, 520), Point::new(1918, 520), false);
        assert_eq!(v, Verdict::Swallow);
        assert!(t.k.driving());

        // 热键回来也一样
        t.key(0x91, true);
        t.key(0x91, false);
        t.t += 8;
        assert_eq!(t.mouse_cur(MouseKind::Move, Point::new(1925, 520), Point::new(1919, 520), false), (Verdict::Pass, vec![]));
    }

    #[test]
    fn rearm_by_pushing_or_waiting_at_edge() {
        let mut t = setup();
        let edge = |t: &mut T, dx: i32| {
            t.t += 8;
            t.mouse_cur(MouseKind::Move, Point::new(1919 + dx, 500), Point::new(1919, 500), false).0
        };
        // 刚回来又想马上过去：贴着边继续往外推够 REARM_PUSH_PX 就过去，不用先往回挪
        t.cross();
        t.rel(-10, 0);
        assert!(!t.k.driving());
        assert_eq!(edge(&mut t, 6), Verdict::Pass);
        assert_eq!(edge(&mut t, 6), Verdict::Pass);
        assert_eq!(edge(&mut t, 6), Verdict::Pass);
        assert_eq!(edge(&mut t, 6), Verdict::Swallow);
        assert!(t.k.driving());

        // 停在边上过了 REARM_MS：往外推一下就过去
        t.rel(-10, 0);
        assert!(!t.k.driving());
        assert_eq!(edge(&mut t, 1), Verdict::Pass);
        t.t += REARM_MS as u32;
        assert_eq!(edge(&mut t, 0), Verdict::Pass); // 没往外推：不过去
        assert_eq!(edge(&mut t, 1), Verdict::Swallow);
        assert!(t.k.driving());
    }

    #[test]
    fn driven_end_disarms_crossing() {
        let mut t = setup();
        t.msg(ToInput::Driven(true));
        t.msg(ToInput::Driven(false));
        // 对方放手时光标停在共用的边上：小动一下不能把本机扔过去
        t.t += 8;
        assert_eq!(t.mouse_cur(MouseKind::Move, Point::new(1919, 500), Point::new(1919, 499), false), (Verdict::Pass, vec![]));
        t.t += 8;
        assert_eq!(t.mouse_cur(MouseKind::Move, Point::new(1920, 501), Point::new(1919, 500), false), (Verdict::Pass, vec![]));
        t.cross();
    }

    #[test]
    fn foreground_blocked_refuses_crossing() {
        let mut t = setup();
        assert!(t.poll(true, true).is_empty());
        let warn = Action::Error { msg: FG_BLOCKED_MSG, on: true };
        assert_eq!(t.mv(1919, 500), (Verdict::Pass, vec![warn.clone()]));
        // 只提示一次
        assert_eq!(t.mv(1919, 501), (Verdict::Pass, vec![]));
        assert!(!t.k.driving());
        // 前台换回普通窗口：清掉提示，可以跨
        assert_eq!(t.poll(false, true), vec![Action::Error { msg: FG_BLOCKED_MSG, on: false }]);
        t.cross();
        // 驱动中前台变成管理员窗口：立刻回来并提示
        let e = t.k.epoch;
        let mut want = home_actions(e, Point::new(1918, 500));
        want.push(warn);
        assert_eq!(t.poll(true, true), want);
        assert!(!t.k.driving());
    }

    #[test]
    fn desktop_switch_returns_home_and_restores_after() {
        let mut t = setup();
        t.cross();
        assert!(t.poll(false, true).is_empty());
        let e = t.k.epoch;
        assert_eq!(t.poll(false, false), home_actions(e, Point::new(1918, 500)));
        assert!(!t.k.driving());
        assert!(t.poll(false, false).is_empty());
        // 切回 Default：再停一次落点、补一次显示
        assert_eq!(t.poll(false, true), vec![Action::SetCursor(Point::new(1918, 500)), Action::ShowCursor]);
        assert!(t.poll(false, true).is_empty());
        // 不在驱动时切走再切回：什么都不做
        assert!(t.poll(false, false).is_empty());
        assert!(t.poll(false, true).is_empty());
    }

    #[test]
    fn desktop_switch_clears_hotkey_held() {
        let mut t = setup();
        assert_eq!(t.key(0x91, true), (Verdict::Swallow, vec![Action::Paused(true)]));
        // 热键的抬起落在安全桌面上，钩子没看到
        t.poll(false, false);
        t.poll(false, true);
        assert_eq!(t.key(0x91, true), (Verdict::Swallow, vec![Action::Paused(false)]));
    }

    #[test]
    fn watchdog_detects_lost_hooks_while_driving() {
        let mut t = setup();
        // 不在驱动：不判
        assert_eq!(t.hooks_check(Point::new(100, 100), 5000, true), (false, vec![]));
        t.cross_unparked();
        // 还没停放（光标本来就在出口点）：不判
        assert_eq!(t.hooks_check(Point::new(100, 100), 5000, true), (false, vec![]));
        let e = t.k.epoch;
        t.hidden(e, EDGE);
        // 光标在停放点附近：正常
        assert_eq!(t.hooks_check(Point::new(PARK.x + 30, PARK.y), 5000, true), (false, vec![]));
        // 钩子刚被调用过：正常（马上会重新停放）
        assert_eq!(t.hooks_check(Point::new(100, 100), 200, true), (false, vec![]));
        // 钩子之后没有输入：是别的程序挪了光标，不是钩子丢了
        assert_eq!(t.hooks_check(Point::new(100, 100), 5000, false), (false, vec![]));
        assert!(t.k.driving());
        // 有输入、钩子没动静、光标被挪走：一次就判定，回到本机
        assert_eq!(t.hooks_check(Point::new(100, 100), 300, true), (true, home_actions(e, Point::new(1918, 500))));
        assert!(!t.k.driving());
    }

    #[test]
    fn no_return_while_remote_button_held() {
        let mut t = setup();
        t.cross();
        let e = t.k.epoch;
        t.rel(50, 0); // (50, 500)
        t.mouse(MouseKind::Button(Button::Left, true), PARK, false);
        // 拖向本机那条边：贴边停住，不回来
        assert_eq!(t.rel(-100, 0), (Verdict::Swallow, vec![Action::Move { epoch: e, seq: 2, x: 0, y: 500 }]));
        assert!(t.k.driving());
        // 沿边仍可移动
        assert_eq!(t.rel(-5, 10), (Verdict::Swallow, vec![Action::Move { epoch: e, seq: 3, x: 0, y: 510 }]));
        assert!(t.k.driving());
        // 松开后再推就回来
        t.mouse(MouseKind::Button(Button::Left, false), PARK, false);
        let (_, out) = t.rel(-5, 0);
        assert_eq!(out, home_actions(e, Point::new(1918, 510)));
    }

    #[test]
    fn delta_measured_from_real_cursor_and_reparks() {
        let mut t = setup();
        t.cross();
        let e = t.k.epoch;
        t.t += 100;
        // 别的程序把光标挪到 (100, 100)：位移按真实位置算，并在钩子里重新停放
        let (v, out) = t.mouse_cur(MouseKind::Move, Point::new(103, 100), Point::new(100, 100), false);
        assert_eq!(v, Verdict::Swallow);
        assert_eq!(out, vec![Action::SetCursor(PARK), Action::Move { epoch: e, seq: 1, x: 3, y: 500 }]);
        // 钩子里同步停放后，下一个事件就是按停放点算的
        t.t += 2;
        assert_eq!(
            t.mouse(MouseKind::Move, Point::new(PARK.x + 2, PARK.y), false),
            (Verdict::Swallow, vec![Action::Move { epoch: e, seq: 2, x: 5, y: 500 }])
        );
        // 光标停不回去（ClipCursor）：50ms 内不反复停放，位移仍正确
        t.t += 2;
        let (_, out) = t.mouse_cur(MouseKind::Move, Point::new(101, 100), Point::new(100, 100), false);
        assert_eq!(out, vec![Action::Move { epoch: e, seq: 3, x: 6, y: 500 }]);
        // 过了一阵仍在 (100, 100)：再停放一次
        t.t += 60;
        let (_, out) = t.mouse_cur(MouseKind::Move, Point::new(101, 101), Point::new(100, 100), false);
        assert_eq!(out, vec![Action::SetCursor(PARK), Action::Move { epoch: e, seq: 4, x: 7, y: 501 }]);
    }

    #[test]
    fn alt_held_at_crossing_masked_after_forwarding() {
        // 没转发过东西：Alt 抬起照常放行
        let mut t = setup();
        t.held.borrow_mut()[0xA4] = true;
        t.cross();
        assert_eq!(t.key(0xA4, false), (Verdict::Pass, vec![]));

        // 转发过按键：抬起吞掉，改为注入掩码键 + 抬起
        let mut t = setup();
        t.held.borrow_mut()[0xA4] = true;
        t.cross();
        t.key(0x41, true);
        t.key(0x41, false);
        assert_eq!(t.key(0xA4, false), (Verdict::Swallow, vec![Action::Mask { vk: 0xA4, scan: 0x10, ext: false }]));
        // Shift 不需要
        let mut t = setup();
        t.held.borrow_mut()[0xA0] = true;
        t.cross();
        t.key(0x41, true);
        assert_eq!(t.key(0xA0, false), (Verdict::Pass, vec![]));
    }

    #[test]
    fn win_held_through_return_masked_at_home() {
        let mut t = setup();
        t.held.borrow_mut()[0x5B] = true;
        t.cross();
        t.mouse(MouseKind::Wheel { delta: 120, horizontal: false }, PARK, false);
        t.key(0x91, true); // 热键回家，Win 还按着
        t.key(0x91, false);
        assert!(!t.k.driving());
        // 自动重复的按下放行，不取消防菜单
        assert_eq!(t.key(0x5B, true), (Verdict::Pass, vec![]));
        assert_eq!(t.key(0x5B, false), (Verdict::Swallow, vec![Action::Mask { vk: 0x5B, scan: 0x10, ext: false }]));
        // 只处理一次
        assert_eq!(t.key(0x5B, true), (Verdict::Pass, vec![]));
        assert_eq!(t.key(0x5B, false), (Verdict::Pass, vec![]));
    }
}
