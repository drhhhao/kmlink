//! 两台电脑之间的消息。
//!
//! 可靠通道（TCP，加密分帧）：
//!   帧 = [u32 LE 密文长度][ChaCha20-Poly1305 密文]，明文第一个字节是类型：
//!     0 = JSON 编码的 `Msg`
//!     1 = 剪贴板图片分片：[u32 LE id][u32 LE 偏移][数据]
//!
//! 低延迟通道（UDP，加密）：只发鼠标“绝对位置”，丢了不要紧，下一包带着最新位置。
//!   明文 `UdpMsg::encode` 见下。
//!
//! 鼠标移动用对方坐标的绝对位置（控制端维护虚拟光标），按键/点击带上当时的位置，所以：
//!   - UDP 丢包、乱序不会让光标漂移（只取 seq 最大的那包）；
//!   - 点击总落在正确的位置（先把光标放到点击坐标再按下）。

use crate::geom::{Rect, Side};
use serde::{Deserialize, Serialize};

pub const PROTO_VERSION: u32 = 1;
/// TCP 握手魔数。
pub const MAGIC: &[u8; 4] = b"KML1";
/// 我们自己注入的输入事件带这个 dwExtraInfo，钩子据此忽略。
pub const INJECT_MARK: usize = 0x4B4D_4C4E;

pub const FRAME_MSG: u8 = 0;
pub const FRAME_CHUNK: u8 = 1;
/// 剪贴板图片分片大小（明文数据部分）。
pub const CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Msg {
    /// 连接建立后双方各发一次；显示器变化时重发。
    /// id：本进程启动时随机生成（`instance_id()`），用来发现“连到了自己”。
    Hello {
        ver: u32,
        name: String,
        monitors: Vec<Rect>,
        udp_port: u16,
        #[serde(default)]
        id: u64,
    },
    /// 排布（从发送方的角度）。接收方若 rev 更新，则镜像后保存。
    Layout { side: Side, offset: i32, scale: f64, rev: u64 },
    /// 控制端接管：把光标放到 (x, y)（接收方坐标）。epoch 每次接管 +1。
    Enter { epoch: u32, x: i32, y: i32 },
    /// 控制端放手：接收方松开所有按着的键和按钮。
    Leave { epoch: u32 },
    Button { epoch: u32, x: i32, y: i32, button: Button, down: bool },
    /// delta：一格 = 120；horizontal = 横向滚动。
    Wheel { epoch: u32, x: i32, y: i32, delta: i32, horizontal: bool },
    /// 扫描码 + 扩展键标志；scan 为 0 时用 vk。
    Key { epoch: u32, scan: u16, ext: bool, vk: u16, down: bool },
    /// 被控端本地有人动了鼠标/键盘（或被控端处于暂停）：请控制端放手。
    Yield,
    /// UDP 不可用（绑定失败/还没学到对方地址）时，鼠标位置改走可靠通道。语义同 UdpMsg::Move。
    Move { epoch: u32, seq: u32, x: i32, y: i32 },
    ClipText { text: String },
    /// 剪贴板图片开始：之后跟 total 字节的 FRAME_CHUNK（deflate 压缩后的 CF_DIB/CF_DIBV5 数据）。
    ClipImage { id: u32, format: u32, total: u32, raw_len: u32 },
    Ping { t: u64 },
    Pong { t: u64 },
}

/// UDP 明文。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdpMsg {
    /// 双方每秒互发，用来打通/学习对方 UDP 地址。
    Hi,
    /// 被控端把光标放到 (x, y)。同一 epoch 内只接受 seq 更大的。
    Move { epoch: u32, seq: u32, x: i32, y: i32 },
}

impl UdpMsg {
    pub fn encode(&self) -> Vec<u8> {
        match *self {
            UdpMsg::Hi => vec![0],
            UdpMsg::Move { epoch, seq, x, y } => {
                let mut v = Vec::with_capacity(17);
                v.push(1);
                v.extend_from_slice(&epoch.to_le_bytes());
                v.extend_from_slice(&seq.to_le_bytes());
                v.extend_from_slice(&x.to_le_bytes());
                v.extend_from_slice(&y.to_le_bytes());
                v
            }
        }
    }

    pub fn decode(b: &[u8]) -> Option<UdpMsg> {
        match *b.first()? {
            0 => Some(UdpMsg::Hi),
            1 if b.len() >= 17 => {
                let u = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
                Some(UdpMsg::Move { epoch: u(1), seq: u(5), x: u(9) as i32, y: u(13) as i32 })
            }
            _ => None,
        }
    }
}

/// 本进程的随机 id（Hello 里带上；收到的 Hello.id 等于它说明连到了自己）。
pub fn instance_id() -> u64 {
    static ID: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *ID.get_or_init(|| {
        let mut b = [0u8; 8];
        let _ = getrandom::fill(&mut b);
        u64::from_le_bytes(b) | 1
    })
}

pub fn encode_msg(m: &Msg) -> Vec<u8> {
    let mut v = vec![FRAME_MSG];
    serde_json::to_writer(&mut v, m).expect("serialize");
    v
}

pub fn encode_chunk(id: u32, offset: u32, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(9 + data.len());
    v.push(FRAME_CHUNK);
    v.extend_from_slice(&id.to_le_bytes());
    v.extend_from_slice(&offset.to_le_bytes());
    v.extend_from_slice(data);
    v
}

pub enum Frame {
    Msg(Msg),
    Chunk { id: u32, offset: u32, data: Vec<u8> },
}

pub fn decode_frame(b: &[u8]) -> Option<Frame> {
    match *b.first()? {
        FRAME_MSG => serde_json::from_slice(&b[1..]).ok().map(Frame::Msg),
        FRAME_CHUNK if b.len() >= 9 => Some(Frame::Chunk {
            id: u32::from_le_bytes(b[1..5].try_into().unwrap()),
            offset: u32::from_le_bytes(b[5..9].try_into().unwrap()),
            data: b[9..].to_vec(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn udp_roundtrip() {
        let m = UdpMsg::Move { epoch: 7, seq: 99, x: -5, y: 2160 };
        assert_eq!(UdpMsg::decode(&m.encode()), Some(m));
        assert_eq!(UdpMsg::decode(&UdpMsg::Hi.encode()), Some(UdpMsg::Hi));
        assert_eq!(UdpMsg::decode(&[1, 2]), None);
    }
    #[test]
    fn frame_roundtrip() {
        let m = Msg::Key { epoch: 1, scan: 0x1e, ext: false, vk: 0x41, down: true };
        match decode_frame(&encode_msg(&m)) {
            Some(Frame::Msg(d)) => assert_eq!(d, m),
            _ => panic!(),
        }
        match decode_frame(&encode_chunk(3, 65536, b"abc")) {
            Some(Frame::Chunk { id, offset, data }) => assert_eq!((id, offset, data.as_slice()), (3, 65536, &b"abc"[..])),
            _ => panic!(),
        }
    }
}
