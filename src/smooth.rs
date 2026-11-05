//! 被控端鼠标平滑播放（抖动缓冲）。
//!
//! WiFi 会把鼠标包攒一批一起送到（实测笔记本那头每秒 ~10 次 >8ms 的停顿，300Hz 屏上 23% 的帧没更新）：
//! 到了就当场注入的话，光标是“停一下、瞬移一下”。这里按主控端打的发送时刻匀速播放：
//!
//! - 时间：主控端在 Move 里带上自己的单调时钟（微秒，u32 回绕）。本机算 off = 到达时刻 − 发送时刻，
//!   最近 1~2 秒里最小的 off 当“基准”（最快的那条路），每个包晚到了 late = off − 基准。
//! - 延迟：最近 LATE_N 个包 late 的 P95 当缓冲延迟 D（上限 MAX_DELAY）。网络稳时 D≈0，就是当场注入。
//! - 播放：虚拟时钟 v（“现在该显示发送端哪一刻”）目标是 now − 基准 − D。v 按 1 + 误差/TAU 的速率走
//!   （落后就快进追上、超前就放慢，不跳），不超过已收到的最新包；落后超过 SNAP 直接跳到目标。
//!   每次把发送时刻 ≤ v 的包取出，只注入最新的那个。
//! - 队列空着时（等包）时钟冻结：断流后来的一批包从断处接着快进播放，不会一下瞬移到最后。
//!
//! 纯逻辑，时间由调用方给（微秒）；注入、线程在 inject.rs。

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::Instant;

/// 缓冲延迟上限（微秒）。
pub const MAX_DELAY: i64 = 10_000;
/// 统计最近这么多个包的晚到程度。
const LATE_N: usize = 256;
/// 晚到直方图：每格 250µs，最多统计到 MAX_DELAY。
const BIN: i64 = 250;
const BINS: usize = (MAX_DELAY / BIN) as usize + 1;
/// 取晚到的这个分位数当缓冲延迟（千分比）。
const PCT: usize = 950;
/// 追赶时间常数：落后 TAU 就以 2 倍速播放（落后量按 e^(-t/TAU) 收敛）。
const TAU: f64 = 12_000.0;
const MIN_RATE: f64 = 0.5;
const MAX_RATE: f64 = 6.0;
/// 落后这么多（微秒）就不追了，直接跳到目标。
const SNAP: i64 = 120_000;
/// 基准窗口：每个桶 1 秒，基准取当前桶和上一个桶的最小值。
const WINDOW: i64 = 1_000_000;
/// 队列最多积压这么多个位置（正常远用不到；满了丢最旧的）。
const QUEUE_MAX: usize = 256;

/// 诊断：%APPDATA%\kmlink\smooth-trace 文件存在时（启动时看一次），每个收到的带时间戳位置追加一行
/// “到达µs 发送µs(原始) x y” 到 %APPDATA%\kmlink\smooth-trace.txt，用来离线比较参数。
#[cfg_attr(test, allow(dead_code))]
pub fn trace(now: i64, raw: u32, x: i32, y: i32) {
    use std::io::Write;
    static W: OnceLock<Option<std::sync::Mutex<std::io::BufWriter<std::fs::File>>>> = OnceLock::new();
    let w = W.get_or_init(|| {
        let dir = std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("kmlink");
        if !dir.join("smooth-trace").exists() {
            return None;
        }
        let f = std::fs::File::create(dir.join("smooth-trace.txt")).ok()?;
        Some(std::sync::Mutex::new(std::io::BufWriter::new(f)))
    });
    if let Some(w) = w {
        let mut w = w.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(w, "{now} {raw} {x} {y}");
        let _ = w.flush();
    }
}

/// 本进程单调时钟（微秒）。
pub fn now_us() -> i64 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_micros() as i64
}

/// 发送端打在 Move 里的时间戳（微秒，回绕）。0 留给“没有时间戳”（旧版本 / TCP），所以避开 0。
pub fn stamp() -> u32 {
    (now_us() as u32).max(1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub epoch: u32,
    pub seq: u32,
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy)]
struct Queued {
    s: i64,
    it: Item,
}

