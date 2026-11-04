//! 被控端：把对方发来的输入注入本机（SendInput）。可从任意线程调用（内部 Mutex）。
//!
//! 规则：
//! - 所有注入事件的 dwExtraInfo = proto::INJECT_MARK，让本机钩子忽略它们。
//! - 鼠标位置用 MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK，
//!   坐标按虚拟桌面（GetSystemMetrics SM_XVIRTUALSCREEN/SM_YVIRTUALSCREEN/SM_CXVIRTUALSCREEN/SM_CYVIRTUALSCREEN）
//!   归一化到 0..65535：nx = ((x - vx) * 65535 + (vw - 1) / 2) / (vw - 1)（用 i64 计算）。
//! - Button/Wheel 先移动到消息里的 (x, y) 再按下/抬起/滚动（同一次 SendInput 里两个 INPUT）。
//! - Key：scan != 0 时用 KEYEVENTF_SCANCODE（ext 时加 KEYEVENTF_EXTENDEDKEY）；scan == 0 时用 vk。
//! - 记录本机被按下还没抬起的键（(scan, ext, vk)）和鼠标按钮；leave / release_all / 会话断开时全部抬起，防止卡键。
//! - epoch 不等于当前 epoch 的消息丢弃；Move 只接受同一 epoch 内 seq 更大的。
//! - enter 时通知 input 线程 ToInput::Driven(true)，leave/release_all 时 Driven(false)，并更新 hub.status.driven。
//! - SendInput 返回 0（锁屏/UAC 安全桌面时会这样）不要重试循环，直接放弃这一次。

use crate::hub::{Hub, ToInput};
use crate::proto::{Button, INJECT_MARK};
use std::sync::{Arc, Mutex, MutexGuard};
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

#[derive(Default)]
struct Inner {
    epoch: u32,
    active: bool,
    /// 当前 epoch 内收到的最大 Move seq。
    seq: u32,
    keys: Vec<KeyId>,
    /// Left, Right, Middle, X1, X2
    buttons: [bool; 5],
}

impl Inner {
    /// 本条消息属于当前接管吗？
    fn current(&self, epoch: u32) -> bool {
        self.active && epoch == self.epoch
    }

    /// 记录按键状态（没见过按下的抬起照样注入，无害）。
    fn track_key(&mut self, k: KeyId, down: bool) {
        let pos = self.keys.iter().position(|h| *h == k);
        match (down, pos) {
            (true, None) => self.keys.push(k),
            (false, Some(i)) => {
                self.keys.remove(i);
            }
            _ => {}
        }
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

pub struct Injector {
    hub: Arc<Hub>,
    inner: Mutex<Inner>,
}

impl Injector {
    pub fn new(hub: Arc<Hub>) -> Self {
        Self { hub, inner: Mutex::new(Inner::default()) }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 对方接管：光标放到 (x, y)。
    pub fn enter(&self, epoch: u32, x: i32, y: i32) {
        {
            let mut g = self.lock();
            // 上一次接管没正常结束：先把残留的按键抬起来
            if g.active && g.epoch != epoch {
                release(&mut g);
            }
            g.epoch = epoch;
            g.active = true;
            g.seq = 0;
            send(&[move_input(x, y)]);
        }
        self.hub.to_input(ToInput::Driven(true));
        self.hub.set_status(|s| s.driven = true);
    }

    /// 对方放手：抬起所有按着的键和按钮。
    pub fn leave(&self, epoch: u32) {
        {
            let mut g = self.lock();
            if !g.current(epoch) {
                return;
            }
            release(&mut g);
            g.active = false;
        }
        self.hub.to_input(ToInput::Driven(false));
        self.hub.set_status(|s| s.driven = false);
    }

    /// UDP 来的绝对位置。
    pub fn moved(&self, epoch: u32, seq: u32, x: i32, y: i32) {
        let mut g = self.lock();
        if !g.current(epoch) || seq <= g.seq {
            return;
        }
        g.seq = seq;
        send(&[move_input(x, y)]);
    }

    pub fn button(&self, epoch: u32, x: i32, y: i32, b: Button, down: bool) {
        let mut g = self.lock();
        if !g.current(epoch) {
            return;
        }
        g.buttons[button_index(b)] = down;
        send(&[move_input(x, y), button_input(b, down)]);
    }

    pub fn wheel(&self, epoch: u32, x: i32, y: i32, delta: i32, horizontal: bool) {
        let g = self.lock();
        if !g.current(epoch) || delta == 0 {
            return;
        }
        let flags = if horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL };
        send(&[move_input(x, y), mouse(0, 0, delta as u32, flags)]);
    }

    pub fn key(&self, epoch: u32, scan: u16, ext: bool, vk: u16, down: bool) {
        let mut g = self.lock();
        if !g.current(epoch) || (scan == 0 && vk == 0) {
            return;
        }
        g.track_key((scan, ext, vk), down);
        send(&[key_input((scan, ext, vk), down)]);
    }

    /// 会话断开等：抬起一切，结束被控状态。
    pub fn release_all(&self) {
        {
            let mut g = self.lock();
            release(&mut g);
            g.active = false;
        }
        self.hub.to_input(ToInput::Driven(false));
        self.hub.set_status(|s| s.driven = false);
    }

    pub fn active(&self) -> bool {
        self.lock().active
    }
}

/// 抬起所有记录中按着的键和按钮（持锁调用）。
fn release(g: &mut Inner) {
    let (keys, btns) = g.take_held();
    let mut v: Vec<INPUT> = keys.into_iter().map(|k| key_input(k, false)).collect();
    v.extend(btns.into_iter().map(|b| button_input(b, false)));
    if !v.is_empty() {
        send(&v);
    }
}

/// 注入；失败（安全桌面等）就放弃，不重试。
fn send(inputs: &[INPUT]) {
    unsafe {
        let _ = SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
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

/// 虚拟桌面坐标 → 0..65535。
fn normalize(x: i32, origin: i32, size: i32) -> i32 {
    if size <= 1 {
        return 0;
    }
    let (x, o, s) = (x as i64, origin as i64, size as i64);
    let x = x.clamp(o, o + s - 1);
    (((x - o) * 65535 + (s - 1) / 2) / (s - 1)) as i32
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

    #[test]
    fn normalize_edges() {
        assert_eq!(normalize(0, 0, 1920), 0);
        assert_eq!(normalize(1919, 0, 1920), 65535);
        // 虚拟桌面原点为负（左边还有一块屏）
        assert_eq!(normalize(-1920, -1920, 3840), 0);
        assert_eq!(normalize(1919, -1920, 3840), 65535);
        let mid = normalize(0, -1920, 3840);
        assert!((32760..=32780).contains(&mid), "{mid}");
        // 越界钳住
        assert_eq!(normalize(5000, 0, 1920), 65535);
        assert_eq!(normalize(-5, 0, 1920), 0);
        assert_eq!(normalize(3, 0, 1), 0);
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
    }
}
