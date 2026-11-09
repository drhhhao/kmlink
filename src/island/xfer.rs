//! 传输大岛（方向 C · 液体填充，用户 2026-11-09 定；设计稿 设计\灵动岛\传输大岛\，动效照 island.js 移植）。
//!
//! 粘贴对方的文件时，资源管理器不再弹进度框（FILEDESCRIPTOR 不带 FD_PROGRESSUI），进度全在这里：
//!   ( [📄] 接收中 ) 小胶囊站稳 → 长成 300×52 的卡片：「3 个文件 剩 4 秒        45%」，玻璃里灌绿色的水，水位就是进度；
//!   悬停 120ms 后右边百分比换成「取消」；传完收回成「已收到 3 个文件」，没传完变琥珀收回成「没传完」并轻抖，取消收回成灰「已取消」。
//! 全部按时间算（不按帧）：形状用弹簧（解析解前进 dt），内容用补间，进度用指数平滑追“按速度外推的目标”，只进不退。
//! 每帧：形状在动时整块重算玻璃；停稳后玻璃取缓存，只在卡片范围里画水、图标、文字（便宜，120/300Hz 都跟得上）。

use super::*;
use std::collections::HashMap;

pub(super) const CARD_W: f32 = 300.0;
pub(super) const CARD_H: f32 = 52.0;
const CARD_PAD: f32 = 8.0;
const CARD_IC: f32 = 36.0;
/// 卡片里文字的左边（图标右边 10）
const X0: f32 = CARD_PAD + CARD_IC + 10.0;
/// 右边百分比的右边距；取消按钮的右边距
const PCT_R: f32 = 18.0;
const BTN_RX: f32 = 20.0;
const ETA_PX: f32 = 12.0;
const GRAY: u32 = 0x8E8E93;

const EXPAND_AT: f32 = 600.0;
const LAG: f32 = 30.0;
const DONE_WAIT: f32 = 150.0;
const FAIL_SHOW: f32 = 450.0;
const X_HOLD: f32 = 1800.0;
const X_HOLD_FAIL: f32 = 2200.0;
const X_HOLD_CANCEL: f32 = 1200.0;
const SHAKE_AT: f32 = 520.0;
const SPR_EXPAND: (f32, f32) = (17.0, 0.78);
const SPR_COLLAPSE: (f32, f32) = (20.0, 0.85);
const SPR_RESIZE: (f32, f32) = (22.2, 0.72);
const TAU: f32 = 150.0;
const TAU_DONE: f32 = 80.0;
const EXTRAP: f32 = 400.0;
const CAP: f32 = 0.995;
const RATE_TC: f32 = 1000.0;
const ETA_AFTER: f32 = 1500.0;
const ETA_EVERY: f32 = 1000.0;
const STUCK: f32 = 2000.0;
/// 这么久一点进度都没有（也没说完 / 失败）：按没传完收起，别一直挂着
const GIVE_UP: f32 = 120_000.0;

// ───────────────────────── 弹簧、补间 ─────────────────────────

/// 阻尼弹簧（ζ<1），解析解前进 dt 秒：帧率无关、大 dt 也不发散。
#[derive(Clone, Copy, Debug)]
struct Spring {
    x: f32,
    v: f32,
    t: f32,
    w: f32,
    z: f32,
}

impl Spring {
    fn new(x: f32) -> Spring {
        Spring { x, v: 0.0, t: x, w: 20.0, z: 0.8 }
    }
    fn set(&mut self, x: f32) {
        self.x = x;
        self.v = 0.0;
        self.t = x;
    }
    fn to(&mut self, t: f32, p: (f32, f32)) {
        self.t = t;
        self.w = p.0;
        self.z = p.1;
    }
    fn step(&mut self, dt: f32) {
        let (w, z, x0, v0) = (self.w, self.z, self.x - self.t, self.v);
        if x0.abs() < 1e-3 && v0.abs() < 1e-2 {
            self.x = self.t;
            self.v = 0.0;
            return;
        }
        let wd = w * (1.0 - z * z).sqrt();
        let e = (-z * w * dt).exp();
        let (c, s) = ((wd * dt).cos(), (wd * dt).sin());
        let b = (v0 + z * w * x0) / wd;
        let x = e * (x0 * c + b * s);
        self.v = -z * w * x + e * (-x0 * wd * s + b * wd * c);
        self.x = self.t + x;
    }
    fn settled(&self) -> bool {
        self.x == self.t && self.v == 0.0
    }
}

#[derive(Clone, Copy, Debug)]
enum Ease {
    Out,
    In,
    InOut,
    Lin,
}

fn ease(e: Ease, x: f32) -> f32 {
    match e {
        Ease::Out => ease_out(x),
        Ease::In => ease_in(x),
        Ease::InOut => ease_in_out(x),
        Ease::Lin => sat(x),
    }
}

#[derive(Clone, Copy, Debug)]
struct Seg {
    t0: f32,
    dur: f32,
    from: f32,
    to: f32,
    e: Ease,
}

/// 补间表：每个键一串段，取起点 ≤ now 的最后一段。
#[derive(Default)]
struct Tw {
    m: HashMap<&'static str, (f32, Vec<Seg>)>,
}

impl Tw {
    fn set(&mut self, k: &'static str, v: f32) {
        self.m.insert(k, (v, Vec::new()));
    }
    fn base(&mut self, k: &'static str, v: f32) {
        self.m.entry(k).or_insert((0.0, Vec::new())).0 = v;
    }
    fn get(&self, k: &str, now: f32) -> f32 {
        let Some((b, l)) = self.m.get(k) else { return 0.0 };
        let mut cur: Option<&Seg> = None;
        for a in l {
            if a.t0 <= now && cur.is_none_or(|c| a.t0 >= c.t0) {
                cur = Some(a);
            }
        }
        match cur {
            None => *b,
            Some(c) => c.from + (c.to - c.from) * ease(c.e, sat((now - c.t0) / c.dur.max(1e-3))),
        }
    }
    fn add(&mut self, k: &'static str, t0: f32, dur: f32, to: f32, e: Ease) {
        let from = self.get(k, t0);
        self.add_from(k, t0, dur, from, to, e);
    }
    fn add_from(&mut self, k: &'static str, t0: f32, dur: f32, from: f32, to: f32, e: Ease) {
        let l = &mut self.m.entry(k).or_insert((0.0, Vec::new())).1;
        // 已经结束的旧段只留最后一段（够算起点值）
        if l.len() > 8 {
            l.sort_by(|a, b| a.t0.total_cmp(&b.t0));
            l.drain(..l.len() - 4);
        }
        l.push(Seg { t0, dur, from, to, e });
    }
    /// 还有没走完的段（要继续出帧）。
    fn busy(&self, now: f32) -> bool {
        self.m.values().any(|(_, l)| l.iter().any(|s| now < s.t0 + s.dur))
    }
}

