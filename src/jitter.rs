//! 声音：线上包格式 + 接收端抗抖动缓冲（纯逻辑，不碰系统 API，可离线测试 / 重放）。
//!
//! 包（UDP 明文，加密见 crypto::AudioSealer）：
//!   [codec u8][flags u8][seq u32 LE][send_us u32 LE][数据]
//!   codec 0 = PCM 16 位交错立体声，每包 5ms（240 帧 = 960 字节；整包 8+10+960+16 = 994 字节，不分片）。
//!   seq 每发一包 +1（静音时不发，也不加）；send_us = 发送端单调时钟（微秒，回绕）。
//!   flags bit0 = 一段声音的第一包（发送端从静音恢复 / 刚开始）。
//!
//! 缓冲：按 seq 放进环形槽；先攒到 target 再放，之后按声卡的节奏取。
//!   - 缺的包：后面已经有包了 → 这一包补静音（淡出淡入，不爆音），接着放后面的；后面也没有 → 欠载，重新攒到 target。
//!   - 来得太晚（已经放过去了）的包丢掉。
//!   - target 自适应：每包记“到达时刻 − 发送时刻”，减去最近一分钟的最小值 = 这包晚了多少（两台时钟差抵消掉）；
//!     取最近一分钟里的 q 分位（默认 99.5%）+ 余量，夹在 [min, max] 之间。网络变好了 target 慢慢降，变差立刻升。
//!   - 两台声卡时钟不一样快（±100ppm 级）：`ratio()` 按 实际深度 − target 给出播放速率微调（±0.5%），
//!     播放端用 IAudioClockAdjustment 交给系统重采样；攒得太多（超过 target 一倍且多 100ms）直接丢旧包追上。

pub const RATE: u32 = 48_000;
/// 每包帧数：5ms。
pub const FRAMES: usize = 240;
pub const CHANNELS: usize = 2;
pub const SAMPLES: usize = FRAMES * CHANNELS;
pub const HDR: usize = 10;
pub const CODEC_PCM16: u8 = 0;
pub const FLAG_START: u8 = 1;
/// 明文最大长度（目前只有 PCM）。
pub const MAX_PLAIN: usize = HDR + SAMPLES * 2;

/// 1ms 的帧数。
const MS: usize = RATE as usize / 1000;
/// 环形槽数：128 包 = 640ms。
const SLOTS: usize = 128;
/// 淡入淡出长度（帧）：2ms。
const FADE: usize = 2 * MS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub codec: u8,
    pub flags: u8,
    pub seq: u32,
    pub send_us: u32,
}

/// 编一包 PCM（pcm = 240 帧交错 f32）。
pub fn encode_pcm(seq: u32, flags: u8, send_us: u32, pcm: &[f32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HDR + pcm.len() * 2);
    v.push(CODEC_PCM16);
    v.push(flags);
    v.extend_from_slice(&seq.to_le_bytes());
    v.extend_from_slice(&send_us.to_le_bytes());
    for &s in pcm {
        let x = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v
}

/// 解包：头 + 数据。不认识的 codec / 长度不对 → None。
pub fn decode(b: &[u8]) -> Option<(Header, &[u8])> {
    if b.len() < HDR {
        return None;
    }
    let h = Header {
        codec: b[0],
        flags: b[1],
        seq: u32::from_le_bytes(b[2..6].try_into().ok()?),
        send_us: u32::from_le_bytes(b[6..10].try_into().ok()?),
    };
    let data = &b[HDR..];
    (h.codec == CODEC_PCM16 && data.len() == SAMPLES * 2).then_some((h, data))
}

/// 调参（config.toml [audio] 里可改，见 AudioCfg）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tune {
    /// 自适应 target 用的分位（0.99 = 允许 1% 的包来不及）。
    pub q: f64,
    pub min_ms: u32,
    pub max_ms: u32,
    /// 分位之外再留的余量。
    pub margin_ms: u32,
    /// 还没攒够统计时的 target。
    pub start_ms: u32,
    /// 迟到统计看最近多少秒（越长越稳：偶尔的大卡顿也记得住，target 降得慢）。
    pub window_s: u32,
}

