//! Resampling polifase sinc **anti-alias**, wrapper streaming attorno a rubato.
//!
//! Sostituisce il resampler lineare dello spike: l'interpolazione lineare a un
//! punto lasciava passare aliasing (= quel fruscio di fondo). Il sinc con
//! finestra Blackman-Harris taglia l'aliasing come si deve.
//!
//! rubato `SincFixedIn` consuma `chunk` frame di ingresso per chiamata e
//! produce un numero variabile di frame in uscita; qui sopra ci mettiamo un
//! buffering streaming (feed/pull) cosi' il chiamante non si preoccupa delle
//! dimensioni dei blocchi. Mono (un canale).

use std::collections::VecDeque;

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

pub struct StreamResampler {
    rs: SincFixedIn<f32>,
    chunk: usize,
    in_buf: VecDeque<f32>,
    out_buf: VecDeque<f32>,
    scratch: Vec<f32>,
}

impl StreamResampler {
    /// `chunk` = frame di ingresso per blocco di elaborazione. Piu' grande =
    /// qualita'/efficienza migliori ma piu' latenza. ~10 ms va bene per la voce.
    pub fn new(from_rate: u32, to_rate: u32, chunk: usize) -> Self {
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            oversampling_factor: 256,
            interpolation: SincInterpolationType::Linear,
            window: WindowFunction::BlackmanHarris2,
        };
        let ratio = to_rate as f64 / from_rate as f64;
        let rs = SincFixedIn::<f32>::new(ratio, 1.1, params, chunk, 1)
            .expect("inizializzazione resampler rubato");
        StreamResampler {
            rs,
            chunk,
            in_buf: VecDeque::new(),
            out_buf: VecDeque::new(),
            scratch: Vec::with_capacity(chunk),
        }
    }

    /// Comodita': sceglie un chunk pari a ~10 ms del rate d'ingresso.
    pub fn for_rates(from_rate: u32, to_rate: u32) -> Self {
        let chunk = ((from_rate as usize) / 100).max(128);
        Self::new(from_rate, to_rate, chunk)
    }

    pub fn feed(&mut self, s: &[f32]) {
        self.in_buf.extend(s.iter().copied());
    }

    pub fn pull(&mut self, out: &mut Vec<f32>) {
        while self.in_buf.len() >= self.chunk {
            self.scratch.clear();
            self.scratch.extend(self.in_buf.drain(..self.chunk));
            match self.rs.process(&[&self.scratch], None) {
                Ok(res) => self.out_buf.extend(res[0].iter().copied()),
                Err(_) => break,
            }
        }
        out.extend(self.out_buf.drain(..));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    #[test]
    fn downsample_48k_to_8k_is_sane() {
        let mut r = StreamResampler::new(48_000, 8_000, 480);
        // 1 s di sinusoide a 1 kHz @48k (ben dentro la banda passante a 8k)
        let input: Vec<f32> = (0..48_000)
            .map(|i| (2.0 * PI * 1000.0 * i as f32 / 48_000.0).sin())
            .collect();
        r.feed(&input);
        let mut out = Vec::new();
        r.pull(&mut out);

        // ~8000 campioni (a meno del warm-up/coda nei buffer interni)
        assert!(out.len() > 7_000 && out.len() <= 8_300, "len={}", out.len());
        assert!(out.iter().all(|x| x.is_finite()));
        // il tono deve sopravvivere al filtro
        let peak = out.iter().cloned().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.5, "peak={peak}");
    }

    #[test]
    fn upsample_8k_to_48k_is_sane() {
        let mut r = StreamResampler::new(8_000, 48_000, 160);
        let input: Vec<f32> = (0..8_000)
            .map(|i| (2.0 * PI * 500.0 * i as f32 / 8_000.0).sin())
            .collect();
        r.feed(&input);
        let mut out = Vec::new();
        r.pull(&mut out);
        assert!(out.len() > 44_000 && out.len() <= 49_000, "len={}", out.len());
        assert!(out.iter().all(|x| x.is_finite()));
    }
}