// ───────────────────────── 文字层 ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum F {
    Bold,
    Reg,
    Small,
    Btn,
}

/// 一段文字的灰度蒙版 + 晕影（物理像素），左上角相对“文字左边、竖直中线”的偏移 (ox, oy)。
struct TextLayer {
    w: usize,
    h: usize,
    cov: Vec<f32>,
    halo: Vec<f32>,
    /// 文字宽（逻辑像素）
    lw: f32,
    ox: f32,
    oy: f32,
}

struct Texts {
    s: f32,
    bold: Font,
    reg: Font,
    small: Font,
    btn: Font,
    probe: Option<Dib>,
    cache: HashMap<(F, String), TextLayer>,
}

impl Texts {
    fn new(s: f32) -> Texts {
        Texts {
            s,
            bold: Font::ui_smooth(FONT_PX, 700, s),
            reg: Font::ui_smooth(FONT_PX, 400, s),
            small: Font::ui_smooth(ETA_PX, 400, s),
            btn: Font::ui_smooth(BTN_FONT_PX, 700, s),
            probe: Dib::new(1, 1),
            cache: HashMap::new(),
        }
    }
    fn font(&self, f: F) -> &Font {
        match f {
            F::Bold => &self.bold,
            F::Reg => &self.reg,
            F::Small => &self.small,
            F::Btn => &self.btn,
        }
    }
    /// 文字宽（逻辑像素）。
    fn width(&self, f: F, t: &str) -> f32 {
        match &self.probe {
            Some(p) => text_px(p.dc, self.font(f), t) as f32 / self.s,
            None => 0.0,
        }
    }
    fn layer(&mut self, f: F, t: &str) -> Option<&TextLayer> {
        if t.is_empty() {
            return None;
        }
        let key = (f, t.to_string());
        if !self.cache.contains_key(&key) {
            if self.cache.len() > 64 {
                self.cache.clear();
            }
            let s = self.s;
            let lw = self.width(f, t);
            let m = (4.0 * s).ceil() as i32;
            let w = (lw * s).ceil() as i32 + 2 * m;
            let h = ((FONT_PX + 12.0) * s).ceil() as i32;
            let font = self.font(f);
            let base = font.baseline_at(h as f32 / 2.0 + 0.5 * s).round() as i32;
            let cov = text_cov(font, w, h, m, base, t)?;
            let (wu, hu) = (w as usize, h as usize);
            let mut halo: Vec<[f32; 3]> = cov.iter().map(|v| [*v, *v, *v]).collect();
            blur(&mut halo, wu, hu, 1.6 * s);
            let halo = halo.into_iter().map(|c| c[0]).collect();
            self.cache.insert(key.clone(), TextLayer { w: wu, h: hu, cov, halo, lw, ox: -(m as f32), oy: -(h as f32) / 2.0 });
        }
        self.cache.get(&key)
    }
    /// 放不下就中间截断（保留扩展名）：季度报告…终版.pdf
    fn mid_cut(&self, name: &str, f: F, max: f32) -> String {
        if self.width(f, name) <= max {
            return name.to_string();
        }
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 => (&name[..i], &name[i..]),
            _ => (name, ""),
        };
        let ch: Vec<char> = stem.chars().collect();
        let cut = ch.len().div_ceil(2);
        let (mut head, mut tail) = (ch[..cut].to_vec(), ch[cut..].to_vec());
        while head.len() + tail.len() > 2 {
            if head.len() >= tail.len() {
                head.pop();
            } else {
                tail.remove(0);
            }
            let s = format!("{}…{}{}", head.iter().collect::<String>(), tail.iter().collect::<String>(), ext);
            if self.width(f, &s) <= max {
                return s;
            }
        }
        format!("{}…{}", head.iter().collect::<String>(), ext)
    }
}

// ───────────────────────── 状态 ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum End {
    Done,
    Fail,
    Cancel,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum XPhase {
    Appear,
    Pill,
    Card,
    End,
    Leave,
    Gone,
}

#[derive(Clone, Copy, Debug)]
enum Job {
    Expand,
    Sh(f32, (f32, f32)),
    Sw(f32, (f32, f32)),
    Collapse(End),
}

struct Eta {
    label: String,
    sec: Option<i32>,
    at: f32,
    slot: usize,
}

pub(super) struct Xfer {
    start: Instant,
    /// 测试用：固定的“现在”（毫秒）
    fake: Option<f32>,
    pub(super) phase: XPhase,
    ts: f32,
    tw: Tw,
    jobs: Vec<(f32, Job)>,
    sw: Spring,
    sh: Spring,
    w: f32,
    h: f32,
    alpha: f32,
    dy: f32,
    dx: f32,
    cm: f32,
    d: f32,
    rep: Option<(f32, f32)>,
    had_prev: bool,
    rate: f32,
    total: f32,
    t0x: Option<f32>,
    move_t: f32,
    done: bool,
    failed: bool,
    done_queued: bool,
    pub(super) end: Option<End>,
    end_words: (String, String),
    ew: f32,
    hold_until: f32,
    shake_at: Option<f32>,
    tl: f32,
    lw: f32,
    eta: Eta,
    eta_text: [String; 2],
    pw: f32,
    title: String,
    n_files: u32,
    hover: bool,
    last: Option<f32>,
    texts: Texts,
    pct_shown: i32,
}

