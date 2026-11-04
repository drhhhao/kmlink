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

pub const PROTO_VERSION: u32 = 2;
/// TCP 握手魔数（KML2：握手带 X25519 临时公钥）。
pub const MAGIC: &[u8; 4] = b"KML2";
/// 旧版魔数：收到它说明对方版本太旧。
pub const MAGIC_V1: &[u8; 4] = b"KML1";
/// 我们自己注入的输入事件带这个 dwExtraInfo，钩子据此忽略。
pub const INJECT_MARK: usize = 0x4B4D_4C4E;

pub const FRAME_MSG: u8 = 0;
pub const FRAME_CHUNK: u8 = 1;
/// 剪贴板图片分片大小（明文数据部分）。小一点：按键等消息最多排在一个分片后面。
pub const CHUNK: usize = 16 * 1024;
/// 对方显示器：最多这么多块，坐标绝对值不超过这个数。
pub const MAX_MONITORS: usize = 16;
pub const MAX_COORD: i32 = 1 << 20;
/// 对方名字最多这么多个字符。
pub const MAX_NAME: usize = 64;

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
    /// epoch = 被控端当前（或刚拒绝的）接管轮次；控制端只认当前这一轮的 Yield（0 = 不确定，总是接受）。
    Yield {
        #[serde(default)]
        epoch: u32,
    },
    /// UDP 不可用（绑定失败/还没学到对方地址）时，鼠标位置改走可靠通道。语义同 UdpMsg::Move。
    Move { epoch: u32, seq: u32, x: i32, y: i32 },
    ClipText { text: String },
    /// 本机剪贴板变成了不同步的内容（敏感/超限/暂停期间复制的等）：两边不再一致，对方清掉“已同步”记录，
    /// 之后对方再复制和之前同样的内容也会照常发过来。只在“已同步 → 不一致”时发一次，收到不回发，不会来回。
    ClipForget,
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

/// 解出来的帧；分片数据直接借用明文缓冲（不复制）。
pub enum Frame<'a> {
    Msg(Msg),
    Chunk { id: u32, offset: u32, data: &'a [u8] },
}

pub fn decode_frame(b: &[u8]) -> Option<Frame<'_>> {
    match *b.first()? {
        FRAME_MSG => serde_json::from_slice(&b[1..]).ok().map(Frame::Msg),
        FRAME_CHUNK if b.len() >= 9 => Some(Frame::Chunk {
            id: u32::from_le_bytes(b[1..5].try_into().unwrap()),
            offset: u32::from_le_bytes(b[5..9].try_into().unwrap()),
            data: &b[9..],
        }),
        _ => None,
    }
}

/// 清洗对方 Hello 里的显示器：坐标限制在 ±MAX_COORD 内，丢掉空/反向矩形，最多 MAX_MONITORS 块。
/// （下游 geom 里有 clamp(min, max)，min > max 会 panic；release 是 panic=abort。）结果可能为空 = 当没有显示器。
pub fn sanitize_monitors(v: Vec<Rect>) -> Vec<Rect> {
    let c = |x: i32| x.clamp(-MAX_COORD, MAX_COORD);
    v.into_iter()
        .map(|r| Rect::new(c(r.l), c(r.t), c(r.r), c(r.b)))
        .filter(|r| r.r > r.l && r.b > r.t)
        .take(MAX_MONITORS)
        .collect()
}

/// 名字截到 MAX_NAME 个字符（按字符截，不会切坏 UTF-8）。
pub fn sanitize_name(s: String) -> String {
    match s.char_indices().nth(MAX_NAME) {
        Some((i, _)) => s[..i].to_string(),
        None => s,
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
            Some(Frame::Chunk { id, offset, data }) => assert_eq!((id, offset, data), (3, 65536, &b"abc"[..])),
            _ => panic!(),
        }
    }
    #[test]
    fn hello_monitors_sanitized() {
        let big = i32::MAX;
        let v = vec![
            Rect::new(0, 0, 1920, 1080),     // 正常
            Rect::new(10, 0, 10, 100),       // 宽 0
            Rect::new(0, 50, 100, 20),       // 反向
            Rect::new(-big, -big, big, big), // 超大：限制到 ±2^20
            Rect::new(big - 5, 0, big, 100), // 整块在范围外：限制后宽 0，丢掉
        ];
        let s = sanitize_monitors(v);
        assert_eq!(s, vec![Rect::new(0, 0, 1920, 1080), Rect::new(-MAX_COORD, -MAX_COORD, MAX_COORD, MAX_COORD)]);
        let many: Vec<Rect> = (0..100).map(|i| Rect::new(i, 0, i + 1, 1)).collect();
        assert_eq!(sanitize_monitors(many).len(), MAX_MONITORS);
        assert!(sanitize_monitors(vec![Rect::new(5, 5, 1, 1)]).is_empty());
        assert_eq!(sanitize_name("中".repeat(100)).chars().count(), MAX_NAME);
        assert_eq!(sanitize_name("PC".into()), "PC");
    }
}
