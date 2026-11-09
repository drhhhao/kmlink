//! 加密：两台共用一个 32 字节预共享密钥（config.key）。
//!
//! 握手（明文）：客户端发 MAGIC + 32 字节随机 nonce_c + 32 字节 X25519 临时公钥；
//! 服务端回 MAGIC + 32 字节随机 nonce_s + 32 字节 X25519 临时公钥。
//! 会话密钥 = HKDF-SHA256(ikm = psk ‖ DH 共享密钥, salt = nonce_c ‖ nonce_s)，info 分别为
//!   "kmlink c2s" / "kmlink s2c" / "kmlink udp c2s" / "kmlink udp s2c" / "kmlink audio c2s" / "kmlink audio s2c"，
//!   各 32 字节。
//! 临时私钥用完即清零：日后密钥泄露也解不开录下来的旧会话（前向保密）。
//! 之后每个方向一个 ChaCha20-Poly1305：
//!   TCP：nonce = 4 字节 0 ‖ u64 LE 计数器（从 0 开始，每帧 +1，接收方要求严格递增 —— 防重放/乱序）。
//!   UDP：包 = u64 LE 计数器 ‖ 密文；nonce = 4 字节 0 ‖ 计数器；接收方只接受比见过的最大值更大的计数器。
//!   声音：和鼠标同一个 UDP 端口，但密钥另算，计数器从 2^63 开始（最高位 = 1，收包时据此分流）；
//!   接收方用 64 包滑动窗口：允许乱序，同一计数器只收一次（防重放）。
//! 第一帧（Hello）能解开即证明对方有同一个密钥。计数器用 checked_add，溢出就失败（会话断开），绝不复用 nonce。

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

pub struct SessionKeys {
    pub c2s: [u8; 32],
    pub s2c: [u8; 32],
    pub udp_c2s: [u8; 32],
    pub udp_s2c: [u8; 32],
    pub audio_c2s: [u8; 32],
    pub audio_s2c: [u8; 32],
    /// 文件连接的会话票据：两边相同、只在本会话有效（文件连接握手后第一帧带上，证明是同一会话的对方）。
    pub file_ticket: [u8; 32],
}

pub fn derive(psk: &[u8; 32], dh: &[u8; 32], nonce_c: &[u8; 32], nonce_s: &[u8; 32]) -> SessionKeys {
    let k = expander(psk, dh, nonce_c, nonce_s);
    SessionKeys {
        c2s: k(b"kmlink c2s"),
        s2c: k(b"kmlink s2c"),
        udp_c2s: k(b"kmlink udp c2s"),
        udp_s2c: k(b"kmlink udp s2c"),
        audio_c2s: k(b"kmlink audio c2s"),
        audio_s2c: k(b"kmlink audio s2c"),
        file_ticket: k(b"kmlink file ticket"),
    }
}

/// 文件连接（KMLF）两个方向的密钥：info 和主连接不同（密钥域分开）——中间人把 KMLF 改成 KML2，
/// 两边派生出的密钥对不上，第一帧就解不开。
pub fn derive_file(psk: &[u8; 32], dh: &[u8; 32], nonce_c: &[u8; 32], nonce_s: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let k = expander(psk, dh, nonce_c, nonce_s);
    (k(b"kmlink file c2s"), k(b"kmlink file s2c"))
}

fn expander(psk: &[u8; 32], dh: &[u8; 32], nonce_c: &[u8; 32], nonce_s: &[u8; 32]) -> impl Fn(&[u8]) -> [u8; 32] {
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(nonce_c);
    salt[32..].copy_from_slice(nonce_s);
    let mut ikm = [0u8; 64];
    ikm[..32].copy_from_slice(psk);
    ikm[32..].copy_from_slice(dh);
    let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    wipe(&mut ikm);
    move |info: &[u8]| {
        let mut out = [0u8; 32];
        // 32 字节远小于 HKDF-SHA256 上限（255*32），不会失败
        hk.expand(info, &mut out).expect("hkdf expand 32");
        out
    }
}

/// X25519 临时密钥对（每次握手一个）。私钥在 Drop 时清零。
pub struct Ephemeral {
    sk: [u8; 32],
    pub pk: [u8; 32],
}

impl Ephemeral {
    pub fn new() -> Ephemeral {
        let sk = random32();
        let pk = x25519_dalek::x25519(sk, x25519_dalek::X25519_BASEPOINT_BYTES);
        Ephemeral { sk, pk }
    }

    /// 与对方公钥算共享密钥；对方给的是小阶点（结果全 0）时返回 None。
    pub fn agree(self, peer_pk: &[u8; 32]) -> Option<[u8; 32]> {
        let dh = x25519_dalek::x25519(self.sk, *peer_pk);
        if dh.iter().fold(0u8, |a, b| a | b) == 0 { None } else { Some(dh) }
    }
}

