//! I/O audio cross-platform (cpal) con resampling integrato a/da 8 kHz.
//!
//! Incapsula microfono + altoparlante + resampler in un solo oggetto:
//! - `take_frames_8k()` -> i frame PCM 8 kHz catturati dal mic (gia' ricampionati);
//! - `play_ulaw()` -> decodifica G.711 in arrivo e la accoda alla riproduzione.
//!
//! Mono. `cpal::Stream` non e' `Send`: usare su un runtime tokio single-thread
//! (`flavor = "current_thread"`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

use crate::g711;
use crate::resample::StreamResampler;

/// Frequenza di lavoro IAX/G.711.
pub const SAMPLE_RATE: u32 = 8000;
/// Tetto del buffer di riproduzione (in campioni a frequenza device): ~200 ms.
const PLAY_CAP_MS: u32 = 200;
/// Tetto del buffer di cattura (~1 s) per evitare crescita illimitata.
const CAP_CAP: usize = 48_000;

pub struct AudioIo {
    _in_stream: cpal::Stream,
    _out_stream: cpal::Stream,
    cap_q: Arc<Mutex<VecDeque<f32>>>,
    play_q: Arc<Mutex<VecDeque<f32>>>,
    play_cap: usize,
    cap_rs: StreamResampler,
    play_rs: StreamResampler,
    cap_acc: Vec<i16>,
    pub in_rate: u32,
    pub out_rate: u32,
    pub in_ch: u16,
    pub out_ch: u16,
}

impl AudioIo {
    pub fn new() -> Result<Self, String> {
        let cap_q: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
        let play_q: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));

        let (in_rate, in_ch, in_stream) = build_input(cap_q.clone())?;
        let (out_rate, out_ch, out_stream) = build_output(play_q.clone())?;

        Ok(AudioIo {
            _in_stream: in_stream,
            _out_stream: out_stream,
            cap_q,
            play_q,
            play_cap: (out_rate * PLAY_CAP_MS / 1000) as usize * out_ch as usize,
            cap_rs: StreamResampler::for_rates(in_rate, SAMPLE_RATE),
            play_rs: StreamResampler::for_rates(SAMPLE_RATE, out_rate),
            cap_acc: Vec::new(),
            in_rate,
            out_rate,
            in_ch,
            out_ch,
        })
    }

    /// Estrae dal microfono tutti i frame PCM 8 kHz pronti (lunghezza `frame_len`).
    /// Il resto (< frame_len) resta in coda per la chiamata successiva.
    pub fn take_frames_8k(&mut self, frame_len: usize) -> Vec<Vec<i16>> {
        let mut chunk = Vec::new();
        if let Ok(mut q) = self.cap_q.lock() {
            chunk.extend(q.drain(..));
        }
        self.cap_rs.feed(&chunk);
        let mut out8k = Vec::new();
        self.cap_rs.pull(&mut out8k);
        for v in out8k {
            self.cap_acc.push((v.clamp(-1.0, 1.0) * 32767.0) as i16);
        }
        let mut frames = Vec::new();
        while self.cap_acc.len() >= frame_len {
            frames.push(self.cap_acc.drain(..frame_len).collect());
        }
        frames
    }

    /// Decodifica µ-law in ingresso e la accoda alla riproduzione.
    pub fn play_ulaw(&mut self, ulaw: &[u8]) {
        if ulaw.is_empty() {
            return;
        }
        let mut pcm = Vec::with_capacity(ulaw.len());
        g711::decode_block(ulaw, &mut pcm);
        self.play_pcm_8k(&pcm);
    }

    /// Accoda PCM 8 kHz (i16) alla riproduzione: ricampiona a frequenza device
    /// e accoda, scartando il piu' vecchio in overflow (meglio un buco che
    /// latenza crescente). Usato sia dalla voce decodificata sia dal PLC.
    pub fn play_pcm_8k(&mut self, pcm: &[i16]) {
        if pcm.is_empty() {
            return;
        }
        let f32s: Vec<f32> = pcm.iter().map(|&v| v as f32 / 32768.0).collect();
        self.play_rs.feed(&f32s);
        let mut out = Vec::new();
        self.play_rs.pull(&mut out);
        if let Ok(mut q) = self.play_q.lock() {
            q.extend(out);
            while q.len() > self.play_cap {
                q.pop_front();
            }
        }
    }

    /// Svuota le code (a fine chiamata).
    pub fn flush(&mut self) {
        if let Ok(mut q) = self.cap_q.lock() {
            q.clear();
        }
        if let Ok(mut q) = self.play_q.lock() {
            q.clear();
        }
        self.cap_acc.clear();
    }
}

