//! 被控端：把对方发来的输入注入本机（SendInput）。可从任意线程调用（内部 Mutex）。
//!
//! 规则：
//! - 所有注入事件的 dwExtraInfo = proto::INJECT_MARK，让本机钩子忽略它们。
//! - 鼠标位置用 MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK，
//!   坐标按虚拟桌面（GetSystemMetrics SM_XVIRTUALSCREEN/SM_YVIRTUALSCREEN/SM_CXVIRTUALSCREEN/SM_CYVIRTUALSCREEN）
//!   归一化到 0..65535：nx = (x - vx) * 65536 / vw + 1（i64 计算，钳在 0..=65535）。
//!   系统反算像素是 nx * vw / 65536 向下取整，+1 保证不会落到前一个像素。
//! - Button/Wheel 先移动到消息里的 (x, y) 再按下/抬起/滚动（同一次 SendInput 里两个 INPUT）。
//! - Key：scan != 0 时用 KEYEVENTF_SCANCODE（ext 时加 KEYEVENTF_EXTENDEDKEY）；scan == 0 时用 vk。
//!   vk > 0xFF 或 scan > 0x1FF 的畸形消息丢弃；按着的键最多记 MAX_KEYS 个，记满了新按下的键不注入。
//! - 记录本机被按下还没抬起的键（(scan, ext, vk)）和鼠标按钮；leave / stale / release_all / 会话断开时全部抬起，
//!   防止卡键。抬起的键里有 Alt/Win 时，先注入掩码键 vk 0xE8 按下抬起，免得弹出菜单栏/开始菜单。
//! - epoch 不等于当前 epoch 的消息丢弃；Move 只接受同一 epoch 内 seq 更大的。
//! - 带时间戳的 Move 进平滑播放（smooth::Playout）：到点的当场在调用线程注入，没到点的由 inject-play 线程
//!   按时注入。Button/Wheel 带着最新位置，先清掉排着的旧位置；enter/leave/会话变化时整个重来。
//! - enter 时通知 input 线程 ToInput::Driven(true)，leave/release_all 时 Driven(false)，并更新 hub.status.driven。
//!   这两个通知在持锁时发出（都不阻塞），并发的 enter/leave 不会把顺序弄反。
//!   当前被接管的轮次记在 DRIVEN_EPOCH，input 线程发 Yield 时带上（`driven_epoch()`）。
//! - enter 时不接管、直接回 Msg::Yield{epoch}（不移动光标）：本机暂停共享；或本机最近 LOCAL_BUSY_MS 内
//!   有物理输入（input 线程记录，`input::last_physical_input`）——两边同时有人操作时别抢。
//! - SendInput 返回 0 / 少于请求数（锁屏/UAC 安全桌面、前台是管理员窗口被 UIPI 拦截）不要重试循环，直接放弃这一次，
//!   并回 Msg::Yield{epoch} 让对方回去（最多每 FAIL_YIELD_MS 一次），免得两边光标脱节；
//!   抬起没注入成功的键/按钮留在记录里，下次 enter/leave/release_all 再抬。
//! - 键盘跟随（只收键盘）：kbd_enter 后接受这一轮（kbd epoch）的 Key，不移动光标、不进入被控状态（本机鼠标照常用、
//!   照常能跨屏）。kbd_leave / enter / 会话变化 / release_all 结束它并抬起按着的键。开始/结束都在持锁时通知
//!   input 线程 ToInput::KbdIn（set_kbd 统一出口，不会漏）。当前轮次记在 KBD_EPOCH（`kbd_epoch()`）。
//!   本机暂停时 kbd_enter 直接回 Yield{epoch}。新的一轮一开始就弹“键盘”提示（ToDesktop::KbdNote），
//!   不等打字；从驱动直接转过来的（prev 接上）键盘本来就在这台，不弹。
//!   对方跨屏过来（enter_from 新的一轮）也弹；从只收键盘转过来的（prev 接上）不弹。

use crate::hub::{Hub, ToInput};
use crate::proto::{Button, INJECT_MARK, Msg};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use crate::smooth::{self, Item, Playout};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// 一个按着的键：(scan, ext, vk)。
type KeyId = (u16, bool, u16);

/// 按着的键最多记这么多个（真键盘不可能更多；防对方畸形数据把表撑大）。
const MAX_KEYS: usize = 64;
const MAX_VK: u16 = 0xFF;
const MAX_SCAN: u16 = 0x1FF;
/// 本机这么久之内有物理输入：不接受对方接管。
const LOCAL_BUSY_MS: u32 = 500;
/// 注入失败时回 Yield 的最小间隔。
const FAIL_YIELD_MS: u32 = 1000;
/// 抬起前要先注入掩码键的 Alt/Win。
const MENU_VKS: [u16; 5] = [0x12, 0xA4, 0xA5, 0x5B, 0x5C];
/// 未分配的虚拟键，用来打断 Alt/Win 单按（同 AutoHotkey 的 vkE8）。
const VK_MASK: u16 = 0xE8;

/// 本机正在接收对方键盘的轮次（只收键盘；0 = 没有）。
static KBD_EPOCH: AtomicU32 = AtomicU32::new(0);

/// 本机正在接收对方键盘的轮次（0 = 没有）：本机要请对方收回键盘时 Yield 带上它。
pub fn kbd_epoch() -> u32 {
    KBD_EPOCH.load(Ordering::Relaxed)
}

/// 本机当前（或最近一次）被接管的轮次。
static DRIVEN_EPOCH: AtomicU32 = AtomicU32::new(0);

/// 本机当前（或最近一次）被接管的轮次：被控期间本机有人动时，Yield 带上它（对方只认当前这一轮）。
pub fn driven_epoch() -> u32 {
    DRIVEN_EPOCH.load(Ordering::Relaxed)
}

