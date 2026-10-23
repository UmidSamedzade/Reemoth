//! Minimal STUN (RFC 8489) message codec: just what binding discovery and the
//! TURN client need. ICE connectivity checks are str0m's business.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use sha1::Sha1;

pub const MAGIC_COOKIE: u32 = 0x2112_A442;
const HEADER_LEN: usize = 20;
const FINGERPRINT_XOR: u32 = 0x5354_554E;

pub mod method {
    pub const BINDING: u16 = 0x001;
    pub const ALLOCATE: u16 = 0x003;
    pub const REFRESH: u16 = 0x004;
    pub const SEND: u16 = 0x006;
    pub const DATA: u16 = 0x007;
    pub const CREATE_PERMISSION: u16 = 0x008;
}

pub mod attr {
    pub const USERNAME: u16 = 0x0006;
    pub const MESSAGE_INTEGRITY: u16 = 0x0008;
    pub const ERROR_CODE: u16 = 0x0009;
    pub const LIFETIME: u16 = 0x000D;
    pub const XOR_PEER_ADDRESS: u16 = 0x0012;
    pub const DATA: u16 = 0x0013;
    pub const REALM: u16 = 0x0014;
    pub const NONCE: u16 = 0x0015;
    pub const XOR_RELAYED_ADDRESS: u16 = 0x0016;
    pub const REQUESTED_TRANSPORT: u16 = 0x0019;
    pub const XOR_MAPPED_ADDRESS: u16 = 0x0020;
    pub const SOFTWARE: u16 = 0x8022;
    pub const FINGERPRINT: u16 = 0x8028;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Request,
    Indication,
    Success,
    Error,
}

impl Class {
    fn bits(self) -> u16 {
        match self {
            Self::Request => 0b00,
            Self::Indication => 0b01,
            Self::Success => 0b10,
            Self::Error => 0b11,
        }
    }

    fn from_bits(b: u16) -> Self {
        match b {
            0b00 => Self::Request,
            0b01 => Self::Indication,
            0b10 => Self::Success,
            _ => Self::Error,
        }
    }
}

fn message_type(class: Class, method: u16) -> u16 {
    let c = class.bits();
    (method & 0x000F)
        | ((method & 0x0070) << 1)
        | ((method & 0x0F80) << 2)
        | ((c & 0b01) << 4)
        | ((c & 0b10) << 7)
}

fn split_type(t: u16) -> (Class, u16) {
    let method = (t & 0x000F) | ((t >> 1) & 0x0070) | ((t >> 2) & 0x0F80);
    let class = ((t >> 4) & 0b01) | ((t >> 7) & 0b10);
    (Class::from_bits(class), method)
}

pub type TransactionId = [u8; 12];

pub fn new_transaction_id() -> TransactionId {
    let mut id = [0u8; 12];
    getrandom::getrandom(&mut id).expect("OS randomness is available");
    id
}

/// Cheap check used to demultiplex a UDP socket: STUN starts with two zero
/// bits and carries the magic cookie.
pub fn is_stun(buf: &[u8]) -> bool {
    buf.len() >= HEADER_LEN
        && buf[0] < 4
        && u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) == MAGIC_COOKIE
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub class: Class,
    pub method: u16,
    pub transaction_id: TransactionId,
    pub attrs: Vec<(u16, Vec<u8>)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StunError {
    Malformed(&'static str),
}

impl std::fmt::Display for StunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "malformed STUN message: {why}"),
        }
    }
}

impl std::error::Error for StunError {}

impl Message {
    pub fn new(class: Class, method: u16, transaction_id: TransactionId) -> Self {
        Self {
            class,
            method,
            transaction_id,
            attrs: Vec::new(),
        }
    }

    pub fn with(mut self, attr: u16, value: impl Into<Vec<u8>>) -> Self {
        self.attrs.push((attr, value.into()));
        self
    }

    pub fn with_xor_addr(self, attr: u16, addr: SocketAddr) -> Self {
        let value = encode_xor_addr(addr, &self.transaction_id);
        self.with(attr, value)
    }

    pub fn get(&self, attr: u16) -> Option<&[u8]> {
        self.attrs
            .iter()
            .find(|(t, _)| *t == attr)
            .map(|(_, v)| v.as_slice())
    }

