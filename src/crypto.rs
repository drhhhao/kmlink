//! 加密：两台共用一个 32 字节预共享密钥（config.key）。
//!
//! 握手（明文）：客户端发 MAGIC + 32 字节随机 nonce_c；服务端回 MAGIC + 32 字节随机 nonce_s。
//! 会话密钥 = HKDF-SHA256(ikm = psk, salt = nonce_c ‖ nonce_s)，info 分别为
//!   "kmlink c2s" / "kmlink s2c" / "kmlink udp c2s" / "kmlink udp s2c"，各 32 字节。
//! 之后每个方向一个 ChaCha20-Poly1305：
//!   TCP：nonce = 4 字节 0 ‖ u64 LE 计数器（从 0 开始，每帧 +1，接收方要求严格递增 —— 防重放/乱序）。
//!   UDP：包 = u64 LE 计数器 ‖ 密文；nonce = 4 字节 0 ‖ 计数器；接收方只接受比见过的最大值更大的计数器。
//! 第一帧（Hello）能解开即证明对方有同一个密钥。

pub struct SessionKeys {
    pub c2s: [u8; 32],
    pub s2c: [u8; 32],
    pub udp_c2s: [u8; 32],
    pub udp_s2c: [u8; 32],
}

pub fn derive(psk: &[u8; 32], nonce_c: &[u8; 32], nonce_s: &[u8; 32]) -> SessionKeys {
    todo!("crypto::derive")
}

/// TCP 方向加密器。
pub struct Sealer {}
impl Sealer {
    pub fn new(key: &[u8; 32]) -> Self {
        todo!()
    }
    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        todo!()
    }
}

/// TCP 方向解密器（计数器严格递增）。
pub struct Opener {}
impl Opener {
    pub fn new(key: &[u8; 32]) -> Self {
        todo!()
    }
    pub fn open(&mut self, ciphertext: &[u8]) -> Option<Vec<u8>> {
        todo!()
    }
}

/// UDP 发送：返回完整的包（计数器 ‖ 密文）。
pub struct UdpSealer {}
impl UdpSealer {
    pub fn new(key: &[u8; 32]) -> Self {
        todo!()
    }
    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        todo!()
    }
}

/// UDP 接收：只接受更大的计数器。
pub struct UdpOpener {}
impl UdpOpener {
    pub fn new(key: &[u8; 32]) -> Self {
        todo!()
    }
    pub fn open(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        todo!()
    }
}

pub fn random32() -> [u8; 32] {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("getrandom");
    b
}
