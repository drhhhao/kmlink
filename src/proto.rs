//! 两台电脑之间的消息。
//!
//! 可靠通道（TCP，加密分帧）：
//!   帧 = [u32 LE 密文长度][ChaCha20-Poly1305 密文]，明文第一个字节是类型：
//!     0 = JSON 编码的 `Msg`
//!     1 = 剪贴板压缩流分片：[u32 LE id][u32 LE 偏移（压缩数据里的）][数据]
//!     2 = 剪贴板压缩流结束：[u32 LE id][u32 LE 压缩数据总长]
//!     3 = 剪贴板压缩流作废（发送方中途放弃）：[u32 LE id]
//!
//! 剪贴板图片 / 大段文字是流式的：`Msg::ClipStart{id, format, raw_len}` → 若干分片 → 结束帧。
//! 压缩格式是 raw deflate（level 1）；发送方边压缩边发（`Deflater`），接收方收到开始就按 raw_len 分配好最终缓冲，
//! 每片直接解压进去（`Inflater`）。接收方见到新 id 丢弃旧的。
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
pub const FRAME_END: u8 = 2;
pub const FRAME_ABORT: u8 = 3;
/// 剪贴板分片大小上限（明文数据部分）。小一点：按键等消息最多排在一个分片后面。
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
    /// 剪贴板压缩流开始：之后是这个 id 的 FRAME_CHUNK（raw deflate），以 FRAME_END 结束（或 FRAME_ABORT 作废）。
    /// format = CF_DIB(8) / CF_DIBV5(17) 图片，或 CF_UNICODETEXT(13) 大段文字（UTF-16 LE，不带结尾 0）；
    /// raw_len = 解压后的长度（接收方据此一次分配好）。
    ClipStart { id: u32, format: u32, raw_len: u32 },
    /// 本机复制的内容没同步（太大 / 敏感）：对方也弹“未同步”提示。kind：0 文字、1 图片、2 敏感；
    /// detail = 第二行（原因，≤ 24 字）。旧版本收到会当成无法解析的帧忽略。
    ClipNote { kind: u8, detail: String },
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

pub fn encode_end(id: u32, total: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(9);
    v.push(FRAME_END);
    v.extend_from_slice(&id.to_le_bytes());
    v.extend_from_slice(&total.to_le_bytes());
    v
}

pub fn encode_abort(id: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(5);
    v.push(FRAME_ABORT);
    v.extend_from_slice(&id.to_le_bytes());
    v
}

/// 解出来的帧；分片数据直接借用明文缓冲（不复制）。
pub enum Frame<'a> {
    Msg(Msg),
    Chunk { id: u32, offset: u32, data: &'a [u8] },
    End { id: u32, total: u32 },
    Abort { id: u32 },
}

pub fn decode_frame(b: &[u8]) -> Option<Frame<'_>> {
    let u = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    match *b.first()? {
        FRAME_MSG => serde_json::from_slice(&b[1..]).ok().map(Frame::Msg),
        FRAME_CHUNK if b.len() >= 9 => Some(Frame::Chunk { id: u(1), offset: u(5), data: &b[9..] }),
        FRAME_END if b.len() >= 9 => Some(Frame::End { id: u(1), total: u(5) }),
        FRAME_ABORT if b.len() >= 5 => Some(Frame::Abort { id: u(1) }),
        _ => None,
    }
}

// ---------- 剪贴板压缩流的编解码（raw deflate，流式） ----------

/// 流式压缩（level 1）：一段段喂原始数据，凑满一片（CHUNK 字节）就交出去；内部只有压缩器状态和一片大小的缓冲。
pub struct Deflater {
    c: Box<miniz_oxide::deflate::core::CompressorOxide>,
    out: Vec<u8>,
    pos: usize,
}

impl Deflater {
    pub fn new() -> Deflater {
        use miniz_oxide::deflate::core::{CompressorOxide, create_comp_flags_from_zip_params};
        Deflater { c: Box::new(CompressorOxide::new(create_comp_flags_from_zip_params(1, 0, 0))), out: vec![0; CHUNK], pos: 0 }
    }