    pub fn get_str(&self, attr: u16) -> Option<&str> {
        self.get(attr).and_then(|v| std::str::from_utf8(v).ok())
    }

    pub fn xor_addr(&self, attr: u16) -> Option<SocketAddr> {
        decode_xor_addr(self.get(attr)?, &self.transaction_id)
    }

    /// `(code, reason)` from ERROR-CODE.
    pub fn error_code(&self) -> Option<(u16, String)> {
        let v = self.get(attr::ERROR_CODE)?;
        if v.len() < 4 {
            return None;
        }
        let code = u16::from(v[2] & 0x07) * 100 + u16::from(v[3]);
        Some((code, String::from_utf8_lossy(&v[4..]).into_owned()))
    }

    pub fn lifetime(&self) -> Option<u32> {
        let v = self.get(attr::LIFETIME)?;
        Some(u32::from_be_bytes(v.try_into().ok()?))
    }

    /// Serializes, optionally appending MESSAGE-INTEGRITY (keyed with
    /// `integrity_key`) and FINGERPRINT.
    pub fn encode(&self, integrity_key: Option<&[u8]>, fingerprint: bool) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_LEN + 64);
        buf.extend_from_slice(&message_type(self.class, self.method).to_be_bytes());
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        buf.extend_from_slice(&self.transaction_id);
        for (t, v) in &self.attrs {
            push_attr(&mut buf, *t, v);
        }
        if let Some(key) = integrity_key {
            let len = buf.len() - HEADER_LEN + 24;
            set_len(&mut buf, len);
            let mac = hmac_sha1(key, &buf);
            push_attr(&mut buf, attr::MESSAGE_INTEGRITY, &mac);
        }
        if fingerprint {
            let len = buf.len() - HEADER_LEN + 8;
            set_len(&mut buf, len);
            let crc = crc32fast::hash(&buf) ^ FINGERPRINT_XOR;
            push_attr(&mut buf, attr::FINGERPRINT, &crc.to_be_bytes());
        }
        let len = buf.len() - HEADER_LEN;
        set_len(&mut buf, len);
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self, StunError> {
        if !is_stun(buf) {
            return Err(StunError::Malformed("not STUN"));
        }
        let len = usize::from(u16::from_be_bytes([buf[2], buf[3]]));
        if len % 4 != 0 || buf.len() < HEADER_LEN + len {
            return Err(StunError::Malformed("bad length"));
        }
        let (class, method) = split_type(u16::from_be_bytes([buf[0], buf[1]]));
        let mut msg = Self::new(class, method, buf[8..20].try_into().expect("12 bytes"));
        let body = &buf[HEADER_LEN..HEADER_LEN + len];
        let mut i = 0;
        while i < body.len() {
            if i + 4 > body.len() {
                return Err(StunError::Malformed("truncated attribute header"));
            }
            let t = u16::from_be_bytes([body[i], body[i + 1]]);
            let l = usize::from(u16::from_be_bytes([body[i + 2], body[i + 3]]));
            let start = i + 4;
            if start + l > body.len() {
                return Err(StunError::Malformed("truncated attribute"));
            }
            msg.attrs.push((t, body[start..start + l].to_vec()));
            i = start + l.next_multiple_of(4);
        }
        Ok(msg)
    }
}

/// Checks MESSAGE-INTEGRITY of a raw message.
pub fn verify_integrity(buf: &[u8], key: &[u8]) -> bool {
    let Some(mi_offset) = find_attr_offset(buf, attr::MESSAGE_INTEGRITY) else {
        return false;
    };
    let Some(mac) = buf.get(mi_offset + 4..mi_offset + 24) else {
        return false;
    };
    let mut covered = buf[..mi_offset].to_vec();
    set_len(&mut covered, mi_offset - HEADER_LEN + 24);
    let mut m = Hmac::<Sha1>::new_from_slice(key).expect("any key length works");
    m.update(&covered);
    m.verify_slice(mac).is_ok()
}

/// Checks FINGERPRINT of a raw message (true if absent is not accepted).
pub fn verify_fingerprint(buf: &[u8]) -> bool {
    let Some(offset) = find_attr_offset(buf, attr::FINGERPRINT) else {
        return false;
    };
    let Some(value) = buf.get(offset + 4..offset + 8) else {
        return false;
    };
    let crc = crc32fast::hash(&buf[..offset]) ^ FINGERPRINT_XOR;
    value == crc.to_be_bytes()
}

