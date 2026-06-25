//! G.711 µ-law (PCMU) — codifica/decodifica campione per campione.
//!
//! Riferimento: implementazione classica di Sun (g711.c). Puro, niente I/O,
//! niente allocazioni: un i16 PCM <-> un byte µ-law. 8 kHz, 20 ms = 160 byte.

const BIAS: i32 = 0x84;
const CLIP: i32 = 32635;

/// PCM lineare 16 bit -> µ-law.
pub fn encode(sample: i16) -> u8 {
    let mut s = sample as i32;
    let sign = if s < 0 {
        s = -s;
        0x80
    } else {
        0
    };
    if s > CLIP {
        s = CLIP;
    }
    s += BIAS;

    // Esponente: posizione del bit piu' alto nel range 7..=14.
    let mut exponent: i32 = 7;
    let mut mask = 0x4000;
    while exponent > 0 && (s & mask) == 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = (s >> (exponent + 3)) & 0x0F;
    !(sign | (exponent << 4) | mantissa) as u8
}

/// µ-law -> PCM lineare 16 bit.
pub fn decode(ulaw: u8) -> i16 {
    const EXP_LUT: [i32; 8] = [0, 132, 396, 924, 1980, 4092, 8316, 16764];
    let u = !ulaw;
    let sign = u & 0x80;
    let exponent = ((u >> 4) & 0x07) as usize;
    let mantissa = (u & 0x0F) as i32;
    let mut sample = EXP_LUT[exponent] + (mantissa << (exponent as i32 + 3));
    if sign != 0 {
        sample = -sample;
    }
    sample as i16
}

/// Comodita': codifica un blocco PCM in µ-law.
pub fn encode_block(pcm: &[i16], out: &mut Vec<u8>) {
    out.extend(pcm.iter().map(|&s| encode(s)));
}

/// Comodita': decodifica un blocco µ-law in PCM.
pub fn decode_block(ulaw: &[u8], out: &mut Vec<i16>) {
    out.extend(ulaw.iter().map(|&b| decode(b)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_is_close() {
        // µ-law e' lossy ma monotono: il roundtrip deve restare "vicino".
        for s in (-32000..=32000).step_by(257) {
            let back = decode(encode(s as i16));
            let err = (s - back as i32).abs();
            // tolleranza generosa: µ-law comprime forte sugli alti
            assert!(err <= (s.abs() / 8 + 256), "s={s} back={back} err={err}");
        }
    }

    #[test]
    fn zero_and_silence() {
        // il silenzio deve restare piccolo
        assert!(decode(encode(0)).abs() < 256);
    }
}