impl Xfer {
    /// title = 「3 个文件」或文件名；n_files = 收完时说的个数。
    pub(super) fn new(title: &str, n_files: u32, s: f32) -> Xfer {
        let texts = Texts::new(s);
        let pw = PAD_L + ICON + GAP + texts.width(F::Bold, "接收中") + PAD_R;
        let max_title = (CARD_W - PCT_R) - 40.0 - 8.0 - 52.0 - 8.0 - X0;
        let title = texts.mid_cut(title, F::Bold, max_title);
        let mut tw = Tw::default();
        for k in ["pill", "r1", "end", "gCheck", "gX", "amb", "gray", "eta1", "btn"] {
            tw.set(k, 0.0);
        }
        for k in ["gFile", "fillO", "wave", "info", "eta0"] {
            tw.set(k, 1.0);
        }
        let mut x = Xfer {
            start: Instant::now(),
            fake: None,
            phase: XPhase::Appear,
            ts: 0.0,
            tw,
            jobs: Vec::new(),
            sw: Spring::new(20.0),
            sh: Spring::new(20.0),
            w: 20.0,
            h: 20.0,
            alpha: 0.0,
            dy: 0.0,
            dx: 0.0,
            cm: 1.0,
            d: 0.0,
            rep: None,
            had_prev: false,
            rate: 0.0,
            total: 0.0,
            t0x: None,
            move_t: 0.0,
            done: false,
            failed: false,
            done_queued: false,
            end: None,
            end_words: (String::new(), String::new()),
            ew: 100.0,
            hold_until: f32::INFINITY,
            shake_at: None,
            tl: 0.0,
            lw: 0.0,
            eta: Eta { label: "接收中".into(), sec: None, at: -1e9, slot: 0 },
            eta_text: ["接收中".into(), String::new()],
            pw,
            title,
            n_files,
            hover: false,
            last: None,
            texts,
            pct_shown: -1,
        };
        x.at(EXPAND_AT, Job::Expand);
        x
    }

    pub(super) fn now(&self) -> f32 {
        self.fake.unwrap_or_else(|| self.start.elapsed().as_secs_f32() * 1000.0)
    }

    fn at(&mut self, t: f32, j: Job) {
        self.jobs.push((t, j));
        self.jobs.sort_by(|a, b| a.0.total_cmp(&b.0));
    }

    fn run(&mut self, t: f32, j: Job) {
        match j {
            Job::Expand => {
                if self.end.is_none() {
                    self.expand(t);
                }
            }
            Job::Sh(v, p) => self.sh.to(v, p),
            Job::Sw(v, p) => self.sw.to(v, p),
            Job::Collapse(k) => self.collapse(k, t),
        }
    }

    fn expand(&mut self, now: f32) {
        self.phase = XPhase::Card;
        self.tw.add("pill", now, 100.0, 0.0, Ease::In);
        self.sw.to(CARD_W, SPR_EXPAND);
        self.at(now + LAG, Job::Sh(CARD_H, SPR_EXPAND));
        self.tw.add_from("r1", now + 160.0, 240.0, 0.0, 1.0, Ease::Out);
    }

    /// 收到进度报告（已读 / 总字节）。
    pub(super) fn report(&mut self, bytes: u64, total: u64) {
        let now = self.now();
        let (b, total) = (bytes as f32, total.max(1) as f32);
        self.total = total;
        if self.t0x.is_none() {
            self.t0x = Some(now);
        }
        if let Some((rb, rt)) = self.rep {
            let dt = now - rt;
            if dt > 0.0 {
                let inst = (b - rb) / dt;
                let k = if self.had_prev { 1.0 - (-dt / RATE_TC).exp() } else { 1.0 };
                self.rate += (inst - self.rate) * k;
                self.had_prev = true;
            }
        }
        if self.rep.is_none_or(|(rb, _)| b > rb) {
            self.move_t = now;
        }
        self.rep = Some((b, now));
    }

    /// 全部读完。
    pub(super) fn finish(&mut self) {
        let now = self.now();
        if self.end.is_some() || self.done {
            return;
        }
        self.done = true;
        self.eta_out(now);
        if matches!(self.phase, XPhase::Appear | XPhase::Pill) {
            // 还没长大就传完了：不长大，原地换成「已收到」
            self.jobs.clear();
            let t = now.max(self.ts + APPEAR_MS);
            self.at(t, Job::Collapse(End::Done));
        } else if self.total == 0.0 {
            self.total = 1.0;
            self.rep = Some((1.0, now));
        }
    }

    pub(super) fn fail(&mut self) {
        let now = self.now();
        if self.end.is_some() || self.failed {
            return;
        }
        self.failed = true;
        self.tw.add("amb", now, 150.0, 1.0, Ease::Lin);
        self.tw.add("wave", now, 200.0, 0.0, Ease::Out);
        self.eta_out(now);
        if matches!(self.phase, XPhase::Appear | XPhase::Pill) {
            self.jobs.clear();
        }
        self.at(now + FAIL_SHOW, Job::Collapse(End::Fail));
    }

    pub(super) fn cancel(&mut self) {
        let now = self.now();
        if self.end.is_none() {
            self.jobs.clear();
            self.collapse(End::Cancel, now);
        }
    }

    /// 光标进 / 出卡片：120ms 后「取消」淡入、百分比淡出。
    pub(super) fn hover(&mut self, on: bool) {
        let now = self.now();
        if self.hover == on || self.end.is_some() || self.phase != XPhase::Card {
            self.hover = on && self.end.is_none();
            return;
        }
        self.hover = on;
        let t = if on { now + 120.0 } else { now + 300.0 };
        self.tw.add("btn", t, if on { 180.0 } else { 140.0 }, if on { 1.0 } else { 0.0 }, if on { Ease::Out } else { Ease::In });
        self.tw.add("info", t, if on { 120.0 } else { 180.0 }, if on { 0.0 } else { 1.0 }, if on { Ease::In } else { Ease::Out });
    }

    /// 取消按钮现在点得到吗（看得见了）。
    pub(super) fn button_live(&self) -> bool {
        self.end.is_none() && self.phase == XPhase::Card && self.tw.get("btn", self.now()) > 0.5
    }

    fn eta_out(&mut self, now: f32) {
        let k = if self.eta.slot == 0 { "eta0" } else { "eta1" };
        self.tw.add(k, now, 120.0, 0.0, Ease::In);
    }

    fn pill_w(&self, word: &str, verb: &str) -> f32 {
        PAD_L + ICON + GAP + self.texts.width(F::Bold, word) + if verb.is_empty() { 0.0 } else { WORD_GAP + self.texts.width(F::Reg, verb) } + PAD_R
    }