#[derive(Default)]
struct Inner {
    /// 当前会话号（net 的 Session.id）。旧会话的读线程晚到的消息一律忽略。
    sid: u64,
    epoch: u32,
    active: bool,
    /// 当前 epoch 内收到的最大 Move seq。
    seq: u32,
    keys: Vec<KeyId>,
    /// Left, Right, Middle, X1, X2
    buttons: [bool; 5],
    /// 上次因注入失败回 Yield 的时间
    fail_yield: Option<u32>,
    /// 鼠标位置平滑播放
    play: Playout,
    /// 正在只收键盘的轮次（键盘跟随）
    kbd: Option<u32>,
    /// 这一轮只收键盘已经弹过“键盘 已收回”
    kbd_lost: bool,
}

impl Inner {
    /// 本条消息属于当前接管吗？
    fn current(&self, epoch: u32) -> bool {
        self.active && epoch == self.epoch
    }

    /// 这个按键属于当前接管或当前只收键盘的轮次吗？
    fn key_ok(&self, epoch: u32) -> bool {
        self.current(epoch) || self.kbd == Some(epoch)
    }

    fn session(&self, sid: u64) -> bool {
        self.sid == sid
    }

    /// 这个键能按下吗：已经记着（自动重复）或表还没满。
    fn can_press(&self, k: KeyId) -> bool {
        self.keys.len() < MAX_KEYS || self.keys.contains(&k)
    }

    /// 记录按键状态（没见过按下的抬起照样注入，无害）。
    fn track_key(&mut self, k: KeyId, down: bool) {
        let pos = self.keys.iter().position(|h| *h == k);
        match (down, pos) {
            (true, None) if self.keys.len() < MAX_KEYS => self.keys.push(k),
            (false, Some(i)) => {
                self.keys.remove(i);
            }
            _ => {}
        }
    }

    fn holds_any(&self) -> bool {
        !self.keys.is_empty() || self.buttons.iter().any(|b| *b)
    }

    /// 取出所有按着的键（按按下的逆序抬起）和按钮，并清空记录。
    fn take_held(&mut self) -> (Vec<KeyId>, Vec<Button>) {
        let mut keys = std::mem::take(&mut self.keys);
        keys.reverse();
        let mut btns = Vec::new();
        for (i, held) in self.buttons.iter_mut().enumerate() {
            if std::mem::take(held) {
                btns.push(BUTTONS[i]);
            }
        }
        (keys, btns)
    }

    /// 抬起没注入成功：放回记录（keys 是 take_held 的抬起顺序），下次再抬。
    fn put_back(&mut self, keys: &[KeyId], btns: &[Button]) {
        for k in keys {
            if !self.keys.contains(k) {
                self.keys.insert(0, *k);
            }
        }
        for b in btns {
            self.buttons[button_index(*b)] = true;
        }
    }
}

const BUTTONS: [Button; 5] = [Button::Left, Button::Right, Button::Middle, Button::X1, Button::X2];

fn button_index(b: Button) -> usize {
    match b {
        Button::Left => 0,
        Button::Right => 1,
        Button::Middle => 2,
        Button::X1 => 3,
        Button::X2 => 4,
    }
}

/// 用到的系统调用（测试时换成假的，不真的注入）。
#[derive(Clone, Copy)]
struct Sys {
    send: fn(&[INPUT]) -> u32,
    /// 本机最后一次物理输入的时间（GetTickCount 时基）
    last_input: fn() -> Option<u32>,
    now: fn() -> u32,
}

fn tick() -> u32 {
    unsafe { GetTickCount() }
}

#[cfg_attr(test, allow(dead_code))]
const REAL: Sys = Sys { send, last_input: crate::input::last_physical_input, now: tick };

pub struct Injector {
    hub: Arc<Hub>,
    inner: Mutex<Inner>,
    /// 叫醒 inject-play（有排着的位置了）
    play_cv: Condvar,
    sys: Sys,
}

impl Injector {
    pub fn new(hub: Arc<Hub>) -> Self {
        // 测试里（net 的端到端测试也会建 Injector）绝不能真的注入：否则跑 cargo test 会挪动本机光标、往前台窗口打字
        #[cfg(test)]
        let sys = Sys { send: |i| i.len() as u32, last_input: || None, now: tick };
        #[cfg(not(test))]
        let sys = REAL;
        Self { hub, inner: Mutex::new(Inner::default()), play_cv: Condvar::new(), sys }
    }

