//! 剪贴板读写（Win32）。由 desktop 线程调用，hwnd 是它的隐藏窗口。
//! OpenClipboard 可能因为别的程序占用而失败：最多重试 10 次，每次间隔 10ms；仍失败则返回 Busy，由调用方稍后再试。
//! 密码管理器标记为“不要监视/不进历史/不上云”的内容不读（见 `concealed`）。
//! 写入：先 GlobalAlloc 并拷好数据，再 OpenClipboard → EmptyClipboard → SetClipboardData，
//! 分配失败不会清掉用户原来的剪贴板，剪贴板也只被占用一瞬间。写入的内容带 `CanUploadToCloudClipboard` = 0，
//! 不让 Windows 云剪贴板再传一圈（两边都开云剪贴板时会形成回路）。

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};

pub const CF_DIB: u32 = 8;
pub const CF_UNICODETEXT: u32 = 13;
pub const CF_DIBV5: u32 = 17;

/// 读剪贴板的结果。
pub enum Content {
    Text(String),
    /// format = CF_DIBV5 或 CF_DIB，data = GlobalLock 出来的原始字节（已校验过 DIB 结构）。
    Image { format: u32, data: Vec<u8> },
    /// 被标记为敏感（密码管理器等）：不同步。
    Concealed,
    /// 超过上限：不同步（文字超限时也不再退而读图片）。size = 字节数（文字为估算的下限）。
    TooBig { image: bool, size: usize },
    /// 没有可同步的内容（没有文字/图片、图片同步关闭、图片格式不对）。
    Other,
}

/// 打开期间持有；Drop 时 CloseClipboard。
struct Opened;

