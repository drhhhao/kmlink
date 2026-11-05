//! 声音采集探针（`kmlink audio-probe`，实测用，不影响主程序）：在本机跑几组实验并打印结果。
//!   1. 空闲：没人放声音时进程环回给不给数据、标不标静音
//!   2. 外部进程放 1 kHz 音：排除 kmlink 的环回能采到
//!   3. 本进程自己放：排除模式采不到（防回声环路的关键），包含模式能采到
//!   4. 把默认输出设备静音后，外部进程放音：还能不能采到（“只发不放”那台要用）
//! 结果同时写日志（%APPDATA%\kmlink\audio-probe.log）。

use crate::audio_cap::{self, Capture};
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    IMMDeviceEnumerator, MMDeviceEnumerator, PlaySoundW, SND_ASYNC, SND_FILENAME, SND_LOOP, SND_NODEFAULT, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::core::PCWSTR;

/// 写一个 1 kHz、-34 dB（几乎听不见）、5 秒的 48k/16 位立体声 WAV。
fn write_tone(path: &std::path::Path) {
    let n = 48_000 * 5;
    let mut d = Vec::with_capacity(44 + n * 4);
    let data_len = (n * 4) as u32;
    d.extend_from_slice(b"RIFF");
    d.extend_from_slice(&(36 + data_len).to_le_bytes());
    d.extend_from_slice(b"WAVEfmt ");
    d.extend_from_slice(&16u32.to_le_bytes());
    d.extend_from_slice(&1u16.to_le_bytes());
    d.extend_from_slice(&2u16.to_le_bytes());
    d.extend_from_slice(&48_000u32.to_le_bytes());
    d.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
    d.extend_from_slice(&4u16.to_le_bytes());
    d.extend_from_slice(&16u16.to_le_bytes());
    d.extend_from_slice(b"data");
    d.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..n {
        let v = (0.02 * (std::f32::consts::TAU * 1000.0 * i as f32 / 48_000.0).sin() * 32767.0) as i16;
        d.extend_from_slice(&v.to_le_bytes());
        d.extend_from_slice(&v.to_le_bytes());
    }
    let _ = std::fs::write(path, d);
}

/// 采 secs 秒：(每秒回调次数, 包里标静音的比例, 峰值)。
fn measure(include: bool, secs: f32) -> Result<(f32, f32, f32), String> {
    let mut c = Capture::open(include).map_err(|e| format!("打开采集失败：{e}"))?;
    let t0 = Instant::now();
    let (mut packets, mut silent, mut peak, mut wakes) = (0u32, 0u32, 0f32, 0u32);
    while t0.elapsed() < Duration::from_secs_f32(secs) {
        let n = c
            .read(100, |s, sil| {
                packets += 1;
                if sil {
                    silent += 1;
                }
                for v in s {
                    peak = peak.max(v.abs());
                }
            })
            .map_err(|e| format!("读取失败：{e}"))?;
        if n > 0 {
            wakes += 1;
        }
    }
    Ok((wakes as f32 / secs, if packets == 0 { 0.0 } else { silent as f32 / packets as f32 }, peak))
}

fn report(out: &mut String, name: &str, r: Result<(f32, f32, f32), String>) {
    let line = match r {
        Ok((w, s, p)) => format!("{name}：每秒有数据 {w:.0} 次，静音包 {:.0}%，峰值 {p:.4}（{:.1} dBFS）", s * 100.0, 20.0 * p.max(1e-9).log10()),
        Err(e) => format!("{name}：{e}"),
    };
    println!("{line}");
    out.push_str(&line);
    out.push('\n');
}

/// 外部进程放音 secs 秒（PowerShell SoundPlayer 循环播放）。经 WMI 启动：不能是本进程的子进程，
/// 否则属于 kmlink 进程树，会被“排除本进程树”一起排除掉。
fn external_player(wav: &std::path::Path, secs: u32) {
    use std::os::windows::process::CommandExt;
    let inner = format!(
        "powershell -NoProfile -WindowStyle Hidden -Command \"$p = New-Object Media.SoundPlayer '{}'; $p.PlayLooping(); Start-Sleep {secs}\"",
        wav.display()
    );
    let cmd = format!(
        "Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{CommandLine='{}'}} | Out-Null",
        inner.replace('\'', "''")
    );
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &cmd])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .status();
}