    /// 起 inject-play 线程：按时注入平滑缓冲里没到点的位置。
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        let _ = std::thread::Builder::new().name("inject-play".into()).spawn(move || me.run_play());
    }

    fn run_play(&self) {
        crate::sys::boost_thread(true);
        let mut g = self.lock();
        loop {
            let now = smooth::now_us();
            g = match self.play(&mut g, now) {
                None => self.play_cv.wait(g).unwrap_or_else(|e| e.into_inner()),
                Some(w) => {
                    let d = Duration::from_micros((w - now).max(0) as u64);
                    self.play_cv.wait_timeout(g, d).unwrap_or_else(|e| e.into_inner()).0
                }
            };
        }
    }

    /// 推进平滑播放，注入到点的位置（持锁）。返回下次该醒的时刻。
    fn play(&self, g: &mut Inner, now: i64) -> Option<i64> {
        let (it, wake) = g.play.poll(now);
        if let Some(it) = it {
            if g.active && g.current(it.epoch) && it.seq > g.seq {
                g.seq = it.seq;
                if (self.sys.send)(&[move_input(it.x, it.y)]) != 1 {
                    self.failed(g);
                }
            }
        }
        wake
    }

    /// 平滑播放整个重来（持锁）。有统计就记一行日志。
    fn reset_play(&self, g: &mut Inner) {
        let st = g.play.stats;
        if st.pushed > 0 {
            crate::log!(
                "鼠标平滑：收 {} 个，注入 {} 次，大断流跳过 {} 次，最大缓冲 {:.1}ms",
                st.pushed,
                st.injected,
                st.snapped,
                st.max_delay as f64 / 1000.0
            );
        }
        g.play.reset();
        g.play.stats = Default::default();
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 持锁时调用（参数只为证明持锁）：通知 input 线程、更新状态，都不阻塞，顺序与锁内变化一致。
    fn set_driven(&self, _g: &MutexGuard<'_, Inner>, b: bool) {
        self.hub.to_input(ToInput::Driven(b));
        self.hub.set_status(|s| s.driven = b);
    }

    /// 持锁时调用：只收键盘开始/结束（统一出口，变化时通知 input 线程，顺序与锁内变化一致）。
    fn set_kbd(&self, g: &mut Inner, kbd: Option<u32>) {
        let was = g.kbd.is_some();
        g.kbd = kbd;
        g.kbd_lost = false;
        KBD_EPOCH.store(kbd.unwrap_or(0), Ordering::Relaxed);
        if was != kbd.is_some() {
            self.hub.to_input(ToInput::KbdIn(kbd.is_some()));
        }
    }

    /// 只收键盘时对方的键盘意外断了（对方没动静、注入失败）：本机也弹“键盘 已收回”，每轮一次。
    fn kbd_lost_note(&self, g: &mut Inner) {
        if g.kbd.is_some() && !g.kbd_lost {
            g.kbd_lost = true;
            self.hub.to_desktop(crate::hub::ToDesktop::KbdNote(false));
        }
    }

    /// 本机最近 LOCAL_BUSY_MS 内有物理输入。
    fn local_busy(&self) -> bool {
        match (self.sys.last_input)() {
            Some(t) => (self.sys.now)().wrapping_sub(t) < LOCAL_BUSY_MS,
            None => false,
        }
    }

    /// 注入失败（安全桌面、UIPI）：请对方回去，最多每 FAIL_YIELD_MS 一次。
    fn failed(&self, g: &mut Inner) {
        let e = g.epoch;
        self.failed_at(g, e);
    }

    /// 同 failed，Yield 带指定轮次（只收键盘时是键盘轮次）。
    fn failed_at(&self, g: &mut Inner, epoch: u32) {
        let now = (self.sys.now)();
        if g.fail_yield.is_some_and(|t| now.wrapping_sub(t) < FAIL_YIELD_MS) {
            return;
        }
        g.fail_yield = Some(now);
        crate::log!("注入失败（前台是管理员窗口、锁屏或 UAC？），请对方放手");
        self.hub.send(Msg::Yield { epoch });
    }

    /// 抬起所有记录中按着的键和按钮（持锁调用）。没注入成功的放回记录。
    fn release(&self, g: &mut Inner) {
        let (keys, btns) = g.take_held();
        let (v, pre) = release_inputs(&keys, &btns);
        if v.is_empty() {
            return;
        }
        let n = ((self.sys.send)(&v) as usize).saturating_sub(pre);
        let (rk, rb) = unsent(&keys, &btns, n);
        g.put_back(rk, rb);
    }

    #[cfg(test)]
    pub fn enter(&self, sid: u64, epoch: u32, x: i32, y: i32) {
        self.enter_from(sid, epoch, x, y, 0)
    }

    /// 只抬起按着的鼠标按钮（键留着）。
    fn release_buttons(&self, g: &mut Inner) {
        let keys = std::mem::take(&mut g.keys);
        self.release(g);
        g.keys = keys;
    }

    /// 对方接管：光标放到 (x, y)。本机暂停共享、或本机有人正在用时不接管，回 Yield（不移动光标）。
    /// prev = 对方刚结束的只转发键盘轮次：本机正处在那一轮就把按着的键原样接到这一轮（不先抬起）。
    pub fn enter_from(&self, sid: u64, epoch: u32, x: i32, y: i32, prev: u32) {
        let paused = self.hub.status().paused;
        let mut g = self.lock();
        if !g.session(sid) {
            return;
        }
        if paused || self.local_busy() {
            if g.active {
                self.release(&mut g);
                g.active = false;
                self.set_driven(&g, false);
            }
            // 对方转驱动时不再发 KbdLeave：不接管也要把只收键盘那一轮结束掉（抬起按着的键）
            if g.kbd.is_some() {
                self.release(&mut g);
                self.set_kbd(&mut g, None);
            }
            crate::log!("{}，不接管，请对方回去", if paused { "本机已暂停共享" } else { "本机刚有人在用键鼠" });
            self.hub.send(Msg::Yield { epoch });
            return;
        }
        // 从只转发键盘直接转过来：按着的键原样接到这一轮
        let carry = prev != 0 && g.kbd == Some(prev);
        // 新的一轮（不是重复的 Enter）
        let fresh = !(g.active && g.epoch == epoch);
        // 上一次接管没正常结束，或上次抬起没注入成功：先把残留的按键抬起来
        if !carry && ((g.active && g.epoch != epoch) || g.holds_any()) {
            self.release(&mut g);
        }
        // 对方的鼠标过来了：只收键盘那一轮结束（按着的键上面已经抬起）
        if g.kbd.is_some() {
            self.set_kbd(&mut g, None);
        }
        self.reset_play(&mut g);
        g.epoch = epoch;
        g.active = true;
        g.seq = 0;
        g.fail_yield = None;
        DRIVEN_EPOCH.store(epoch, Ordering::Relaxed);
        let ok = (self.sys.send)(&[move_input(x, y)]) == 1;
        self.set_driven(&g, true);
        if !ok {
            self.failed(&mut g);
        } else if fresh && !carry && self.hub.cfg.read().map_or(true, |c| c.share.keyboard) {
            // 对方的鼠标跨过来了，键盘跟着到了这台：马上提示。从只收键盘转过来的键盘本来就在这台，不弹
            self.hub.to_desktop(crate::hub::ToDesktop::KbdNote(true));
        }
    }

    /// 对方放手：抬起所有按着的键和按钮。
    pub fn leave(&self, sid: u64, epoch: u32) {
        let mut g = self.lock();
        if !g.session(sid) || !g.current(epoch) {
            return;
        }
        self.release(&mut g);
        g.active = false;
        self.reset_play(&mut g);
        self.set_driven(&g, false);
    }

    /// 对方发来的绝对位置。t = 对方发送时刻（微秒，0 = 没有）：有就进平滑播放，没有就当场注入。
    pub fn moved(&self, sid: u64, epoch: u32, seq: u32, x: i32, y: i32, t: u32) {
        let mut g = self.lock();
        if !g.session(sid) || !g.current(epoch) || seq <= g.seq {
            return;
        }
        if t == 0 {
            g.seq = seq;
            if (self.sys.send)(&[move_input(x, y)]) != 1 {
                self.failed(&mut g);
            }
            return;
        }
        let now = smooth::now_us();
        #[cfg(not(test))]
        smooth::trace(now, t, x, y);
        g.play.push(now, t, Item { epoch, seq, x, y });
        self.play(&mut g, now);
        if g.play.pending() {
            self.play_cv.notify_one();
        }
    }

    pub fn button(&self, sid: u64, epoch: u32, x: i32, y: i32, b: Button, down: bool) {
        let mut g = self.lock();
        if !g.session(sid) || !g.current(epoch) {
            return;
        }
        g.play.flush();
        let ok = (self.sys.send)(&[move_input(x, y), button_input(b, down)]) == 2;
        // 按下总记下；抬起没注入成功就留着，放手时再抬
        if down || ok {
            g.buttons[button_index(b)] = down;
        }
        if !ok {
            self.failed(&mut g);
        }
    }

    pub fn wheel(&self, sid: u64, epoch: u32, x: i32, y: i32, delta: i32, horizontal: bool) {
        let mut g = self.lock();
        if !g.session(sid) || !g.current(epoch) || delta == 0 {
            return;
        }
        g.play.flush();
        let flags = if horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL };
        if (self.sys.send)(&[move_input(x, y), mouse(0, 0, delta as u32, flags)]) != 2 {
            self.failed(&mut g);
        }
    }

    pub fn key(&self, sid: u64, epoch: u32, scan: u16, ext: bool, vk: u16, down: bool) {
        let mut g = self.lock();
        if !g.session(sid) || !g.key_ok(epoch) || (scan == 0 && vk == 0) || vk > MAX_VK || scan > MAX_SCAN {
            return;
        }
        let k = (scan, ext, vk);
        if down && !g.can_press(k) {
            return; // 表满了：不注入，免得按下去抬不起来
        }
        let ok = (self.sys.send)(&[key_input(k, down)]) == 1;
        // 同上：抬起失败（Win+L 后进了安全桌面等）留在记录里
        if down || ok {
            g.track_key(k, down);
        }
        if !ok {
            self.failed_at(&mut g, epoch);
            if g.kbd == Some(epoch) {
                self.kbd_lost_note(&mut g);
            }
        }
    }

    #[cfg(test)]
    pub fn kbd_enter(&self, sid: u64, epoch: u32) {
        self.kbd_enter_from(sid, epoch, 0)
    }

    /// 对方开始把它的键盘打到本机（键盘跟随）：不移动光标、不进入被控状态。本机暂停时回 Yield。
    /// prev = 对方刚结束的驱动轮次（对方不再单发 Leave）：本机正被那一轮接管就结束被控，按着的键原样接过来。
    pub fn kbd_enter_from(&self, sid: u64, epoch: u32, prev: u32) {
        let paused = self.hub.status().paused;
        let mut g = self.lock();
        if !g.session(sid) {
            return;
        }
        let carry = prev != 0 && g.active && g.epoch == prev;
        if paused {
            crate::log!("本机已暂停共享，不接收对方键盘");
            // 对方转只转发键盘时不再发 Leave：被控那一轮也在这里结束
            if g.active {
                self.release(&mut g);
                g.active = false;
                self.reset_play(&mut g);
                self.set_driven(&g, false);
            }
            if g.kbd.is_some() {
                self.release(&mut g);
                self.set_kbd(&mut g, None);
            }
            self.hub.send(Msg::Yield { epoch });
            return;
        }
        // 对方在只转发键盘，就不再驱动本机：接管结束（从驱动直接转过来时键留着，只抬鼠标按钮）
        if g.active {
            if carry {
                self.release_buttons(&mut g);
            } else {
                self.release(&mut g);
            }
            g.active = false;
            self.reset_play(&mut g);
            self.set_driven(&g, false);
        }
        if !carry && g.holds_any() {
            self.release(&mut g);
        }
        g.fail_yield = None;
        let fresh = g.kbd != Some(epoch);
        self.set_kbd(&mut g, Some(epoch));
        // 对方的键盘刚换到这台（用户动了这台的鼠标）：马上提示，不等打字。从驱动转过来的本来就在这台，不弹
        if fresh && !carry {
            self.hub.to_desktop(crate::hub::ToDesktop::KbdNote(true));
        }
    }

    /// 对方收回键盘：抬起这一轮按着的键。
    pub fn kbd_leave(&self, sid: u64, epoch: u32) {
        let mut g = self.lock();
        if !g.session(sid) || g.kbd != Some(epoch) {
            return;
        }
        self.release(&mut g);
        self.set_kbd(&mut g, None);
    }

    /// 对方好一阵没有任何消息（net 判断）：会话不断、接管不结束，只把按着的键和按钮抬起来。
    pub fn stale(&self, sid: u64) {
        let mut g = self.lock();
        if g.session(sid) {
            self.release(&mut g);
            self.kbd_lost_note(&mut g);
        }
    }

    /// 新会话登记：抬起旧会话留下的一切，之后只接受这个会话的消息。
    pub fn begin_session(&self, sid: u64) {
        let mut g = self.lock();
        self.release(&mut g);
        g.active = false;
        self.set_kbd(&mut g, None);
        g.sid = sid;
        self.set_driven(&g, false);
    }

    /// 会话结束：只有它仍是当前会话时才抬起一切。
    pub fn end_session(&self, sid: u64) {
        let mut g = self.lock();
        if g.session(sid) {
            self.release(&mut g);
            g.active = false;
            self.set_kbd(&mut g, None);
            self.reset_play(&mut g);
            self.set_driven(&g, false);
        }
    }

    /// 抬起一切，结束被控状态（退出程序时用）。
    pub fn release_all(&self) {
        let mut g = self.lock();
        self.release(&mut g);
        g.active = false;
        self.set_kbd(&mut g, None);
        self.reset_play(&mut g);
        self.set_driven(&g, false);
    }
}