impl Drop for Opened {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn open(hwnd: HWND) -> Option<Opened> {
    for i in 0..10 {
        if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
            return Some(Opened);
        }
        if i < 9 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    None
}

/// GlobalLock 的守卫：Drop 时 GlobalUnlock。
struct Locked {
    h: HGLOBAL,
    p: *mut u8,
}

impl Locked {
    fn new(h: HGLOBAL) -> Option<Locked> {
        let p = unsafe { GlobalLock(h) } as *mut u8;
        if p.is_null() { None } else { Some(Locked { h, p }) }
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        unsafe {
            // 锁计数归零时也返回“失败”，忽略
            let _ = GlobalUnlock(self.h);
        }
    }
}

/// 自己分配的 HGLOBAL：交给剪贴板之前出错就 Drop 时释放；交出去后 `give` 放手（内存归系统）。
struct Owned(Option<HGLOBAL>);

impl Owned {
    /// 分配 len 字节并让 fill 填好。
    fn new(len: usize, fill: impl FnOnce(&mut [u8])) -> Option<Owned> {
        let hg = unsafe { GlobalAlloc(GMEM_MOVEABLE, len.max(1)) }.ok()?;
        let me = Owned(Some(hg));
        let lk = Locked::new(hg)?;
        fill(unsafe { std::slice::from_raw_parts_mut(lk.p, len) });
        drop(lk);
        Some(me)
    }
    /// SetClipboardData；成功则内存归系统。
    fn give(&mut self, format: u32) -> bool {
        let Some(hg) = self.0 else { return false };
        let ok = unsafe { SetClipboardData(format, Some(HANDLE(hg.0))) }.is_ok();
        if ok {
            self.0 = None;
        }
        ok
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        if let Some(hg) = self.0.take() {
            unsafe {
                let _ = GlobalFree(Some(hg));
            }
        }
    }
}

fn available(format: u32) -> bool {
    unsafe { IsClipboardFormatAvailable(format) }.is_ok()
}

/// 取某格式的 HGLOBAL 及其大小（剪贴板须已打开）。
fn get_global(format: u32) -> Option<(HGLOBAL, usize)> {
    let h = unsafe { GetClipboardData(format) }.ok()?;
    let hg = HGLOBAL(h.0);
    let size = unsafe { GlobalSize(hg) };
    Some((hg, size))
}

/// 剪贴板被别的程序占着，打不开（稍后重试）。
#[derive(Debug, PartialEq, Eq)]
pub struct Busy;

/// 写入失败的原因：Busy = 剪贴板被占（值得重试）；Failed = 分配/设置失败。
#[derive(Debug, PartialEq, Eq)]
pub enum WriteError {
    Busy,
    Failed,
}

/// 密码管理器等用来标记“别同步/别进历史/别上云”的注册格式（0 = 注册失败）。
fn conceal_formats() -> [u32; 4] {
    static F: std::sync::OnceLock<[u32; 4]> = std::sync::OnceLock::new();
    *F.get_or_init(|| unsafe {
        [
            RegisterClipboardFormatW(windows::core::w!("ExcludeClipboardContentFromMonitorProcessing")),
            RegisterClipboardFormatW(windows::core::w!("Clipboard Viewer Ignore")),
            RegisterClipboardFormatW(windows::core::w!("CanIncludeInClipboardHistory")),
            RegisterClipboardFormatW(windows::core::w!("CanUploadToCloudClipboard")),
        ]
    })
}

/// 该格式存在且内容是 DWORD 0（剪贴板须已打开）。
fn dword_zero(format: u32) -> bool {
    if format == 0 || !available(format) {
        return false;
    }
    let Some((hg, size)) = get_global(format) else { return false };
    if size < 4 {
        return false;
    }
    let Some(lk) = Locked::new(hg) else { return false };
    let v = unsafe { (lk.p as *const u32).read_unaligned() };
    v == 0
}

/// 内容被标记为敏感（剪贴板须已打开）。
/// 我们自己写入的对方内容带 CanUploadToCloudClipboard = 0，但那次更新按序号已被忽略，不会走到这里。
fn concealed() -> bool {
    let [exclude, ignore, history, cloud] = conceal_formats();
    if (exclude != 0 && available(exclude)) || (ignore != 0 && available(ignore)) {
        return true;
    }
    // CanIncludeInClipboardHistory / CanUploadToCloudClipboard：DWORD 0 = 不许进历史 / 不许上云
    dword_zero(history) || dword_zero(cloud)
}

/// 读取剪贴板：有文字就只看文字（空则看图片；超过 max_text 字节直接跳过，不再退而读图片——
/// Excel 等会为此临时渲染巨幅位图，卡住消息循环甚至弹窗）；没有文字且 want_image 时读图片。
/// 打不开剪贴板返回 Err(Busy)。
pub fn read(hwnd: HWND, max_text: usize, want_image: bool, max_image: usize) -> Result<Content, Busy> {
    let _g = open(hwnd).ok_or(Busy)?;
    if concealed() {
        return Ok(Content::Concealed);
    }
    if available(CF_UNICODETEXT) {
        match read_text(max_text) {
            TextRead::Ok(t) => return Ok(Content::Text(t)),
            TextRead::TooBig(n) => return Ok(Content::TooBig { image: false, size: n }),
            TextRead::Empty => {}
        }
    }
    if !want_image {
        return Ok(Content::Other);
    }
    for format in [CF_DIBV5, CF_DIB] {
        if available(format) {
            return Ok(read_image(format, max_image));
        }
    }
    Ok(Content::Other)
}

#[derive(Debug, PartialEq, Eq)]
enum TextRead {
    Ok(String),
    TooBig(usize),
    Empty,
}

fn read_text(max_text: usize) -> TextRead {
    let Some((hg, size)) = get_global(CF_UNICODETEXT) else { return TextRead::Empty };
    if size < 2 {
        return TextRead::Empty;
    }
    let Some(lk) = Locked::new(hg) else { return TextRead::Empty };
    let units = unsafe { std::slice::from_raw_parts(lk.p as *const u16, size / 2) };
    text_within(units, max_text)
}

/// UTF-16（到第一个 0 为止）转文字，超过 max 字节（UTF-8）则只报大小、不转换（免得为超大文字分配内存）。
fn text_within(units: &[u16], max: usize) -> TextRead {
    let n = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    // UTF-8 字节数 ≥ UTF-16 单元数：单元数已超就不用转换
    if n > max {
        return TextRead::TooBig(n);
    }
    let s = String::from_utf16_lossy(&units[..n]);
    if s.is_empty() {
        TextRead::Empty
    } else if s.len() > max {
        TextRead::TooBig(s.len())
    } else {
        TextRead::Ok(s)
    }
}

fn read_image(format: u32, max: usize) -> Content {
    let Some((hg, size)) = get_global(format) else { return Content::Other };
    if size == 0 {
        return Content::Other;
    }
    if size > max {
        return Content::TooBig { image: true, size };
    }
    let Some(lk) = Locked::new(hg) else { return Content::Other };
    let bytes = unsafe { std::slice::from_raw_parts(lk.p as *const u8, size) };
    // 结构不对的图片不发（对方也会拒收）
    if dib(bytes).is_err() {
        return Content::Other;
    }
    Content::Image { format, data: bytes.to_vec() }
}

/// DIB（CF_DIB / CF_DIBV5 的内容）的结构：头 + 掩码 + 颜色表 + 像素。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dib {
    pub width: i32,
    /// 负数 = 自上而下。
    pub height: i32,
    pub bit_count: u16,
    /// 颜色表起点（头 + 掩码之后）。
    pub palette: usize,
    /// 像素起点。
    pub bits: usize,
    /// 像素字节数（行宽 × |高|）。
    pub bits_len: usize,
}

