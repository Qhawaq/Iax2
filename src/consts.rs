//! Costanti di protocollo IAX2, da RFC 5456.
//!
//! Numeri "as-is" dallo standard. Tenuti separati cosi' il resto del codice
//! resta leggibile e questi valori sono verificabili a colpo d'occhio contro
//! la RFC / chan_iax2.c.

/// Frame types (full frame, byte 10).
pub mod frametype {
    pub const DTMF: u8 = 0x01;
    pub const VOICE: u8 = 0x02;
    pub const VIDEO: u8 = 0x03;
    pub const CONTROL: u8 = 0x04;
    pub const NULL: u8 = 0x05;
    pub const IAX: u8 = 0x06; // controllo di protocollo (NEW, REGREQ, ...)
    pub const TEXT: u8 = 0x07;
    pub const IMAGE: u8 = 0x08;
    pub const HTML: u8 = 0x09;
    pub const CNG: u8 = 0x0A;
}

/// Subclass per frame di tipo IAX (frametype == IAX).
pub mod iax {
    pub const NEW: u8 = 0x01;
    pub const PING: u8 = 0x02;
    pub const PONG: u8 = 0x03;
    pub const ACK: u8 = 0x04;
    pub const HANGUP: u8 = 0x05;
    pub const REJECT: u8 = 0x06;
    pub const ACCEPT: u8 = 0x07;
    pub const AUTHREQ: u8 = 0x08;
    pub const AUTHREP: u8 = 0x09;
    pub const INVAL: u8 = 0x0A;
    pub const LAGRQ: u8 = 0x0B;
    pub const LAGRP: u8 = 0x0C;
    pub const REGREQ: u8 = 0x0D;
    pub const REGAUTH: u8 = 0x0E;
    pub const REGACK: u8 = 0x0F;
    pub const REGREJ: u8 = 0x10;
    pub const REGREL: u8 = 0x11;
    pub const VNAK: u8 = 0x12;
    pub const POKE: u8 = 0x1E;
    pub const CALLTOKEN: u8 = 0x28; // 40
}

/// Subclass per frame di tipo CONTROL (segnalazione di stato chiamata).
/// Valori AST_CONTROL_*.
pub mod control {
    pub const HANGUP: u8 = 0x01;
    pub const RING: u8 = 0x02;
    pub const RINGING: u8 = 0x03;
    pub const ANSWER: u8 = 0x04;
    pub const BUSY: u8 = 0x05;
    pub const CONGESTION: u8 = 0x08;
    pub const PROGRESS: u8 = 0x0E;
    pub const PROCEEDING: u8 = 0x0F;
    pub const HOLD: u8 = 0x10; // AST_CONTROL_HOLD — attesa con MOH lato PBX
    pub const UNHOLD: u8 = 0x11; // AST_CONTROL_UNHOLD
}

/// Information Element types (RFC 5456 §8.6).
pub mod ie {
    pub const CALLED_NUMBER: u8 = 0x01;
    pub const CALLING_NUMBER: u8 = 0x02;
    pub const CALLING_NAME: u8 = 0x04;
    pub const CALLED_CONTEXT: u8 = 0x05;
    pub const USERNAME: u8 = 0x06;
    pub const PASSWORD: u8 = 0x07;
    pub const CAPABILITY: u8 = 0x08;
    pub const FORMAT: u8 = 0x09;
    pub const VERSION: u8 = 0x0B;
    pub const AUTHMETHODS: u8 = 0x0E;
    pub const CHALLENGE: u8 = 0x0F;
    pub const MD5_RESULT: u8 = 0x10;
    pub const RSA_RESULT: u8 = 0x11;
    pub const APPARENT_ADDR: u8 = 0x12;
    pub const REFRESH: u8 = 0x13;
    pub const CAUSE: u8 = 0x16;
    pub const DATETIME: u8 = 0x1F;
    pub const CAUSECODE: u8 = 0x2A; // 42 — Asterisk usa spesso questo, non CAUSE
    pub const CALLTOKEN: u8 = 0x36; // 54 — anti-spoofing dei NEW/REGREQ
}

/// Bitmask metodi di autenticazione (IE AUTHMETHODS).
pub mod authmethod {
    pub const PLAINTEXT: u16 = 0x0001;
    pub const MD5: u16 = 0x0002;
    pub const RSA: u16 = 0x0004;
}

/// Bitmask formati audio (IE CAPABILITY / FORMAT).
pub mod format {
    pub const G723_1: u32 = 1 << 0;
    pub const GSM: u32 = 1 << 1;
    pub const ULAW: u32 = 1 << 2;
    pub const ALAW: u32 = 1 << 3;
    pub const SLINEAR: u32 = 1 << 6;
    pub const G729: u32 = 1 << 8;
    pub const SPEEX: u32 = 1 << 9;
}

/// Versione di protocollo dichiarata nei NEW (IE VERSION).
pub const IAX_PROTO_VERSION: u16 = 2;