/// 抬起这些键和按钮要注入的事件：有 Alt/Win 时最前面加掩码键按下抬起。返回 (事件, 掩码事件个数)。
fn release_inputs(keys: &[KeyId], btns: &[Button]) -> (Vec<INPUT>, usize) {
    if keys.is_empty() && btns.is_empty() {
        return (Vec::new(), 0);
    }
    let mut v = Vec::with_capacity(keys.len() + btns.len() + 2);
    let mut pre = 0;
    if keys.iter().any(|k| MENU_VKS.contains(&k.2)) {
        v.push(key_input((0, false, VK_MASK), true));
        v.push(key_input((0, false, VK_MASK), false));
        pre = 2;
    }
    v.extend(keys.iter().map(|k| key_input(*k, false)));
    v.extend(btns.iter().map(|b| button_input(*b, false)));
    (v, pre)
}

/// 先键后按钮注入了前 n 个：剩下没注入的键和按钮。
fn unsent<'a>(keys: &'a [KeyId], btns: &'a [Button], n: usize) -> (&'a [KeyId], &'a [Button]) {
    let nk = n.min(keys.len());
    let nb = n.saturating_sub(keys.len()).min(btns.len());
    (&keys[nk..], &btns[nb..])
}

/// 注入，返回实际注入个数；失败（安全桌面等）不重试。少于请求数时记一次日志（恢复正常后再失败会再记）。
#[cfg_attr(test, allow(dead_code))]
fn send(inputs: &[INPUT]) -> u32 {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let n = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if (n as usize) < inputs.len() {
        if !WARNED.swap(true, Ordering::Relaxed) {
            crate::log!("注入失败：{n}/{} 个事件（锁屏、UAC 或管理员窗口？）", inputs.len());
        }
    } else {
        WARNED.store(false, Ordering::Relaxed);
    }
    n
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: INJECT_MARK } },
    }
}