type StreamInfo = (u32, u16, cpal::Stream);

fn build_input(cap_q: Arc<Mutex<VecDeque<f32>>>) -> Result<StreamInfo, String> {
    let host = cpal::default_host();
    let dev = host.default_input_device().ok_or("no input device")?;
    let cfg = dev.default_input_config().map_err(|e| e.to_string())?;
    let rate = cfg.sample_rate().0;
    let ch = cfg.channels();
    let fmt = cfg.sample_format();
    let conf: cpal::StreamConfig = cfg.into();
    let err = |e| eprintln!("[x] stream input: {e}");

    let push = |mono: f32, q: &Arc<Mutex<VecDeque<f32>>>| {
        if let Ok(mut q) = q.lock() {
            q.push_back(mono);
            while q.len() > CAP_CAP {
                q.pop_front();
            }
        }
    };

    let stream = match fmt {
        SampleFormat::F32 => {
            let q = cap_q;
            dev.build_input_stream(
                &conf,
                move |data: &[f32], _: &_| {
                    for frame in data.chunks(ch as usize) {
                        push(frame.iter().sum::<f32>() / ch as f32, &q);
                    }
                },
                err,
                None,
            )
        }
        SampleFormat::I16 => {
            let q = cap_q;
            dev.build_input_stream(
                &conf,
                move |data: &[i16], _: &_| {
                    for frame in data.chunks(ch as usize) {
                        let m = frame.iter().map(|&x| x as f32 / 32768.0).sum::<f32>() / ch as f32;
                        push(m, &q);
                    }
                },
                err,
                None,
            )
        }
        other => return Err(format!("unsupported input format: {other:?}")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((rate, ch, stream))
}

fn build_output(play_q: Arc<Mutex<VecDeque<f32>>>) -> Result<StreamInfo, String> {
    let host = cpal::default_host();
    let dev = host.default_output_device().ok_or("no output device")?;
    let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
    let rate = cfg.sample_rate().0;
    let ch = cfg.channels();
    let fmt = cfg.sample_format();
    let conf: cpal::StreamConfig = cfg.into();
    let err = |e| eprintln!("[x] stream output: {e}");

    let stream = match fmt {
        SampleFormat::F32 => {
            let q = play_q;
            dev.build_output_stream(
                &conf,
                move |data: &mut [f32], _: &_| {
                    let mut ql = q.lock().unwrap();
                    for frame in data.chunks_mut(ch as usize) {
                        let s = ql.pop_front().unwrap_or(0.0);
                        for o in frame.iter_mut() {
                            *o = s;
                        }
                    }
                },
                err,
                None,
            )
        }
        SampleFormat::I16 => {
            let q = play_q;
            dev.build_output_stream(
                &conf,
                move |data: &mut [i16], _: &_| {
                    let mut ql = q.lock().unwrap();
                    for frame in data.chunks_mut(ch as usize) {
                        let s = ql.pop_front().unwrap_or(0.0);
                        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        for o in frame.iter_mut() {
                            *o = v;
                        }
                    }
                },
                err,
                None,
            )
        }
        other => return Err(format!("unsupported output format: {other:?}")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((rate, ch, stream))
}
