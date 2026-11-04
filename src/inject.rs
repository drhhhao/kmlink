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

use crate::hub::Hub;
use crate::proto::Button;
use std::sync::Arc;

pub struct Injector {
    hub: Arc<Hub>,
}

impl Injector {
    pub fn new(hub: Arc<Hub>) -> Self {
        Self { hub }
    }
    /// 对方接管：光标放到 (x, y)。
    pub fn enter(&self, epoch: u32, x: i32, y: i32) {
        todo!()
    }
    /// 对方放手：抬起所有按着的键和按钮。
    pub fn leave(&self, epoch: u32) {
        todo!()
    }
    /// UDP 来的绝对位置。
    pub fn moved(&self, epoch: u32, seq: u32, x: i32, y: i32) {
        todo!()
    }
    pub fn button(&self, epoch: u32, x: i32, y: i32, b: Button, down: bool) {
        todo!()
    }
    pub fn wheel(&self, epoch: u32, x: i32, y: i32, delta: i32, horizontal: bool) {
        todo!()
    }
    pub fn key(&self, epoch: u32, scan: u16, ext: bool, vk: u16, down: bool) {
        todo!()
    }
    /// 会话断开等：抬起一切，结束被控状态。
    pub fn release_all(&self) {
        todo!()
    }
    pub fn active(&self) -> bool {
        todo!()
    }
}