    fn collapse(&mut self, kind: End, now: f32) {
        if self.phase == XPhase::Appear {
            self.sw.set(self.pw);
            self.sh.set(H);
            self.tw.base("pill", 1.0);
            self.phase = XPhase::Pill;
        }
        let (word, verb) = match kind {
            End::Done => ("已收到".to_string(), format!("{} 个文件", self.n_files)),
            End::Fail => ("没传完".to_string(), String::new()),
            End::Cancel => ("已取消".to_string(), String::new()),
        };
        self.end = Some(kind);
        self.ew = self.pill_w(&word, &verb).max(MIN_W.min(self.pill_w(&word, &verb)));
        self.end_words = (word, verb);
        let was_card = self.phase == XPhase::Card;
        self.phase = XPhase::End;
        if was_card {
            self.tw.add("r1", now, 120.0, 0.0, Ease::In);
            self.tw.add("btn", now, 100.0, 0.0, Ease::In);
            self.tw.add("fillO", now + 60.0, 200.0, 0.0, Ease::InOut);
            self.at(now + 40.0, Job::Sh(H, SPR_COLLAPSE));
            self.at(now + 40.0 + LAG, Job::Sw(self.ew, SPR_COLLAPSE));
        } else {
            self.tw.add("pill", now, 100.0, 0.0, Ease::In);
            self.sw.to(self.ew, SPR_RESIZE);
        }
        match kind {
            End::Done => {
                self.tw.add("gFile", now + 60.0, 120.0, 0.0, Ease::Lin);
                self.tw.add("gCheck", now + 60.0, 120.0, 1.0, Ease::Lin);
            }
            End::Cancel => {
                self.tw.add("gFile", now + 60.0, 120.0, 0.0, Ease::Lin);
                self.tw.add("gX", now + 60.0, 120.0, 1.0, Ease::Lin);
                self.tw.add("gray", now + 60.0, 140.0, 1.0, Ease::Lin);
            }
            End::Fail => {
                // 还没变琥珀（没长大就失败了）：现在变
                if self.tw.get("amb", now) < 1.0 {
                    self.tw.add("amb", now, 150.0, 1.0, Ease::Lin);
                }
            }
        }
        self.tw.add_from("end", now + 200.0, 200.0, 0.0, 1.0, Ease::Out);
        let hold = match kind {
            End::Done => X_HOLD,
            End::Fail => X_HOLD_FAIL,
            End::Cancel => X_HOLD_CANCEL,
        };
        self.hold_until = now + 400.0 + hold;
        if kind == End::Fail {
            self.shake_at = Some(now + SHAKE_AT);
        }
    }

    // ───────────── 每帧 ─────────────

    pub(super) fn update(&mut self, now: f32) {
        let dt = self.last.map_or(0.0, |l| (now - l).max(0.0) / 1000.0);
        self.last = Some(now);
        while let Some(&(t, j)) = self.jobs.first() {
            if t > now {
                break;
            }
            self.jobs.remove(0);
            self.run(t, j);
        }
        match self.phase {
            XPhase::Appear => {
                let t = now - self.ts;
                let p = appear(t, self.pw);
                (self.w, self.h, self.alpha, self.dy, self.cm) = (p.w, p.h, p.alpha, p.dy, 1.0);
                self.tw.base("pill", p.content);
                if t >= APPEAR_MS {
                    self.phase = if self.end.is_some() { XPhase::End } else { XPhase::Pill };
                    self.sw.set(self.pw);
                    self.sh.set(H);
                    self.tw.base("pill", 1.0);
                }
            }
            XPhase::Pill | XPhase::Card | XPhase::End => {
                self.sw.step(dt);
                self.sh.step(dt);
                (self.w, self.h, self.alpha, self.dy, self.cm) = (self.sw.x, self.sh.x, 1.0, 0.0, 1.0);
                if self.phase == XPhase::End && now >= self.hold_until {
                    self.phase = XPhase::Leave;
                    self.tl = now;
                    self.lw = self.w;
                }
            }
            XPhase::Leave | XPhase::Gone => {}
        }
        if self.phase == XPhase::Leave {
            let t = now - self.tl;
            let p = leave(t, self.lw);
            (self.w, self.h, self.alpha, self.dy, self.cm) = (p.w, p.h, p.alpha, p.dy, p.content);
            if t >= LEAVE_MS {
                self.phase = XPhase::Gone;
            }
        }
        self.dx = self.shake_at.map_or(0.0, |t0| burst(now - t0, 4.0, SHAKE_MS, 3.0));

        if self.end.is_none() && !self.done && !self.failed && self.rep.is_some_and(|(_, rt)| now - rt > GIVE_UP) {
            self.fail();
        }
        // 进度：追「按速度外推的目标」，指数平滑，只进不退
        if let Some((rb, rt)) = self.rep {
            let tgt = if self.done {
                1.0
            } else if self.failed || self.end.is_some() {
                // 没传完 / 取消：水位停在原地
                self.d
            } else {
                let ahead = (now - rt).min(EXTRAP);
                (rb + self.rate.max(0.0) * ahead).min(self.total * CAP) / self.total
            };
            let tau = if self.done { TAU_DONE } else { TAU };
            let nd = self.d + (tgt - self.d) * (1.0 - (-dt * 1000.0 / tau).exp());
            self.d = self.d.max(nd.min(if self.done { 1.0 } else { CAP }));
            if self.done && self.d > 0.999 {
                self.d = 1.0;
            }
            if self.done && self.d >= 1.0 && self.phase == XPhase::Card && self.end.is_none() && !self.done_queued {
                self.done_queued = true;
                self.at(now + DONE_WAIT, Job::Collapse(End::Done));
            }
            // 剩余时间：传够 1.5 秒才说；至多每秒改一次；只在变短、或变长超过 15% 时改；2 秒没进展说「等待中」
            let t0x = self.t0x.unwrap_or(now);
            if !self.done && !self.failed && self.end.is_none() && now - t0x >= ETA_AFTER && now - self.eta.at >= ETA_EVERY && self.rate > 0.0 {
                let est = (rb + self.rate * (now - rt).min(EXTRAP)).min(self.total);
                let s = (self.total - est) / self.rate / 1000.0;
                let mut sec = if s < 10.0 {
                    s.ceil().max(1.0) as i32
                } else if s < 60.0 {
                    ((s / 5.0).ceil() * 5.0) as i32
                } else {
                    ((s / 60.0).ceil() * 60.0) as i32
                };
                let stuck = now - self.move_t >= STUCK;
                if stuck {
                    sec = -1;
                }
                let old = self.eta.sec;
                if stuck || old.is_none_or(|o| o < 0 || sec < o || sec as f32 > o as f32 * 1.15) {
                    let label = if stuck {
                        "等待中".to_string()
                    } else if sec < 60 {
                        format!("剩 {sec} 秒")
                    } else if sec < 3600 {
                        format!("剩 {} 分钟", sec / 60)
                    } else {
                        "剩 1 小时以上".to_string()
                    };
                    if label != self.eta.label {
                        let (a, b) = (self.eta.slot, 1 - self.eta.slot);
                        let (ka, kb) = (if a == 0 { "eta0" } else { "eta1" }, if b == 0 { "eta0" } else { "eta1" });
                        self.tw.add(ka, now, 120.0, 0.0, Ease::In);
                        self.tw.add_from(kb, now + 60.0, 180.0, 0.0, 1.0, Ease::Out);
                        self.eta_text[b] = label.clone();
                        self.eta = Eta { label, sec: Some(sec), at: now, slot: b };
                    } else {
                        self.eta.sec = Some(sec);
                    }
                }
            }
        }
    }

