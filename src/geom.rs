//! 屏幕几何：显示器矩形、两台电脑的相对排布、跨边映射。全部是纯函数，单位都是各自电脑的物理像素。
//!
//! 排布模型（Layout）：从“本机”看，对方的整个桌面（所有显示器的外接矩形）按 `scale` 缩放到本机像素后，
//! 贴在本机外接矩形的 `side` 一侧；`offset` 是沿这条边的错开量：
//!   side = Left/Right：offset = 对方矩形上边 - 本机矩形上边（本机像素）
//!   side = Top/Bottom：offset = 对方矩形左边 - 本机矩形左边（本机像素）
//! 对方电脑保存的是镜像后的同一个排布（见 `Layout::mirrored`）。

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// 半开区间矩形：[l, r) × [t, b)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

impl Rect {
    pub const fn new(l: i32, t: i32, r: i32, b: i32) -> Self {
        Self { l, t, r, b }
    }
    /// 宽度（退化矩形为 0；用 i64 算，对方发来的极端坐标也不会溢出）。
    pub fn w(&self) -> i32 {
        (self.r as i64 - self.l as i64).clamp(0, i32::MAX as i64) as i32
    }
    pub fn h(&self) -> i32 {
        (self.b as i64 - self.t as i64).clamp(0, i32::MAX as i64) as i32
    }
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.l && p.x < self.r && p.y >= self.t && p.y < self.b
    }
    /// 钳进矩形。退化矩形（r <= l 或 b <= t）不 panic：钳到 l / t。
    pub fn clamp(&self, p: Point) -> Point {
        let r = self.l.max(self.r.saturating_sub(1));
        let b = self.t.max(self.b.saturating_sub(1));
        Point::new(p.x.clamp(self.l, r), p.y.clamp(self.t, b))
    }
    pub fn center(&self) -> Point {
        Point::new((self.l as i64 + self.w() as i64 / 2) as i32, (self.t as i64 + self.h() as i64 / 2) as i32)
    }
    fn dist2(&self, p: Point) -> i64 {
        let c = self.clamp(p);
        let dx = c.x as i64 - p.x as i64;
        let dy = c.y as i64 - p.y as i64;
        dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    #[default]
    Right,
    Top,
    Bottom,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }
    pub fn horizontal(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }
    pub fn label_zh(self) -> &'static str {
        match self {
            Side::Left => "左侧",
            Side::Right => "右侧",
            Side::Top => "上方",
            Side::Bottom => "下方",
        }
    }
}

/// 一台电脑的桌面：若干显示器（物理像素，虚拟桌面坐标）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct Desk {
    pub monitors: Vec<Rect>,
}

impl Desk {
    pub fn new(monitors: Vec<Rect>) -> Self {
        Self { monitors }
    }

    pub fn is_empty(&self) -> bool {
        self.monitors.is_empty()
    }

    /// 所有显示器的外接矩形。
    pub fn bounds(&self) -> Rect {
        let mut it = self.monitors.iter();
        let Some(f) = it.next() else {
            return Rect::new(0, 0, 1, 1);
        };
        it.fold(*f, |a, m| Rect::new(a.l.min(m.l), a.t.min(m.t), a.r.max(m.r), a.b.max(m.b)))
    }

    pub fn contains(&self, p: Point) -> bool {
        self.monitors.iter().any(|m| m.contains(p))
    }

    /// 离 p 最近的、落在某块显示器里的点（显示器之间的空洞不能停留）。不知道光标原来在哪时用。
    pub fn clamp(&self, p: Point) -> Point {
        if self.contains(p) || self.monitors.is_empty() {
            return p;
        }
        let m = self.monitors.iter().min_by_key(|m| m.dist2(p)).unwrap();
        m.clamp(p)
    }

    /// 光标从 from 移向 p：p 落在某块显示器里就是 p；否则钳在 from 所在的那块显示器里
    /// （L 形多显示器时不会跳到“离 p 最近”的另一块屏上）。from 不在任何显示器上时退回 clamp。
    pub fn clamp_from(&self, from: Point, p: Point) -> Point {
        if self.contains(p) || self.monitors.is_empty() {
            return p;
        }
        match self.monitors.iter().find(|m| m.contains(from)) {
            Some(m) => m.clamp(p),
            None => self.clamp(p),
        }
    }