fn endpoint_volume() -> Option<IAudioEndpointVolume> {
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let dev = en.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        dev.Activate(CLSCTX_ALL, None).ok()
    }
}

pub fn run() {
    audio_cap::com_init();
    let mut out = String::new();
    let wav = std::env::temp_dir().join("kmlink-probe-tone.wav");
    write_tone(&wav);
    let wavw = crate::sys::wide(&wav.to_string_lossy());

    report(&mut out, "1 空闲（排除本进程）", measure(false, 3.0));

    external_player(&wav, 5);
    std::thread::sleep(Duration::from_millis(1000));
    report(&mut out, "2 外部放音（排除本进程）", measure(false, 3.0));
    std::thread::sleep(Duration::from_millis(2500));

    unsafe {
        let _ = PlaySoundW(PCWSTR(wavw.as_ptr()), None, SND_ASYNC | SND_FILENAME | SND_LOOP | SND_NODEFAULT);
    }
    std::thread::sleep(Duration::from_millis(800));
    report(&mut out, "3a 本进程放音（排除本进程）", measure(false, 3.0));
    report(&mut out, "3b 本进程放音（只采本进程）", measure(true, 3.0));
    unsafe {
        let _ = PlaySoundW(PCWSTR::null(), None, Default::default());
    }

    match endpoint_volume() {
        Some(vol) => unsafe {
            let was = vol.GetMute().map(|b| b.as_bool()).unwrap_or(false);
            let _ = vol.SetMute(true, std::ptr::null());
            external_player(&wav, 5);
            std::thread::sleep(Duration::from_millis(1000));
            report(&mut out, "4 输出设备静音 + 外部放音（排除本进程）", measure(false, 3.0));
            let _ = vol.SetMute(was, std::ptr::null());
        },
        None => report(&mut out, "4 输出设备静音", Err("拿不到默认输出设备的音量接口".into())),
    }
    std::thread::sleep(Duration::from_millis(1500));
    let _ = std::fs::remove_file(&wav);
    let _ = std::fs::write(crate::config::dir().join("audio-probe.log"), out);
}

/// 录 pid 进程树放出来的声音 secs 秒：每 5ms 一个 RMS 写 csv（毫秒,rms），打印断音统计。
pub fn rec(args: &[String]) {
    let (Some(pid), Some(secs), Some(out)) =
        (args.first().and_then(|s| s.parse::<u32>().ok()), args.get(1).and_then(|s| s.parse::<f32>().ok()), args.get(2))
    else {
        println!("用法：kmlink audio-rec <pid> <秒> <输出.csv>");
        return;
    };
    audio_cap::com_init();
    let mut c = match Capture::open_pid(pid, true) {
        Ok(c) => c,
        Err(e) => {
            println!("打开采集失败：{e}");
            return;
        }
    };
    const BLK: usize = 240 * 2;
    let mut acc: Vec<f32> = Vec::new();
    let mut rms: Vec<f32> = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs_f32(secs) {
        if c.read(100, |s, _| acc.extend_from_slice(s)).is_err() {
            break;
        }
        let mut off = 0;
        while acc.len() - off >= BLK {
            let b = &acc[off..off + BLK];
            rms.push((b.iter().map(|v| v * v).sum::<f32>() / BLK as f32).sqrt());
            off += BLK;
        }
        acc.drain(..off);
    }
    let mut csv = String::new();
    for (i, r) in rms.iter().enumerate() {
        csv.push_str(&format!("{},{:.6}\n", i * 5, r));
    }
    let _ = std::fs::write(out, csv);
    // 有声的块：超过最大值的 30%
    let peak = rms.iter().cloned().fold(0f32, f32::max);
    let on: Vec<bool> = rms.iter().map(|&r| r > peak * 0.3).collect();
    let first = on.iter().position(|&b| b);
    let last = on.iter().rposition(|&b| b);
    let mut line = format!("块 {}（{:.1} 秒），峰值 RMS {:.4}", rms.len(), rms.len() as f32 * 0.005, peak);
    if let (Some(a), Some(b)) = (first, last) {
        let mut gaps = Vec::new();
        let mut run = 0;
        for &x in &on[a..=b] {
            if x {
                if run > 0 {
                    gaps.push(run * 5);
                }
                run = 0;
            } else {
                run += 1;
            }
        }
        let low: usize = on[a..=b].iter().filter(|&&x| !x).count();
        line += &format!(
            "；有声段 {:.2}–{:.2} 秒，其中断音 {} 次，共 {} ms（{:.2}%）；断音长度 ms：{:?}",
            a as f32 * 0.005,
            b as f32 * 0.005,
            gaps.len(),
            low * 5,
            low as f32 * 100.0 / (b - a + 1) as f32,
            &gaps[..gaps.len().min(40)]
        );
    } else {
        line += "；没录到声音";
    }
    println!("{line}");
    let _ = std::fs::write(format!("{out}.txt"), line);
}