/// 校验 DIB：biSize 为 40/52/56/108/124 之一、planes = 1、位深 1/4/8/16/24/32、宽 > 0、高 ≠ 0，
/// 只支持不压缩（BI_RGB）和位域（BI_BITFIELDS / BI_ALPHABITFIELDS，限 16/32 位），
/// 并确认 头 + 掩码 + 颜色表 + 行宽 × |高| 不超过数据长度（全程溢出检查）。
pub fn dib(data: &[u8]) -> Result<Dib, &'static str> {
    let u32_at = |o: usize| data.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let u16_at = |o: usize| data.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let hs = u32_at(0).ok_or("数据太短")? as usize;
    if !matches!(hs, 40 | 52 | 56 | 108 | 124) {
        return Err("头大小不对");
    }
    if data.len() < hs {
        return Err("头不完整");
    }
    // 头已完整（≥ 40 字节），以下读取都在范围内
    let width = u32_at(4).ok_or("头不完整")? as i32;
    let height = u32_at(8).ok_or("头不完整")? as i32;
    let planes = u16_at(12).ok_or("头不完整")?;
    let bit_count = u16_at(14).ok_or("头不完整")?;
    let compression = u32_at(16).ok_or("头不完整")?;
    let clr_used = u32_at(32).ok_or("头不完整")?;
    if planes != 1 {
        return Err("planes 不是 1");
    }
    if !matches!(bit_count, 1 | 4 | 8 | 16 | 24 | 32) {
        return Err("位深不对");
    }
    if width <= 0 || height == 0 {
        return Err("宽高不对");
    }
    // BI_RGB = 0，BI_BITFIELDS = 3，BI_ALPHABITFIELDS = 6；只有 40 字节的旧头把掩码放在头后面
    let masks: u64 = match compression {
        0 => 0,
        3 | 6 => {
            if bit_count != 16 && bit_count != 32 {
                return Err("位域只能是 16/32 位");
            }
            match (hs, compression) {
                (40, 3) => 12,
                (40, _) => 16,
                _ => 0,
            }
        }
        _ => return Err("不支持的压缩方式"),
    };
    let colors: u64 = if clr_used != 0 {
        clr_used as u64
    } else if bit_count <= 8 {
        1 << bit_count
    } else {
        0
    };
    let stride = (width as u64 * bit_count as u64).div_ceil(32) * 4;
    let bits_len = stride.checked_mul(height.unsigned_abs() as u64).ok_or("尺寸溢出")?;
    let palette = hs as u64 + masks;
    let bits = palette.checked_add(colors * 4).ok_or("尺寸溢出")?;
    let need = bits.checked_add(bits_len).ok_or("尺寸溢出")?;
    if need > data.len() as u64 {
        return Err("数据不完整");
    }
    // need ≤ data.len()，各值都放得进 usize
    Ok(Dib { width, height, bit_count, palette: palette as usize, bits: bits as usize, bits_len: bits_len as usize })
}