    /// 喂一段原始数据（last = 最后一段，之后不能再喂）。每凑满一片（≤ CHUNK 字节，结束时可能更短）调一次 emit；
    /// emit 返回 false 时立刻停止并返回 false。压缩器出错也返回 false。
    pub fn write(&mut self, mut input: &[u8], last: bool, emit: &mut dyn FnMut(Vec<u8>) -> bool) -> bool {
        use miniz_oxide::deflate::core::{TDEFLFlush, TDEFLStatus, compress};
        let flush = if last { TDEFLFlush::Finish } else { TDEFLFlush::None };
        loop {
            let (st, used, wrote) = compress(&mut self.c, input, &mut self.out[self.pos..], flush);
            self.pos += wrote;
            input = &input[used..];
            let done = match st {
                TDEFLStatus::Done => true,
                TDEFLStatus::Okay => false,
                _ => return false,
            };
            if self.pos == self.out.len() || (done && self.pos > 0) {
                let piece = self.out[..self.pos].to_vec();
                self.pos = 0;
                if !emit(piece) {
                    return false;
                }
                if done {
                    return true;
                }
                continue;
            }
            // 输出缓冲没满：压缩器手里没有待交的输出了
            if done || (!last && input.is_empty()) {
                return true;
            }
            if used == 0 && wrote == 0 {
                return false; // 没有进展（不应发生）
            }
        }
    }
}

/// 流式解压：直接解进调用方给的整块最终缓冲（长度 = 原始长度；不环绕，回溯引用读的就是这块缓冲本身）。
pub struct Inflater {
    d: Box<miniz_oxide::inflate::core::DecompressorOxide>,
    /// 已解出的字节数。
    out: usize,
    done: bool,
}

impl Inflater {
    pub fn new() -> Inflater {
        Inflater { d: Box::default(), out: 0, done: false }
    }

    /// 已解出的字节数（out[..written] 有效）。
    pub fn written(&self) -> usize {
        self.out
    }

    /// 压缩流已完整结束。
    pub fn done(&self) -> bool {
        self.done
    }

    /// 解一片：out 每次都是同一块完整的最终缓冲。数据比 out 长、损坏、结束后还有多余数据：Err。
    pub fn feed(&mut self, mut input: &[u8], out: &mut [u8]) -> Result<(), &'static str> {
        use miniz_oxide::inflate::TINFLStatus;
        use miniz_oxide::inflate::core::{decompress, inflate_flags};
        let flags = inflate_flags::TINFL_FLAG_HAS_MORE_INPUT | inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        loop {
            if self.done {
                return if input.is_empty() { Ok(()) } else { Err("结束后还有多余数据") };
            }
            let (st, used, wrote) = decompress(&mut self.d, input, out, self.out, flags);
            input = &input[used..];
            self.out += wrote;
            match st {
                TINFLStatus::Done => self.done = true,
                TINFLStatus::NeedsMoreInput => {
                    if input.is_empty() {
                        return Ok(());
                    }
                    if used == 0 && wrote == 0 {
                        return Err("解压没有进展");
                    }
                }
                // 输出已满且这片也用完了：miniz 在“缺输入但输出已满”时也这么报，可能只差结束标记 ——
                // 等下一片（真的多出数据时，下一片会带着剩余输入再报一次；结束时没 done 也会被拒收）
                TINFLStatus::HasMoreOutput if input.is_empty() => return Ok(()),
                TINFLStatus::HasMoreOutput => return Err("解压后比声称的长"),
                _ => return Err("数据损坏"),
            }
        }
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
pub(crate) mod tests {
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
        match decode_frame(&encode_end(4, 123_456)) {
            Some(Frame::End { id, total }) => assert_eq!((id, total), (4, 123_456)),
            _ => panic!(),
        }
        match decode_frame(&encode_abort(5)) {
            Some(Frame::Abort { id }) => assert_eq!(id, 5),
            _ => panic!(),
        }
        assert!(decode_frame(&encode_end(4, 1)[..8]).is_none());
        assert!(decode_frame(&[FRAME_ABORT, 1, 2]).is_none());
        let m = Msg::ClipStart { id: 9, format: 17, raw_len: 1 << 20 };
        match decode_frame(&encode_msg(&m)) {
            Some(Frame::Msg(d)) => assert_eq!(d, m),
            _ => panic!(),
        }
    }