/// 放一个正弦音 secs 秒（默认 1 kHz、幅度 0.02 ≈ -34 dB），本进程放（不是 kmlink 的进程树，会被 kmlink 采到）。
pub fn tone(args: &[String]) {
    let secs = args.first().and_then(|s| s.parse::<u32>().ok()).unwrap_or(10);
    let freq = args.get(1).and_then(|s| s.parse::<f32>().ok()).unwrap_or(1000.0);
    let amp = args.get(2).and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.02);
    let n = 48_000 * secs as usize;
    let mut d = Vec::with_capacity(44 + n * 4);
    let data_len = (n * 4) as u32;
    d.extend_from_slice(b"RIFF");
    d.extend_from_slice(&(36 + data_len).to_le_bytes());
    d.extend_from_slice(b"WAVEfmt ");
    for v in [16u32] {
        d.extend_from_slice(&v.to_le_bytes());
    }
    d.extend_from_slice(&1u16.to_le_bytes());
    d.extend_from_slice(&2u16.to_le_bytes());
    d.extend_from_slice(&48_000u32.to_le_bytes());
    d.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
    d.extend_from_slice(&4u16.to_le_bytes());
    d.extend_from_slice(&16u16.to_le_bytes());
    d.extend_from_slice(b"data");
    d.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..n {
        let v = (amp * (std::f32::consts::TAU * freq * i as f32 / 48_000.0).sin() * 32767.0) as i16;
        d.extend_from_slice(&v.to_le_bytes());
        d.extend_from_slice(&v.to_le_bytes());
    }
    let wav = std::env::temp_dir().join(format!("kmlink-tone-{}.wav", std::process::id()));
    let _ = std::fs::write(&wav, d);
    let w = crate::sys::wide(&wav.to_string_lossy());
    unsafe {
        // 同步放完
        let _ = PlaySoundW(PCWSTR(w.as_ptr()), None, SND_FILENAME | SND_NODEFAULT);
    }
    let _ = std::fs::remove_file(&wav);
}