/// UTF-16 读到第一个 0 为止（没有 0 就整段）。
pub(crate) fn utf16_until_nul(units: &[u16]) -> String {
    let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// 文字 → CF_UNICODETEXT 的字节（UTF-16 LE + 结尾 0）。
#[cfg(test)]
pub(crate) fn text_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16().chain(std::iter::once(0)).flat_map(|u| u.to_le_bytes()).collect()
}

/// 文字 → UTF-16 LE 字节（不带结尾 0）。大段文字走压缩通道时用。
pub(crate) fn utf16le(text: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(text.len() * 2);
    v.extend(text.encode_utf16().flat_map(|u| u.to_le_bytes()));
    v
}

/// UTF-16 LE 字节 → 文字（读到第一个 0 为止；奇数长度忽略最后一个字节）。不另建 u16 副本。
pub(crate) fn from_utf16le(b: &[u8]) -> String {
    let units = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0);
    char::decode_utf16(units).map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
}

/// “不许上云剪贴板”格式（0 = 注册失败）。
fn cloud_format() -> u32 {
    conceal_formats()[3]
}

/// 写入一个格式：先分配并填好，再打开剪贴板、清空、设置；另带 CanUploadToCloudClipboard = 0。
/// 返回写入后的 GetClipboardSequenceNumber()。
fn write_global(hwnd: HWND, format: u32, len: usize, fill: impl FnOnce(&mut [u8])) -> Result<u32, WriteError> {
    let mut main = Owned::new(len, fill).ok_or(WriteError::Failed)?;
    let cloud = cloud_format();
    let mut mark = if cloud != 0 { Owned::new(4, |b| b.copy_from_slice(&0u32.to_le_bytes())) } else { None };
    let g = open(hwnd).ok_or(WriteError::Busy)?;
    unsafe { EmptyClipboard() }.map_err(|_| WriteError::Failed)?;
    if !main.give(format) {
        return Err(WriteError::Failed);
    }
    if let Some(m) = mark.as_mut() {
        // 失败也无妨：内容已写好，只是少了“不上云”标记
        m.give(cloud);
    }
    drop(g);
    Ok(seq())
}

/// 写入文字（CF_UNICODETEXT，直接编码进 GlobalAlloc 的内存，不另建副本）。
pub fn write_text(hwnd: HWND, text: &str) -> Result<u32, WriteError> {
    let units = text.encode_utf16().count() + 1;
    write_global(hwnd, CF_UNICODETEXT, units * 2, |dst| {
        let mut i = 0;
        for u in text.encode_utf16().chain(std::iter::once(0)) {
            dst[i..i + 2].copy_from_slice(&u.to_le_bytes());
            i += 2;
        }
    })
}

/// 写入图片（调用方应已用 `dib` 校验过）。
pub fn write_image(hwnd: HWND, format: u32, data: &[u8]) -> Result<u32, WriteError> {
    if data.is_empty() || (format != CF_DIB && format != CF_DIBV5) {
        return Err(WriteError::Failed);
    }
    write_global(hwnd, format, data.len(), |dst| dst.copy_from_slice(data))
}

