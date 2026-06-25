//! Jitter buffer adattivo per audio (frame da 20 ms, timestamp IAX in ms).
//!
//! Sans-io e senza dipendenze: si guida con `push`/`pull` e si collauda al
//! tavolino. Tiene i frame in ordine di timestamp, assorbe il jitter di rete
//! mantenendo una profondita' di buffer che si adatta alla varianza misurata
//! degli arrivi (stima alla RFC 3550), e segnala i buchi (frame persi/in
//! ritardo) al chiamante perche' faccia il packet-loss concealment nel dominio
//! PCM. Non decodifica nulla: lavora sui payload grezzi.
//!
//! Timestamp IAX2: sono in **millisecondi** dall'inizio della chiamata; un
//! frame da 20 ms avanza il timestamp di 20. Il buffer assume questo passo.

use std::collections::BTreeMap;
use std::time::Instant;

/// Durata di un frame in ms (G.711 a 8 kHz: 160 campioni = 20 ms).
pub const FRAME_MS: u32 = 20;

/// Profondita' minima del buffer (ms): non scende sotto, anche a jitter nullo.
const MIN_DEPTH_MS: u32 = 40;
/// Profondita' massima: oltre, meglio scartare che accumulare latenza.
const MAX_DEPTH_MS: u32 = 200;
/// Profondita' di base sommata al contributo del jitter.
const BASE_DEPTH_MS: u32 = 40;
/// Quanti "jitter stimati" sommare alla base per la profondita' obiettivo.
const JITTER_K: f64 = 3.0;
/// Tetto di frame bufferizzati (anti-crescita se nessuno fa pull): ~2 s.
const MAX_FRAMES: usize = 100;

/// Esito di una richiesta di riproduzione.
#[derive(Debug, Clone, PartialEq)]
pub enum Pull {
    /// Payload pronto da decodificare e riprodurre.
    Play(Vec<u8>),
    /// Frame atteso mancante ma stream attivo: fai PLC (es. ripeti attenuando).
    Conceal,
    /// Buffer in riempimento (priming) o esaurito: riproduci silenzio.
    Silence,
}

/// Statistiche utili per diagnostica/UI.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub played: u64,
    pub concealed: u64,
    pub late: u64,
    pub overflow: u64,
    pub target_ms: u32,
    pub buffered: usize,
}