/// 离线重放 audio-trace.txt：按到达时刻把包喂给抗抖动缓冲，声卡每 10ms 取 480 帧，
/// 比较几组参数下的断音（补静音 + 欠载）和平均延迟。用法：kmlink audio-sim <trace> [q,min,max ...]
pub fn sim(args: &[String]) {
    use crate::jitter::{FRAMES, Header, Jitter, SAMPLES, Tune};
    let Some(path) = args.first() else {
        println!("用法：kmlink audio-sim <audio-trace.txt> [q,min,max ...]");
        return;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        println!("读不了 {path}");
        return;
    };
    // (到达 µs 展开成 i64, seq, send_us, flags)
    let mut pk: Vec<(i64, u32, u32, u8)> = Vec::new();
    let mut last: Option<u32> = None;
    let mut acc: i64 = 0;
    for l in text.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 4 {
            continue;
        }
        let (Ok(seq), Ok(send), Ok(arr), Ok(fl)) = (f[0].parse::<u32>(), f[1].parse::<u32>(), f[2].parse::<u32>(), f[3].parse::<u8>()) else {
            continue;
        };
        if let Some(p) = last {
            // 到达时刻只会往前走：按无符号差算（静音很久也不会被当成倒退）
            acc += arr.wrapping_sub(p) as i64;
        }
        last = Some(arr);
        pk.push((acc, seq, send, fl));
    }
    if pk.is_empty() {
        println!("没有包");
        return;
    }
    let mut sets: Vec<Tune> = args[1..]
        .iter()
        .filter_map(|a| {
            let v: Vec<f64> = a.split(',').filter_map(|x| x.parse().ok()).collect();
            (v.len() >= 3).then(|| Tune {
                q: v[0],
                min_ms: v[1] as u32,
                max_ms: v[2] as u32,
                window_s: v.get(3).map_or(60, |w| *w as u32),
                ..Tune::default()
            })
        })
        .collect();
    if sets.is_empty() {
        for window_s in [60, 300] {
            for q in [0.995, 0.999, 1.0] {
                for min in [40, 80, 120] {
                    sets.push(Tune { q, min_ms: min, max_ms: 400, window_s, ..Tune::default() });
                }
            }
        }
    }
    let dur_s = (pk.last().unwrap().0 - pk[0].0) as f64 / 1e6;
    println!("{} 包，{:.0} 秒", pk.len(), dur_s);
    println!("  q      min  max  窗口 | 断音次数 补偿包 欠载 丢 | 平均缓冲ms  缓冲P50/P95");
    let pcm = vec![0u8; SAMPLES * 2];
    for t in sets {
        let mut j = Jitter::new(t);
        let mut out = vec![0f32; FRAMES * 2 * 2];
        let t0 = pk[0].0;
        let end = pk.last().unwrap().0 + 1_000_000;
        let mut i = 0;
        let mut now = t0;
        let (mut depth_sum, mut n) = (0f64, 0u64);
        let mut depths: Vec<u32> = Vec::new();
        let mut events = 0u64;
        let mut prev_bad = false;
        // 只有补偿算立刻断音；欠载要等下一包确认（下一包带 START = 发送端静音，不是断音）
        let mut pending = false;
        let mut last_arrival = i64::MIN / 2;
        while now < end {
            while i < pk.len() && pk[i].0 <= now {
                let (a, seq, send, fl) = pk[i];
                let u0 = j.stats.underruns;
                j.push(Header { codec: 0, flags: fl, seq, send_us: send }, &pcm, (a as u64 & 0xffff_ffff) as u32, (a / 1000) as u64);
                if pending {
                    if j.stats.underruns >= u0 {
                        events += 1;
                    }
                    pending = false;
                }
                last_arrival = a;
                i += 1;
            }
            let (c0, u0) = (j.stats.concealed, j.stats.underruns);
            j.pull(&mut out, (now / 1000) as u64);
            let bad = j.stats.concealed > c0;
            if bad && !prev_bad {
                events += 1;
            }
            prev_bad = bad;
            if j.stats.underruns > u0 {
                pending = true;
            }
            // 延迟只在有声音的时候统计（发送端静音时缓冲本来就是 0）
            if now - last_arrival <= 200_000 {
                let d = j.depth() as f64 / 48.0;
                depth_sum += d;
                n += 1;
                depths.push(d as u32);
            }
            now += 10_000;
        }
        if depths.is_empty() {
            depths.push(0);
            n = 1;
        }
        depths.sort_unstable();
        let p = |q: f64| depths[((depths.len() - 1) as f64 * q) as usize];
        let s = j.stats;
        println!(
            "  {:<6} {:>3} {:>4} {:>4} | {:>7} {:>6} {:>4} {:>3} | {:>9.0}  {}/{}",
            t.q, t.min_ms, t.max_ms, t.window_s, events, s.concealed, s.underruns, s.dropped, depth_sum / n as f64, p(0.5), p(0.95)
        );
    }
}