fn move_input(x: i32, y: i32) -> INPUT {
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let nx = normalize(x, vx, vw);
    let ny = normalize(y, vy, vh);
    mouse(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)
}

/// 虚拟桌面坐标 → 0..65535：(x - origin) * 65536 / size + 1，钳在 0..=65535。
fn normalize(x: i32, origin: i32, size: i32) -> i32 {
    if size <= 0 {
        return 0;
    }
    let (x, o, s) = (x as i64, origin as i64, size as i64);
    let x = x.clamp(o, o + s - 1);
    ((x - o) * 65536 / s + 1).clamp(0, 65535) as i32
}

fn button_input(b: Button, down: bool) -> INPUT {
    let (flags, data) = button_flags(b, down);
    mouse(0, 0, data, flags)
}

fn button_flags(b: Button, down: bool) -> (MOUSE_EVENT_FLAGS, u32) {
    match (b, down) {
        (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (Button::X1, true) => (MOUSEEVENTF_XDOWN, 1),
        (Button::X1, false) => (MOUSEEVENTF_XUP, 1),
        (Button::X2, true) => (MOUSEEVENTF_XDOWN, 2),
        (Button::X2, false) => (MOUSEEVENTF_XUP, 2),
    }
}

fn key_flags(scan: u16, ext: bool, down: bool) -> KEYBD_EVENT_FLAGS {
    let mut f = KEYBD_EVENT_FLAGS(0);
    if scan != 0 {
        f |= KEYEVENTF_SCANCODE;
    }
    if ext {
        f |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        f |= KEYEVENTF_KEYUP;
    }
    f
}

fn key_input((scan, ext, vk): KeyId, down: bool) -> INPUT {
    let wvk = if scan != 0 { 0 } else { vk };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(wvk),
                wScan: scan,
                dwFlags: key_flags(scan, ext, down),
                time: 0,
                dwExtraInfo: INJECT_MARK,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{HubRx, Out};
    use std::cell::{Cell, RefCell};

    #[test]
    fn normalize_edges() {
        assert_eq!(normalize(0, 0, 1920), 1);
        assert_eq!(normalize(1919, 0, 1920), 65502);
        // 系统按 nx * w / 65536 向下取整反算：每个像素都回到自己
        for w in [1920, 2560, 3840, 7680] {
            for x in 0..w {
                assert_eq!(normalize(x, 0, w) as i64 * w as i64 / 65536, x as i64, "w={w} x={x}");
            }
        }
        // 虚拟桌面原点为负（左边还有一块屏）
        assert_eq!(normalize(-1920, -1920, 3840), 1);
        assert_eq!(normalize(-1920, -1920, 3840) as i64 * 3840 / 65536 - 1920, -1920);
        assert_eq!(normalize(1919, -1920, 3840) as i64 * 3840 / 65536 - 1920, 1919);
        // 越界钳住；超宽虚拟桌面也不超过 65535
        assert_eq!(normalize(5000, 0, 1920), 65502);
        assert_eq!(normalize(-5, 0, 1920), 1);
        assert_eq!(normalize(40000, 0, 40000), 65535);
        assert!(normalize(39999, 0, 40000) <= 65535);
        assert_eq!(normalize(3, 0, 0), 0);
    }

    #[test]
    fn key_flags_mapping() {
        assert_eq!(key_flags(0x1e, false, true), KEYEVENTF_SCANCODE);
        assert_eq!(key_flags(0x1d, true, false), KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP);
        assert_eq!(key_flags(0, false, false), KEYEVENTF_KEYUP);
        let i = key_input((0, false, 0x41), true);
        unsafe {
            assert_eq!(i.Anonymous.ki.wVk, VIRTUAL_KEY(0x41));
            assert_eq!(i.Anonymous.ki.dwExtraInfo, INJECT_MARK);
        }
        let i = key_input((0x1e, false, 0x41), true);
        unsafe {
            assert_eq!(i.Anonymous.ki.wVk, VIRTUAL_KEY(0));
            assert_eq!(i.Anonymous.ki.wScan, 0x1e);
        }
    }

    #[test]
    fn button_mapping() {
        assert_eq!(button_flags(Button::X2, true), (MOUSEEVENTF_XDOWN, 2));
        assert_eq!(button_flags(Button::Left, false), (MOUSEEVENTF_LEFTUP, 0));
    }

    #[test]
    fn held_tracking() {
        let mut g = Inner { active: true, epoch: 3, ..Default::default() };
        assert!(g.current(3));
        assert!(!g.current(2));
        g.track_key((0x1d, false, 0xa2), true);
        g.track_key((0x1e, false, 0x41), true);
        g.track_key((0x1e, false, 0x41), true); // 自动重复不重复记录
        g.track_key((0x30, false, 0x42), false); // 没见过的抬起
        g.buttons[button_index(Button::Right)] = true;
        let (keys, btns) = g.take_held();
        assert_eq!(keys, vec![(0x1e, false, 0x41), (0x1d, false, 0xa2)]);
        assert_eq!(btns, vec![Button::Right]);
        let (keys, btns) = g.take_held();
        assert!(keys.is_empty() && btns.is_empty());
        assert!(!g.holds_any());
    }

    #[test]
    fn failed_release_is_kept() {
        // take_held 的抬起顺序：0x1d 先按，0x1e 后按
        let keys = [(0x1e, false, 0x41), (0x1d, false, 0xa2)];
        let btns = [Button::Left];
        assert_eq!(unsent(&keys, &btns, 0), (&keys[..], &btns[..]));
        assert_eq!(unsent(&keys, &btns, 1), (&keys[1..], &btns[..]));
        assert_eq!(unsent(&keys, &btns, 2), (&keys[2..], &btns[..]));
        assert_eq!(unsent(&keys, &btns, 3), (&keys[2..], &btns[1..]));

        let mut g = Inner::default();
        g.track_key((0x30, false, 0x42), true);
        g.put_back(&keys, &btns);
        assert!(g.holds_any());
        // 放回后仍按原先的按下顺序逆序抬起
        let (k, b) = g.take_held();
        assert_eq!(k, vec![(0x30, false, 0x42), (0x1e, false, 0x41), (0x1d, false, 0xa2)]);
        assert_eq!(b, vec![Button::Left]);
    }

    // ── 用假的 SendInput 测 Injector（每个测试一个线程，thread_local 互不干扰） ──

    // 注入记录：键盘 = (vk 或 scan, 是否抬起)；鼠标按钮 = (0x1000 | flags, true)；移动/滚轮不记。
    thread_local! {
        static SENT: RefCell<Vec<(u32, bool)>> = const { RefCell::new(Vec::new()) };
        static FAIL: Cell<bool> = const { Cell::new(false) };
        static NOW: Cell<u32> = const { Cell::new(10_000) };
        static LAST: Cell<Option<u32>> = const { Cell::new(None) };
    }

    fn fake_send(inputs: &[INPUT]) -> u32 {
        if FAIL.with(|f| f.get()) {
            return 0;
        }
        SENT.with(|s| {
            let mut s = s.borrow_mut();
            for i in inputs {
                unsafe {
                    if i.r#type == INPUT_KEYBOARD {
                        let k = i.Anonymous.ki;
                        let id = if k.wScan != 0 { k.wScan as u32 } else { k.wVk.0 as u32 };
                        s.push((id, k.dwFlags.0 & KEYEVENTF_KEYUP.0 != 0));
                    } else {
                        let f = i.Anonymous.mi.dwFlags;
                        if f.0 & MOUSEEVENTF_MOVE.0 == 0 && f.0 & (MOUSEEVENTF_WHEEL.0 | MOUSEEVENTF_HWHEEL.0) == 0 {
                            s.push((0x1000 | f.0, true));
                        }
                    }
                }
            }
        });
        inputs.len() as u32
    }

    fn sent() -> Vec<(u32, bool)> {
        SENT.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    const FAKE: Sys = Sys { send: fake_send, last_input: || LAST.with(|c| c.get()), now: || NOW.with(|c| c.get()) };

    fn injector() -> (Injector, HubRx) {
        let (hub, rx) = Hub::new(crate::config::Config::default());
        let inj = Injector { hub: Arc::new(hub), inner: Mutex::new(Inner::default()), play_cv: Condvar::new(), sys: FAKE };
        inj.begin_session(1);
        (inj, rx)
    }

    fn yields(rx: &HubRx) -> Vec<u32> {
        let mut v = vec![];
        while let Ok(m) = rx.net.try_recv() {
            if let Out::Msg(Msg::Yield { epoch }) = m {
                v.push(epoch);
            }
        }
        v
    }

    #[test]
    fn stale_releases_held_keys_and_keeps_session() {
        let (inj, rx) = injector();
        inj.enter(1, 5, 10, 10);
        inj.key(1, 5, 0x1e, false, 0x41, true);
        inj.key(1, 5, 0x38, false, 0xA4, true); // LAlt
        inj.button(1, 5, 10, 10, Button::Left, true);
        sent();
        inj.stale(2); // 别的会话：不管
        assert!(sent().is_empty());
        inj.stale(1);
        // 掩码键按下抬起 → 逆序抬起 LAlt、A → 抬起左键
        assert_eq!(
            sent(),
            vec![
                (VK_MASK as u32, false),
                (VK_MASK as u32, true),
                (0x38, true),
                (0x1e, true),
                (0x1000 | MOUSEEVENTF_LEFTUP.0, true)
            ]
        );
        // 接管没结束：之后的按键照常注入
        inj.key(1, 5, 0x1e, false, 0x41, true);
        assert_eq!(sent(), vec![(0x1e, false)]);
        // 再 stale 一次只抬这个键（不需要掩码）
        inj.stale(1);
        assert_eq!(sent(), vec![(0x1e, true)]);
        assert!(yields(&rx).is_empty());
    }

    /// 收到的 input 通知里 KbdIn / Driven 的序列。
    fn input_events(rx: &HubRx) -> Vec<String> {
        rx.input
            .try_iter()
            .filter_map(|m| match m {
                ToInput::KbdIn(b) => Some(format!("kbd:{b}")),
                ToInput::Driven(b) => Some(format!("driven:{b}")),
                _ => None,
            })
            .collect()
    }

    fn kbd_notes(rx: &HubRx) -> Vec<bool> {
        rx.desktop
            .try_iter()
            .filter_map(|m| match m {
                crate::hub::ToDesktop::KbdNote(ok) => Some(ok),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn kbd_enter_accepts_keys_without_being_driven() {
        let (inj, rx) = injector();
        input_events(&rx);
        // 没开始：按键不注入
        inj.key(1, 9, 0x1e, false, 0x41, true);
        assert!(sent().is_empty());
        inj.kbd_enter(1, 9);
        assert_eq!(input_events(&rx), vec!["kbd:true"]);
        assert_eq!(inj.lock().kbd, Some(9));
        // 一开始就弹“键盘”（不等打字）；同一轮重复的 KbdEnter 不再弹
        assert_eq!(kbd_notes(&rx), vec![true]);
        inj.kbd_enter(1, 9);
        assert!(kbd_notes(&rx).is_empty());
        // 不移动光标、不进入被控
        assert!(sent().is_empty());
        assert!(!inj.hub.status().driven);
        // 这一轮的键注入；别的轮次不注入；打字时不再弹
        inj.key(1, 9, 0x1e, false, 0x41, true);
        inj.key(1, 8, 0x30, false, 0x42, true);
        inj.key(1, 9, 0x1e, false, 0x41, false);
        inj.key(1, 9, 0x1f, false, 0x53, true);
        assert_eq!(sent(), vec![(0x1e, false), (0x1e, true), (0x1f, false)]);
        assert!(kbd_notes(&rx).is_empty());
        // 只收键盘时鼠标消息不理
        inj.button(1, 9, 1, 1, Button::Left, true);
        assert!(sent().is_empty());
        // 旧轮次的 KbdLeave 不理；本轮的抬起按着的键
        inj.kbd_leave(1, 8);
        assert!(sent().is_empty());
        inj.kbd_leave(1, 9);
        assert_eq!(sent(), vec![(0x1f, true)]);
        assert_eq!(input_events(&rx), vec!["kbd:false"]);
        assert_eq!(inj.lock().kbd, None);
        inj.key(1, 9, 0x1e, false, 0x41, true);
        assert!(sent().is_empty());
        assert!(yields(&rx).is_empty());
    }

    #[test]
    fn kbd_enter_refused_when_paused_and_cleared_by_enter_and_session() {
        let (inj, rx) = injector();
        inj.hub.set_status(|s| s.paused = true);
        inj.kbd_enter(1, 4);
        assert_eq!(yields(&rx), vec![4]);
        inj.key(1, 4, 0x1e, false, 0x41, true);
        assert!(sent().is_empty());
        inj.hub.set_status(|s| s.paused = false);

        // 对方的鼠标跨过来（enter）：只收键盘结束，按着的键先抬起
        inj.kbd_enter(1, 5);
        inj.key(1, 5, 0x1e, false, 0x41, true);
        sent();
        input_events(&rx);
        inj.enter(1, 6, 10, 10);
        let s = sent();
        assert!(s.contains(&(0x1e, true)), "{s:?}");
        assert_eq!(input_events(&rx), vec!["kbd:false", "driven:true"]);
        inj.key(1, 5, 0x1e, false, 0x41, true);
        assert!(sent().is_empty());

        // 只收键盘时对方发来 KbdEnter 前还在“驱动”（残留）：结束被控
        inj.kbd_enter(1, 7);
        assert_eq!(input_events(&rx), vec!["driven:false", "kbd:true"]);
        // 会话结束：清掉
        inj.key(1, 7, 0x1e, false, 0x41, true);
        sent();
        inj.end_session(1);
        assert_eq!(sent(), vec![(0x1e, true)]);
        assert_eq!(input_events(&rx), vec!["kbd:false", "driven:false"]);
    }

    #[test]
    fn crossing_in_shows_keyboard_tip_once() {
        let (inj, rx) = injector();
        inj.enter(1, 5, 10, 10);
        assert_eq!(kbd_notes(&rx), vec![true]);
        // 同一轮重复的 Enter 不再弹；新一轮再弹
        inj.enter(1, 5, 10, 10);
        assert!(kbd_notes(&rx).is_empty());
        inj.leave(1, 5);
        inj.enter(1, 6, 10, 10);
        assert_eq!(kbd_notes(&rx), vec![true]);
        // 注入失败（安全桌面）：不弹
        inj.leave(1, 6);
        FAIL.with(|f| f.set(true));
        inj.enter(1, 7, 10, 10);
        FAIL.with(|f| f.set(false));
        assert!(kbd_notes(&rx).is_empty());
    }

    #[test]
    fn switching_rounds_keeps_held_keys() {
        // 驱动 → 只转发键盘（KbdEnter.prev = 驱动轮次）：键留着，鼠标按钮抬起，被控结束
        let (inj, rx) = injector();
        inj.enter(1, 5, 10, 10);
        inj.key(1, 5, 0x2a, false, 0xA0, true); // LShift
        inj.button(1, 5, 10, 10, Button::Left, true);
        sent();
        input_events(&rx);
        // 对方跨屏过来：马上弹“键盘”
        assert_eq!(kbd_notes(&rx), vec![true]);
        inj.kbd_enter_from(1, 6, 5);
        assert_eq!(sent(), vec![(0x1000 | MOUSEEVENTF_LEFTUP.0, true)]);
        assert_eq!(input_events(&rx), vec!["driven:false", "kbd:true"]);
        // 键盘本来就在这台（从驱动转过来）：不弹
        assert!(kbd_notes(&rx).is_empty());
        // 迟到的旧轮次 Leave 不理；新一轮的抬起照常
        inj.leave(1, 5);
        assert!(sent().is_empty());
        inj.key(1, 6, 0x2a, false, 0xA0, false);
        assert_eq!(sent(), vec![(0x2a, true)]);

        // 只转发键盘 → 驱动（Enter.prev = 键盘轮次）：键留着
        let (inj, rx) = injector();
        inj.kbd_enter(1, 7);
        inj.key(1, 7, 0x1d, false, 0xA2, true); // LCtrl
        sent();
        kbd_notes(&rx);
        inj.enter_from(1, 8, 10, 10, 7);
        assert!(sent().iter().all(|k| k.0 != 0x1d), "Ctrl 不该被抬起");
        // 键盘本来就在这台（从只收键盘转过来）：不弹
        assert!(kbd_notes(&rx).is_empty());
        assert!(inj.lock().kbd.is_none());
        inj.key(1, 8, 0x1d, false, 0xA2, false);
        assert_eq!(sent(), vec![(0x1d, true)]);
        // prev 对不上：照旧先抬起残留
        inj.kbd_enter(1, 9);
        inj.key(1, 9, 0x1d, false, 0xA2, true);
        sent();
        inj.enter_from(1, 10, 10, 10, 3);
        assert!(sent().contains(&(0x1d, true)));
        // 暂停时不接管：只收键盘那一轮也结束（对方不会再发 KbdLeave），按着的键抬起
        inj.kbd_enter(1, 11);
        inj.key(1, 11, 0x1d, false, 0xA2, true);
        sent();
        inj.hub.set_status(|s| s.paused = true);
        inj.enter_from(1, 12, 10, 10, 11);
        assert_eq!(sent(), vec![(0x1d, true)]);
        assert!(inj.lock().kbd.is_none());
        assert_eq!(yields(&rx), vec![12]);
    }

    #[test]
    fn kbd_injection_failure_yields_with_kbd_epoch() {
        let (inj, rx) = injector();
        inj.kbd_enter(1, 11);
        assert_eq!(kbd_notes(&rx), vec![true]);
        FAIL.with(|f| f.set(true));
        inj.key(1, 11, 0x1e, false, 0x41, true);
        FAIL.with(|f| f.set(false));
        assert_eq!(yields(&rx), vec![11]);
        assert_eq!(kbd_notes(&rx), vec![false]);
        // 对方没动静：抬起按着的键、提示一次（同一轮不重复）
        inj.kbd_enter(1, 12);
        inj.stale(1);
        inj.stale(1);
        assert_eq!(kbd_notes(&rx), vec![true, false]);
    }

    #[test]
    fn enter_refused_while_local_user_active() {
        let (inj, rx) = injector();
        NOW.with(|c| c.set(10_000));
        LAST.with(|c| c.set(Some(9_700))); // 300ms 前本机有人动过
        inj.enter(1, 7, 10, 10);
        assert_eq!(yields(&rx), vec![7]);
        // 没接管：按键不注入
        inj.key(1, 7, 0x1e, false, 0x41, true);
        assert!(sent().is_empty());
        // 过了 500ms：接管
        LAST.with(|c| c.set(Some(9_400)));
        inj.enter(1, 8, 10, 10);
        assert!(yields(&rx).is_empty());
        inj.key(1, 8, 0x1e, false, 0x41, true);
        assert_eq!(sent(), vec![(0x1e, false)]);
    }

    #[test]
    fn injection_failure_yields_at_most_once_per_second() {
        let (inj, rx) = injector();
        inj.enter(1, 9, 10, 10);
        FAIL.with(|f| f.set(true));
        inj.moved(1, 9, 1, 20, 20, 0);
        inj.moved(1, 9, 2, 21, 20, 0);
        inj.key(1, 9, 0x1e, false, 0x41, true);
        assert_eq!(yields(&rx), vec![9]);
        NOW.with(|c| c.set(c.get() + 1_100));
        inj.wheel(1, 9, 20, 20, 120, false);
        assert_eq!(yields(&rx), vec![9]);
        // 按下没注入成功也记着，恢复后放手时会抬起
        FAIL.with(|f| f.set(false));
        inj.leave(1, 9);
        assert_eq!(sent(), vec![(0x1e, true)]);
    }

    #[test]
    fn timestamped_moves_play_and_clicks_drop_queued_positions() {
        let (inj, _rx) = injector();
        inj.enter(1, 4, 0, 0);
        // 第一个带时间戳的位置：还没有晚到统计，到了就播
        inj.moved(1, 4, 1, 5, 5, 1_000);
        assert_eq!(inj.lock().seq, 1);
        // 排进去一个还没到点的位置（发送时刻在很久以后），点击带着最新位置：排着的必须扔掉
        {
            let mut g = inj.lock();
            let now = smooth::now_us();
            g.play.push(now, 1_000 + 50_000, Item { epoch: 4, seq: 2, x: 6, y: 6 });
            g.play.push(now, 1_000 + 52_000, Item { epoch: 4, seq: 3, x: 7, y: 7 });
        }
        inj.button(1, 4, 9, 9, Button::Left, true);
        assert!(!inj.lock().play.pending());
        // 新的接管：平滑状态全部重来，seq 从头算
        inj.leave(1, 4);
        inj.enter(1, 5, 0, 0);
        inj.moved(1, 5, 1, 1, 1, 77);
        assert_eq!(inj.lock().seq, 1);
    }

    #[test]
    fn malformed_keys_and_table_limit() {
        let (inj, _rx) = injector();
        inj.enter(1, 3, 0, 0);
        inj.key(1, 3, 0x1e, false, 0x100, true); // vk 越界
        inj.key(1, 3, 0x200, false, 0x41, true); // scan 越界
        assert!(sent().is_empty());
        for i in 0..70u16 {
            inj.key(1, 3, 0x100 + i, false, 0x41, true);
        }
        assert_eq!(sent().len(), MAX_KEYS);
        // 已记着的键自动重复照常；抬起照常
        inj.key(1, 3, 0x100, false, 0x41, true);
        inj.key(1, 3, 0x100, false, 0x41, false);
        assert_eq!(sent(), vec![(0x100, false), (0x100, true)]);
        // 腾出一个位置后又能按
        inj.key(1, 3, 0x150, false, 0x41, true);
        assert_eq!(sent(), vec![(0x150, false)]);
        inj.leave(1, 3);
        assert_eq!(sent().len(), MAX_KEYS);
    }

    #[test]
    fn release_masks_alt_and_win_only() {
        let (v, pre) = release_inputs(&[(0x2a, false, 0xA0)], &[]);
        assert_eq!((v.len(), pre), (1, 0));
        let (v, pre) = release_inputs(&[(0x5b, true, 0x5B)], &[Button::Right]);
        assert_eq!((v.len(), pre), (4, 2));
        let (v, pre) = release_inputs(&[], &[]);
        assert_eq!((v.len(), pre), (0, 0));
    }
}