impl Drop for Ephemeral {
    fn drop(&mut self) {
        wipe(&mut self.sk);
    }
}

/// 清零（volatile 写，不会被优化掉）。
pub fn wipe(b: &mut [u8]) {
    for x in b.iter_mut() {
        // SAFETY：x 是有效的 &mut u8
        unsafe { std::ptr::write_volatile(x, 0) };
    }
}

fn cipher(key: &[u8; 32]) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(&Key::from(*key))
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    Nonce::from(n)
}

/// TCP 方向加密器。
pub struct Sealer {
    aead: ChaCha20Poly1305,
    counter: u64,
}
impl Sealer {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { aead: cipher(key), counter: 0 }
    }
    /// 计数器用尽（2^64 帧，实际不可能）返回 None。
    pub fn seal(&mut self, plaintext: &[u8]) -> Option<Vec<u8>> {
        let n = nonce(self.counter);
        self.counter = self.counter.checked_add(1)?;
        // 只有明文超过 ~256 GiB 才会失败
        self.aead.encrypt(&n, plaintext).ok()
    }
}

/// TCP 方向解密器（计数器严格递增）。
pub struct Opener {
    aead: ChaCha20Poly1305,
    counter: u64,
}
impl Opener {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { aead: cipher(key), counter: 0 }
    }
    /// 失败（篡改/重放/乱序/密钥不对）返回 None，计数器不前进。
    pub fn open(&mut self, ciphertext: &[u8]) -> Option<Vec<u8>> {
        let next = self.counter.checked_add(1)?;
        let p = self.aead.decrypt(&nonce(self.counter), ciphertext).ok()?;
        self.counter = next;
        Some(p)
    }
}

/// UDP 发送：返回完整的包（计数器 ‖ 密文）。
pub struct UdpSealer {
    aead: ChaCha20Poly1305,
    counter: u64,
}
impl UdpSealer {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { aead: cipher(key), counter: 0 }
    }
    /// 计数器用尽返回 None（不发）。
    pub fn seal(&mut self, plaintext: &[u8]) -> Option<Vec<u8>> {
        let c = self.counter;
        self.counter = c.checked_add(1)?;
        let ct = self.aead.encrypt(&nonce(c), plaintext).ok()?;
        let mut p = Vec::with_capacity(8 + ct.len());
        p.extend_from_slice(&c.to_le_bytes());
        p.extend_from_slice(&ct);
        Some(p)
    }
}

/// UDP 接收：只接受更大的计数器。
pub struct UdpOpener {
    aead: ChaCha20Poly1305,
    max_seen: Option<u64>,
}
impl UdpOpener {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { aead: cipher(key), max_seen: None }
    }
    pub fn open(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        if packet.len() < 8 + 16 {
            return None;
        }
        let c = u64::from_le_bytes(packet[..8].try_into().ok()?);
        if self.max_seen.is_some_and(|m| c <= m) {
            return None;
        }
        let p = self.aead.decrypt(&nonce(c), &packet[8..]).ok()?;
        self.max_seen = Some(c);
        Some(p)
    }
}

/// 声音包计数器的起点：最高位为 1，和鼠标包（从 0 开始）一眼分开。
pub const AUDIO_COUNTER0: u64 = 1 << 63;

/// 是不是声音包（看计数器最高位，不用解密）。
pub fn is_audio(packet: &[u8]) -> bool {
    packet.len() >= 8 && packet[7] & 0x80 != 0
}

/// 声音发送：包 = 计数器 ‖ 密文，计数器从 2^63 开始。
pub struct AudioSealer {
    aead: ChaCha20Poly1305,
    counter: u64,
}
impl AudioSealer {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { aead: cipher(key), counter: AUDIO_COUNTER0 }
    }
    /// 计数器用尽返回 None（不发）。
    pub fn seal(&mut self, plaintext: &[u8]) -> Option<Vec<u8>> {
        let c = self.counter;
        self.counter = c.checked_add(1)?;
        let ct = self.aead.encrypt(&nonce(c), plaintext).ok()?;
        let mut p = Vec::with_capacity(8 + ct.len());
        p.extend_from_slice(&c.to_le_bytes());
        p.extend_from_slice(&ct);
        Some(p)
    }
}