    /// 还要不要按显示器刷新率出帧（否则只需在 hold 到期时醒一次）。
    pub(super) fn animating(&self, now: f32) -> bool {
        match self.phase {
            XPhase::Appear | XPhase::Pill | XPhase::Card | XPhase::Leave => true,
            XPhase::Gone => false,
            XPhase::End => !(self.sw.settled() && self.sh.settled()) || self.tw.busy(now) || self.shake_at.is_some_and(|t| now < t + SHAKE_MS) || !self.jobs.is_empty(),
        }
    }

    /// 停着等 hold 到期：还要多久（毫秒）。
    pub(super) fn wait_ms(&self, now: f32) -> u32 {
        (self.hold_until - now).clamp(1.0, 60_000.0) as u32
    }

    /// 玻璃形状（逻辑像素）：卡片贴底往上长（底边不动）。
    pub(super) fn pose(&self) -> Pose {
        let grow = (self.h.max(H) - H) / 2.0;
        Pose { w: self.w, h: self.h, alpha: self.alpha, content: 0.0, dy: self.dy - grow, dx: self.dx }
    }

    // ───────────── 画（玻璃之上：水、图标、文字） ─────────────

    /// out = 已画好玻璃的窗口位图（预乘 BGRA，ww×wh），(cx, cy) = 胶囊停稳时的中心（物理像素）。
    pub(super) fn overlay(&mut self, out: &mut [u8], ww: usize, wh: usize, cx: f32, cy: f32, s: f32, light: bool, now: f32) {
        let pose = self.pose();
        let (w, h) = (pose.w * s, pose.h * s);
        let (scx, scy) = (cx + pose.dx * s, cy + pose.dy * s);
        let (left, top) = (scx - w / 2.0, scy - h / 2.0);
        let r = (w / 2.0).min(h / 2.0);
        let alpha = pose.alpha;
        if alpha <= 0.0 || w < 1.0 {
            return;
        }
        let sdf = |x: f32, y: f32| sd_rrect(x, y, scx, scy, w / 2.0, h / 2.0, r);
        let ok = if light { OK_LIGHT } else { OK_DARK };
        let mixc = |a: [f32; 3], b: [f32; 3], t: f32| [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * sat(t));
        let amb = self.tw.get("amb", now);
        let gray = self.tw.get("gray", now);
        let fc = mixc(rgb(ok), rgb(FAIL), amb);
        let icol = mixc(fc, rgb(GRAY), gray);
        let x0 = (left - 2.0).floor().max(0.0) as usize;
        let x1 = ((left + w + 2.0).ceil() as usize).min(ww);
        let y0 = (top - 2.0).floor().max(0.0) as usize;
        let y1 = ((top + h + 2.0).ceil() as usize).min(wh);

        // 水（只在卡片和收回时）
        let fo = self.tw.get("fillO", now) * if matches!(self.phase, XPhase::Card | XPhase::End) { 1.0 } else { 0.0 };
        let water = self.d * w;
        let amp = 1.3 * s * self.tw.get("wave", now) * if self.d > 0.995 { 0.0 } else { 1.0 };
        let ph = now / 1000.0 * 5.0;
        let (a0, a1) = if light { (0.22, 0.36) } else { (0.30, 0.46) };
        let edge_a = if light { 0.75 } else { 0.45 } * if self.d > 0.004 && self.d < 0.995 { 1.0 } else { 0.0 };
        let grad = 16.0 * s;

        // 图标：随形状从 22 长到 36，左边距 7 → 8
        let p = if matches!(self.phase, XPhase::Appear | XPhase::Leave) { 0.0 } else { ((pose.h - H) / (CARD_H - H)).clamp(0.0, 1.1) };
        let isz = (ICON + (CARD_IC - ICON) * p) * s;
        let pad = (PAD_L + (CARD_PAD - PAD_L) * p) * s;
        let (ic_left, ic_op) = match self.phase {
            XPhase::Appear => (w / 2.0 - self.pw * s / 2.0 + PAD_L * s, self.tw.get("pill", now)),
            XPhase::Leave => (w / 2.0 - self.lw * s / 2.0 + PAD_L * s, self.cm),
            _ => (pad, 1.0),
        };
        let (icx, icy, ir) = (left + ic_left + isz / 2.0, scy, isz / 2.0);
        let (g_file, g_check, g_x) = (self.tw.get("gFile", now), self.tw.get("gCheck", now), self.tw.get("gX", now));
        let u = isz / 22.0;
        let sw = 1.7 * u;

        for y in y0..y1 {
            let fy = y as f32 + 0.5;
            // 这一行水面的位置（相对卡片左边）
            let wx = water + amp * (std::f32::consts::TAU * (fy - top) / (17.0 * s) + ph).sin();
            for x in x0..x1 {
                let fx = x as f32 + 0.5;
                let d = sdf(fx, fy);
                let cover = sat(0.5 - d) * alpha;
                if cover <= 0.0 {
                    continue;
                }
                let o = (y * ww + x) * 4;
                let mut px = [out[o + 2] as f32 / 255.0, out[o + 1] as f32 / 255.0, out[o] as f32 / 255.0, out[o + 3] as f32 / 255.0];
                let mut changed = false;
                if fo > 0.0 && water > 0.0 {
                    let xl = fx - left;
                    let inside = sat(wx - xl + 0.5);
                    if inside > 0.0 {
                        let g = if xl < wx - grad { a0 } else { a0 + (a1 - a0) * sat((xl - (wx - grad)) / grad) };
                        over4(&mut px, fc, g * inside * cover * fo);
                        changed = true;
                    }
                    if edge_a > 0.0 {
                        let e = sat(0.5 * s + 0.5 - (xl - wx).abs());
                        if e > 0.0 {
                            over4(&mut px, [1.0, 1.0, 1.0], edge_a * e * cover * fo);
                            changed = true;
                        }
                    }
                }
                // 图标
                let dcirc = ((fx - icx).powi(2) + (fy - icy).powi(2)).sqrt() - ir;
                let cc = sat(0.5 - dcirc) * ic_op * self.cm * cover;
                if cc > 0.0 {
                    over4(&mut px, icol, cc);
                    let k = |v: f32| v * u;
                    let mut glyph = 0.0f32;
                    if g_file > 0.0 {
                        let (gx, gy) = (fx - (icx - k(11.0)), fy - (icy - k(11.0)));
                        let fold = gx > 12.3 * u && gy < 7.7 * u;
                        let mut dd = if fold { f32::MAX } else { sd_rrect(fx, fy, icx, icy, k(4.5), k(6.5), k(1.3)).abs() - sw / 2.0 };
                        let (ax, ay) = (icx + k(1.5), icy - k(6.5));
                        let (bx, by) = (icx + k(4.5), icy - k(3.5));
                        dd = dd.min(seg_d(fx, fy, ax, ay, bx, by) - sw / 2.0).min(seg_d(fx, fy, ax, ay, ax, by) - sw / 2.0).min(seg_d(fx, fy, ax, by, bx, by) - sw / 2.0);
                        glyph = glyph.max(sat(0.5 - dd) * g_file);
                    }
                    if g_check > 0.0 {
                        let dd = seg_d(fx, fy, icx - k(4.2), icy + k(0.2), icx - k(1.2), icy + k(3.2)).min(seg_d(fx, fy, icx - k(1.2), icy + k(3.2), icx + k(4.5), icy - k(3.0))) - sw * 0.6;
                        glyph = glyph.max(sat(0.5 - dd) * g_check);
                    }
                    if g_x > 0.0 {
                        let dd = seg_d(fx, fy, icx - k(3.2), icy - k(3.2), icx + k(3.2), icy + k(3.2)).min(seg_d(fx, fy, icx + k(3.2), icy - k(3.2), icx - k(3.2), icy + k(3.2))) - sw * 0.55;
                        glyph = glyph.max(sat(0.5 - dd) * g_x);
                    }
                    if glyph > 0.0 {
                        over4(&mut px, [1.0, 1.0, 1.0], glyph * cc);
                    }
                    changed = true;
                }
                if changed {
                    put4(out, o, px);
                }
            }
        }

        // 文字
        let tc = if light { [0.051, 0.051, 0.059] } else { [1.0, 1.0, 1.0] };
        let (na, va, ma) = if light { (0.92, 0.70, 0.52) } else { (1.0, 0.82, 0.60) };
        let (halo_c, halo_a) = if light { ([1.0, 1.0, 1.0], 0.55) } else { ([0.0, 0.0, 0.0], 0.45) };
        let mut geo = Geo { out, ww, wh, left, scy, s, cover: &sdf, alpha };
        // 小胶囊「接收中」
        let pill = self.tw.get("pill", now) * self.cm;
        if pill > 0.0 {
            let lx = if self.phase == XPhase::Appear { w / 2.0 - self.pw * s / 2.0 } else { 0.0 };
            if let Some(l) = self.texts.layer(F::Bold, "接收中") {
                geo.blit(l, lx + (PAD_L + ICON + GAP) * s, 0.0, tc, na * pill, halo_c, halo_a * pill);
            }
        }
        // 卡片内容（以卡片左边、卡片中线为准；右边的按卡片最终宽度放）
        let r1 = self.tw.get("r1", now) * self.cm;
        if r1 > 0.0 {
            let lift = (1.0 - r1) * if self.end.is_some() { -2.0 } else { 4.0 } * s;
            let title = self.title.clone();
            let tw_title = self.texts.width(F::Bold, &title);
            if let Some(l) = self.texts.layer(F::Bold, &title) {
                geo.blit(l, X0 * s, lift, tc, na * r1, halo_c, halo_a * r1);
            }
            for slot in 0..2 {
                let a = self.tw.get(if slot == 0 { "eta0" } else { "eta1" }, now) * r1;
                let t = self.eta_text[slot].clone();
                if a > 0.0
                    && let Some(l) = self.texts.layer(F::Small, &t)
                {
                    geo.blit(l, (X0 + tw_title + 8.0) * s, lift, tc, ma * a, halo_c, halo_a * a);
                }
            }
            let info = self.tw.get("info", now);
            let pct = (self.d * 100.0 + 1e-4).floor() as i32;
            self.pct_shown = pct;
            let pt = format!("{pct}%");
            let pw = self.texts.width(F::Reg, &pt);
            if info * r1 > 0.0
                && let Some(l) = self.texts.layer(F::Reg, &pt)
            {
                geo.blit(l, (CARD_W - PCT_R - pw) * s, lift, tc, va * info * r1, halo_c, halo_a * info * r1);
            }
            // 取消按钮
            let bt = self.tw.get("btn", now) * r1;
            if bt > 0.0 {
                let (bx, by, bw, bh) = self.btn_rect();
                let lift_b = (1.0 - bt) * 3.0 * s;
                let (bg, fg) = if light { ([0.110, 0.110, 0.118], [1.0, 1.0, 1.0]) } else { ([0.949, 0.949, 0.969], [0.110, 0.110, 0.118]) };
                geo.pill(left + bx * s, scy + (by - CARD_H / 2.0) * s + lift_b, bw * s, bh * s, bg, bt);
                if let Some(l) = self.texts.layer(F::Btn, "取消") {
                    let tx = bx + (bw - l.lw) / 2.0;
                    geo.blit(l, tx * s, lift_b, fg, bt, halo_c, 0.0);
                }
            }
        }
        // 收回后的小胶囊
        let en = self.tw.get("end", now) * self.cm;
        if en > 0.0 {
            let lx = if self.phase == XPhase::Leave { w / 2.0 - self.lw * s / 2.0 } else { 0.0 };
            let lift = (1.0 - en) * 3.0 * s;
            let (word, verb) = self.end_words.clone();
            let ww_word = self.texts.width(F::Bold, &word);
            if let Some(l) = self.texts.layer(F::Bold, &word) {
                geo.blit(l, lx + (PAD_L + ICON + GAP) * s, lift, tc, na * en, halo_c, halo_a * en);
            }
            if let Some(l) = self.texts.layer(F::Reg, &verb) {
                geo.blit(l, lx + (PAD_L + ICON + GAP + ww_word + WORD_GAP) * s, lift, tc, va * en, halo_c, halo_a * en);
            }
        }
    }