pub struct JitterBuffer {
    frames: BTreeMap<u32, Vec<u8>>,
    next_ts: u32,
    playing: bool,
    target_ms: u32,
    jitter: f64,                       // stima RFC 3550, in ms
    last_arrival: Option<(u32, Instant)>, // (ts, istante) del frame precedente
    played: u64,
    concealed: u64,
    late: u64,
    overflow: u64,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterBuffer {
    pub fn new() -> Self {
        JitterBuffer {
            frames: BTreeMap::new(),
            next_ts: 0,
            playing: false,
            target_ms: MIN_DEPTH_MS,
            jitter: 0.0,
            last_arrival: None,
            played: 0,
            concealed: 0,
            late: 0,
            overflow: 0,
        }
    }

    /// Azzera lo stato mantenendo l'adattamento gia' appreso (cambio chiamata).
    pub fn reset(&mut self) {
        self.frames.clear();
        self.playing = false;
        self.last_arrival = None;
    }

    /// Inserisce un frame ricevuto (ts in ms, payload grezzo).
    pub fn push(&mut self, ts: u32, payload: Vec<u8>, now: Instant) {
        // aggiorna la stima del jitter (solo su arrivi in avanti, per non
        // sporcare la stima coi riordini)
        if let Some((prev_ts, prev_inst)) = self.last_arrival {
            if ts > prev_ts {
                let arrival_delta = now.saturating_duration_since(prev_inst).as_millis() as f64;
                let ts_delta = (ts - prev_ts) as f64;
                // D = differenza tra ritardo di transito di due frame
                let d = arrival_delta - ts_delta;
                self.jitter += (d.abs() - self.jitter) / 16.0;
                self.recompute_target();
            }
        }
        if ts > self.last_arrival.map(|(t, _)| t).unwrap_or(0) || self.last_arrival.is_none() {
            self.last_arrival = Some((ts, now));
        }

        // frame gia' "passato" rispetto al punto di riproduzione: troppo tardi
        if self.playing && ts < self.next_ts {
            self.late += 1;
            return;
        }

        self.frames.insert(ts, payload);

        // anti-crescita: se nessuno consuma, butta i piu' vecchi
        while self.frames.len() > MAX_FRAMES {
            if let Some((&old, _)) = self.frames.iter().next() {
                self.frames.remove(&old);
                self.overflow += 1;
            } else {
                break;
            }
        }
    }

    /// Estrae il prossimo frame da riprodurre (un passo da 20 ms).
    pub fn pull(&mut self) -> Pull {
        if !self.playing {
            // priming: aspetta di avere abbastanza buffer da coprire il target
            if self.buffered_span_ms() >= self.target_ms {
                if let Some((&first, _)) = self.frames.iter().next() {
                    self.next_ts = first;
                    self.playing = true;
                }
            }
            if !self.playing {
                return Pull::Silence;
            }
        }

        if let Some(payload) = self.frames.remove(&self.next_ts) {
            self.next_ts = self.next_ts.wrapping_add(FRAME_MS);
            self.played += 1;
            Pull::Play(payload)
        } else if !self.frames.is_empty() {
            // il frame atteso non c'e' ma ci sono frame futuri: buco -> PLC
            self.next_ts = self.next_ts.wrapping_add(FRAME_MS);
            self.concealed += 1;
            Pull::Conceal
        } else {
            // buffer esaurito: rifai priming al prossimo arrivo
            self.playing = false;
            Pull::Silence
        }
    }

    pub fn stats(&self) -> Stats {
        Stats {
            played: self.played,
            concealed: self.concealed,
            late: self.late,
            overflow: self.overflow,
            target_ms: self.target_ms,
            buffered: self.frames.len(),
        }
    }

    fn recompute_target(&mut self) {
        let t = BASE_DEPTH_MS as f64 + JITTER_K * self.jitter;
        self.target_ms = (t as u32).clamp(MIN_DEPTH_MS, MAX_DEPTH_MS);
    }

    /// Ampiezza temporale del materiale bufferizzato (ms).
    fn buffered_span_ms(&self) -> u32 {
        match (self.frames.keys().next(), self.frames.keys().next_back()) {
            (Some(&lo), Some(&hi)) => hi.wrapping_sub(lo).wrapping_add(FRAME_MS),
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn pcm(tag: u8) -> Vec<u8> {
        vec![tag; 160]
    }

    // arrivi regolari, in ordine: dopo il priming escono in sequenza.
    #[test]
    fn in_order_playout() {
        let t0 = Instant::now();
        let mut jb = JitterBuffer::new();
        // 4 frame (80 ms) coprono il target minimo di 40 ms
        for i in 0..4u32 {
            jb.push(i * FRAME_MS, pcm(i as u8), t0 + Duration::from_millis((i * 20) as u64));
        }
        let mut out = Vec::new();
        for _ in 0..4 {
            if let Pull::Play(p) = jb.pull() {
                out.push(p[0]);
            }
        }
        assert_eq!(out, vec![0, 1, 2, 3], "riproduzione in ordine");
    }

    // frame fuori ordine: il buffer li riallinea per timestamp.
    #[test]
    fn reorders_by_timestamp() {
        let t0 = Instant::now();
        let mut jb = JitterBuffer::new();
        for (n, i) in [2u32, 0, 3, 1].into_iter().enumerate() {
            jb.push(i * FRAME_MS, pcm(i as u8), t0 + Duration::from_millis((n as u64) * 20));
        }
        let mut out = Vec::new();
        for _ in 0..4 {
            if let Pull::Play(p) = jb.pull() {
                out.push(p[0]);
            }
        }
        assert_eq!(out, vec![0, 1, 2, 3], "riallineati per ts nonostante l'ordine d'arrivo");
    }

    // un frame mancante in mezzo produce un Conceal, poi prosegue.
    #[test]
    fn gap_yields_conceal() {
        let t0 = Instant::now();
        let mut jb = JitterBuffer::new();
        // ts 0,20,_,60,80  (manca il 40)
        for i in [0u32, 20, 60, 80] {
            jb.push(i, pcm((i / 20) as u8), t0 + Duration::from_millis(i as u64));
        }
        let mut seq = Vec::new();
        for _ in 0..5 {
            seq.push(jb.pull());
        }
        assert_eq!(seq[0], Pull::Play(pcm(0)));
        assert_eq!(seq[1], Pull::Play(pcm(1)));
        assert_eq!(seq[2], Pull::Conceal, "il buco produce PLC");
        assert_eq!(seq[3], Pull::Play(pcm(3)));
        assert!(jb.stats().concealed >= 1);
    }

    // un frame arrivato dopo il suo slot di riproduzione viene scartato (late).
    #[test]
    fn late_frame_is_dropped() {
        let t0 = Instant::now();
        let mut jb = JitterBuffer::new();
        for i in [0u32, 20, 40, 60] {
            jb.push(i, pcm((i / 20) as u8), t0 + Duration::from_millis(i as u64));
        }
        // consuma 0,20 -> next_ts = 40
        let _ = jb.pull();
        let _ = jb.pull();
        // arriva in ritardo un frame con ts=20 (gia' passato)
        jb.push(20, pcm(99), t0 + Duration::from_millis(100));
        assert_eq!(jb.stats().late, 1, "il frame in ritardo e' contato e scartato");
    }

    // con arrivi a singhiozzo la profondita' obiettivo cresce sopra il minimo.
    #[test]
    fn target_depth_grows_with_jitter() {
        let t0 = Instant::now();
        let mut jb = JitterBuffer::new();
        assert_eq!(jb.stats().target_ms, MIN_DEPTH_MS);
        // ts regolari ogni 20 ms, ma arrivi sballati (jitter forte)
        let arrivals = [0u64, 5, 70, 80, 200, 210, 215, 400];
        for (n, a) in arrivals.into_iter().enumerate() {
            jb.push(n as u32 * FRAME_MS, pcm(n as u8), t0 + Duration::from_millis(a));
        }
        assert!(jb.stats().target_ms > MIN_DEPTH_MS, "il target si adatta al jitter");
    }
}
