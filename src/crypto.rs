//! 加密：两台共用一个 32 字节预共享密钥（config.key）。
//!
//! 握手（明文）：客户端发 MAGIC + 32 字节随机 nonce_c；服务端回 MAGIC + 32 字节随机 nonce_s。
//! 会话密钥 = HKDF-SHA256(ikm = psk, salt = nonce_c ‖ nonce_s)，info 分别为
//!   "kmlink c2s" / "kmlink s2c" / "kmlink udp c2s" / "kmlink udp s2c"，各 32 字节。
//! 之后每个方向一个 ChaCha20-Poly1305：
//!   TCP：nonce = 4 字节 0 ‖ u64 LE 计数器（从 0 开始，每帧 +1，接收方要求严格递增 —— 防重放/乱序）。
//!   UDP：包 = u64 LE 计数器 ‖ 密文；nonce = 4 字节 0 ‖ 计数器；接收方只接受比见过的最大值更大的计数器。
//! 第一帧（Hello）能解开即证明对方有同一个密钥。

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

pub struct SessionKeys {
    pub c2s: [u8; 32],
    pub s2c: [u8; 32],
    pub udp_c2s: [u8; 32],
    pub udp_s2c: [u8; 32],
}

pub fn derive(psk: &[u8; 32], nonce_c: &[u8; 32], nonce_s: &[u8; 32]) -> SessionKeys {
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(nonce_c);
    salt[32..].copy_from_slice(nonce_s);
    let hk = Hkdf::<Sha256>::new(Some(&salt), psk);
    let k = |info: &[u8]| {
        let mut out = [0u8; 32];
        // 32 字节远小于 HKDF-SHA256 上限（255*32），不会失败
        hk.expand(info, &mut out).expect("hkdf expand 32");
        out
    };
    SessionKeys {
        c2s: k(b"kmlink c2s"),
        s2c: k(b"kmlink s2c"),
        udp_c2s: k(b"kmlink udp c2s"),
        udp_s2c: k(b"kmlink udp s2c"),
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
    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let n = nonce(self.counter);
        self.counter += 1;
        // 只有明文超过 ~256 GiB 才会失败
        self.aead.encrypt(&n, plaintext).expect("chacha20poly1305 encrypt")
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
        let p = self.aead.decrypt(&nonce(self.counter), ciphertext).ok()?;
        self.counter += 1;
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
    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let c = self.counter;
        self.counter += 1;
        let ct = self.aead.encrypt(&nonce(c), plaintext).expect("chacha20poly1305 encrypt");
        let mut p = Vec::with_capacity(8 + ct.len());
        p.extend_from_slice(&c.to_le_bytes());
        p.extend_from_slice(&ct);
        p
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

pub fn random32() -> [u8; 32] {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("getrandom");
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> SessionKeys {
        derive(&[7u8; 32], &[1u8; 32], &[2u8; 32])
    }

    #[test]
    fn derive_distinct_and_deterministic() {
        let k = keys();
        let all = [k.c2s, k.s2c, k.udp_c2s, k.udp_s2c];
        for i in 0..4 {
            for j in i + 1..4 {
                assert_ne!(all[i], all[j]);
            }
        }
        let k2 = keys();
        assert_eq!(k.c2s, k2.c2s);
        assert_eq!(k.udp_s2c, k2.udp_s2c);
        // 不同 nonce / 不同密钥 → 不同会话密钥
        assert_ne!(derive(&[7u8; 32], &[1u8; 32], &[3u8; 32]).c2s, k.c2s);
        assert_ne!(derive(&[8u8; 32], &[1u8; 32], &[2u8; 32]).c2s, k.c2s);
    }

    #[test]
    fn tcp_roundtrip() {
        let k = keys();
        let mut s = Sealer::new(&k.c2s);
        let mut o = Opener::new(&k.c2s);
        for i in 0..5u8 {
            let msg = vec![i; i as usize * 10];
            let ct = s.seal(&msg);
            assert_eq!(ct.len(), msg.len() + 16);
            assert_eq!(o.open(&ct).unwrap(), msg);
        }
    }

    #[test]
    fn tcp_tamper_rejected() {
        let k = keys();
        let mut s = Sealer::new(&k.c2s);
        let mut o = Opener::new(&k.c2s);
        let mut ct = s.seal(b"hello");
        ct[0] ^= 1;
        assert!(o.open(&ct).is_none());
        ct[0] ^= 1;
        // 计数器没有因失败前进
        assert_eq!(o.open(&ct).unwrap(), b"hello");
        // 截断
        let ct2 = s.seal(b"world");
        assert!(o.open(&ct2[..ct2.len() - 1]).is_none());
        assert!(o.open(&[]).is_none());
    }

    #[test]
    fn tcp_replay_and_reorder_rejected() {
        let k = keys();
        let mut s = Sealer::new(&k.c2s);
        let mut o = Opener::new(&k.c2s);
        let a = s.seal(b"a");
        let b = s.seal(b"b");
        let c = s.seal(b"c");
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
        assert!(o.open(&s.seal(b"x")).is_none());
        let mut us = UdpSealer::new(&k.udp_c2s);
        let mut uo = UdpOpener::new(&k.udp_s2c);
        assert!(uo.open(&us.seal(b"x")).is_none());
        // TCP 密钥不能用于 UDP
        let mut uo2 = UdpOpener::new(&k.c2s);
        assert!(uo2.open(&UdpSealer::new(&k.udp_c2s).seal(b"x")).is_none());
    }

    #[test]
    fn udp_only_increasing() {
        let k = keys();
        let mut s = UdpSealer::new(&k.udp_c2s);
        let mut o = UdpOpener::new(&k.udp_c2s);
        let p0 = s.seal(b"0");
        let p1 = s.seal(b"1");
        let p2 = s.seal(b"2");
        let p3 = s.seal(b"3");
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
        let mut p = s.seal(b"move");
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
}
