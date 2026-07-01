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

const MISS_BUDGET_THRESHOLD: u32 = 60;
const MISS_BUDGET_DECAY_ON_PLAY: u32 = 3;



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
    miss_budget: u32,
    // consec_concealed: u32,
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
            miss_budget: 0,
            //consec_concealed: 0,
        }
    }

    /// Azzera lo stato mantenendo l'adattamento gia' appreso (cambio chiamata).
    pub fn reset(&mut self) {
        self.frames.clear();
        self.playing = false;
        self.last_arrival = None;
        self.jitter = 0.0;
        self.target_ms = MIN_DEPTH_MS;
        self.miss_budget = 0; 
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

        // Risincronizzazione: se il cursore di playout e' rimasto indietro
        // rispetto al flusso live di piu' del doppio della profondita' massima,
        // non lo recuperera' mai (avanza di FRAME_MS a ogni pull, alla stessa
        // velocita' con cui arrivano i nuovi frame: il divario e' costante).
        // Forza un nuovo priming sul materiale fresco invece di scartare in
        // Conceal/overflow all'infinito.
        if self.playing {
            let lag = ts as i64 - self.next_ts as i64;
            if lag > (MAX_DEPTH_MS as i64) * 2 {
                self.frames.clear();
                self.playing = false;
            }
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

        if self.miss_budget > MISS_BUDGET_THRESHOLD {
            if let Some((&newest, _)) = self.frames.iter().next_back() {
                self.next_ts = newest;
            }
            self.miss_budget = 0;
        }


        if let Some(payload) = self.frames.remove(&self.next_ts) {
            self.next_ts = self.next_ts.wrapping_add(FRAME_MS);
            self.played += 1;
            self.miss_budget = self.miss_budget.saturating_sub(MISS_BUDGET_DECAY_ON_PLAY);
            Pull::Play(payload)
        } else if !self.frames.is_empty() {
            self.next_ts = self.next_ts.wrapping_add(FRAME_MS);
            self.concealed += 1;
            self.miss_budget = self.miss_budget.saturating_add(1);
            Pull::Conceal
        } else {
            self.playing = false;
            self.miss_budget = 0;
            Pull::Silence
        }

        // if let Some(payload) = self.frames.remove(&self.next_ts) {
        //    self.next_ts = self.next_ts.wrapping_add(FRAME_MS);
        //    self.played += 1;
        //    self.consec_concealed = 0;
        //    Pull::Play(payload)
        //} else if !self.frames.is_empty() {

        //    self.consec_concealed += 1;
            // Il cursore non recupera mai un gap persistente: Conceal avanza
            // alla stessa velocita' con cui arrivano i frame nuovi, qualunque
            // sia l'ampiezza del disallineamento. Dopo una serie troppo lunga
            // di scarti consecutivi, rincancora il cursore sul frame piu'
            // recente disponibile invece di continuare all'infinito.
        //    const MAX_CONCEAL_STREAK: u32 = (MAX_DEPTH_MS / FRAME_MS) * 2; // ~400ms
        //    if self.consec_concealed > MAX_CONCEAL_STREAK {
        //        if let Some((&newest, _)) = self.frames.iter().next_back() {
        //            self.next_ts = newest;
        //        }
        //        self.consec_concealed = 0;
        //    }

            // il frame atteso non c'e' ma ci sono frame futuri: buco -> PLC
        //    self.next_ts = self.next_ts.wrapping_add(FRAME_MS);
        //    self.concealed += 1;
        //    Pull::Conceal
        
        
        
        //} else {
            // buffer esaurito: rifai priming al prossimo arrivo
        //    self.playing = false;
        //    Pull::Silence
        //}

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

    #[test]
    fn resyncs_when_cursor_falls_permanently_behind() {
    // Riproduce il bug osservato con softphone.rs: dopo il priming, il
    // flusso reale "salta avanti" di molto (es. un errore di ricostruzione
    // del timestamp a 16 bit sul lato IAX2). Senza risincronizzazione, il
    // cursore (next_ts) rincorre a +FRAME_MS per pull mentre i nuovi
    // arrivi continuano ad allontanarsi alla stessa velocita': non lo
    // raggiunge mai, ed e' silenzio permanente (Conceal/overflow
    // all'infinito), esattamente come nel log reale (played fermo per 9s
    // mentre concealed/overflow salivano in lockstep).
    let t0 = Instant::now();
    let mut jb = JitterBuffer::new();

    // Fase A: priming normale, 5 frame consecutivi -> played consuma solo
    // il primo, next_ts resta a 20.
    for i in 0..5u32 {
        jb.push(i * FRAME_MS, pcm(i as u8), t0 + Duration::from_millis((i * 20) as u64));
    }
    assert!(matches!(jb.pull(), Pull::Play(_)), "priming completato, primo frame riprodotto");

    // Fase B: il flusso vero salta ben oltre la soglia di resync (doppio
    // di MAX_DEPTH_MS). Senza il fix questi frame finirebbero tutti
    // legittimi in coda dietro un cursore che non li raggiungera' mai.
    let jump = 20 + MAX_DEPTH_MS * 3;
    for k in 0..5u32 {
        let ts = jump + k * FRAME_MS;
        jb.push(ts, pcm(100 + k as u8), t0 + Duration::from_millis(1000 + (k * 20) as u64));
    }

    let mut played = Vec::new();
    for _ in 0..5 {
        if let Pull::Play(p) = jb.pull() {
            played.push(p[0]);
        }
    }
    assert!(!played.is_empty(), "dopo il resync il buffer deve riprendere a riprodurre, non restare bloccato in Conceal/Silence per sempre");
    assert_eq!(played[0], 100, "riparte dal primo frame del cluster fresco, non da un vecchio frame stantio rimasto in coda");
    assert_eq!(played, vec![100, 101, 102, 103, 104], "tutto il cluster fresco viene riprodotto in ordine dopo il resync");
    }

    #[test]
    fn recovers_from_small_persistent_gap_via_conceal_streak() {
        // Riproduce il caso softphone.rs reale: un disallineamento PICCOLO
        // (sotto la soglia di resync su push, quindi quel fix da solo non
        // basta) ma persistente. Senza la rete di sicurezza sul lato pull(),
        // il cursore non lo recupera mai: Conceal avanza alla stessa velocita'
        // con cui arrivano i nuovi frame, il gap resta costante per sempre.
        let t0 = Instant::now();
        let mut jb = JitterBuffer::new();

        for i in 0..5u32 {
            jb.push(i * FRAME_MS, pcm(i as u8), t0 + Duration::from_millis((i * 20) as u64));
        }
        assert!(matches!(jb.pull(), Pull::Play(_)));
        // next_ts = 20 dopo il primo pull

        // gap piccolo e persistente: 150ms, sotto la soglia di resync su push
        // (400ms), ma che Conceal non potra' mai chiudere da solo.
        let gap_start = 20 + 150;
        for k in 0..60u32 {
            let ts = gap_start + k * FRAME_MS;
            jb.push(ts, pcm(200), t0 + Duration::from_millis(1000 + (k * 20) as u64));
        }

        let mut saw_play_again = false;
        for _ in 0..60 {
            if matches!(jb.pull(), Pull::Play(_)) {
                saw_play_again = true;
                break;
            }
        }
        assert!(saw_play_again, "il buffer deve riprendere a riprodurre entro la soglia di streak, non restare bloccato in Conceal per sempre");
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

    #[test]
    fn resyncs_on_sustained_partial_misalignment_even_with_sporadic_hits() {
        // Riproduce il caso reale: non uno streak ininterrotto di Conceal, ma
        // un flusso dove Play e Conceal/late si alternano, con i miss in
        // minoranza numerica dominante. Un contatore a streak puro (azzerato
        // da ogni singolo Play) non lo riconoscerebbe mai come problema.
        let t0 = Instant::now();
        let mut jb = JitterBuffer::new();

        for i in 0..5u32 {
            jb.push(i * FRAME_MS, pcm(i as u8), t0 + Duration::from_millis((i * 20) as u64));
        }
        assert!(matches!(jb.pull(), Pull::Play(_)));

        // 80 round: ogni round un push leggermente disallineato (crea Conceal
        // o late alternati a qualche Play fortuito), mai un salto isolato
        // grande, mai uno streak ininterrotto.
        let mut saw_play = 0usize;
        let mut saw_conceal_or_late = 0usize;
        for k in 0..80u32 {
            let ts = 20 + 60 + k * FRAME_MS; // disallineamento persistente ~60ms
            jb.push(ts, pcm(200), t0 + Duration::from_millis(1000 + (k * 20) as u64));
            match jb.pull() {
                Pull::Play(_) => saw_play += 1,
                Pull::Conceal => saw_conceal_or_late += 1,
                Pull::Silence => {}
            }
        }
        assert!(saw_play > 40, "dopo il resync la maggioranza dei pull deve tornare a essere Play, non restare bloccata a meta' strada (Play={saw_play}, Conceal={saw_conceal_or_late})");
    }
}