    /// 取消按钮在卡片里的位置（逻辑像素：左、上、宽、高）。
    pub(super) fn btn_rect(&self) -> (f32, f32, f32, f32) {
        let bw = self.texts.width(F::Btn, "取消") + 2.0 * BTN_PAD;
        (CARD_W - BTN_RX - bw, (CARD_H - BTN_H) / 2.0, bw, BTN_H)
    }
}

fn over4(d: &mut [f32; 4], c: [f32; 3], a: f32) {
    if a <= 0.0 {
        return;
    }
    let k = 1.0 - a;
    d[0] = c[0] * a + d[0] * k;
    d[1] = c[1] * a + d[1] * k;
    d[2] = c[2] * a + d[2] * k;
    d[3] = a + d[3] * k;
}

fn put4(out: &mut [u8], o: usize, px: [f32; 4]) {
    let a = sat(px[3]);
    out[o] = (sat(px[2]).min(a) * 255.0).round() as u8;
    out[o + 1] = (sat(px[1]).min(a) * 255.0).round() as u8;
    out[o + 2] = (sat(px[0]).min(a) * 255.0).round() as u8;
    out[o + 3] = (a * 255.0).round() as u8;
}

/// 画文字 / 按钮用的几何：卡片左边 left、卡片中线 scy（物理像素），裁到胶囊里（离边 2px 内渐隐）。
struct Geo<'a, C: Fn(f32, f32) -> f32> {
    out: &'a mut [u8],
    ww: usize,
    wh: usize,
    left: f32,
    scy: f32,
    s: f32,
    cover: &'a C,
    alpha: f32,
}

