//! Information Elements: codifica/decodifica pura.
//!
//! Ogni IE sul filo e' [type:1][len:1][data:len]. Nessuna allocazione magica,
//! nessun I/O. Solo byte in, byte out — cosi' e' testabile a tavolino.

/// Un IE decodificato: tipo + payload grezzo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ie {
    pub kind: u8,
    pub data: Vec<u8>,
}

impl Ie {
    pub fn new(kind: u8, data: impl Into<Vec<u8>>) -> Self {
        Ie { kind, data: data.into() }
    }

    /// IE con valore u16 big-endian (REFRESH, AUTHMETHODS, ...).
    pub fn u16(kind: u8, v: u16) -> Self {
        Ie { kind, data: v.to_be_bytes().to_vec() }
    }

    /// IE con valore u32 big-endian (CAPABILITY, FORMAT, ...).
    pub fn u32(kind: u8, v: u32) -> Self {
        Ie { kind, data: v.to_be_bytes().to_vec() }
    }

    /// IE stringa (USERNAME, MD5_RESULT, CHALLENGE, ...).
    pub fn str(kind: u8, s: &str) -> Self {
        Ie { kind, data: s.as_bytes().to_vec() }
    }

    /// IE vuoto (es. CALLTOKEN nel primo invio, per segnalare il supporto).
    pub fn empty(kind: u8) -> Self {
        Ie { kind, data: Vec::new() }
    }

    pub fn as_str(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }

    pub fn as_u16(&self) -> Option<u16> {
        if self.data.len() == 2 {
            Some(u16::from_be_bytes([self.data[0], self.data[1]]))
        } else {
            None
        }
    }

    pub fn as_u32(&self) -> Option<u32> {
        if self.data.len() == 4 {
            Some(u32::from_be_bytes([self.data[0], self.data[1], self.data[2], self.data[3]]))
        } else {
            None
        }
    }
}

/// Serializza una lista di IE in coda al frame.
pub fn encode(ies: &[Ie], out: &mut Vec<u8>) {
    for ie in ies {
        // len e' un byte: il payload di un singolo IE non puo' superare 255.
        debug_assert!(ie.data.len() <= u8::MAX as usize, "IE troppo lungo");
        out.push(ie.kind);
        out.push(ie.data.len() as u8);
        out.extend_from_slice(&ie.data);
    }
}

/// Decodifica gli IE dal payload di un full frame.
/// Ritorna None se il buffer e' troncato (len dichiarata oltre i byte reali).
pub fn decode(mut buf: &[u8]) -> Option<Vec<Ie>> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        if buf.len() < 2 {
            return None;
        }
        let kind = buf[0];
        let len = buf[1] as usize;
        if buf.len() < 2 + len {
            return None;
        }
        out.push(Ie::new(kind, &buf[2..2 + len]));
        buf = &buf[2 + len..];
    }
    Some(out)
}

/// Cerca il primo IE di un certo tipo.
pub fn find(ies: &[Ie], kind: u8) -> Option<&Ie> {
    ies.iter().find(|ie| ie.kind == kind)
}