impl Default for Tune {
    fn default() -> Self {
        Tune { q: 1.0, min_ms: 40, max_ms: 400, margin_ms: 10, start_ms: 120, window_s: 300 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub packets: u64,
    /// 来晚了（已经放过去了）丢掉的。
    pub late: u64,
    /// 缺了、用补偿（前一段样本来回放，淡出）补上的包。
    pub concealed: u64,
    /// 欠载（缓冲放空，重新攒）次数。
    pub underruns: u64,
    /// 攒太多丢掉的包。
    pub dropped: u64,
    /// 重新开始（发送端重启 / 跳得太远）次数。
    pub resets: u64,
}

// ---------- 迟到统计 ----------

/// 统计窗口至少分 6 桶，每桶最长 60 秒：每桶只和自己的最小值比，两台时钟的漂移（~20-100ppm）在一桶里不过几毫秒。
const BUCKETS: usize = 6;
const BUCKET_MAX_MS: u64 = 60_000;
/// 直方图：每格 2ms，256 格 = 512ms（更晚的都算最后一格）。
const BIN_US: i64 = 2_000;
const BINS: usize = 256;

#[derive(Clone)]
struct Bucket {
    start_ms: u64,
    /// 这一桶里 (到达 − 发送) 的最小值（相对 base，微秒）。
    min: i64,
    /// 相对本桶 min 的直方图。只在 min 变小时整体右移。
    hist: [u32; BINS],
    n: u32,
}

impl Bucket {
    fn new(start_ms: u64) -> Bucket {
        Bucket { start_ms, min: i64::MAX, hist: [0; BINS], n: 0 }
    }
    fn add(&mut self, v: i64) {
        if v < self.min {
            if self.n > 0 {
                // 新的更小：已有的值都右移
                let shift = ((self.min - v) / BIN_US) as usize;
                if shift > 0 {
                    let mut h = [0u32; BINS];
                    for (i, &c) in self.hist.iter().enumerate() {
                        h[(i + shift).min(BINS - 1)] += c;
                    }
                    self.hist = h;
                }
            }
            self.min = v;
        }
        let bin = (((v - self.min) / BIN_US) as usize).min(BINS - 1);
        self.hist[bin] += 1;
        self.n += 1;
    }
}

struct Late {
    /// 基准 (到达 − 发送)：之后都相对它（u32 回绕的差，转成有符号）。桶全过期后重新取（两台时钟漂移不会累积）。
    base: Option<u32>,
    buckets: Vec<Bucket>,
    window_ms: u64,
    nb: usize,
}

impl Late {
    fn new(window_s: u32) -> Late {
        let window_ms = window_s.clamp(6, 3600) as u64 * 1000;
        let nb = BUCKETS.max(window_ms.div_ceil(BUCKET_MAX_MS) as usize);
        Late { base: None, buckets: Vec::new(), window_ms, nb }
    }
    fn bucket_ms(&self) -> u64 {
        self.window_ms / self.nb as u64
    }
    /// 丢掉窗口外的桶（静音时不来包，桶不会自己轮换）。
    fn expire(&mut self, now_ms: u64) {
        let w = self.window_ms;
        self.buckets.retain(|b| now_ms.saturating_sub(b.start_ms) < w);
        if self.buckets.is_empty() {
            self.base = None;
        }
    }
    fn add(&mut self, arrive_us: u32, send_us: u32, now_ms: u64) {
        self.expire(now_ms);
        let d = arrive_us.wrapping_sub(send_us);
        let base = *self.base.get_or_insert(d);
        let v = d.wrapping_sub(base) as i32 as i64;
        let bm = self.bucket_ms();
        if self.buckets.last().is_none_or(|b| now_ms.saturating_sub(b.start_ms) >= bm) {
            self.buckets.push(Bucket::new(now_ms));
            if self.buckets.len() > self.nb {
                self.buckets.remove(0);
            }
        }
        if let Some(b) = self.buckets.last_mut() {
            b.add(v);
        }
    }
    /// 窗口里“比最快的那包晚多少”的 q 分位（毫秒）。样本太少 → None。
    fn quantile_ms(&self, q: f64, now_ms: u64) -> Option<u32> {
        let w = self.window_ms;
        let live = || self.buckets.iter().filter(|b| b.n > 0 && now_ms.saturating_sub(b.start_ms) < w);
        let n: u64 = live().map(|b| b.n as u64).sum();
        if n < 400 {
            return None; // 不到 2 秒
        }
        // 每桶相对自己的最小值直接合并（不对齐到全窗口最小值：那样会把整个窗口的时钟漂移算成迟到）
        let mut h = [0u64; BINS];
        for b in live() {
            for (i, &c) in b.hist.iter().enumerate() {
                h[i] += c as u64;
            }
        }
        let want = ((n as f64) * q).ceil() as u64;
        let mut acc = 0;
        for (i, &c) in h.iter().enumerate() {
            acc += c;
            if acc >= want {
                return Some(((i as i64 + 1) * BIN_US / 1000) as u32);
            }
        }
        Some((BINS as i64 * BIN_US / 1000) as u32)
    }
}

// ---------- 缓冲 ----------

/// target 上限：环形槽装得下（留 8 包余量）。
const TARGET_CAP: usize = (SLOTS - 8) * FRAMES;
/// 攒太多的丢包线上限：也要在环里。
const DROP_CAP: usize = (SLOTS - 4) * FRAMES;
/// 攒的时候这么久没来新包（且不超过 target）：这一段已经发完了（短提示音），有多少先放多少。
const END_GAP_MS: u64 = 100;
/// 刚开始放的这段时间统计还不全：target 不低于 start_ms（上次学到的，见 audio.rs）。
const WARM_MS: u64 = 60_000;
/// 包络每帧变化量：FADE 帧从 0 到 1。
const STEP: f32 = 1.0 / FADE as f32;
/// 丢包补偿：最近 5ms 的真实样本来回放（镜像，接缝处连续），满幅保持 HOLD 帧，再 DECAY 帧淡到 0。
const HIST: usize = FRAMES;
const PLC_HOLD: usize = FRAMES;
const PLC_DECAY: usize = 2 * FRAMES;

pub struct Jitter {
    pcm: Vec<i16>,
    seqs: Vec<u32>,
    valid: Vec<bool>,
    /// 这个槽的包是一段声音的开头（FLAG_START）：追赶时不丢。
    starts: Vec<bool>,
    /// 下一个要放的包、在它里面放到第几帧。
    next: u32,
    pos: usize,
    /// 收到过的最新 seq。
    newest: u32,
    any: bool,
    /// 正在攒（还没开始放 / 欠载后）。
    buffering: bool,
    /// 真实样本的增益包络（0～1）：淡入、丢包前淡出都按 STEP 走，跨 pull 连续。
    env: f32,
    /// 最近放过的 HIST 帧真实样本（环形，hist_w = 下一个写的位置）和补偿的读位置。
    hist: Vec<f32>,
    hist_w: usize,
    hist_n: usize,
    /// 补偿开始时 hist 的快照（按时间顺序，最后一帧最新）：补偿和随后的交叉淡化都从它读，
    /// 接回真实样本时 hist 还在写，不能直接读 hist。
    plc_buf: Vec<f32>,
    /// 补偿：已补了多少帧（0 = 没在补）、从最新样本往回的偏移、方向、开始时的增益、当前增益。
    plc_n: usize,
    plc_off: usize,
    plc_back: bool,
    plc_g0: f32,
    plc_g: f32,
    /// 补偿后接回真实样本：还要交叉淡化的帧数。
    xf: usize,
    /// 攒太多、要丢旧包：先淡出到 0 再跳。
    skip: bool,
    /// 上一次欠载后还没来新包（新包带 START = 那是发送端静音，不算欠载）。
    gap_open: bool,
    /// 这次攒是因为网络真卡了（不是发送端静音后的新一段）：攒够后可以丢掉涌来的旧包追到 target。
    stalled: bool,
    /// 放着的时候来了新一段的开头（发送端的静音比缓冲短，没放空）：放到它时缓冲不够 target 就先补回静音再接着放。
    resume_at: Option<u32>,
    /// 迟到统计出过结果：之后统计过期（长时间静音）时沿用学到的 target，不退回 start_ms。
    learned: bool,
    /// 第一包到的时刻：之后 WARM_MS 内统计还不全（没见过几次卡顿），target 不低于 start_ms。
    born_ms: Option<u64>,
    last_push_ms: u64,
    /// 平滑后的深度（帧），给速率微调用。
    depth_ema: f64,
    late: Late,
    tune: Tune,
    target: usize,
    pub stats: Stats,
}

impl Jitter {
    pub fn new(tune: Tune) -> Jitter {
        Jitter {
            pcm: vec![0; SLOTS * SAMPLES],
            seqs: vec![0; SLOTS],
            valid: vec![false; SLOTS],
            starts: vec![false; SLOTS],
            next: 0,
            pos: 0,
            newest: 0,
            any: false,
            buffering: true,
            env: 0.0,
            hist: vec![0.0; HIST * CHANNELS],
            hist_w: 0,
            hist_n: 0,
            plc_buf: Vec::with_capacity(HIST * CHANNELS),
            plc_n: 0,
            plc_off: 0,
            plc_back: true,
            plc_g0: 0.0,
            plc_g: 0.0,
            xf: 0,
            skip: false,
            gap_open: false,
            stalled: false,
            resume_at: None,
            learned: false,
            born_ms: None,
            last_push_ms: 0,
            depth_ema: 0.0,
            late: Late::new(tune.window_s),
            tune,
            target: (tune.start_ms.clamp(tune.min_ms, tune.max_ms.max(tune.min_ms)) as usize * MS).min(TARGET_CAP),
            stats: Stats::default(),
        }
    }

    pub fn set_tune(&mut self, t: Tune) {
        if t.window_s != self.tune.window_s {
            self.late = Late::new(t.window_s);
        }
        self.tune = t;
    }

    fn reset(&mut self, seq: u32) {
        self.valid.iter_mut().for_each(|v| *v = false);
        self.next = seq;
        self.pos = 0;
        self.newest = seq;
        self.buffering = true;
        self.skip = false;
        self.stalled = false;
        self.resume_at = None;
        self.stats.resets += 1;
    }

    /// 收到一包。data = 960 字节 PCM。now_ms = 本机单调毫秒时钟（统计分桶、判断一段结束用）。
    pub fn push(&mut self, h: Header, data: &[u8], arrive_us: u32, now_ms: u64) {
        self.stats.packets += 1;
        self.born_ms.get_or_insert(now_ms);
        self.late.add(arrive_us, h.send_us, now_ms);
        self.last_push_ms = now_ms;
        let start = h.flags & FLAG_START != 0;
        let was_gap = self.gap_open;
        if was_gap && start {
            // 上次“放空”其实是发送端静音停发了：不算欠载，攒够后也不追赶（多出来的是新声音的开头）
            self.stats.underruns = self.stats.underruns.saturating_sub(1);
            self.stalled = false;
        }
        self.gap_open = false;
        let seq = h.seq;
        if !self.any {
            self.any = true;
            self.next = seq;
            self.newest = seq;
            self.buffering = true;
        }
        let d = seq.wrapping_sub(self.next) as i32;
        if d < -(SLOTS as i32) || d as i64 >= SLOTS as i64 {
            // 发送端重启（seq 从头来）/ 跳得太远（长时间断流）：旧的不要了
            self.reset(seq);
        } else if d < 0 {
            if self.buffering && start {
                // 攒的时候新一段的开头晚到了：从它开始（已经存好的更新的包留着）
                self.next = seq;
                self.pos = 0;
            } else {
                self.stats.late += 1;
                return;
            }
        }
        let i = seq as usize % SLOTS;
        if self.valid[i] && self.seqs[i] == seq {
            return; // 重复
        }
        if start && !self.buffering && !was_gap && seq.wrapping_sub(self.next) as i32 > 0 {
            self.resume_at = Some(seq);
        }
        self.seqs[i] = seq;
        self.valid[i] = true;
        self.starts[i] = start;
        let dst = &mut self.pcm[i * SAMPLES..(i + 1) * SAMPLES];
        for (o, c) in dst.iter_mut().zip(data.chunks_exact(2)) {
            *o = i16::from_le_bytes([c[0], c[1]]);
        }
        if seq.wrapping_sub(self.newest) as i32 > 0 {
            self.newest = seq;
        }
    }

    /// 从播放位置到最新收到的包末尾还有多少帧（中间缺的也算）。
    pub fn depth(&self) -> usize {
        if !self.any {
            return 0;
        }
        let d = self.newest.wrapping_sub(self.next) as i32;
        if d < 0 { 0 } else { (d as usize + 1) * FRAMES - self.pos }
    }

    pub fn target_ms(&self) -> u32 {
        (self.target / MS) as u32
    }

    #[cfg(test)]
    pub fn buffering(&self) -> bool {
        self.buffering
    }

    fn update_target(&mut self, now_ms: u64) {
        let t = &self.tune;
        let hi = t.max_ms.max(t.min_ms);
        let ms = match self.late.quantile_ms(t.q, now_ms) {
            Some(q) => {
                self.learned = true;
                let warm = self.born_ms.is_some_and(|b| now_ms.saturating_sub(b) < WARM_MS);
                let floor = if warm { t.start_ms.clamp(t.min_ms, hi) } else { t.min_ms };
                (q + t.margin_ms).clamp(floor, hi)
            }
            // 统计过期了（长时间静音）：网络多半没变好，沿用学到的
            None if self.learned => (self.target / MS) as u32,
            None => t.start_ms.clamp(t.min_ms, hi),
        };
        self.target = (ms as usize * MS).min(TARGET_CAP);
    }

    /// 播放速率微调：>1 放快一点（攒多了），<1 放慢一点。
    pub fn ratio(&self) -> f64 {
        if self.buffering {
            return 1.0;
        }
        let err_ms = (self.depth_ema - self.target as f64) / MS as f64;
        1.0 + (err_ms * 1e-4).clamp(-0.005, 0.005)
    }

    fn slot_ok(&self, seq: u32) -> bool {
        let i = seq as usize % SLOTS;
        self.valid[i] && self.seqs[i] == seq
    }

    /// 填满 out（交错立体声 f32）。now_ms 同 push。
    pub fn pull(&mut self, out: &mut [f32], now_ms: u64) {
        self.update_target(now_ms);
        // 攒得太多：标记丢旧包追上（先淡出）
        if !self.buffering && !self.skip && self.depth() > (self.target * 2 + 100 * MS).min(DROP_CAP) {
            self.skip = true;
        }
        for fr in out.chunks_exact_mut(CHANNELS) {
            self.frame(fr, now_ms);
        }
        let d = self.depth() as f64;
        self.depth_ema += (d - self.depth_ema) * 0.02;
    }

    /// 补偿的下一帧样本（镜像来回读最近 HIST 帧，读位置的移动是连续的，所以不会有跳变）。
    fn plc_next(&mut self, fr: &mut [f32]) {
        let n = self.plc_buf.len() / CHANNELS;
        if n == 0 {
            fr.iter_mut().for_each(|v| *v = 0.0);
            return;
        }
        let i = n - 1 - self.plc_off.min(n - 1);
        fr[..CHANNELS].copy_from_slice(&self.plc_buf[i * CHANNELS..(i + 1) * CHANNELS]);
        if n > 1 {
            if self.plc_back {
                self.plc_off += 1;
                if self.plc_off >= n - 1 {
                    self.plc_back = false;
                }
            } else {
                self.plc_off -= 1;
                if self.plc_off == 0 {
                    self.plc_back = true;
                }
            }
        }
    }

    /// 没有真实样本（缺包 / 放空 / 攒着）：先补偿 HOLD 帧，再淡到 0。
    fn silent(&mut self, fr: &mut [f32]) {
        if self.plc_n == 0 {
            if self.xf > 0 && self.hist_n < FADE / 2 {
                // 上一次补偿刚接回真实样本没多久（交叉淡化前一半，输出主要还是旧快照）：接着用旧快照，增益不抬高
                self.plc_g0 = self.plc_g;
            } else {
                // 快照最近 hist_n 帧（按时间顺序）
                self.plc_buf.clear();
                let n = self.hist_n;
                for k in 0..n {
                    let i = (self.hist_w + HIST - n + k) % HIST;
                    self.plc_buf.extend_from_slice(&self.hist[i * CHANNELS..(i + 1) * CHANNELS]);
                }
                self.plc_off = 0;
                self.plc_back = true;
                self.plc_g0 = self.env;
            }
        }
        let k = self.plc_n.saturating_sub(PLC_HOLD);
        self.plc_g = if k >= PLC_DECAY { 0.0 } else { self.plc_g0 * (1.0 - k as f32 / PLC_DECAY as f32) };
        self.plc_n += 1;
        self.xf = 0;
        if self.plc_g <= 0.0 {
            // 补完了：之后接回真实样本要从 0 淡入
            self.env = 0.0;
            fr.iter_mut().for_each(|v| *v = 0.0);
            return;
        }
        self.plc_next(fr);
        let g = self.plc_g;
        fr.iter_mut().for_each(|v| *v *= g);
    }

    /// 放一帧真实样本 v。
    fn real(&mut self, fr: &mut [f32], v: [f32; CHANNELS]) {
        if self.plc_n > 0 {
            // 刚补偿完：和补偿信号交叉淡化；hist 从头攒，之后的快照只含缺口之后连续的样本
            self.xf = if self.plc_g > 0.0 { FADE } else { 0 };
            self.plc_n = 0;
            self.hist_n = 0;
        }
        self.env = if self.skip { (self.env - STEP).max(0.0) } else { (self.env + STEP).min(1.0) };
        if self.xf > 0 {
            let k = 1.0 - self.xf as f32 / FADE as f32;
            let mut p = [0f32; CHANNELS];
            self.plc_next(&mut p);
            let g = self.plc_g;
            for c in 0..CHANNELS {
                fr[c] = v[c] * self.env * k + p[c] * g * (1.0 - k);
            }
            self.xf -= 1;
        } else {
            for c in 0..CHANNELS {
                fr[c] = v[c] * self.env;
            }
        }
        let w = self.hist_w;
        self.hist[w * CHANNELS..(w + 1) * CHANNELS].copy_from_slice(&v);
        self.hist_w = (w + 1) % HIST;
        self.hist_n = (self.hist_n + 1).min(HIST);
    }

    fn frame(&mut self, fr: &mut [f32], now_ms: u64) {
        if self.buffering {
            let ended = now_ms.saturating_sub(self.last_push_ms) >= END_GAP_MS.min(self.target_ms() as u64);
            if self.any && self.depth() > 0 && (self.depth() >= self.target.max(FRAMES) || ended) {
                // 网络卡顿后一下子涌来的包比 target 多出一截：多出来的是已经断过音的旧声音，丢掉直接追到 target
                // （不然要几十秒才消化掉）。多一两包不追（pull 的粒度就是一两包，交给 ratio）；新一段的开头不丢。
                if self.stalled {
                    let slack = (4 * FRAMES).max(self.target / 2);
                    while self.depth() > self.target + slack {
                        let i = self.next as usize % SLOTS;
                        let here = self.valid[i] && self.seqs[i] == self.next;
                        if here && self.starts[i] {
                            break;
                        }
                        if here {
                            self.stats.dropped += 1;
                        }
                        self.valid[i] = false;
                        self.next = self.next.wrapping_add(1);
                        self.pos = 0;
                    }
                }
                self.stalled = false;
                // 从第一个到了的包开始放
                while !self.slot_ok(self.next) && self.newest.wrapping_sub(self.next) as i32 > 0 {
                    self.next = self.next.wrapping_add(1);
                }
                self.pos = 0;
                self.buffering = false;
                self.depth_ema = self.depth() as f64;
            } else {
                return self.silent(fr);
            }
        }
        if self.skip && self.env <= 0.0 {
            while self.depth() > self.target + FRAMES {
                let i = self.next as usize % SLOTS;
                self.valid[i] = false;
                self.next = self.next.wrapping_add(1);
                self.pos = 0;
                self.stats.dropped += 1;
            }
            self.skip = false;
        }
        if self.pos == 0 && self.resume_at == Some(self.next) {
            self.resume_at = None;
            if self.depth() < self.target {
                // 发送端的静音被缓冲吃掉了一截：先在这里攒回 target（放的是静音），不然后面一卡就断
                self.buffering = true;
                return self.silent(fr);
            }
        }
        if self.slot_ok(self.next) {
            let i = self.next as usize % SLOTS;
            let at = i * SAMPLES + self.pos * CHANNELS;
            let mut v = [0f32; CHANNELS];
            for c in 0..CHANNELS {
                v[c] = self.pcm[at + c] as f32 / 32768.0;
            }
            self.real(fr, v);
            self.pos += 1;
            if self.pos == FRAMES {
                self.valid[i] = false;
                self.next = self.next.wrapping_add(1);
                self.pos = 0;
            }
        } else if self.newest.wrapping_sub(self.next) as i32 > 0 {
            // 缺这一包，后面有：补偿
            self.silent(fr);
            self.pos += 1;
            if self.pos == FRAMES {
                self.next = self.next.wrapping_add(1);
                self.pos = 0;
                self.stats.concealed += 1;
            }
        } else {
            // 放空了
            self.stats.underruns += 1;
            self.gap_open = true;
            self.stalled = true;
            self.buffering = true;
            self.skip = false;
            self.pos = 0;
            self.next = self.newest.wrapping_add(1);
            self.silent(fr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: u32, v: f32) -> Vec<u8> {
        encode_pcm(seq, 0, seq.wrapping_mul(5000), &[v; SAMPLES])
    }

    fn push(j: &mut Jitter, seq: u32, v: f32, arrive_ms: u64) {
        let p = pkt(seq, v);
        let (h, d) = decode(&p).unwrap();
        j.push(h, d, (arrive_ms * 1000) as u32, arrive_ms);
    }

    fn tune(start: u32) -> Tune {
        Tune { start_ms: start, min_ms: start.min(40), ..Default::default() }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let p = encode_pcm(7, FLAG_START, 12345, &[0.5; SAMPLES]);
        assert_eq!(p.len(), MAX_PLAIN);
        let (h, d) = decode(&p).unwrap();
        assert_eq!(h, Header { codec: 0, flags: FLAG_START, seq: 7, send_us: 12345 });
        assert_eq!(i16::from_le_bytes([d[0], d[1]]), 16384);
        assert!(decode(&p[..p.len() - 1]).is_none());
        let mut bad = p.clone();
        bad[0] = 9;
        assert!(decode(&bad).is_none());
    }

    #[test]
    fn buffers_to_target_then_plays_in_order() {
        let mut j = Jitter::new(tune(20)); // 20ms = 4 包
        for s in 0..3 {
            push(&mut j, s, 0.25, s as u64 * 5);
        }
        let mut out = vec![1.0; SAMPLES];
        j.pull(&mut out, 10);
        assert!(out.iter().all(|&v| v == 0.0), "没攒够不出声");
        push(&mut j, 3, 0.25, 15);
        j.pull(&mut out, 15);
        assert!(!j.buffering());
        assert!((out[SAMPLES - 1] - 0.25).abs() < 1e-3);
        assert!(out[0].abs() < 0.01, "开头淡入");
        assert_eq!(j.depth(), 3 * FRAMES);
    }

    #[test]
    fn missing_packet_is_concealed_and_reordered_one_used() {
        let mut j = Jitter::new(tune(10));
        push(&mut j, 0, 0.5, 0);
        push(&mut j, 2, 0.5, 10); // 1 还没到
        let mut out = vec![0.0; SAMPLES];
        j.pull(&mut out, 10); // 放 0
        push(&mut j, 1, 0.5, 11); // 乱序到达，赶上了
        j.pull(&mut out, 11);
        assert!((out[SAMPLES - 1] - 0.5).abs() < 1e-3);
        assert_eq!(j.stats.concealed, 0);
        push(&mut j, 4, 0.5, 20); // 3 丢了
        j.pull(&mut out, 20); // 2
        j.pull(&mut out, 20); // 3 → 补偿：拿前面的样本补上（常量信号补出来还是常量），不是静音
        assert_eq!(j.stats.concealed, 1);
        assert!((out[SAMPLES - 1] - 0.5).abs() < 1e-3);
        j.pull(&mut out, 20); // 4
        assert!((out[SAMPLES - 1] - 0.5).abs() < 1e-3);
        push(&mut j, 3, 0.5, 30); // 来晚了
        assert_eq!(j.stats.late, 1);
    }

    #[test]
    fn underrun_rebuffers() {
        let mut j = Jitter::new(tune(10));
        push(&mut j, 0, 0.5, 0);
        push(&mut j, 1, 0.5, 5);
        // 2 包 + 补偿 3 包（满幅 1 包 + 淡出 2 包）+ 1 包静音
        let mut out = vec![0.0; SAMPLES * 6];
        j.pull(&mut out, 5);
        assert_eq!(j.stats.underruns, 1);
        assert!((out[SAMPLES * 3 - 1] - 0.5).abs() < 1e-3, "补偿先满幅");
        assert!(out[SAMPLES * 4 - 1] > 0.0 && out[SAMPLES * 4 - 1] < 0.5, "再淡出");
        assert_eq!(out[SAMPLES * 6 - 1], 0.0);
        assert!(j.buffering());
        // 新一段（seq 接着）：攒够 2 包才放
        push(&mut j, 2, 0.5, 100);
        let mut o2 = vec![1.0; SAMPLES];
        j.pull(&mut o2, 100);
        assert!(o2.iter().all(|&v| v == 0.0));
        push(&mut j, 3, 0.5, 105);
        j.pull(&mut o2, 105);
        assert!((o2[SAMPLES - 1] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn sender_restart_resets() {
        let mut j = Jitter::new(tune(5));
        for s in 1000..1004 {
            push(&mut j, s, 0.5, 0);
        }
        let mut out = vec![0.0; SAMPLES * 4];
        j.pull(&mut out, 0);
        push(&mut j, 0, 0.5, 10); // seq 从 0 重来
        assert_eq!(j.stats.resets, 1);
        let mut o = vec![0.0; SAMPLES];
        j.pull(&mut o, 10);
        assert!((o[SAMPLES - 1] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn overfull_drops_to_target() {
        let mut j = Jitter::new(tune(10));
        push(&mut j, 0, 0.5, 0);
        push(&mut j, 1, 0.5, 0);
        let mut out = vec![0.0; SAMPLES];
        j.pull(&mut out, 0);
        for s in 2..80 {
            push(&mut j, s, 0.5, 400); // 一下子涌来 390ms
        }
        j.pull(&mut out, 400);
        assert!(j.stats.dropped > 50);
        assert!(j.depth() <= 10 * MS + 2 * FRAMES);
    }

    #[test]
    fn target_follows_lateness_quantile() {
        let mut j = Jitter::new(Tune { q: 0.99, margin_ms: 10, start_ms: 40, ..Default::default() });
        assert_eq!(j.target_ms(), 40);
        // 3 秒：每 100 包有 2 包晚 80ms，其余准时
        let mut out = vec![0.0; SAMPLES];
        for s in 0..600u32 {
            let late = if s % 100 < 2 { 80 } else { 0 };
            let p = pkt(s, 0.1);
            let (h, d) = decode(&p).unwrap();
            let arrive = s * 5000 + 3000 + late * 1000;
            j.push(h, d, arrive, s as u64 * 5);
            j.pull(&mut out, s as u64 * 5);
        }
        // 2% 晚 80ms，99% 分位落在 80ms 那格 → 82 + 10
        assert_eq!(j.target_ms(), 92);
        // 99.5% 也一样；90% 分位就只剩余量
        j.set_tune(Tune { q: 0.9, margin_ms: 10, start_ms: 40, ..Default::default() });
        let p = pkt(600, 0.1);
        let (h, d) = decode(&p).unwrap();
        j.push(h, d, 600 * 5000 + 3000, 3000);
        j.pull(&mut out, 3000);
        assert_eq!(j.target_ms(), 40); // 2 + 10 被夹到 min 40
    }

    #[test]
    fn lateness_survives_clock_wrap() {
        let mut l = Late::new(60);
        // 发送端时钟快回绕，到达端时钟差一大截
        for i in 0..1000u32 {
            let send = (u32::MAX - 2_000_000).wrapping_add(i * 5000);
            let arrive = send.wrapping_add(7_000_000).wrapping_add(if i % 10 == 0 { 30_000 } else { 0 });
            l.add(arrive, send, i as u64 * 5);
        }
        assert_eq!(l.quantile_ms(0.5, 5000), Some(2));
        assert_eq!(l.quantile_ms(0.95, 5000), Some(32));
    }

    #[test]
    fn ratio_tracks_depth_error() {
        let mut out = vec![0.0; SAMPLES];
        let mut j = Jitter::new(tune(100));
        for s in 0..21 {
            push(&mut j, s, 0.1, 0);
        }
        j.pull(&mut out, 0);
        for s in 21..50 {
            push(&mut j, s, 0.1, 1); // 放的时候又涌来一批：约 245ms，没到丢包线（2×100+100）
        }
        for _ in 0..10 {
            j.pull(&mut out, 1);
        }
        assert_eq!(j.stats.dropped, 0);
        assert!(j.ratio() > 1.001, "攒多了放快：{}", j.ratio());
        let mut j2 = Jitter::new(tune(100));
        for s in 0..21 {
            push(&mut j2, s, 0.1, 0);
        }
        for _ in 0..18 {
            j2.pull(&mut out, 0);
        }
        assert!(j2.ratio() < 1.0, "不够放慢：{}", j2.ratio());
    }

    #[test]
    fn short_burst_plays_without_reaching_target() {
        let mut j = Jitter::new(tune(300));
        for s in 0..10 {
            push(&mut j, s, 0.5, s as u64 * 5); // 50ms 的提示音，远不到 300ms
        }
        let mut out = vec![0.0; SAMPLES];
        j.pull(&mut out, 50);
        assert!(out.iter().all(|&v| v == 0.0), "刚停发还在等");
        j.pull(&mut out, 45 + END_GAP_MS);
        assert!(!j.buffering(), "停发 100ms 后有多少放多少");
        assert!((out[SAMPLES - 1] - 0.5).abs() < 1e-3);
        for _ in 0..12 {
            j.pull(&mut out, 300);
        }
        assert_eq!(j.depth(), 0, "放完了，输出流能关");
    }

    #[test]
    fn envelope_is_continuous_across_pull_boundaries() {
        // 每次只取 7 帧，中间缺包、包晚到一半、欠载、攒太多丢包：相邻两帧的跳变都不超过一个包络步长
        let mut j = Jitter::new(Tune { q: 0.5, min_ms: 40, start_ms: 40, ..Default::default() });
        let mut prev = 0f32;
        let mut max_jump = 0f32;
        let mut now = 0u64;
        let mut seq = 0u32;
        let step = |j: &mut Jitter, now: u64, n: usize, prev: &mut f32, max_jump: &mut f32| {
            for _ in 0..n {
                let mut o = [0f32; 14];
                j.pull(&mut o, now);
                for f in o.chunks_exact(2) {
                    *max_jump = max_jump.max((f[0] - *prev).abs());
                    *prev = f[0];
                }
            }
        };
        for round in 0..40u32 {
            for _ in 0..6 {
                // 第 7 轮缺一包、第 13 轮晚到、第 20 轮一下子来 60 包
                let skip_one = round % 7 == 3 && seq % 6 == 2;
                if !skip_one {
                    push(&mut j, seq, 0.8, seq as u64 * 5);
                }
                seq += 1;
            }
            if round == 20 {
                for _ in 0..60 {
                    push(&mut j, seq, 0.8, seq as u64 * 5);
                    seq += 1;
                }
            }
            // 6 包 = 1440 帧，每次取 7 帧 → 约 206 次
            step(&mut j, now, 206, &mut prev, &mut max_jump);
            now += 30;
            if round % 9 == 8 {
                step(&mut j, now, 300, &mut prev, &mut max_jump); // 放空
            }
        }
        assert!(j.stats.concealed > 0 && j.stats.underruns > 0 && j.stats.dropped > 0, "{:?}", j.stats);
        // 淡入和交叉淡化叠在一起时一帧最多两个步长（仍是 2ms 的平滑过渡，不是阶跃）
        assert!(max_jump <= 2.0 * 0.8 * STEP + 1e-4, "跳变 {max_jump}");
    }

    #[test]
    fn lateness_window_expires_after_silence() {
        let mut l = Late::new(60);
        for i in 0..1000u32 {
            let late = if i % 10 == 0 { 200_000 } else { 0 };
            l.add(i * 5000 + late, i * 5000, i as u64 * 5);
        }
        assert!(l.quantile_ms(0.95, 5000).unwrap() > 150);
        // 静音两分钟后：旧桶全过期，基准重取（时钟漂移不累积）
        assert_eq!(l.quantile_ms(0.95, 125_000), None);
        for i in 0..1000u32 {
            l.add(7_000_000 + i * 5000, i * 5000, 125_000 + i as u64 * 5);
        }
        assert_eq!(l.quantile_ms(0.95, 130_000), Some(2));
    }

    #[test]
    fn target_capped_by_ring() {
        let mut j = Jitter::new(Tune { start_ms: 900, ..Default::default() });
        assert!(j.target <= TARGET_CAP);
        for s in 0..(SLOTS as u32 - 8) {
            push(&mut j, s, 0.1, 0);
        }
        let mut out = vec![0.0; SAMPLES];
        j.pull(&mut out, 0);
        assert!(!j.buffering(), "能攒够");
    }

    #[test]
    fn late_start_packet_while_buffering_keeps_newer() {
        let mut j = Jitter::new(tune(10));
        push(&mut j, 11, 0.5, 0); // 乱序：第二包先到
        let p = encode_pcm(10, FLAG_START, 50_000, &[0.5; SAMPLES]);
        let (h, d) = decode(&p).unwrap();
        j.push(h, d, 1000, 1);
        assert_eq!(j.stats.resets, 0);
        let mut out = vec![0.0; SAMPLES * 2];
        j.pull(&mut out, 1);
        assert_eq!(j.stats.concealed, 0);
        assert!((out[SAMPLES * 2 - 1] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn sender_silence_is_not_an_underrun() {
        let mut j = Jitter::new(tune(5));
        push(&mut j, 0, 0.5, 0);
        let mut out = vec![0.0; SAMPLES * 2];
        j.pull(&mut out, 0);
        assert_eq!(j.stats.underruns, 1);
        let p = encode_pcm(1, FLAG_START, 5000, &[0.5; SAMPLES]);
        let (h, d) = decode(&p).unwrap();
        j.push(h, d, 900_000, 900);
        assert_eq!(j.stats.underruns, 0);
    }

    #[test]
    fn concealment_of_sine_has_no_jumps() {
        // 1 kHz 正弦（每帧自然变化最多 2π·1000/48000·0.5 ≈ 0.065），丢第 5、第 9～10 包
        let sine = |seq: u32| -> Vec<u8> {
            let pcm: Vec<f32> = (0..FRAMES)
                .flat_map(|f| {
                    let t = (seq as usize * FRAMES + f) as f32 / 48_000.0;
                    let v = 0.5 * (std::f32::consts::TAU * 1000.0 * t).sin();
                    [v, v]
                })
                .collect();
            encode_pcm(seq, 0, seq * 5000, &pcm)
        };
        let mut j = Jitter::new(tune(100));
        for s in 0..16u32 {
            if s == 5 || s == 9 || s == 10 {
                continue;
            }
            let p = sine(s);
            let (h, d) = decode(&p).unwrap();
            j.push(h, d, s * 5000, 0);
        }
        let mut out = vec![0f32; FRAMES * 2 * 14];
        j.pull(&mut out, 200); // 没凑够 target，停发 100ms 后开始放
        assert_eq!(j.stats.concealed, 3);
        let max_jump = out.chunks_exact(2).zip(out.chunks_exact(2).skip(1)).map(|(a, b)| (b[0] - a[0]).abs()).fold(0f32, f32::max);
        assert!(max_jump < 0.1, "接缝跳变 {max_jump}");
        // 补的那几包不是静音
        let rms = |k: usize| (out[k * SAMPLES..(k + 1) * SAMPLES].iter().map(|v| v * v).sum::<f32>() / SAMPLES as f32).sqrt();
        assert!(rms(5) > 0.3, "补偿有声：{}", rms(5));
    }

    #[test]
    fn catch_up_after_underrun() {
        let mut j = Jitter::new(tune(20));
        for s in 0..4 {
            push(&mut j, s, 0.5, 0);
        }
        let mut out = vec![0.0; SAMPLES * 10];
        j.pull(&mut out, 0); // 放完 4 包 + 欠载
        assert_eq!(j.stats.underruns, 1);
        for s in 4..64 {
            push(&mut j, s, 0.5, 400); // 卡了 300ms，一下子到了 60 包
        }
        let mut o = vec![0.0; SAMPLES];
        j.pull(&mut o, 400);
        assert!(j.depth() <= 20 * MS + 4 * FRAMES, "直接追到 target（多一两包交给 ratio）：{}", j.depth());
        assert!(j.stats.dropped > 50);
    }

    #[test]
    fn new_segment_onset_is_not_dropped() {
        // 发送端静音后新一段：攒的时候一次 pull 之间多来了几包，也不丢开头
        let mut j = Jitter::new(tune(42));
        push(&mut j, 0, 0.5, 0);
        let mut out = vec![0.0; SAMPLES * 4];
        j.pull(&mut out, 200); // 放完 + 放空
        assert_eq!(j.stats.underruns, 1);
        for s in 1..12u32 {
            let p = encode_pcm(s, if s == 1 { FLAG_START } else { 0 }, s * 5000, &[0.25; SAMPLES]);
            let (h, d) = decode(&p).unwrap();
            j.push(h, d, 1_000_000 + s * 5000, 1000);
        }
        assert_eq!(j.stats.underruns, 0, "发送端静音不算欠载");
        let mut o = vec![0.0; SAMPLES];
        j.pull(&mut o, 1000);
        assert_eq!(j.stats.dropped, 0);
        assert_eq!(j.depth(), 11 * FRAMES - FRAMES, "从新一段的第一包开始放");
    }

    #[test]
    fn short_pause_eaten_by_buffer_is_restored() {
        let mut j = Jitter::new(tune(100)); // 100ms = 20 包
        for s in 0..30u32 {
            push(&mut j, s, 0.5, 0);
        }
        let mut out = vec![0.0; SAMPLES];
        j.pull(&mut out, 0); // 开始放（追赶不发生：不是卡顿）
        // 发送端停了一会儿，新一段的开头接着来，此时缓冲里还有尾巴
        let p = encode_pcm(30, FLAG_START, 30 * 5000, &[0.5; SAMPLES]);
        let (h, d) = decode(&p).unwrap();
        j.push(h, d, 300_000, 300);
        for s in 31..34u32 {
            push(&mut j, s, 0.5, 300);
        }
        // 放到新一段时缓冲只有 4 包 < target：先攒回去，不算欠载
        for _ in 0..30 {
            j.pull(&mut out, 300);
        }
        assert!(j.buffering(), "放到新一段开头时重新攒");
        assert_eq!(j.stats.underruns, 0);
        for s in 34..60u32 {
            push(&mut j, s, 0.5, 400);
        }
        j.pull(&mut out, 400);
        assert!(!j.buffering());
        assert_eq!(j.stats.dropped, 0);
    }

    #[test]
    fn drift_is_not_lateness() {
        // 100ppm 漂移、5 分钟窗口、网络完全平稳：target 不该被漂移抬高
        let mut l = Late::new(300);
        for i in 0..60_000u32 {
            let send = i * 5000;
            let arrive = send + 3000 + (i as u64 * 5000 / 10_000) as u32; // 每秒多 100µs
            l.add(arrive, send, i as u64 * 5);
        }
        assert!(l.quantile_ms(1.0, 300_000).unwrap() <= 8, "{:?}", l.quantile_ms(1.0, 300_000));
    }

    #[test]
    fn learned_target_survives_stats_expiry() {
        let mut j = Jitter::new(Tune { window_s: 10, ..Default::default() });
        let mut out = vec![0.0; SAMPLES];
        for s in 0..1000u32 {
            let late = if s % 100 == 0 { 200 } else { 0 };
            let p = pkt(s, 0.1);
            let (h, d) = decode(&p).unwrap();
            j.push(h, d, s * 5000 + late * 1000, s as u64 * 5);
            j.pull(&mut out, s as u64 * 5);
        }
        let t = j.target_ms();
        assert!(t > 200, "{t}");
        j.pull(&mut out, 60_000); // 一分钟后统计全过期
        assert_eq!(j.target_ms(), t);
    }

    #[test]
    fn warm_up_keeps_start_target() {
        let mut j = Jitter::new(Tune { start_ms: 280, ..Default::default() });
        let mut out = vec![0.0; SAMPLES];
        for s in 0..2000u32 {
            push(&mut j, s, 0.1, s as u64 * 5); // 网络平稳
            j.pull(&mut out, s as u64 * 5);
        }
        assert_eq!(j.target_ms(), 280, "头一分钟不低于上次学到的");
        for s in 2000..14_000u32 {
            push(&mut j, s, 0.1, s as u64 * 5);
            j.pull(&mut out, s as u64 * 5);
        }
        assert!(j.target_ms() < 100, "过了一分钟按统计来：{}", j.target_ms());
    }
}