    /// p 是否在 `side` 方向的“外边缘”上：p 在某块显示器的那条边上，且再往外一个像素已不在任何显示器里。
    pub fn on_outer_edge(&self, side: Side, p: Point) -> bool {
        let Some(m) = self.monitors.iter().find(|m| m.contains(p)) else {
            return false;
        };
        let (at_edge, beyond) = match side {
            Side::Left => (p.x <= m.l, Point::new(p.x.saturating_sub(1), p.y)),
            Side::Right => (p.x >= m.r.saturating_sub(1), Point::new(p.x.saturating_add(1), p.y)),
            Side::Top => (p.y <= m.t, Point::new(p.x, p.y.saturating_sub(1))),
            Side::Bottom => (p.y >= m.b.saturating_sub(1), Point::new(p.x, p.y.saturating_add(1))),
        };
        at_edge && !self.contains(beyond)
    }

    /// 主显示器（包含原点的那块）中心，用作驱动对方时本机光标的停放点。
    pub fn park_point(&self) -> Point {
        self.monitors
            .iter()
            .find(|m| m.contains(Point::new(0, 0)))
            .or(self.monitors.first())
            .map(|m| m.center())
            .unwrap_or(Point::new(0, 0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// 对方在本机的哪一侧。
    pub side: Side,
    /// 沿边错开量（本机像素），定义见模块注释。
    pub offset: i32,
    /// 对方像素 → 本机像素 的缩放（两边 DPI 差别大时可调；默认 1）。
    pub scale: f64,
}

impl Default for Layout {
    fn default() -> Self {
        Self { side: Side::Right, offset: 0, scale: 1.0 }
    }
}

/// 一维区间映射所需的数据：本机边上的坐标 a（本机像素）↔ 对方边上的坐标 b（对方像素）。
impl Layout {
    fn scale(&self) -> f64 {
        if self.scale.is_finite() && self.scale > 0.01 { self.scale } else { 1.0 }
    }

    /// 对方电脑看到的同一个排布。
    pub fn mirrored(&self) -> Layout {
        let s = self.scale();
        Layout { side: self.side.opposite(), offset: (-(self.offset as f64) / s).round() as i32, scale: 1.0 / s }
    }

    /// 对方外接矩形放到本机坐标系里的位置（本机像素，浮点）。返回 (l, t, r, b)。
    pub fn peer_rect_in_mine(&self, mine: &Desk, peer: &Desk) -> (f64, f64, f64, f64) {
        let me = mine.bounds();
        let pb = peer.bounds();
        let s = self.scale();
        let (w, h) = (pb.w() as f64 * s, pb.h() as f64 * s);
        let o = self.offset as f64;
        match self.side {
            Side::Right => (me.r as f64, me.t as f64 + o, me.r as f64 + w, me.t as f64 + o + h),
            Side::Left => (me.l as f64 - w, me.t as f64 + o, me.l as f64, me.t as f64 + o + h),
            Side::Bottom => (me.l as f64 + o, me.b as f64, me.l as f64 + o + w, me.b as f64 + h),
            Side::Top => (me.l as f64 + o, me.t as f64 - h, me.l as f64 + o + w, me.t as f64),
        }
    }

    /// 本机光标在外边缘 p 处越界：算出它在对方桌面上的入口点。
    /// 不在两边接触的区段内（对方那一段没有屏幕）时返回 None。
    pub fn exit_to_peer(&self, mine: &Desk, peer: &Desk, p: Point) -> Option<Point> {
        if mine.is_empty() || peer.is_empty() || !mine.on_outer_edge(self.side, p) {
            return None;
        }
        let me = mine.bounds();
        let pb = peer.bounds();
        let s = self.scale();
        let o = self.offset as f64;
        // 沿边坐标：本机像素 → 对方像素
        let (along_mine, start_mine) = if self.side.horizontal() { (p.y as f64, me.t as f64 + o) } else { (p.x as f64, me.l as f64 + o) };
        let along_peer_rel = (along_mine - start_mine) / s;
        let (len_peer, base) = if self.side.horizontal() { (pb.h(), pb.t) } else { (pb.w(), pb.l) };
        if along_peer_rel < 0.0 || along_peer_rel >= len_peer as f64 {
            return None;
        }
        let a = base.saturating_add(along_peer_rel.floor() as i32);
        // 进入对方桌面：贴着对方朝向本机的那条边
        let probe = match self.side {
            Side::Right => Point::new(pb.l, a),
            Side::Left => Point::new(pb.r.saturating_sub(1), a),
            Side::Bottom => Point::new(a, pb.t),
            Side::Top => Point::new(a, pb.b.saturating_sub(1)),
        };
        // 那一行/列上对方最靠近本机的显示器边缘
        let entry = edge_point_facing(peer, self.side.opposite(), probe)?;
        Some(entry)
    }

    /// 对方桌面上的虚拟光标想从 p 继续往本机方向移动（已在对方朝向本机的外边缘）：算出回到本机的落点。
    /// 本机那一段没有屏幕时返回 None（挡住，不回来）。
    pub fn return_to_me(&self, mine: &Desk, peer: &Desk, p: Point) -> Option<Point> {
        if mine.is_empty() || peer.is_empty() || !peer.on_outer_edge(self.side.opposite(), p) {
            return None;
        }
        let me = mine.bounds();
        let pb = peer.bounds();
        let s = self.scale();
        let o = self.offset as f64;
        let along_peer = if self.side.horizontal() { (p.y - pb.t) as f64 } else { (p.x - pb.l) as f64 };
        let along_mine = along_peer * s + o + if self.side.horizontal() { me.t as f64 } else { me.l as f64 };
        let a = along_mine.floor() as i32;
        let probe = match self.side {
            Side::Right => Point::new(me.r.saturating_sub(1), a),
            Side::Left => Point::new(me.l, a),
            Side::Bottom => Point::new(a, me.b.saturating_sub(1)),
            Side::Top => Point::new(a, me.t),
        };
        edge_point_facing(mine, self.side, probe)
    }
}

/// 在 `desk` 上，沿着 `probe` 所在的行（side 为左右时）或列（上下时），找 `side` 方向最外侧显示器的边缘点。
/// 例如 side=Right：在 y=probe.y 这一行上，取所有覆盖这一行的显示器中 r 最大的那块，返回 (r-1, y)。
fn edge_point_facing(desk: &Desk, side: Side, probe: Point) -> Option<Point> {
    let cands = desk.monitors.iter().filter(|m| if side.horizontal() { probe.y >= m.t && probe.y < m.b } else { probe.x >= m.l && probe.x < m.r });
    match side {
        Side::Right => cands.max_by_key(|m| m.r).map(|m| Point::new(m.r.saturating_sub(1).max(m.l), probe.y)),
        Side::Left => cands.min_by_key(|m| m.l).map(|m| Point::new(m.l, probe.y)),
        Side::Bottom => cands.max_by_key(|m| m.b).map(|m| Point::new(probe.x, m.b.saturating_sub(1).max(m.t))),
        Side::Top => cands.min_by_key(|m| m.t).map(|m| Point::new(probe.x, m.t)),
    }
}

/// 对方桌面上的虚拟光标：控制端在本地维护，不等网络往返就能判断“回来”。
#[derive(Clone, Debug)]
pub struct VirtualCursor {
    pub pos: Point,
    rem_x: f64,
    rem_y: f64,
}

pub enum Step {
    /// 光标仍在对方桌面上，新位置。
    Moved(Point),
    /// 越过朝向本机的边：回到本机这个点。
    Return(Point),
}

impl VirtualCursor {
    pub fn new(pos: Point) -> Self {
        Self { pos, rem_x: 0.0, rem_y: 0.0 }
    }

    /// 应用一次相对移动（本机像素 × speed）。
    pub fn step(&mut self, dx: i32, dy: i32, speed: f64, layout: &Layout, mine: &Desk, peer: &Desk) -> Step {
        let fx = dx as f64 * speed + self.rem_x;
        let fy = dy as f64 * speed + self.rem_y;
        let ix = fx.trunc();
        let iy = fy.trunc();
        self.rem_x = fx - ix;
        self.rem_y = fy - iy;
        let want = Point::new(self.pos.x.saturating_add(ix as i32), self.pos.y.saturating_add(iy as i32));
        // 往本机方向推出对方的外边缘？
        let toward_me = match layout.side {
            Side::Right => want.x < self.pos.x,
            Side::Left => want.x > self.pos.x,
            Side::Bottom => want.y < self.pos.y,
            Side::Top => want.y > self.pos.y,
        };
        // 先钳在当前所在的显示器里，确实落进另一块显示器才换过去
        let clamped = peer.clamp_from(self.pos, want);
        if toward_me && clamped != want {
            // 用“刚好贴边”的那个点判断回程（沿边方向保留移动）
            let edge = match layout.side {
                Side::Right | Side::Left => peer.clamp_from(self.pos, Point::new(want.x, clamped.y)),
                _ => peer.clamp_from(self.pos, Point::new(clamped.x, want.y)),
            };
            if let Some(back) = layout.return_to_me(mine, peer, edge) {
                self.rem_x = 0.0;
                self.rem_y = 0.0;
                return Step::Return(back);
            }
        }
        self.pos = clamped;
        Step::Moved(clamped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(w: i32, h: i32) -> Desk {
        Desk::new(vec![Rect::new(0, 0, w, h)])
    }

    #[test]
    fn mirror_roundtrip() {
        let l = Layout { side: Side::Right, offset: 120, scale: 2.0 };
        let m = l.mirrored();
        assert_eq!(m.side, Side::Left);
        assert_eq!(m.offset, -60);
        assert!((m.scale - 0.5).abs() < 1e-9);
        let back = m.mirrored();
        assert_eq!(back.side, Side::Right);
        assert_eq!(back.offset, 120);
    }

    #[test]
    fn cross_right_and_back_same_size() {
        let a = one(1920, 1080);
        let b = one(1920, 1080);
        let l = Layout::default();
        // 不在边上：不跨
        assert_eq!(l.exit_to_peer(&a, &b, Point::new(1000, 500)), None);
        // 右边缘
        let e = l.exit_to_peer(&a, &b, Point::new(1919, 500)).unwrap();
        assert_eq!(e, Point::new(0, 500));
        // 对方左边缘往回
        let r = l.return_to_me(&a, &b, Point::new(0, 500)).unwrap();
        assert_eq!(r, Point::new(1919, 500));
    }

    #[test]
    fn offset_limits_zone() {
        let a = one(1920, 1080);
        let b = one(1920, 1080);
        // 对方往下错开 500：本机 y<500 的那段边没有对方屏幕
        let l = Layout { side: Side::Right, offset: 500, scale: 1.0 };
        assert_eq!(l.exit_to_peer(&a, &b, Point::new(1919, 100)), None);
        assert_eq!(l.exit_to_peer(&a, &b, Point::new(1919, 600)), Some(Point::new(0, 100)));
        // 对方 y=700 回来 → 本机 y=1200 超出本机屏幕：挡住
        assert_eq!(l.return_to_me(&a, &b, Point::new(0, 700)), None);
        assert_eq!(l.return_to_me(&a, &b, Point::new(0, 300)), Some(Point::new(1919, 800)));
    }

    #[test]
    fn scale_maps_along() {
        let a = one(1920, 1080);
        let b = one(3840, 2160);
        // 对方 4K 按 0.5 缩放后正好和本机一样高
        let l = Layout { side: Side::Left, offset: 0, scale: 0.5 };
        assert_eq!(l.exit_to_peer(&a, &b, Point::new(0, 540)), Some(Point::new(3839, 1080)));
        assert_eq!(l.return_to_me(&a, &b, Point::new(3839, 1080)), Some(Point::new(0, 540)));
    }

    #[test]
    fn multi_monitor_inner_edge_does_not_cross() {
        // 本机两块屏左右并排
        let a = Desk::new(vec![Rect::new(0, 0, 1920, 1080), Rect::new(1920, 0, 3840, 1080)]);
        let b = one(1920, 1080);
        let l = Layout::default();
        assert_eq!(l.exit_to_peer(&a, &b, Point::new(1919, 500)), None); // 内侧边
        assert!(l.exit_to_peer(&a, &b, Point::new(3839, 500)).is_some());
    }

    #[test]
    fn uneven_monitors_clamp() {
        // L 形：右下角没有屏幕
        let d = Desk::new(vec![Rect::new(0, 0, 1920, 1080), Rect::new(1920, 0, 3840, 540)]);
        assert_eq!(d.clamp(Point::new(3000, 900)), Point::new(3000, 539));
        assert!(d.on_outer_edge(Side::Right, Point::new(1919, 800))); // 那段右边外面没屏幕
        assert!(!d.on_outer_edge(Side::Right, Point::new(1919, 300)));
    }

    #[test]
    fn l_shape_clamp_stays_on_current_monitor() {
        // 左边一块 1920x1080，右边一块只有上半截（右下角是空洞）
        let d = Desk::new(vec![Rect::new(0, 0, 1920, 1080), Rect::new(1920, 0, 3840, 540)]);
        // 在右边那块往下推进空洞：贴着它的下边，不跳到左边那块（旧逻辑会跳到“最近的” (1919, 700)）
        assert_eq!(d.clamp_from(Point::new(1930, 530), Point::new(1930, 700)), Point::new(1930, 539));
        assert_eq!(d.clamp(Point::new(1930, 700)), Point::new(1919, 700));
        // 在左边那块往右推进空洞：贴着左边那块的右边
        assert_eq!(d.clamp_from(Point::new(1900, 800), Point::new(1950, 820)), Point::new(1919, 820));
        // 目标确实落在另一块显示器里：换过去
        assert_eq!(d.clamp_from(Point::new(1900, 300), Point::new(1950, 310)), Point::new(1950, 310));
        // from 不在任何显示器上：退回最近的
        assert_eq!(d.clamp_from(Point::new(5000, 5000), Point::new(3000, 900)), Point::new(3000, 539));

        // 对方是 L 形：虚拟光标在右边那块里往下推，停在那块的下边
        let a = one(1920, 1080);
        let l = Layout::default();
        let mut vc = VirtualCursor::new(Point::new(1930, 530));
        match vc.step(0, 100, 1.0, &l, &a, &d) {
            Step::Moved(p) => assert_eq!(p, Point::new(1930, 539)),
            _ => panic!(),
        }
    }

    #[test]
    fn degenerate_and_extreme_rects_do_not_panic() {
        let r = Rect::new(100, 100, 100, 50); // r <= l、b <= t
        assert_eq!(r.w(), 0);
        assert_eq!(r.h(), 0);
        assert_eq!(r.clamp(Point::new(500, -500)), Point::new(100, 100));
        let _ = r.center();
        let r = Rect::new(i32::MIN, i32::MIN, i32::MAX, i32::MAX);
        assert_eq!(r.w(), i32::MAX);
        let _ = r.center();
        let _ = r.dist2(Point::new(i32::MIN, i32::MAX));
        let d = Desk::new(vec![Rect::new(i32::MAX - 10, 0, i32::MAX, 10), Rect::new(5, 5, 5, 5)]);
        let _ = d.clamp(Point::new(i32::MIN, i32::MIN));
        let _ = d.park_point();
        assert!(!d.on_outer_edge(Side::Right, Point::new(i32::MAX, 0)));
        let a = one(1920, 1080);
        let l = Layout { side: Side::Left, offset: i32::MIN, scale: 1e9 };
        let _ = l.exit_to_peer(&a, &d, Point::new(0, 500));
        let _ = l.return_to_me(&a, &d, Point::new(i32::MAX - 10, 3));
        let mut vc = VirtualCursor::new(Point::new(i32::MAX - 5, 3));
        let _ = vc.step(i32::MAX, i32::MIN, 20.0, &l, &a, &d);
    }

    #[test]
    fn virtual_cursor_returns() {
        let a = one(1920, 1080);
        let b = one(1920, 1080);
        let l = Layout::default();
        let mut vc = VirtualCursor::new(Point::new(5, 500));
        match vc.step(-3, 0, 1.0, &l, &a, &b) {
            Step::Moved(p) => assert_eq!(p, Point::new(2, 500)),
            _ => panic!(),
        }
        match vc.step(-10, 4, 1.0, &l, &a, &b) {
            Step::Return(p) => assert_eq!(p, Point::new(1919, 504)),
            _ => panic!("should return"),
        }
    }

    #[test]
    fn virtual_cursor_blocked_outside_zone() {
        let a = one(1920, 1080);
        let b = one(1920, 2160);
        let l = Layout { side: Side::Right, offset: 0, scale: 1.0 };
        // 对方 y=1500 处往左推：本机没有 y=1500，挡住
        let mut vc = VirtualCursor::new(Point::new(0, 1500));
        match vc.step(-10, 0, 1.0, &l, &a, &b) {
            Step::Moved(p) => assert_eq!(p, Point::new(0, 1500)),
            _ => panic!("should be blocked"),
        }
    }

    #[test]
    fn speed_keeps_remainder() {
        let a = one(1920, 1080);
        let b = one(1920, 1080);
        let l = Layout::default();
        let mut vc = VirtualCursor::new(Point::new(100, 100));
        for _ in 0..4 {
            vc.step(1, 0, 0.5, &l, &a, &b);
        }
        assert_eq!(vc.pos, Point::new(102, 100));
    }
}
