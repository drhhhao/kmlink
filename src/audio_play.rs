//! 放对方的声音：WASAPI 共享模式输出流。
//!
//! - 激活 DEVINTERFACE_AUDIO_RENDER（“默认输出设备”这个虚拟设备）：用户换了默认设备（插耳机）系统自动把流迁过去。
//! - 格式同采集端：48 kHz / 2 声道 / f32，AUTOCONVERTPCM + SRC_DEFAULT_QUALITY 由系统转换到设备格式。
//! - RATEADJUST：拿到 IAudioClockAdjustment，按抗抖动缓冲的深度微调播放速率（两台声卡时钟不一样快）。
//!   设备不支持就退回不带它（速率不调，靠缓冲丢包/欠载兜底）。
//! - 缓冲 40ms，每次补到 FILL_MS；事件驱动（设备周期 ~10ms 一次）。
//! - 音量合成器里这个流显示成“<对方名字> 的声音”，可以单独调。

use crate::audio_cap::{self, RATE};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_RATEADJUST, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient, IAudioClockAdjustment,
    IAudioRenderClient, IAudioSessionControl,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::core::PCWSTR;

/// WASAPI 缓冲：40ms。
const BUFFER_HNS: i64 = 400_000;
/// 每次补到设备里攒着这么多（毫秒）。
const FILL_MS: u32 = 30;

/// DEVINTERFACE_AUDIO_RENDER 的字符串形式。
const DEFAULT_RENDER: &str = "{E6327CAD-DCEC-4949-AE8A-991E976A79D2}";

pub struct Render {
    client: IAudioClient,
    rc: IAudioRenderClient,
    clock: Option<IAudioClockAdjustment>,
    pub event: HANDLE,
    frames: u32,
    rate: f32,
}

unsafe impl Send for Render {}

fn init(client: &IAudioClient, rate_adjust: bool) -> windows::core::Result<()> {
    let fmt = audio_cap::format();
    let mut flags =
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    if rate_adjust {
        flags |= AUDCLNT_STREAMFLAGS_RATEADJUST;
    }
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_HNS, 0, &fmt, None) }
}

impl Render {
    /// name = 音量合成器里显示的名字。
    pub fn open(name: &str) -> windows::core::Result<Render> {
        let path = crate::sys::wide(DEFAULT_RENDER);
        let act = || audio_cap::activate(PCWSTR(path.as_ptr()), None);
        let mut client = act()?;
        let mut rate_ok = true;
        if let Err(e) = init(&client, true) {
            crate::log!("声音：输出设备不支持速率微调（{e}），改用固定速率");
            rate_ok = false;
            client = act()?;
            init(&client, false)?;
        }
        unsafe {
            let event = CreateEventW(None, false, false, None)?;
            let r = (|| {
                client.SetEventHandle(event)?;
                let rc: IAudioRenderClient = client.GetService()?;
                let clock: Option<IAudioClockAdjustment> = if rate_ok { client.GetService().ok() } else { None };
                if let Ok(s) = client.GetService::<IAudioSessionControl>() {
                    let n = crate::sys::wide(name);
                    let _ = s.SetDisplayName(PCWSTR(n.as_ptr()), std::ptr::null());
                }
                let frames = client.GetBufferSize()?;
                Ok((rc, clock, frames))
            })();
            match r {
                Ok((rc, clock, frames)) => {
                    let mut me = Render { client, rc, clock, event, frames, rate: RATE as f32 };
                    // 先垫一段静音再启动，不然一启动就欠载
                    me.write(|o| o.fill(0.0))?;
                    me.client.Start()?;
                    Ok(me)
                }
                Err(e) => {
                    let _ = CloseHandle(event);
                    Err(e)
                }
            }
        }
    }

    pub fn rate_adjust(&self) -> bool {
        self.clock.is_some()
    }

    /// 补数据：f 拿到要填的交错样本（可能为空）。
    pub fn write(&mut self, f: impl FnOnce(&mut [f32])) -> windows::core::Result<usize> {
        unsafe {
            let pad = self.client.GetCurrentPadding()?;
            let fill = (RATE * FILL_MS / 1000).min(self.frames);
            if pad >= fill {
                return Ok(0);
            }
            let n = fill - pad;
            let p = self.rc.GetBuffer(n)?;
            let s = std::slice::from_raw_parts_mut(p as *mut f32, n as usize * audio_cap::CHANNELS);
            f(s);
            self.rc.ReleaseBuffer(n, 0)?;
            Ok(n as usize)
        }
    }

    /// 播放速率 = RATE × ratio（只在支持时生效；变化小于 1Hz 不调）。
    pub fn set_ratio(&mut self, ratio: f64) {
        let r = (RATE as f64 * ratio) as f32;
        if let Some(c) = &self.clock {
            if (r - self.rate).abs() >= 1.0 {
                if unsafe { c.SetSampleRate(r) }.is_ok() {
                    self.rate = r;
                }
            }
        }
    }
}

impl Drop for Render {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

/// 本线程登记为 MMCSS “Pro Audio”（avrt.dll 按需加载，没有就算了）。
pub fn mmcss() {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::core::{s, w};
    type F = unsafe extern "system" fn(PCWSTR, *mut u32) -> HANDLE;
    unsafe {
        let Ok(m) = LoadLibraryW(w!("avrt.dll")) else { return };
        let Some(p) = GetProcAddress(m, s!("AvSetMmThreadCharacteristicsW")) else { return };
        let f: F = std::mem::transmute(p);
        let mut idx = 0u32;
        let _ = f(w!("Pro Audio"), &mut idx);
    }
}
