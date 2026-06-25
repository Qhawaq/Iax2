//! Frame IAX2: full frame e mini frame, codifica/decodifica pura.
//!
//! Full frame (RFC 5456 §8.1):
//!   [F|src_call:15][R|dst_call:15][timestamp:32][oseq:8][iseq:8]
//!   [frametype:8][C|subclass:7][corpo...]
//!
//! Il "corpo" e' una lista di IE per i frame di comando (IAX/CONTROL/...),
//! ma e' payload audio GREZZO per i frame media (VOICE/VIDEO/...). Per questo
//! il decode si biforca sul frametype.
//!
//! Mini frame audio (§8.2):
//!   [0|src_call:15][timestamp:16][payload...]

use crate::consts::frametype;
use crate::ie::{self, Ie};

const FULL_FRAME_BIT: u16 = 0x8000;
const RETRANSMIT_BIT: u16 = 0x8000;
const CALL_MASK: u16 = 0x7FFF;
const SUBCLASS_C_BIT: u8 = 0x80;

/// Vero se il frametype trasporta payload media grezzo (no IE).
fn is_media(frametype: u8) -> bool {
    matches!(
        frametype,
        frametype::DTMF
            | frametype::VOICE
            | frametype::VIDEO
            | frametype::TEXT
            | frametype::IMAGE
            | frametype::HTML
            | frametype::CNG
    )
}

#[derive(Debug, Clone)]
pub struct FullFrame {
    pub src_call: u16,
    pub dst_call: u16,
    pub retransmit: bool,
    pub timestamp: u32,
    pub oseq: u8,
    pub iseq: u8,
    pub frametype: u8,
    pub subclass: u8,
    /// IE (solo per frame di comando).
    pub ies: Vec<Ie>,
    /// Payload grezzo (solo per frame media in ingresso).
    pub media_payload: Vec<u8>,
}

impl FullFrame {
    pub fn new(
        src_call: u16,
        dst_call: u16,
        timestamp: u32,
        oseq: u8,
        iseq: u8,
        frametype: u8,
        subclass: u8,
        ies: Vec<Ie>,
    ) -> Self {
        FullFrame {
            src_call,
            dst_call,
            retransmit: false,
            timestamp,
            oseq,
            iseq,
            frametype,
            subclass,
            ies,
            media_payload: Vec::new(),
        }
    }

    fn header_into(&self, out: &mut Vec<u8>) {
        let src = FULL_FRAME_BIT | (self.src_call & CALL_MASK);
        let mut dst = self.dst_call & CALL_MASK;
        if self.retransmit {
            dst |= RETRANSMIT_BIT;
        }
        out.extend_from_slice(&src.to_be_bytes());
        out.extend_from_slice(&dst.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.push(self.oseq);
        out.push(self.iseq);
        out.push(self.frametype);
        // C-bit a 0: la subclass e' il valore diretto. Per i formati audio che
        // entrano in 7 bit (ULAW=4, ALAW=8) va bene cosi'.
        out.push(self.subclass & 0x7F);
    }

    /// Codifica un frame di comando: header + IE.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + 32);
        self.header_into(&mut out);
        ie::encode(&self.ies, &mut out);
        out
    }

    /// Codifica un frame media (VOICE/...): header + payload grezzo.
    pub fn encode_media(&self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + payload.len());
        self.header_into(&mut out);
        out.extend_from_slice(payload);
        out
    }

    pub fn decode(buf: &[u8]) -> Option<FullFrame> {
        if buf.len() < 12 {
            return None;
        }
        let w0 = u16::from_be_bytes([buf[0], buf[1]]);
        let w1 = u16::from_be_bytes([buf[2], buf[3]]);
        if w0 & FULL_FRAME_BIT == 0 {
            return None; // mini frame
        }
        let frametype = buf[10];
        let body = &buf[12..];

        let (ies, media_payload) = if is_media(frametype) {
            (Vec::new(), body.to_vec())
        } else {
            (ie::decode(body)?, Vec::new())
        };

        Some(FullFrame {
            src_call: w0 & CALL_MASK,
            dst_call: w1 & CALL_MASK,
            retransmit: w1 & RETRANSMIT_BIT != 0,
            timestamp: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            oseq: buf[8],
            iseq: buf[9],
            frametype,
            subclass: buf[11] & !SUBCLASS_C_BIT,
            ies,
            media_payload,
        })
    }
}

#[derive(Debug, Clone)]
pub struct MiniFrame {
    pub src_call: u16,
    pub timestamp16: u16,
    pub payload: Vec<u8>,
}

impl MiniFrame {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.payload.len());
        out.extend_from_slice(&(self.src_call & CALL_MASK).to_be_bytes());
        out.extend_from_slice(&self.timestamp16.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode(buf: &[u8]) -> Option<MiniFrame> {
        if buf.len() < 4 {
            return None;
        }
        let w0 = u16::from_be_bytes([buf[0], buf[1]]);
        if w0 & FULL_FRAME_BIT != 0 {
            return None; // full frame
        }
        Some(MiniFrame {
            src_call: w0 & CALL_MASK,
            timestamp16: u16::from_be_bytes([buf[2], buf[3]]),
            payload: buf[4..].to_vec(),
        })
    }
}

/// Vero se il datagramma e' un full frame (bit F del primo word).
pub fn is_full_frame(buf: &[u8]) -> bool {
    buf.len() >= 2 && (buf[0] & 0x80 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::{frametype, iax, ie as iec};

    #[test]
    fn command_frame_roundtrip() {
        let f = FullFrame::new(
            5, 0, 1234, 0, 0, frametype::IAX, iax::REGREQ,
            vec![Ie::str(iec::USERNAME, "8001"), Ie::u16(iec::REFRESH, 60)],
        );
        let d = FullFrame::decode(&f.encode()).expect("decode");
        assert_eq!(d.subclass, iax::REGREQ);
        assert_eq!(ie::find(&d.ies, iec::USERNAME).unwrap().as_str(), "8001");
        assert!(d.media_payload.is_empty());
    }

    #[test]
    fn voice_full_frame_keeps_raw_payload() {
        use crate::consts::format;
        let audio = vec![0x7Fu8; 160];
        let f = FullFrame::new(3, 7, 20, 1, 1, frametype::VOICE, format::ULAW as u8, vec![]);
        let bytes = f.encode_media(&audio);
        let d = FullFrame::decode(&bytes).expect("decode");
        assert_eq!(d.frametype, frametype::VOICE);
        assert_eq!(d.media_payload.len(), 160);
        assert!(d.ies.is_empty(), "il payload audio non va interpretato come IE");
    }

    #[test]
    fn mini_frame_roundtrip() {
        let m = MiniFrame { src_call: 7, timestamp16: 20, payload: vec![0xAA; 160] };
        let bytes = m.encode();
        assert!(!is_full_frame(&bytes));
        let d = MiniFrame::decode(&bytes).expect("decode");
        assert_eq!(d.src_call, 7);
        assert_eq!(d.payload.len(), 160);
    }
}