/// 声音接收：64 包滑动窗口。比窗口还旧的、收过的都丢；解密成功才记账。
pub struct AudioOpener {
    aead: ChaCha20Poly1305,
    max_seen: Option<u64>,
    /// 第 i 位 = 计数器 max_seen - i 收过了
    seen: u64,
}
impl AudioOpener {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { aead: cipher(key), max_seen: None, seen: 0 }
    }
    pub fn open(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        if packet.len() < 8 + 16 {
            return None;
        }
        let c = u64::from_le_bytes(packet[..8].try_into().ok()?);
        if c < AUDIO_COUNTER0 {
            return None;
        }
        if let Some(m) = self.max_seen {
            if c <= m {
                let back = m - c;
                if back >= 64 || self.seen >> back & 1 != 0 {
                    return None;
                }
            }
        }
        let p = self.aead.decrypt(&nonce(c), &packet[8..]).ok()?;
        match self.max_seen {
            Some(m) if c <= m => self.seen |= 1 << (m - c),
            Some(m) => {
                let ahead = c - m;
                self.seen = if ahead >= 64 { 1 } else { self.seen << ahead | 1 };
                self.max_seen = Some(c);
            }
            None => {
                self.seen = 1;
                self.max_seen = Some(c);
            }
        }
        Some(p)
    }
}

