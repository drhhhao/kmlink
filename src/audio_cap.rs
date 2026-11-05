//! 系统声音采集：WASAPI 进程环回（Windows 10 20348+ / Windows 11）。
//!
//! - 激活 "VAD\Process_Loopback"，AUDIOCLIENT_ACTIVATION_PARAMS = PROCESS_LOOPBACK，TargetProcessId = 本进程，
//!   EXCLUDE_TARGET_PROCESS_TREE：采本机正在放的所有声音，**除了 kmlink 自己放的**（收到的对方声音不会再被采回去，
//!   切断回声环路）。include = true 只给探针/测试用（只采本进程树）。
//! - 进程环回不能 GetMixFormat：格式自己指定，固定 48 kHz / 2 声道 / f32，AUTOCONVERTPCM + SRC_DEFAULT_QUALITY
//!   由系统转换。事件驱动，缓冲 20ms。
//! - ActivateAudioInterfaceAsync 在 mmdevapi.dll 里：按需 LoadLibrary + GetProcAddress，功能关着时进程不加载它。
//! - 调用方线程要先 CoInitializeEx(MTA)（`com_init`）。

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::{BLOB, COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{GUID, HRESULT, IUnknown, Interface, PCWSTR, Ref, implement, s, w};

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// WASAPI 缓冲（100ns 单位）：20ms。
const BUFFER_HNS: i64 = 200_000;

/// 线上和设备上都用的格式：48 kHz、2 声道、32 位浮点。
pub fn format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: CHANNELS as u16,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * CHANNELS as u32 * 4,
        nBlockAlign: (CHANNELS * 4) as u16,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

/// 本线程进入 COM 多线程套间（重复调用无害）。
pub fn com_init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

type ActivateFn = unsafe extern "system" fn(PCWSTR, *const GUID, *const PROPVARIANT, *mut c_void, *mut *mut c_void) -> HRESULT;

/// mmdevapi!ActivateAudioInterfaceAsync（按需加载；没有就 None）。
fn activate_fn() -> Option<ActivateFn> {
    static F: OnceLock<Option<usize>> = OnceLock::new();
    let p = *F.get_or_init(|| unsafe {
        let m = LoadLibraryW(w!("mmdevapi.dll")).ok()?;
        GetProcAddress(m, s!("ActivateAudioInterfaceAsync")).map(|f| f as usize)
    });
    p.map(|p| unsafe { std::mem::transmute::<usize, ActivateFn>(p) })
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Done(mpsc::Sender<windows::core::Result<IUnknown>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Done_Impl {
    fn ActivateCompleted(&self, op: Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let r = (|| {
            let op = op.ok()?;
            let mut hr = HRESULT::default();
            let mut itf: Option<IUnknown> = None;
            unsafe { op.GetActivateResult(&mut hr, &mut itf)? };
            hr.ok()?;
            itf.ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_NOINTERFACE))
        })();
        let _ = self.0.send(r);
        Ok(())
    }
}

/// 同步激活一个 IAudioClient（最多等 5 秒）。
pub fn activate(path: PCWSTR, params: Option<&PROPVARIANT>) -> windows::core::Result<IAudioClient> {
    let f = activate_fn().ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_NOTIMPL))?;
    let (tx, rx) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Done(tx).into();
    let mut op: *mut c_void = std::ptr::null_mut();
    let pv = params.map_or(std::ptr::null(), |p| p as *const PROPVARIANT);
    unsafe { f(path, &IAudioClient::IID, pv, handler.as_raw(), &mut op) }.ok()?;
    // 异步操作对象：拿到就放掉（结果走回调）
    let _op = (!op.is_null()).then(|| unsafe { IActivateAudioInterfaceAsyncOperation::from_raw(op) });
    let r = rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| windows::core::Error::from_hresult(windows::Win32::Foundation::E_ABORT))?;
    r?.cast()
}

/// 进程环回采集流。
pub struct Capture {
    client: IAudioClient,
    cap: IAudioCaptureClient,
    event: HANDLE,
}

// COM 对象在 MTA 里，可以跨线程交给采集线程
unsafe impl Send for Capture {}

impl Capture {
    /// include = false：采本机除 kmlink 进程树以外的所有声音；true：只采 kmlink 自己的（测试用）。
    pub fn open(include: bool) -> windows::core::Result<Capture> {
        Self::open_pid(unsafe { GetCurrentProcessId() }, include)
    }

    /// 以 pid 进程树为目标：include = 只采它，否则采除它以外的（测试工具录 kmlink 放出来的声音用）。
    pub fn open_pid(pid: u32, include: bool) -> windows::core::Result<Capture> {
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: if include {
                        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
                    } else {
                        PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
                    },
                },
            },
        };
        // 不能让它析构：PROPVARIANT 的 Drop 会对 VT_BLOB 调 PropVariantClear，去释放栈上的 params（堆损坏）
        let pv = std::mem::ManuallyDrop::new(PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_BLOB,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: PROPVARIANT_0_0_0 {
                        blob: BLOB {
                            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                            pBlobData: &mut params as *mut _ as *mut u8,
                        },
                    },
                }),
            },
        });
        let client = activate(w!("VAD\\Process_Loopback"), Some(&pv))?;
        let fmt = format();
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
            | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        unsafe {
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_HNS, 0, &fmt, None)?;
            let event = CreateEventW(None, false, false, None)?;
            let r = (|| {
                client.SetEventHandle(event)?;
                let cap: IAudioCaptureClient = client.GetService()?;
                client.Start()?;
                Ok(cap)
            })();
            match r {
                Ok(cap) => Ok(Capture { client, cap, event }),
                Err(e) => {
                    let _ = CloseHandle(event);
                    Err(e)
                }
            }
        }
    }

    /// 等数据（最多 timeout_ms），把拿到的每个包交给 f(交错样本, 系统标了静音)。返回取到的帧数。
    pub fn read(&mut self, timeout_ms: u32, mut f: impl FnMut(&[f32], bool)) -> windows::core::Result<usize> {
        let mut total = 0;
        unsafe {
            if WaitForSingleObject(self.event, timeout_ms) != WAIT_OBJECT_0 {
                // 没等到数据：问一下流还活着没有（音频服务重启 / 设备失效时事件再也不会来，这里会报错 → 调用方重开）
                self.cap.GetNextPacketSize()?;
                return Ok(0);
            }
            loop {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                if frames == 0 {
                    break; // AUDCLNT_S_BUFFER_EMPTY
                }
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let n = frames as usize * CHANNELS;
                if silent || data.is_null() {
                    f(&ZEROS[..n.min(ZEROS.len())], true);
                } else {
                    f(std::slice::from_raw_parts(data as *const f32, n), false);
                }
                self.cap.ReleaseBuffer(frames)?;
                total += frames as usize;
            }
        }
        Ok(total)
    }
}

/// 系统标了静音的包：交出这么多零（一个包不会超过 20ms 缓冲）。
static ZEROS: [f32; 4096] = [0.0; 4096];

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}