/// TURN long-term credential key: MD5(username:realm:password).
pub fn long_term_key(username: &str, realm: &str, password: &str) -> Vec<u8> {
    Md5::digest(format!("{username}:{realm}:{password}")).to_vec()
}

fn find_attr_offset(buf: &[u8], wanted: u16) -> Option<usize> {
    if !is_stun(buf) {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([buf[2], buf[3]]));
    let end = (HEADER_LEN + len).min(buf.len());
    let mut i = HEADER_LEN;
    while i + 4 <= end {
        let t = u16::from_be_bytes([buf[i], buf[i + 1]]);
        if t == wanted {
            return Some(i);
        }
        let l = usize::from(u16::from_be_bytes([buf[i + 2], buf[i + 3]]));
        i += 4 + l.next_multiple_of(4);
    }
    None
}

fn push_attr(buf: &mut Vec<u8>, t: u16, v: &[u8]) {
    buf.extend_from_slice(&t.to_be_bytes());
    buf.extend_from_slice(&(v.len() as u16).to_be_bytes());
    buf.extend_from_slice(v);
    buf.resize(buf.len().next_multiple_of(4), 0);
}

fn set_len(buf: &mut [u8], len: usize) {
    buf[2..4].copy_from_slice(&(len as u16).to_be_bytes());
}

fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    let mut m = Hmac::<Sha1>::new_from_slice(key).expect("any key length works");
    m.update(data);
    m.finalize().into_bytes().into()
}

fn encode_xor_addr(addr: SocketAddr, txid: &TransactionId) -> Vec<u8> {
    let port = addr.port() ^ (MAGIC_COOKIE >> 16) as u16;
    let mut v = vec![0];
    match addr.ip() {
        IpAddr::V4(ip) => {
            v.push(0x01);
            v.extend_from_slice(&port.to_be_bytes());
            v.extend_from_slice(&(u32::from(ip) ^ MAGIC_COOKIE).to_be_bytes());
        }
        IpAddr::V6(ip) => {
            v.push(0x02);
            v.extend_from_slice(&port.to_be_bytes());
            let mask = xor_mask_v6(txid);
            v.extend(ip.octets().iter().zip(mask).map(|(a, b)| a ^ b));
        }
    }
    v
}