pub fn random32() -> [u8; 32] {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("getrandom");
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> SessionKeys {
        derive(&[7u8; 32], &[5u8; 32], &[1u8; 32], &[2u8; 32])
    }

    #[test]
    fn derive_distinct_and_deterministic() {
        let k = keys();
        let all = [k.c2s, k.s2c, k.udp_c2s, k.udp_s2c, k.audio_c2s, k.audio_s2c];
        for i in 0..all.len() {
            for j in i + 1..all.len() {
                assert_ne!(all[i], all[j]);
            }
        }
        let k2 = keys();
        assert_eq!(k.c2s, k2.c2s);
        assert_eq!(k.udp_s2c, k2.udp_s2c);
        // 不同 nonce / 不同密钥 / 不同 DH → 不同会话密钥
        assert_ne!(derive(&[7u8; 32], &[5u8; 32], &[1u8; 32], &[3u8; 32]).c2s, k.c2s);
        assert_ne!(derive(&[8u8; 32], &[5u8; 32], &[1u8; 32], &[2u8; 32]).c2s, k.c2s);
        assert_ne!(derive(&[7u8; 32], &[6u8; 32], &[1u8; 32], &[2u8; 32]).c2s, k.c2s);
    }

    #[test]
    fn x25519_agreement() {
        let (a, b) = (Ephemeral::new(), Ephemeral::new());
        let (apk, bpk) = (a.pk, b.pk);
        assert_ne!(apk, bpk);
        let s1 = a.agree(&bpk).unwrap();
        let s2 = b.agree(&apk).unwrap();
        assert_eq!(s1, s2);
        // 小阶点（全 0）被拒绝
        assert!(Ephemeral::new().agree(&[0u8; 32]).is_none());
    }

    #[test]
    fn counter_overflow_fails() {
        let k = keys();
        let mut s = Sealer::new(&k.c2s);
        s.counter = u64::MAX;
        assert!(s.seal(b"x").is_none());
        let mut o = Opener::new(&k.c2s);
        o.counter = u64::MAX;
        assert!(o.open(&[0u8; 32]).is_none());
        let mut u = UdpSealer::new(&k.udp_c2s);
        u.counter = u64::MAX;
        assert!(u.seal(b"x").is_none());
    }

    #[test]
    fn tcp_roundtrip() {
        let k = keys();
        let mut s = Sealer::new(&k.c2s);
        let mut o = Opener::new(&k.c2s);
        for i in 0..5u8 {
            let msg = vec![i; i as usize * 10];
            let ct = s.seal(&msg).unwrap();
            assert_eq!(ct.len(), msg.len() + 16);
            assert_eq!(o.open(&ct).unwrap(), msg);
        }
    }

    #[test]
    fn tcp_tamper_rejected() {
        let k = keys();
        let mut s = Sealer::new(&k.c2s);
        let mut o = Opener::new(&k.c2s);
        let mut ct = s.seal(b"hello").unwrap();
        ct[0] ^= 1;
        assert!(o.open(&ct).is_none());
        ct[0] ^= 1;
        // 计数器没有因失败前进
        assert_eq!(o.open(&ct).unwrap(), b"hello");
        // 截断
        let ct2 = s.seal(b"world").unwrap();
        assert!(o.open(&ct2[..ct2.len() - 1]).is_none());
        assert!(o.open(&[]).is_none());
    }

    #[test]
    fn tcp_replay_and_reorder_rejected() {
        let k = keys();
        let mut s = Sealer::new(&k.c2s);
        let mut o = Opener::new(&k.c2s);
        let a = s.seal(b"a").unwrap();
        let b = s.seal(b"b").unwrap();
        let c = s.seal(b"c").unwrap();
        assert!(o.open(&b).is_none()); // 乱序
        assert_eq!(o.open(&a).unwrap(), b"a");
        assert!(o.open(&a).is_none()); // 重放
        assert_eq!(o.open(&b).unwrap(), b"b");
        assert_eq!(o.open(&c).unwrap(), b"c");
    }

    #[test]
    fn directions_not_interchangeable() {
        let k = keys();
        let mut s = Sealer::new(&k.c2s);
        let mut o = Opener::new(&k.s2c);
        assert!(o.open(&s.seal(b"x").unwrap()).is_none());
        let mut us = UdpSealer::new(&k.udp_c2s);
        let mut uo = UdpOpener::new(&k.udp_s2c);
        assert!(uo.open(&us.seal(b"x").unwrap()).is_none());
        // TCP 密钥不能用于 UDP
        let mut uo2 = UdpOpener::new(&k.c2s);
        assert!(uo2.open(&UdpSealer::new(&k.udp_c2s).seal(b"x").unwrap()).is_none());
    }

    #[test]
    fn udp_only_increasing() {
        let k = keys();
        let mut s = UdpSealer::new(&k.udp_c2s);
        let mut o = UdpOpener::new(&k.udp_c2s);
        let p0 = s.seal(b"0").unwrap();
        let p1 = s.seal(b"1").unwrap();
        let p2 = s.seal(b"2").unwrap();
        let p3 = s.seal(b"3").unwrap();
        assert_eq!(&p1[..8], &1u64.to_le_bytes());
        assert_eq!(o.open(&p2).unwrap(), b"2"); // 丢包可以跳过
        assert!(o.open(&p0).is_none()); // 旧的不要
        assert!(o.open(&p1).is_none());
        assert!(o.open(&p2).is_none()); // 重放
        assert_eq!(o.open(&p3).unwrap(), b"3");
    }

    #[test]
    fn udp_tamper_rejected() {
        let k = keys();
        let mut s = UdpSealer::new(&k.udp_c2s);
        let mut o = UdpOpener::new(&k.udp_c2s);
        let mut p = s.seal(b"move").unwrap();
        // 改计数器 → nonce 不同，认证失败；且不应抬高 max_seen
        p[0] ^= 0x10;
        assert!(o.open(&p).is_none());
        p[0] ^= 0x10;
        let last = p.len() - 1;
        p[last] ^= 1;
        assert!(o.open(&p).is_none());
        p[last] ^= 1;
        assert_eq!(o.open(&p).unwrap(), b"move");
        assert!(o.open(&[0u8; 10]).is_none());
    }

    #[test]
    fn audio_window_reorder_and_replay() {
        let k = keys();
        let mut tx = AudioSealer::new(&k.audio_c2s);
        let mut rx = AudioOpener::new(&k.audio_c2s);
        let p: Vec<Vec<u8>> = (0..100u8).map(|i| tx.seal(&[i]).unwrap()).collect();
        assert!(p.iter().all(|x| is_audio(x)));
        assert_eq!(rx.open(&p[5]).unwrap(), [5]);
        assert_eq!(rx.open(&p[3]).unwrap(), [3]); // 乱序：晚到的旧包也收
        assert!(rx.open(&p[3]).is_none()); // 重放
        assert!(rx.open(&p[5]).is_none());
        assert_eq!(rx.open(&p[90]).unwrap(), [90]);
        assert!(rx.open(&p[20]).is_none()); // 落出窗口（90-20 >= 64）
        assert_eq!(rx.open(&p[40]).unwrap(), [40]);
        assert!(rx.open(&p[40]).is_none());
        assert_eq!(rx.open(&p[91]).unwrap(), [91]);
        assert!(rx.open(&p[90]).is_none());
        // 篡改的包不记账：原包随后还能收
        let mut bad = p[95].clone();
        bad[10] ^= 1;
        assert!(rx.open(&bad).is_none());
        assert_eq!(rx.open(&p[95]).unwrap(), [95]);
    }

    #[test]
    fn audio_and_mouse_packets_dont_mix() {
        let k = keys();
        let mut m = UdpSealer::new(&k.udp_c2s);
        let mut a = AudioSealer::new(&k.audio_c2s);
        let mp = m.seal(b"move").unwrap();
        let ap = a.seal(b"pcm").unwrap();
        assert!(!is_audio(&mp));
        assert!(is_audio(&ap));
        // 声音包打不开鼠标通道（老版本收到也只是丢掉），反之亦然
        assert!(UdpOpener::new(&k.udp_c2s).open(&ap).is_none());
        assert!(AudioOpener::new(&k.audio_c2s).open(&mp).is_none());
        let mut rx = UdpOpener::new(&k.udp_c2s);
        assert_eq!(rx.open(&mp).unwrap(), b"move");
    }
}