/// 最近 1~2 秒的最小值。
#[derive(Default)]
struct MinWindow {
    start: i64,
    cur: Option<i64>,
    prev: Option<i64>,
}

impl MinWindow {
    fn add(&mut self, now: i64, v: i64) {
        if self.cur.is_none() && self.prev.is_none() {
            self.start = now;
        }
        let age = now - self.start;
        if age >= 2 * WINDOW {
            self.prev = None;
            self.cur = None;
            self.start = now;
        } else if age >= WINDOW {
            self.prev = self.cur;
            self.cur = None;
            self.start += WINDOW;
        }
        self.cur = Some(self.cur.map_or(v, |c| c.min(v)));
    }
    fn get(&self) -> Option<i64> {
        match (self.cur, self.prev) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// 最近 LATE_N 个晚到值的直方图。
struct LateHist {
    ring: VecDeque<u16>,
    bins: [u16; BINS],
}

impl Default for LateHist {
    fn default() -> Self {
        Self { ring: VecDeque::with_capacity(LATE_N), bins: [0; BINS] }
    }
}

impl LateHist {
    fn add(&mut self, late: i64) {
        let b = (late.clamp(0, MAX_DELAY) / BIN) as u16;
        if self.ring.len() == LATE_N {
            let old = self.ring.pop_front().unwrap();
            self.bins[old as usize] -= 1;
        }
        self.ring.push_back(b);
        self.bins[b as usize] += 1;
    }
    /// P95 晚到（微秒，取格子上沿）。
    fn pct(&self) -> i64 {
        let n = self.ring.len();
        if n == 0 {
            return 0;
        }
        let want = (n * PCT).div_ceil(1000);
        let mut acc = 0usize;
        for (i, &c) in self.bins.iter().enumerate() {
            acc += c as usize;
            if acc >= want {
                return if i == 0 { 0 } else { ((i as i64 + 1) * BIN).min(MAX_DELAY) };
            }
        }
        MAX_DELAY
    }
    fn clear(&mut self) {
        self.ring.clear();
        self.bins = [0; BINS];
    }
}

/// 播放速率：落后 err（微秒）时 1 + err/TAU，夹在 [MIN_RATE, MAX_RATE]。
fn rate_for(err: f64) -> f64 {
    (1.0 + err / TAU).clamp(MIN_RATE, MAX_RATE)
}

#[derive(Default)]
pub struct Playout {
    q: VecDeque<Queued>,
    /// 上一个原始时间戳及其展开值
    last_raw: Option<(u32, i64)>,
    base: MinWindow,
    late: LateHist,
    /// 已收到的最新发送时刻
    newest: i64,
    /// 虚拟时钟（发送端时刻）及上次推进它的本机时刻
    v: Option<f64>,
    v_at: i64,
    /// 统计（诊断日志）
    pub stats: Stats,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    pub pushed: u32,
    pub injected: u32,
    pub snapped: u32,
    pub max_delay: i64,
}

impl Playout {
    /// 缓冲延迟（微秒）。
    pub fn delay(&self) -> i64 {
        self.late.pct()
    }

    /// 收到一个带时间戳的位置。
    pub fn push(&mut self, now: i64, raw: u32, it: Item) {
        let s = match self.last_raw {
            Some((r, e)) => e + raw.wrapping_sub(r) as i32 as i64,
            None => raw as i64,
        };
        // 乱序晚到的旧包不更新展开基点
        if self.last_raw.is_none_or(|(_, e)| s >= e) {
            self.last_raw = Some((raw, s));
        }
        self.stats.pushed = self.stats.pushed.wrapping_add(1);
        let off = now - s;
        self.base.add(now, off);
        let base = self.base.get().unwrap_or(off);
        self.late.add(off - base);
        self.stats.max_delay = self.stats.max_delay.max(self.delay());
        // 已经播过去的时刻：丢掉（更新的位置已经显示了）
        if self.v.is_some_and(|v| (s as f64) <= v) {
            return;
        }
        if self.q.is_empty() {
            // 刚才在等包：时钟从现在接着走，等待的这段不算落后
            self.v_at = now;
        }
        self.newest = if self.v.is_none() && self.q.is_empty() { s } else { self.newest.max(s) };
        let at = self.q.iter().rposition(|q| q.s <= s).map_or(0, |i| i + 1);
        self.q.insert(at, Queued { s, it });
        if self.q.len() > QUEUE_MAX {
            self.q.pop_front();
        }
    }

    /// 推进时钟，返回现在该注入的位置（没有就 None）和下次该醒的时刻（队列空了就 None）。
    pub fn poll(&mut self, now: i64) -> (Option<Item>, Option<i64>) {
        let Some(front) = self.q.front().map(|q| q.s) else { return (None, None) };
        let base = self.base.get().unwrap_or(0);
        let target = (now - base - self.delay()) as f64;
        let adv = match self.v {
            None => f64::MIN,
            Some(v) if target - v > SNAP as f64 => {
                self.stats.snapped = self.stats.snapped.wrapping_add(1);
                target
            }
            Some(v) => v + (now - self.v_at).max(0) as f64 * rate_for(target - v),
        };
        // 队首已经到点：至少播到它（两个包之间的空白时刻跳过也不会漏掉位置）
        let v = adv.max(target.min(front as f64));
        let v = v.min(self.newest as f64);
        self.v = Some(v);
        self.v_at = now;
        let mut out = None;
        while let Some(q) = self.q.front() {
            if q.s as f64 > v {
                break;
            }
            out = Some(q.it);
            self.q.pop_front();
        }
        if out.is_some() {
            self.stats.injected = self.stats.injected.wrapping_add(1);
        }
        let wake = self.q.front().map(|q| {
            let dt = ((q.s as f64 - v) / rate_for(target - v)).ceil() as i64;
            now + dt.clamp(200, 4_000)
        });
        (out, wake)
    }

    /// 扔掉排着的位置（点击/滚轮带着最新位置，排着的旧位置不能再播）。统计保留。
    pub fn flush(&mut self) {
        self.q.clear();
        if self.v.is_some() {
            self.v = Some(self.newest as f64);
        }
    }

    /// 新的接管 / 会话：全部重来。
    pub fn reset(&mut self) {
        self.q.clear();
        self.last_raw = None;
        self.base = MinWindow::default();
        self.late.clear();
        self.newest = 0;
        self.v = None;
    }

    pub fn pending(&self) -> bool {
        !self.q.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(seq: u32) -> Item {
        Item { epoch: 1, seq, x: seq as i32, y: 0 }
    }

    /// 按 (到达时刻, 发送时刻) 喂包，每 250µs poll 一次，返回 (注入时刻, seq)。
    fn run(arrivals: &[(i64, i64)], until: i64) -> Vec<(i64, u32)> {
        let mut p = Playout::default();
        let mut out = vec![];
        let mut k = 0;
        let mut t = 0;
        while t <= until {
            while k < arrivals.len() && arrivals[k].0 <= t {
                p.push(t, arrivals[k].1 as u32, it(k as u32 + 1));
                k += 1;
            }
            if let (Some(i), _) = p.poll(t) {
                out.push((t, i.seq));
            }
            t += 250;
        }
        out
    }

    #[test]
    fn steady_stream_plays_immediately() {
        // 每 2ms 一个，网络延迟恒定 3ms：缓冲延迟应为 0，到了就播
        let a: Vec<_> = (0..200).map(|i| (1_000_000 + i * 2000 + 3000, 1_000_000 + i * 2000)).collect();
        let out = run(&a, 1_500_000);
        assert_eq!(out.len(), 200);
        for (i, (t, seq)) in out.iter().enumerate() {
            assert_eq!(*seq, i as u32 + 1);
            assert!(*t - a[i].0 <= 250, "包 {i} 晚播 {}µs", t - a[i].0);
        }
    }

    #[test]
    fn burst_after_gap_is_spread_out() {
        // 稳定 300 个，然后 20ms 断流，断流期间发的 10 个和之后的一起到
        let mut a = vec![];
        for i in 0..300i64 {
            a.push((i * 2000 + 3000, i * 2000));
        }
        let t0 = 300 * 2000;
        for i in 0..10i64 {
            a.push((t0 + 20_000 + 3000, t0 + i * 2000));
        }
        for i in 10..100i64 {
            a.push((t0 + i * 2000 + 3000, t0 + i * 2000));
        }
        let out = run(&a, t0 + 300_000);
        // 那 10 个不能在同一时刻全挤出来：最多一次跳过几个
        let burst: Vec<_> = out.iter().filter(|(_, s)| (301..=310).contains(s)).collect();
        assert!(burst.len() >= 5, "断流后的一批只播出 {} 个", burst.len());
        // 每个 seq 至多注入一次，顺序递增
        for w in out.windows(2) {
            assert!(w[1].1 > w[0].1);
        }
    }

    #[test]
    fn jittery_stream_gets_buffer_and_no_long_freeze() {
        // 每 2ms 发；每 10 个包里有一个晚到 6ms（WiFi 攒包）
        let a: Vec<_> = (0..1000i64).map(|i| (i * 2000 + 2000 + if i % 10 == 5 { 6000 } else { 0 }, i * 2000)).collect();
        let mut p = Playout::default();
        let (mut k, mut t, mut last) = (0, 0i64, None::<i64>);
        let mut worst = 0;
        while t <= 2_100_000 {
            while k < a.len() && a[k].0 <= t {
                p.push(t, a[k].1 as u32, it(k as u32 + 1));
                k += 1;
            }
            if let (Some(_), _) = p.poll(t) {
                if t > 500_000 && t < 1_900_000 {
                    if let Some(l) = last {
                        worst = worst.max(t - l);
                    }
                }
                last = Some(t);
            }
            t += 250;
        }
        assert!(p.delay() > 0 && p.delay() <= MAX_DELAY);
        // 有缓冲之后，注入间隔不应再出现 >5ms 的停顿（不加缓冲会有 ~6ms 的）
        assert!(worst <= 5000, "最长停顿 {worst}µs");
    }

    #[test]
    fn huge_gap_snaps() {
        let mut a = vec![];
        for i in 0..100i64 {
            a.push((i * 2000 + 1000, i * 2000));
        }
        // 300ms 断流后一起到
        let t0 = 100 * 2000;
        for i in 0..150i64 {
            a.push((t0 + 300_000, t0 + i * 2000));
        }
        let mut p = Playout::default();
        for (k, &(at, s)) in a.iter().enumerate() {
            p.push(at, s as u32, it(k as u32 + 1));
            p.poll(at);
        }
        let (_, _) = p.poll(t0 + 300_000 + 1000);
        assert!(p.stats.snapped >= 1);
    }

    #[test]
    fn wraparound_and_flush() {
        let mut p = Playout::default();
        let base = u32::MAX as i64 - 3000;
        for i in 0..5i64 {
            let s = (base + i * 2000) as u32; // 第 3 个起回绕
            p.push(10_000 + i * 2000, s, it(i as u32 + 1));
            let (got, _) = p.poll(10_000 + i * 2000);
            assert_eq!(got.map(|g| g.seq), Some(i as u32 + 1));
        }
        p.push(30_000, (base + 50_000) as u32, it(6));
        p.flush();
        assert!(!p.pending());
        assert_eq!(p.poll(40_000).0, None);
    }

    #[test]
    fn min_window_follows_rising_offset() {
        let mut w = MinWindow::default();
        w.add(0, 100);
        w.add(500_000, 50);
        assert_eq!(w.get(), Some(50));
        w.add(1_200_000, 80);
        assert_eq!(w.get(), Some(50));
        w.add(2_300_000, 90);
        assert_eq!(w.get(), Some(80));
        w.add(5_000_000, 200);
        assert_eq!(w.get(), Some(200));
    }
}