    /// 流式压缩成分片（每次喂 step 字节；step = 0 表示一次喂完）。
    pub(crate) fn deflate_pieces(raw: &[u8], step: usize) -> Vec<Vec<u8>> {
        let mut z = Deflater::new();
        let mut pieces = Vec::new();
        let mut emit = |p: Vec<u8>| {
            pieces.push(p);
            true
        };
        if step == 0 || raw.is_empty() {
            assert!(z.write(raw, true, &mut emit));
        } else {
            let mut it = raw.chunks(step).peekable();
            while let Some(b) = it.next() {
                assert!(z.write(b, it.peek().is_none(), &mut emit));
            }
        }
        pieces
    }

    /// 按给定的片解压到 raw_len 大小的缓冲。
    fn inflate_pieces(pieces: &[Vec<u8>], raw_len: usize) -> Result<Vec<u8>, &'static str> {
        let mut out = vec![0u8; raw_len];
        let mut z = Inflater::new();
        for p in pieces {
            z.feed(p, &mut out)?;
        }
        if !z.done() || z.written() != raw_len {
            return Err("不完整");
        }
        Ok(out)
    }

    fn noise(n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        getrandom::fill(&mut v).unwrap();
        v
    }

    #[test]
    fn stream_codec_roundtrip() {
        let mut compressible: Vec<u8> = (0..CHUNK * 40).map(|i| ((i / 7) ^ (i >> 12)) as u8).collect();
        compressible.extend(noise(CHUNK * 3));
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),                 // 空
            vec![42],                   // 1 字节
            noise(CHUNK * 20 + 1),      // 不可压缩：分片数 ≈ 原始大小 / CHUNK
            noise(CHUNK),               // 恰好一片大小
            vec![0u8; 3 * 1024 * 1024], // 极易压缩
            compressible,
        ];
        for raw in &cases {
            for step in [0, 1000, CHUNK, 65536] {
                let pieces = deflate_pieces(raw, step);
                assert!(pieces.iter().all(|p| !p.is_empty() && p.len() <= CHUNK));
                // 和一次性解压的格式一致
                let all: Vec<u8> = pieces.concat();
                assert_eq!(&miniz_oxide::inflate::decompress_to_vec(&all).unwrap(), raw);
                // 按原来的分片解
                assert_eq!(&inflate_pieces(&pieces, raw.len()).unwrap(), raw);
                // 换一种切法（分片边界落在任意位置，含 1 字节一片）
                for cut in [1, 7, CHUNK - 1, CHUNK + 1] {
                    let re: Vec<Vec<u8>> = all.chunks(cut).map(|c| c.to_vec()).collect();
                    if re.len() <= 20_000 {
                        assert_eq!(&inflate_pieces(&re, raw.len()).unwrap(), raw);
                    }
                }
            }
        }
        // 不可压缩数据：输出不明显变大
        let n = noise(1 << 20);
        let total: usize = deflate_pieces(&n, 0).iter().map(Vec::len).sum();
        assert!(total < n.len() + n.len() / 100);
    }

    #[test]
    fn stream_codec_rejects_bad_data() {
        let raw = noise(100_000);
        let pieces = deflate_pieces(&raw, 0);
        let all = pieces.concat();
        // 声称的长度比实际短：超长 → Err
        let mut out = vec![0u8; raw.len() - 1];
        let mut z = Inflater::new();
        assert!(z.feed(&all, &mut out).is_err());
        // 声称的长度比实际长：结束了但没填满
        let mut out = vec![0u8; raw.len() + 1];
        let mut z = Inflater::new();
        assert!(z.feed(&all, &mut out).is_ok());
        assert!(z.done() && z.written() == raw.len());
        // 截断：没有结束
        let mut out = vec![0u8; raw.len()];
        let mut z = Inflater::new();
        assert!(z.feed(&all[..all.len() - 10], &mut out).is_ok());
        assert!(!z.done());
        // 结束后还有多余数据
        let mut z = Inflater::new();
        let mut extra = all.clone();
        extra.extend_from_slice(&[1, 2, 3]);
        assert!(z.feed(&extra, &mut out).is_err());
        // 垃圾
        let mut z = Inflater::new();
        assert!(z.feed(&[0xff; 64], &mut out).is_err());
        // emit 返回 false：压缩中止
        let mut z = Deflater::new();
        let mut n = 0;
        assert!(!z.write(&raw, true, &mut |_| {
            n += 1;
            n < 2
        }));
        assert_eq!(n, 2);
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