pub fn seq() -> u32 {
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 拼一个 DIB：hs 字节的头 + 掩码/颜色表 + 像素。
    pub(crate) fn make_dib(hs: u32, w: i32, h: i32, bits: u16, comp: u32, clr_used: u32, extra: usize, pixels: usize) -> Vec<u8> {
        let mut v = vec![0u8; hs as usize];
        v[0..4].copy_from_slice(&hs.to_le_bytes());
        v[4..8].copy_from_slice(&w.to_le_bytes());
        v[8..12].copy_from_slice(&h.to_le_bytes());
        v[12..14].copy_from_slice(&1u16.to_le_bytes());
        v[14..16].copy_from_slice(&bits.to_le_bytes());
        v[16..20].copy_from_slice(&comp.to_le_bytes());
        v[32..36].copy_from_slice(&clr_used.to_le_bytes());
        v.extend(std::iter::repeat_n(0u8, extra));
        v.extend((0..pixels).map(|i| i as u8));
        v
    }

    #[test]
    fn dib_valid() {
        // 2x2 32 位 BI_RGB：行宽 8，像素 16 字节
        let d = make_dib(40, 2, 2, 32, 0, 0, 0, 16);
        assert_eq!(dib(&d), Ok(Dib { width: 2, height: 2, bit_count: 32, palette: 40, bits: 40, bits_len: 16 }));
        // 自上而下（负高）
        assert_eq!(dib(&make_dib(40, 2, -2, 32, 0, 0, 0, 16)).unwrap().bits_len, 16);
        // 24 位 3 像素宽：9 字节 → 行宽 12
        assert_eq!(dib(&make_dib(40, 3, 1, 24, 0, 0, 0, 12)).unwrap().bits_len, 12);
        // 1 位：调色板 2 色 × 4 字节；宽 1 → 行宽 4
        let one = dib(&make_dib(40, 1, 1, 1, 0, 0, 8, 4)).unwrap();
        assert_eq!((one.palette, one.bits, one.bits_len), (40, 48, 4));
        // 8 位、clrUsed = 3
        assert_eq!(dib(&make_dib(40, 4, 1, 8, 0, 3, 12, 4)).unwrap().bits, 52);
        // 40 字节头 + BI_BITFIELDS：头后 12 字节掩码
        assert_eq!(dib(&make_dib(40, 1, 1, 32, 3, 0, 12, 4)).unwrap().bits, 52);
        // BI_ALPHABITFIELDS：16 字节
        assert_eq!(dib(&make_dib(40, 1, 1, 32, 6, 0, 16, 4)).unwrap().bits, 56);
        // V5 头 + BI_BITFIELDS：掩码在头里
        let v5 = make_dib(124, 2, 2, 32, 3, 0, 0, 16);
        assert_eq!(dib(&v5).unwrap().bits, 124);
        for hs in [52, 56, 108] {
            assert!(dib(&make_dib(hs, 2, 2, 32, 0, 0, 0, 16)).is_ok());
        }
        // 尾部多出的数据（比如 V5 的颜色配置文件）不算错
        assert!(dib(&make_dib(124, 2, 2, 32, 3, 0, 0, 100)).is_ok());
    }

    #[test]
    fn dib_invalid() {
        assert!(dib(&[]).is_err());
        assert!(dib(&[40, 0, 0]).is_err());
        assert!(dib(&make_dib(64, 2, 2, 32, 0, 0, 0, 16)).is_err()); // 头大小
        let mut d = make_dib(40, 2, 2, 32, 0, 0, 0, 16);
        d[12] = 2; // planes
        assert!(dib(&d).is_err());
        assert!(dib(&make_dib(40, 2, 2, 12, 0, 0, 0, 16)).is_err()); // 位深
        assert!(dib(&make_dib(40, 0, 2, 32, 0, 0, 0, 16)).is_err()); // 宽 0
        assert!(dib(&make_dib(40, -2, 2, 32, 0, 0, 0, 16)).is_err()); // 宽负
        assert!(dib(&make_dib(40, 2, 0, 32, 0, 0, 0, 16)).is_err()); // 高 0
        assert!(dib(&make_dib(40, 2, 2, 32, 0, 0, 0, 15)).is_err()); // 像素少 1 字节
        assert!(dib(&make_dib(40, 1, 1, 32, 3, 0, 0, 4)).is_err()); // 缺掩码
        assert!(dib(&make_dib(40, 1, 1, 24, 3, 0, 12, 4)).is_err()); // 位域配 24 位
        assert!(dib(&make_dib(40, 4, 1, 8, 1, 0, 1024, 4)).is_err()); // RLE8
        assert!(dib(&make_dib(40, 4, 1, 8, 0, 0, 1000, 4)).is_err()); // 调色板不全
        // 头声明 124 字节但数据只有 40
        let mut short = make_dib(40, 1, 1, 32, 0, 0, 0, 4);
        short[0] = 124;
        assert!(dib(&short).is_err());
        // 溢出：巨大的宽高 / clrUsed
        assert!(dib(&make_dib(40, i32::MAX, i32::MIN, 32, 0, 0, 0, 16)).is_err());
        assert!(dib(&make_dib(40, i32::MAX, i32::MAX, 32, 0, 0, 0, 16)).is_err());
        assert!(dib(&make_dib(40, 1, 1, 32, 0, u32::MAX, 0, 16)).is_err());
    }

    #[test]
    fn text_limits() {
        let u = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        assert_eq!(text_within(&u("abc"), 3), TextRead::Ok("abc".into()));
        assert_eq!(text_within(&u("abcd"), 3), TextRead::TooBig(4));
        // 中文：2 个单元、6 个 UTF-8 字节
        assert_eq!(text_within(&u("中文"), 5), TextRead::TooBig(6));
        assert_eq!(text_within(&u("中文"), 6), TextRead::Ok("中文".into()));
        // 读到 0 为止：后面的垃圾不算
        let mut v = u("ab");
        v.push(0);
        v.extend(std::iter::repeat_n(65u16, 1000));
        assert_eq!(text_within(&v, 2), TextRead::Ok("ab".into()));
        assert_eq!(text_within(&[0, 65], 10), TextRead::Empty);
        assert_eq!(text_within(&[], 10), TextRead::Empty);
    }

    #[test]
    fn utf16_nul() {
        let v: Vec<u16> = "你好a".encode_utf16().chain([0, 65, 66]).collect();
        assert_eq!(utf16_until_nul(&v), "你好a");
        let w: Vec<u16> = "abc".encode_utf16().collect();
        assert_eq!(utf16_until_nul(&w), "abc");
        assert_eq!(utf16_until_nul(&[]), "");
    }

    #[test]
    fn text_bytes_roundtrip() {
        let b = text_bytes("剪贴板 ok");
        assert_eq!(b.len() % 2, 0);
        assert_eq!(&b[b.len() - 2..], &[0, 0]);
        let u: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(utf16_until_nul(&u), "剪贴板 ok");
    }

    #[test]
    fn utf16le_roundtrip() {
        let t = "大段文字 😀 abc";
        let b = utf16le(t);
        assert_eq!(b.len(), t.encode_utf16().count() * 2);
        assert_eq!(from_utf16le(&b), t);
        assert_eq!(&text_bytes(t)[..b.len()], &b[..]);
        let mut odd = b.clone();
        odd.push(0x41);
        assert_eq!(from_utf16le(&odd), t);
        assert_eq!(from_utf16le(&[]), "");
        // 读到 0 为止；孤立代理项替换为 U+FFFD
        assert_eq!(from_utf16le(&[0x41, 0, 0, 0, 0x42, 0]), "A");
        assert_eq!(from_utf16le(&[0x00, 0xD8, 0x41, 0]), "\u{FFFD}A");
    }
}
