//! `iax2` — implementazione nativa del protocollo Inter-Asterisk eXchange v2
//! (RFC 5456) in Rust puro.
//!
//! Il nucleo (`frame`, `ie`, `consts`, `g711`) e' **sans-io** e senza
//! dipendenze pesanti. Le parti opzionali stanno dietro a feature:
//! - `dsp`   -> `resample` (resampling polifase sinc, via rubato)
//! - `audio` -> `audio` (I/O audio cross-platform, via cpal); implica `dsp`+`net`
//! - `net`   -> abilita tokio per i binari di esempio (driver UDP)

pub mod client;
pub mod consts;
pub mod frame;
pub mod g711;
pub mod ie;
pub mod jitter;

#[cfg(feature = "dsp")]
pub mod resample;

#[cfg(feature = "audio")]
pub mod audio;

pub use client::{Command, Config, Event, PbxClient};
pub use frame::{FullFrame, MiniFrame};
pub use ie::Ie;
pub use jitter::{JitterBuffer, Pull};

/// Risposta MD5 challenge-response: `md5_hex(challenge + secret)`,
/// stringa esadecimale minuscola di 32 caratteri (semantica chan_iax2).
pub fn md5_response(challenge: &str, secret: &str) -> String {
    let mut input = String::with_capacity(challenge.len() + secret.len());
    input.push_str(challenge);
    input.push_str(secret);
    format!("{:x}", md5::compute(input.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_known_vector() {
        let r = md5_response("abc", "secret");
        assert_eq!(r.len(), 32);
        assert!(r.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