impl<C: Fn(f32, f32) -> f32> Geo<'_, C> {
    /// 文字层：左边在 left + x，竖直中线在 scy + lift（双线性取样，位置可以是小数：淡入时上移不跳格）。
    fn blit(&mut self, l: &TextLayer, x: f32, lift: f32, col: [f32; 3], a: f32, halo_c: [f32; 3], halo_a: f32) {
        if a <= 0.0 {
            return;
        }
        let (ox, oy) = (self.left + x + l.ox, self.scy + lift + l.oy);
        let xs = ox.floor().max(0.0) as usize;
        let ys = oy.floor().max(0.0) as usize;
        let xe = ((ox + l.w as f32 + 1.0).ceil() as usize).min(self.ww);
        let ye = ((oy + l.h as f32 + 1.0).ceil() as usize).min(self.wh);
        let samp = |m: &Vec<f32>, sx: f32, sy: f32| -> f32 {
            let (x0, y0) = (sx.floor(), sy.floor());
            let (fx, fy) = (sx - x0, sy - y0);
            let get = |xx: f32, yy: f32| {
                if xx < 0.0 || yy < 0.0 || xx >= l.w as f32 || yy >= l.h as f32 { 0.0 } else { m[yy as usize * l.w + xx as usize] }
            };
            let top = get(x0, y0) * (1.0 - fx) + get(x0 + 1.0, y0) * fx;
            let bot = get(x0, y0 + 1.0) * (1.0 - fx) + get(x0 + 1.0, y0 + 1.0) * fx;
            top * (1.0 - fy) + bot * fy
        };
        for y in ys..ye {
            for x in xs..xe {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let d = (self.cover)(fx, fy);
                let clip = sat(-d - 2.0 * self.s) * self.alpha;
                if clip <= 0.0 {
                    continue;
                }
                let (sx, sy) = (fx - 0.5 - ox, fy - 0.5 - oy);
                let t = samp(&l.cov, sx, sy);
                let hl = if halo_a > 0.0 { samp(&l.halo, sx, sy) } else { 0.0 };
                if t <= 0.0 && hl <= 0.0 {
                    continue;
                }
                let o = (y * self.ww + x) * 4;
                let mut px = [self.out[o + 2] as f32 / 255.0, self.out[o + 1] as f32 / 255.0, self.out[o] as f32 / 255.0, self.out[o + 3] as f32 / 255.0];
                over4(&mut px, halo_c, hl * halo_a * clip);
                over4(&mut px, col, t * a * clip);
                put4(self.out, o, px);
            }
        }
    }

    /// 实心圆角条（按钮底）。
    fn pill(&mut self, x: f32, y: f32, w: f32, h: f32, col: [f32; 3], a: f32) {
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        let xs = x.floor().max(0.0) as usize;
        let ys = y.floor().max(0.0) as usize;
        let xe = ((x + w).ceil() as usize + 1).min(self.ww);
        let ye = ((y + h).ceil() as usize + 1).min(self.wh);
        for yy in ys..ye {
            for xx in xs..xe {
                let (fx, fy) = (xx as f32 + 0.5, yy as f32 + 0.5);
                let clip = sat(-(self.cover)(fx, fy) - 2.0 * self.s) * self.alpha;
                let c = sat(0.5 - sd_rrect(fx, fy, cx, cy, w / 2.0, h / 2.0, h / 2.0)) * clip * a;
                if c <= 0.0 {
                    continue;
                }
                let o = (yy * self.ww + xx) * 4;
                let mut px = [self.out[o + 2] as f32 / 255.0, self.out[o + 1] as f32 / 255.0, self.out[o] as f32 / 255.0, self.out[o + 3] as f32 / 255.0];
                over4(&mut px, col, c);
                put4(self.out, o, px);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_settles_framerate_independent() {
        for dt in [1.0 / 120.0, 1.0 / 300.0, 1.0 / 30.0] {
            let mut s = Spring::new(100.0);
            s.to(300.0, SPR_EXPAND);
            let mut peak = 0.0f32;
            let mut t = 0.0;
            while t < 1.5 {
                s.step(dt);
                peak = peak.max(s.x);
                t += dt;
            }
            assert!((s.x - 300.0).abs() < 0.01, "{dt} {}", s.x);
            // 过冲约 2%
            assert!(peak > 300.5 && peak < 300.0 * 1.04, "{dt} {peak}");
        }
    }

    #[test]
    fn tween_takes_latest_segment() {
        let mut t = Tw::default();
        t.set("a", 1.0);
        assert_eq!(t.get("a", 0.0), 1.0);
        t.add("a", 100.0, 100.0, 0.0, Ease::Lin);
        assert_eq!(t.get("a", 50.0), 1.0);
        assert!((t.get("a", 150.0) - 0.5).abs() < 1e-4);
        assert_eq!(t.get("a", 300.0), 0.0);
        assert!(t.busy(150.0) && !t.busy(300.0));
    }

    fn bmp(out: &[u8], wu: usize, hu: usize, bgc: [f32; 3]) -> Vec<u8> {
        let mut bmp = Vec::new();
        let row = (wu * 3 + 3) & !3;
        let size = 54 + row * hu;
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(size as u32).to_le_bytes());
        bmp.extend_from_slice(&[0; 4]);
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&(wu as i32).to_le_bytes());
        bmp.extend_from_slice(&(hu as i32).to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&24u16.to_le_bytes());
        bmp.extend_from_slice(&[0; 24]);
        for y in (0..hu).rev() {
            let mut r = Vec::with_capacity(row);
            for x in 0..wu {
                let p = &out[(y * wu + x) * 4..][..4];
                let k = 1.0 - p[3] as f32 / 255.0;
                for (c, bgv) in [(p[0], bgc[2]), (p[1], bgc[1]), (p[2], bgc[0])] {
                    r.push((c as f32 + bgv * 255.0 * k).round().clamp(0.0, 255.0) as u8);
                }
            }
            r.resize(row, 0);
            bmp.extend_from_slice(&r);
        }
        bmp
    }

    /// 一次粘贴的全过程按时间渲染成一列帧（手动跑：cargo test -- --ignored dump_xfer），看样子、量每帧耗时。
    #[test]
    #[ignore]
    fn dump_xfer() {
        crate::ui::load_fonts();
        let s = 2.0f32;
        // (标签, 时刻毫秒)；200MB、20MB/s：t=1000 开始报进度
        let shots: &[(&str, f32)] = &[
            ("appear", 240.0),
            ("pill", 560.0),
            ("grow1", 680.0),
            ("grow2", 760.0),
            ("card", 1400.0),
            ("eta", 4200.0),
            ("hover", 6300.0),
            ("full", 11050.0),
            ("collapse", 11330.0),
            ("done", 11900.0),
            ("leave", 13900.0),
        ];
        for (tag, bgc, kind) in [("light", [0.93, 0.94, 0.96], 0), ("dark", [0.07, 0.09, 0.14], 0), ("fail", [0.93, 0.94, 0.96], 1), ("cancel", [0.07, 0.09, 0.14], 2)] {
            let (ww, wh) = Island::win_size(s);
            let (wu, hu) = (ww as usize, wh as usize);
            let bd = Backdrop { w: wu, h: hu, blurred: vec![bgc; wu * hu], mean_lum: lum(bgc) };
            let (cx, cy) = (wu as f32 / 2.0, Island::rest_cy(hu, s));
            let (light, a) = choose_glass(&bd, cx, cy - (CARD_H - H) / 2.0 * s, CARD_W, s);
            let content = Content::empty();
            let f = Frame { bd: &bd, content: &content, light, tint_a: a, s, cx, cy };
            let mut x = Xfer::new("季度报告-2026年第三季度-最终版.pdf", 1, s);
            let total = 200u64 << 20;
            let mut sheet = Vec::new();
            let mut t = 0.0f32;
            let mut next_rep = 1000.0f32;
            let (mut glass_ms, mut over_ms, mut n) = (0.0f64, 0.0f64, 0);
            let late = [8120.0, 8400.0, 8700.0, 9200.0];
            for (i, (name, at)) in shots.iter().enumerate() {
                let at = if kind != 0 && i >= 7 { &late[i - 7] } else { at };
                // 按 120Hz 走到这一刻（中途按 250ms 报进度）
                while t < *at {
                    t += 1000.0 / 120.0;
                    x.fake = Some(t);
                    if t >= next_rep && x.end.is_none() && !x.done {
                        let read = (((t - 1000.0) / 1000.0 * 20.0 * 1048576.0) as u64).min(total);
                        // 4~6 秒卡住一下（看「等待中」）
                        if !(4000.0..6200.0).contains(&t) || kind != 0 {
                            x.report(read, total);
                        }
                        if read >= total {
                            x.finish();
                        }
                        next_rep += 250.0;
                    }
                    if kind == 1 && (t - 8000.0).abs() < 5.0 {
                        x.fail();
                    }
                    if kind == 2 && (t - 8000.0).abs() < 5.0 {
                        x.cancel();
                    }
                    if (t - 5800.0).abs() < 5.0 {
                        x.hover(true);
                    }
                    if (t - 7000.0).abs() < 5.0 {
                        x.hover(false);
                    }
                    x.update(t);
                }
                let mut out = vec![0u8; wu * hu * 4];
                let t0 = Instant::now();
                f.draw(x.pose(), &mut out);
                let t1 = Instant::now();
                x.overlay(&mut out, wu, hu, cx, cy, s, light, t);
                let t2 = Instant::now();
                glass_ms += (t1 - t0).as_secs_f64() * 1000.0;
                over_ms += (t2 - t1).as_secs_f64() * 1000.0;
                n += 1;
                eprintln!("{tag} {name} t={t:.0} phase={:?} w={:.1} h={:.1} d={:.3} eta={:?}", x.phase, x.w, x.h, x.d, x.eta.label);
                // 自底向上的 BMP：后面的帧放上面 → 倒着拼
                sheet.push(out);
            }
            eprintln!("{tag}: 玻璃 {:.2}ms/帧，水和字 {:.2}ms/帧（{wu}×{hu}）", glass_ms / n as f64, over_ms / n as f64);
            let mut all = Vec::new();
            for o in sheet.iter() {
                all.extend_from_slice(o);
            }
            let path = std::env::temp_dir().join(format!("kmlink-xfer-{tag}.bmp"));
            std::fs::write(&path, bmp(&all, wu, hu * sheet.len(), bgc)).unwrap();
        }
    }
}