fn decode_xor_addr(v: &[u8], txid: &TransactionId) -> Option<SocketAddr> {
    if v.len() < 4 {
        return None;
    }
    let port = u16::from_be_bytes([v[2], v[3]]) ^ (MAGIC_COOKIE >> 16) as u16;
    let ip = match v[1] {
        0x01 if v.len() >= 8 => {
            let raw = u32::from_be_bytes(v[4..8].try_into().ok()?) ^ MAGIC_COOKIE;
            IpAddr::V4(Ipv4Addr::from(raw))
        }
        0x02 if v.len() >= 20 => {
            let mask = xor_mask_v6(txid);
            let mut octets = [0u8; 16];
            for (i, o) in octets.iter_mut().enumerate() {
                *o = v[4 + i] ^ mask[i];
            }
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

fn xor_mask_v6(txid: &TransactionId) -> [u8; 16] {
    let mut mask = [0u8; 16];
    mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    mask[4..].copy_from_slice(txid);
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 5769 test vectors.
    const SHORT_TERM_PASSWORD: &[u8] = b"VOkJxbRl1RmTxUk/WvJxBt";

    const SAMPLE_REQUEST: [u8; 108] = [
        0x00, 0x01, 0x00, 0x58, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6,
        0x86, 0xfa, 0x87, 0xdf, 0xae, 0x80, 0x22, 0x00, 0x10, 0x53, 0x54, 0x55, 0x4e, 0x20, 0x74,
        0x65, 0x73, 0x74, 0x20, 0x63, 0x6c, 0x69, 0x65, 0x6e, 0x74, 0x00, 0x24, 0x00, 0x04, 0x6e,
        0x00, 0x01, 0xff, 0x80, 0x29, 0x00, 0x08, 0x93, 0x2f, 0xf9, 0xb1, 0x51, 0x26, 0x3b, 0x36,
        0x00, 0x06, 0x00, 0x09, 0x65, 0x76, 0x74, 0x6a, 0x3a, 0x68, 0x36, 0x76, 0x59, 0x20, 0x20,
        0x20, 0x00, 0x08, 0x00, 0x14, 0x9a, 0xea, 0xa7, 0x0c, 0xbf, 0xd8, 0xcb, 0x56, 0x78, 0x1e,
        0xf2, 0xb5, 0xb2, 0xd3, 0xf2, 0x49, 0xc1, 0xb5, 0x71, 0xa2, 0x80, 0x28, 0x00, 0x04, 0xe5,
        0x7a, 0x3b, 0xcf,
    ];

    const SAMPLE_IPV4_RESPONSE: [u8; 80] = [
        0x01, 0x01, 0x00, 0x3c, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6,
        0x86, 0xfa, 0x87, 0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74, 0x20, 0x76,
        0x65, 0x63, 0x74, 0x6f, 0x72, 0x20, 0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1,
        0x12, 0xa6, 0x43, 0x00, 0x08, 0x00, 0x14, 0x2b, 0x91, 0xf5, 0x99, 0xfd, 0x9e, 0x90, 0xc3,
        0x8c, 0x74, 0x89, 0xf9, 0x2a, 0xf9, 0xba, 0x53, 0xf0, 0x6b, 0xe7, 0xd7, 0x80, 0x28, 0x00,
        0x04, 0xc0, 0x7d, 0x4c, 0x96,
    ];

    const SAMPLE_IPV6_RESPONSE: [u8; 92] = [
        0x01, 0x01, 0x00, 0x48, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6,
        0x86, 0xfa, 0x87, 0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74, 0x20, 0x76,
        0x65, 0x63, 0x74, 0x6f, 0x72, 0x20, 0x00, 0x20, 0x00, 0x14, 0x00, 0x02, 0xa1, 0x47, 0x01,
        0x13, 0xa9, 0xfa, 0xa5, 0xd3, 0xf1, 0x79, 0xbc, 0x25, 0xf4, 0xb5, 0xbe, 0xd2, 0xb9, 0xd9,
        0x00, 0x08, 0x00, 0x14, 0xa3, 0x82, 0x95, 0x4e, 0x4b, 0xe6, 0x7b, 0xf1, 0x17, 0x84, 0xc9,
        0x7c, 0x82, 0x92, 0xc2, 0x75, 0xbf, 0xe3, 0xed, 0x41, 0x80, 0x28, 0x00, 0x04, 0xc8, 0xfb,
        0x0b, 0x4c,
    ];

    const SAMPLE_LONG_TERM_REQUEST: [u8; 116] = [
        0x00, 0x01, 0x00, 0x60, 0x21, 0x12, 0xa4, 0x42, 0x78, 0xad, 0x34, 0x33, 0xc6, 0xad, 0x72,
        0xc0, 0x29, 0xda, 0x41, 0x2e, 0x00, 0x06, 0x00, 0x12, 0xe3, 0x83, 0x9e, 0xe3, 0x83, 0x88,
        0xe3, 0x83, 0xaa, 0xe3, 0x83, 0x83, 0xe3, 0x82, 0xaf, 0xe3, 0x82, 0xb9, 0x00, 0x00, 0x00,
        0x15, 0x00, 0x1c, 0x66, 0x2f, 0x2f, 0x34, 0x39, 0x39, 0x6b, 0x39, 0x35, 0x34, 0x64, 0x36,
        0x4f, 0x4c, 0x33, 0x34, 0x6f, 0x4c, 0x39, 0x46, 0x53, 0x54, 0x76, 0x79, 0x36, 0x34, 0x73,
        0x41, 0x00, 0x14, 0x00, 0x0b, 0x65, 0x78, 0x61, 0x6d, 0x70, 0x6c, 0x65, 0x2e, 0x6f, 0x72,
        0x67, 0x00, 0x00, 0x08, 0x00, 0x14, 0xf6, 0x70, 0x24, 0x65, 0x6d, 0xd6, 0x4a, 0x3e, 0x02,
        0xb8, 0xe0, 0x71, 0x2e, 0x85, 0xc9, 0xa2, 0x8c, 0xa8, 0x96, 0x66,
    ];

    #[test]
    fn rfc5769_sample_request() {
        let msg = Message::decode(&SAMPLE_REQUEST).unwrap();
        assert_eq!((msg.class, msg.method), (Class::Request, method::BINDING));
        assert_eq!(msg.get_str(attr::USERNAME), Some("evtj:h6vY"));
        assert!(verify_integrity(&SAMPLE_REQUEST, SHORT_TERM_PASSWORD));
        assert!(!verify_integrity(&SAMPLE_REQUEST, b"wrong"));
        assert!(verify_fingerprint(&SAMPLE_REQUEST));
    }

    #[test]
    fn rfc5769_ipv4_response() {
        let msg = Message::decode(&SAMPLE_IPV4_RESPONSE).unwrap();
        assert_eq!((msg.class, msg.method), (Class::Success, method::BINDING));
        assert_eq!(
            msg.xor_addr(attr::XOR_MAPPED_ADDRESS),
            Some("192.0.2.1:32853".parse().unwrap())
        );
        assert!(verify_integrity(&SAMPLE_IPV4_RESPONSE, SHORT_TERM_PASSWORD));
        assert!(verify_fingerprint(&SAMPLE_IPV4_RESPONSE));
    }

    #[test]
    fn rfc5769_ipv6_response() {
        let msg = Message::decode(&SAMPLE_IPV6_RESPONSE).unwrap();
        assert_eq!(
            msg.xor_addr(attr::XOR_MAPPED_ADDRESS),
            Some(
                "[2001:db8:1234:5678:11:2233:4455:6677]:32853"
                    .parse()
                    .unwrap()
            )
        );
        assert!(verify_integrity(&SAMPLE_IPV6_RESPONSE, SHORT_TERM_PASSWORD));
    }

    #[test]
    fn rfc5769_long_term_request() {
        let msg = Message::decode(&SAMPLE_LONG_TERM_REQUEST).unwrap();
        let user = msg.get_str(attr::USERNAME).unwrap();
        assert_eq!(user, "\u{30DE}\u{30C8}\u{30EA}\u{30C3}\u{30AF}\u{30B9}");
        assert_eq!(msg.get_str(attr::REALM), Some("example.org"));
        // Password after SASLprep, as given by the RFC.
        let key = long_term_key(user, "example.org", "TheMatrIX");
        assert!(verify_integrity(&SAMPLE_LONG_TERM_REQUEST, &key));
    }

    #[test]
    fn encode_reproduces_integrity_and_fingerprint() {
        // Re-encoding the sample request's attributes (with zero padding
        // instead of the RFC's spaces) must still self-verify.
        let msg = Message::decode(&SAMPLE_REQUEST).unwrap();
        let mut plain = Message::new(msg.class, msg.method, msg.transaction_id);
        plain.attrs = msg
            .attrs
            .iter()
            .filter(|(t, _)| *t != attr::MESSAGE_INTEGRITY && *t != attr::FINGERPRINT)
            .cloned()
            .collect();
        let wire = plain.encode(Some(SHORT_TERM_PASSWORD), true);
        assert_eq!(wire.len(), SAMPLE_REQUEST.len());
        assert!(verify_integrity(&wire, SHORT_TERM_PASSWORD));
        assert!(verify_fingerprint(&wire));
        assert_eq!(Message::decode(&wire).unwrap().attrs.len(), msg.attrs.len());
    }

    #[test]
    fn type_bits_round_trip() {
        for class in [
            Class::Request,
            Class::Indication,
            Class::Success,
            Class::Error,
        ] {
            for m in [
                method::BINDING,
                method::ALLOCATE,
                method::SEND,
                method::DATA,
            ] {
                assert_eq!(split_type(message_type(class, m)), (class, m));
            }
        }
        assert_eq!(message_type(Class::Success, method::BINDING), 0x0101);
        assert_eq!(message_type(Class::Error, method::ALLOCATE), 0x0113);
        assert_eq!(message_type(Class::Indication, method::SEND), 0x0016);
    }

    #[test]
    fn xor_addr_round_trip() {
        let txid = new_transaction_id();
        for a in ["203.0.113.9:4000", "[2001:db8::1]:5"] {
            let addr: SocketAddr = a.parse().unwrap();
            assert_eq!(
                decode_xor_addr(&encode_xor_addr(addr, &txid), &txid),
                Some(addr)
            );
        }
    }

    #[test]
    fn rejects_truncated() {
        assert!(Message::decode(&SAMPLE_REQUEST[..50]).is_err());
        assert!(!is_stun(&[0x16, 0xfe, 0xfd, 0, 0, 0, 0, 0]));
    }
}
