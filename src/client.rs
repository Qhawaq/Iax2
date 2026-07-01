//! `client` — macchina a stati IAX2 **sans-io** per UN account su UN PBX.
//!
//!
//! ## Modello d'uso (sans-io)
//! ```ignore
//! let mut c = PbxClient::new(cfg);
//! loop {
//!     // 1) consegna l'output: c.poll_transmit() -> datagrammi per QUESTO pbx
//!     // 2) reagisci al tempo:  c.handle_timeout(now)
//!     // 3) consegna input:     c.handle_input(&datagram, now)
//!     // 4) dai comandi:        c.handle_command(Command::Answer{call}, now)
//!     // 5) leggi eventi:       while let Some(ev) = c.poll_event() { ... }
//!     // 6) prossimo risveglio: c.poll_timeout()
//! }
//! ```

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::consts::{authmethod, control, format, frametype, iax, ie as iet, IAX_PROTO_VERSION};
use crate::frame::{self, FullFrame, MiniFrame};
use crate::ie::{self, Ie};

// --- parametri di affidabilita' / timing -----------------------------------

const RETX_AFTER: Duration = Duration::from_millis(700);
const RETX_MAX: u32 = 5;
const KEEPALIVE_EVERY: Duration = Duration::from_secs(15);
const REG_GUARD: Duration = Duration::from_secs(10); // anti-spam tra REGREQ
const REG_RETRY: Duration = Duration::from_secs(5); // dopo REGREJ / fallimento

// --- call number riservati (lo spazio e' per-PBX, quindi non collide mai con
//     gli altri PBX perche' ogni PBX ha il suo socket/istanza) ---------------

const CALLNO_REG: u16 = 1; // gamba di registrazione
const CALLNO_KEEPALIVE: u16 = 2; // POKE di keepalive NAT
const CALLNO_CALL_BASE: u16 = 16; // le chiamate partono da qui
const CALLNO_QUALIFY_BASE: u16 = 0x2000; // PONG di qualify, rotante in [base, base+0xFFF]
const CALLNO_QUALIFY_SPAN: u16 = 0x0FFF;

/// Confronto seriale a 8 bit (RFC 1982-like): vero se `a` precede `b`.
fn seq_before(a: u8, b: u8) -> bool {
    let d = b.wrapping_sub(a);
    d != 0 && d < 128
}

/// Ricostruisce il timestamp a 32 bit di un mini-frame dai suoi 16 bit bassi,
/// usando come riferimento l'ultimo timestamp voce completo visto. Gestisce il
/// wrap dei 16 bit scegliendo il candidato piu' vicino al riferimento.
fn reconstruct_ts(last: u32, ts16: u16, seen: bool) -> u32 {
    if !seen {
        return ts16 as u32;
    }
    let high = last & 0xFFFF_0000;
    let cand = high | ts16 as u32;
    // scegli tra cand-1<<16, cand, cand+1<<16 quello piu' vicino a `last`
    let lower = cand.wrapping_sub(0x1_0000);
    let upper = cand.wrapping_add(0x1_0000);
    [lower, cand, upper]
        .into_iter()
        .min_by_key(|&c| (c as i64 - last as i64).unsigned_abs())
        .unwrap_or(cand)
}

// === configurazione account =================================================

#[derive(Clone, Debug)]
pub struct Config {
    /// Etichetta del PBX (mostrata in UI / log). Es. "Catania".
    pub name: String,
    pub username: String,
    pub secret: String,
    /// Refresh registrazione richiesto (secondi).
    pub refresh: u16,
    /// Formato audio (bitmask FORMAT). Default ULAW.
    pub audio_format: u32,
}

impl Config {
    pub fn new(name: impl Into<String>, username: impl Into<String>, secret: impl Into<String>) -> Self {
        Config {
            name: name.into(),
            username: username.into(),
            secret: secret.into(),
            refresh: 60,
            audio_format: format::ULAW,
        }
    }
}

// === eventi verso il driver =================================================

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Event {
    /// Registrazione completata (REGACK).
    Registered,
    /// Registrazione fallita / persa; il client riprovera' da solo.
    RegisterLost { reason: String },
    /// Chiamata in ingresso (sta squillando). Il driver decide se rispondere.
    Incoming { call: u16, from: String, to: String },
    /// La nostra chiamata uscente e' stata inviata (NEW).
    Dialing { call: u16, to: String },
    /// Il remoto sta squillando (CONTROL/RINGING su chiamata uscente).
    Ringing { call: u16 },
    /// Chiamata connessa (ANSWER): l'audio puo' fluire.
    Answered { call: u16 },
    /// Audio ricevuto (payload G.711 grezzo) per una chiamata attiva.
    Voice { call: u16, ts: u32, ulaw: Vec<u8> },
    /// Chiamata terminata (HANGUP/REJECT/BUSY/timeout affidabilita').
    Ended { call: u16, reason: String },
    /// Cifra DTMF ricevuta dal remoto.
    Dtmf { call: u16, digit: char },
    /// Diagnostica (passthrough dei vecchi log `[i]`).
    Log(String),
}

// === comandi dal driver =====================================================

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Command {
    /// Componi un numero (chiamata uscente).
    Dial { number: String },
    /// Rispondi a una chiamata in ingresso che squilla.
    Answer { call: u16 },
    /// Riaggancia / rifiuta una chiamata.
    Hangup { call: u16 },
    /// Metti in attesa (MOH lato PBX); il driver smette di inviare microfono.
    Hold { call: u16 },
    /// Riprendi dalla attesa.
    Unhold { call: u16 },
    /// Invia un frame audio (payload G.711 µ-law gia' codificato dal driver).
    SendVoice { call: u16, ulaw: Vec<u8> },
    /// Invia una cifra DTMF (0-9, *, #, A-D) sulla chiamata.
    Dtmf { call: u16, digit: char },
}

// === stato interno ==========================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegState {
    Idle,
    Registering,
    Registered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    /// Uscente: NEW inviato, in attesa di accettazione/auth.
    Trying,
    /// Sta squillando (in ingresso: noi squilliamo; uscente: il remoto squilla).
    Ringing,
    /// Connessa, audio attivo.
    Up,
    /// In attesa (hold) locale.
    Held,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    In,
    Out,
}

/// Full-frame affidabile non ancora confermato.
struct Outstanding {
    oseq: u8,
    ts: u32,
    bytes: Vec<u8>,
    deadline: Instant,
    tries: u32,
}

/// Numerazione + sequenze + finestra affidabile di una leg.
struct Leg {
    local: u16,
    remote: u16,
    oseq: u8,
    iseq: u8,
    outstanding: VecDeque<Outstanding>,
}

impl Leg {
    fn new(local: u16) -> Self {
        Leg { local, remote: 0, oseq: 0, iseq: 0, outstanding: VecDeque::new() }
    }

    fn reset(&mut self) {
        self.remote = 0;
        self.oseq = 0;
        self.iseq = 0;
        self.outstanding.clear();
    }

    /// Aggiorna remote/iseq dai full-frame ricevuti (semantica del campo:
    /// iseq = ultimo oseq ricevuto + 1).
    fn note_recv(&mut self, f: &FullFrame) {
        if f.src_call != 0 {
            self.remote = f.src_call;
        }
        self.iseq = f.oseq.wrapping_add(1);
    }

    /// Costruisce un full-frame uscente avanzando oseq.
    fn build(&mut self, ts: u32, ft: u8, sc: u8, ies: Vec<Ie>) -> FullFrame {
        let f = FullFrame::new(self.local, self.remote, ts, self.oseq, self.iseq, ft, sc, ies);
        self.oseq = self.oseq.wrapping_add(1);
        f
    }

    /// ACK (non affidabile, non consuma oseq).
    fn ack_bytes(&self, ts: u32) -> Vec<u8> {
        FullFrame::new(self.local, self.remote, ts, self.oseq, self.iseq, frametype::IAX, iax::ACK, vec![]).encode()
    }

    /// Registra un full-frame come affidabile (da ritrasmettere fino all'ACK).
    fn track_reliable(&mut self, f: &FullFrame, now: Instant) {
        self.outstanding.push_back(Outstanding {
            oseq: f.oseq,
            ts: f.timestamp,
            bytes: f.encode(),
            deadline: now + RETX_AFTER,
            tries: 0,
        });
    }

    /// ACK cumulativo: scarta tutti gli outstanding con oseq < iseq ricevuto.
    /// Fallback: se e' un ACK puntuale, scarta per timestamp combaciante.
    fn ack_upto(&mut self, recv_iseq: u8, recv_ts: u32, is_ack: bool) {
        self.outstanding.retain(|o| {
            let acked_cumulative = seq_before(o.oseq, recv_iseq);
            let acked_ts = is_ack && o.ts == recv_ts;
            !(acked_cumulative || acked_ts)
        });
    }
}

struct CallLeg {
    leg: Leg,
    state: CallState,
    dir: Dir,
    peer: String, // chi chiama (in) o numero chiamato (out)
    fmt: u32,
    sent_format: bool,    // primo full VOICE inviato?
    last_voice_ts: u32,   // ultimo timestamp voce a 32 bit (per ricostruire i mini)
    voice_ts_seen: bool,  // abbiamo gia' visto un full VOICE?
}

// === client =================================================================

pub struct PbxClient {
    cfg: Config,
    start: Instant,
    reg: Leg,
    reg_state: RegState,
    refresh: u16,
    calltoken: Option<Vec<u8>>,
    next_reg: Instant,
    next_keepalive: Instant,
    calls: HashMap<u16, CallLeg>,
    remote_index: HashMap<u16, u16>, // remote_call -> local_call (per mini-frame)
    callno_next: u16,
    qualify_next: u16,
    out: VecDeque<Vec<u8>>,
    events: VecDeque<Event>,
}

impl PbxClient {
    pub fn new(cfg: Config) -> Self {
        let now = Instant::now();
        let refresh = cfg.refresh;
        PbxClient {
            cfg,
            start: now,
            reg: Leg::new(CALLNO_REG),
            reg_state: RegState::Idle,
            refresh,
            calltoken: None,
            next_reg: now, // registra subito
            next_keepalive: now + KEEPALIVE_EVERY,
            calls: HashMap::new(),
            remote_index: HashMap::new(),
            callno_next: CALLNO_CALL_BASE,
            qualify_next: CALLNO_QUALIFY_BASE,
            out: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.cfg.name
    }
    pub fn is_registered(&self) -> bool {
        self.reg_state == RegState::Registered
    }
    pub fn active_calls(&self) -> usize {
        self.calls.len()
    }
    pub fn call_state(&self, call: u16) -> Option<CallState> {
        self.calls.get(&call).map(|c| c.state)
    }
    /// Call-number delle chiamate vive, in ordine stabile (crescente).
    pub fn call_ids(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self.calls.keys().copied().collect();
        v.sort_unstable();
        v
    }
    /// Etichetta del peer remoto di una chiamata (numero/nome), se nota.
    pub fn call_peer(&self, call: u16) -> Option<&str> {
        self.calls.get(&call).map(|c| c.peer.as_str())
    }

    fn ts(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    fn push(&mut self, bytes: Vec<u8>) {
        self.out.push_back(bytes);
    }
    fn emit(&mut self, ev: Event) {
        self.events.push_back(ev);
    }

    /// Datagrammi pronti per QUESTO pbx (il driver li manda sul socket giusto).
    pub fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.out.pop_front()
    }

    /// Eventi di alto livello per il driver.
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Prossimo istante in cui chiamare `handle_timeout`.
    pub fn poll_timeout(&self) -> Option<Instant> {
        let mut t: Option<Instant> = Some(self.next_reg);
        let consider = |x: Instant, t: &mut Option<Instant>| {
            *t = Some(match *t {
                Some(c) if c <= x => c,
                _ => x,
            });
        };
        if self.reg_state == RegState::Registered {
            consider(self.next_keepalive, &mut t);
        }
        for o in &self.reg.outstanding {
            consider(o.deadline, &mut t);
        }
        for c in self.calls.values() {
            for o in &c.leg.outstanding {
                consider(o.deadline, &mut t);
            }
        }
        t
    }

    // --- allocazione call number ---------------------------------------

    fn alloc_callno(&mut self) -> u16 {
        // cerca il prossimo libero da CALLNO_CALL_BASE, saltando i riservati
        for _ in 0..0x7000 {
            let n = self.callno_next;
            self.callno_next = if self.callno_next >= 0x1FFF { CALLNO_CALL_BASE } else { self.callno_next + 1 };
            if n >= CALLNO_CALL_BASE && !self.calls.contains_key(&n) {
                return n;
            }
        }
        CALLNO_CALL_BASE // fallback estremo: in pratica irraggiungibile
    }

    fn next_qualify_callno(&mut self) -> u16 {
        let n = self.qualify_next;
        self.qualify_next = CALLNO_QUALIFY_BASE + ((self.qualify_next - CALLNO_QUALIFY_BASE + 1) & CALLNO_QUALIFY_SPAN);
        n
    }

    // --- timer ----------------------------------------------------------

    pub fn handle_timeout(&mut self, now: Instant) {
        // (re)registrazione: parte solo se non c'e' gia' un REGREQ in volo.
        if self.reg.outstanding.is_empty() && now >= self.next_reg {
            self.start_registration(now);
        }

        // keepalive NAT: POKE periodico (LEZIONE 5: tiene aperto il buco UDP,
        // senza i POKE di qualify e i NEW in arrivo non rientrano).
        if self.reg_state == RegState::Registered && now >= self.next_keepalive {
            let poke = FullFrame::new(CALLNO_KEEPALIVE, 0, self.ts(), 0, 0, frametype::IAX, iax::POKE, vec![]).encode();
            self.push(poke);
            self.next_keepalive = now + KEEPALIVE_EVERY;
        }

        // ritrasmissioni finestra reg
        self.retransmit_reg(now);
        // ritrasmissioni finestra chiamate
        self.retransmit_calls(now);
    }

    fn start_registration(&mut self, now: Instant) {
        self.reg.reset();
        self.calltoken = None; 
        self.reg_state = RegState::Registering;
        let ies = self.reg_ies();
        let f = self.reg.build(self.ts(), frametype::IAX, iax::REGREQ, ies);
        self.reg.track_reliable(&f, now);
        self.push(f.encode());
        self.next_reg = now + REG_GUARD; // guardia anti-spam
        self.emit(Event::Log("REGREQ sended".into()));
    }

    fn retransmit_reg(&mut self, now: Instant) {
        let mut lost = false;
        for o in self.reg.outstanding.iter_mut() {
            if now >= o.deadline {
                if o.tries >= RETX_MAX {
                    lost = true;
                } else {
                    let mut b = o.bytes.clone();
                    b[2] |= 0x80; // R-bit: ritrasmissione
                    self.out.push_back(b);
                    o.tries += 1;
                    o.deadline = now + RETX_AFTER;
                }
            }
        }
        if lost {
            self.reg.outstanding.clear();
            self.reg_state = RegState::Idle;
            self.next_reg = now + REG_RETRY;
            self.emit(Event::RegisterLost { reason: "no ACK after retransmissions".into() });
        }
    }

    fn retransmit_calls(&mut self, now: Instant) {
        let mut dead: Vec<u16> = Vec::new();
        let mut to_send: Vec<Vec<u8>> = Vec::new();
        for (&local, c) in self.calls.iter_mut() {
            let mut leg_dead = false;
            for o in c.leg.outstanding.iter_mut() {
                if now >= o.deadline {
                    if o.tries >= RETX_MAX {
                        leg_dead = true;
                    } else {
                        let mut b = o.bytes.clone();
                        b[2] |= 0x80;
                        to_send.push(b);
                        o.tries += 1;
                        o.deadline = now + RETX_AFTER;
                    }
                }
            }
            if leg_dead {
                dead.push(local);
            }
        }
        for b in to_send {
            self.push(b);
        }
        for local in dead {
            self.drop_call(local, "reliability timeout".into());
        }
    }

    fn drop_call(&mut self, local: u16, reason: String) {
        if let Some(c) = self.calls.remove(&local) {
            self.remote_index.retain(|_, &mut v| v != local);
            let _ = c;
            self.emit(Event::Ended { call: local, reason });
        }
    }

    // --- comandi --------------------------------------------------------

    pub fn handle_command(&mut self, cmd: Command, now: Instant) {
        match cmd {
            Command::Dial { number } => self.dial(&number, now),
            Command::Answer { call } => self.answer(call, now),
            Command::Hangup { call } => self.hangup(call, now),
            Command::Hold { call } => self.set_hold(call, true, now),
            Command::Unhold { call } => self.set_hold(call, false, now),
            Command::SendVoice { call, ulaw } => self.send_voice(call, &ulaw),
            Command::Dtmf { call, digit } => self.send_dtmf(call, digit, now),
        }
    }

    fn dial(&mut self, number: &str, now: Instant) {
        let local = self.alloc_callno();
        let mut leg = Leg::new(local);
        let ies = self.new_call_ies(number);
        let f = leg.build(self.ts(), frametype::IAX, iax::NEW, ies);
        leg.track_reliable(&f, now);
        self.push(f.encode());
        self.calls.insert(
            local,
            CallLeg { leg, state: CallState::Trying, dir: Dir::Out, peer: number.to_string(), fmt: self.cfg.audio_format, sent_format: false, last_voice_ts: 0, voice_ts_seen: false },
        );
        self.emit(Event::Dialing { call: local, to: number.to_string() });
    }

    fn answer(&mut self, call: u16, now: Instant) {
        let ts = self.ts();
        if let Some(c) = self.calls.get_mut(&call) {
            if c.dir == Dir::In && c.state == CallState::Ringing {
                let ans = c.leg.build(ts, frametype::CONTROL, control::ANSWER, vec![]);
                c.leg.track_reliable(&ans, now);
                self.push(ans.encode());
                if let Some(c) = self.calls.get_mut(&call) {
                    c.state = CallState::Up;
                    c.sent_format = false;
                }
                self.emit(Event::Answered { call });
            }
        }
    }

    fn hangup(&mut self, call: u16, now: Instant) {
        let ts = self.ts();
        if let Some(c) = self.calls.get_mut(&call) {
            let hb = c.leg.build(ts, frametype::IAX, iax::HANGUP, vec![Ie::str(iet::CAUSE, "Normal Clearing")]);
            c.leg.track_reliable(&hb, now);
            self.push(hb.encode());
        }
        self.drop_call(call, "local hangup".into());
    }

    fn send_dtmf(&mut self, call: u16, digit: char, now: Instant) {
        // DTMF valido: 0-9, *, #, A-D (case-insensitive). Il subclass del frame
        // DTMF porta la cifra come ASCII (compatibile con Asterisk).
        let d = digit.to_ascii_uppercase();
        if !(d.is_ascii_digit() || matches!(d, '*' | '#' | 'A'..='D')) {
            return;
        }
        let ts = self.ts();
        let bytes = self.calls.get_mut(&call).map(|c| {
            let f = c.leg.build(ts, frametype::DTMF, d as u8, vec![]);
            c.leg.track_reliable(&f, now);
            f.encode()
        });
        if let Some(b) = bytes {
            self.push(b);
        }
    }

    fn set_hold(&mut self, call: u16, hold: bool, now: Instant) {
        let ts = self.ts();
        let bytes = self.calls.get_mut(&call).map(|c| {
            let sc = if hold { control::HOLD } else { control::UNHOLD };
            let f = c.leg.build(ts, frametype::CONTROL, sc, vec![]);
            c.leg.track_reliable(&f, now);
            c.state = if hold { CallState::Held } else { CallState::Up };
            f.encode()
        });
        if let Some(b) = bytes {
            self.push(b);
        }
    }

    fn send_voice(&mut self, call: u16, ulaw: &[u8]) {
        let ts = self.ts();
        // estrai i dati che servono senza tenere il borrow mutabile durante push
        let plan = self.calls.get_mut(&call).and_then(|c| {
            if c.state != CallState::Up {
                return None;
            }
            if !c.sent_format {
                c.sent_format = true;
                let vf = c.leg.build(ts, frametype::VOICE, c.fmt as u8, vec![]);
                Some((true, vf.encode_media(ulaw), c.leg.local))
            } else {
                Some((false, Vec::new(), c.leg.local))
            }
        });
        if let Some((is_full, bytes, local)) = plan {
            if is_full {
                self.push(bytes);
            } else {
                let mf = MiniFrame { src_call: local, timestamp16: ts as u16, payload: ulaw.to_vec() };
                self.push(mf.encode());
            }
        }
    }

    // --- ingresso -------------------------------------------------------

    pub fn handle_input(&mut self, dg: &[u8], now: Instant) {
        // Mini-frame audio: instrada per remote_call -> local.
        if !frame::is_full_frame(dg) {
            if let Some(m) = MiniFrame::decode(dg) {
                if let Some(&local) = self.remote_index.get(&m.src_call) {
                    if matches!(self.calls.get(&local).map(|c| c.state), Some(CallState::Up)) {
                        // ricostruisci il timestamp a 32 bit dai 16 del mini-frame
                        // usando i bit alti dell'ultimo full VOICE (gestendo il wrap)
                        let ts = if let Some(c) = self.calls.get_mut(&local) {
                            let rec = reconstruct_ts(c.last_voice_ts, m.timestamp16, c.voice_ts_seen);
                            c.last_voice_ts = rec;
                            c.voice_ts_seen = true;
                            rec
                        } else {
                            m.timestamp16 as u32
                        };
                        self.emit(Event::Voice { call: local, ts, ulaw: m.payload });
                    }
                }
            }
            return;
        }
        let Some(f) = FullFrame::decode(dg) else { return };

        // 1) Qualify standalone: POKE/PING -> PONG.
        if f.frametype == frametype::IAX && (f.subclass == iax::POKE || f.subclass == iax::PING) && f.dst_call == 0 {
            let src = self.next_qualify_callno();
            let pong = FullFrame::new(src, f.src_call, f.timestamp, 0, f.oseq.wrapping_add(1), frametype::IAX, iax::PONG, vec![]).encode();
            self.push(pong);
            return;
        }

        // 2) Call-token pre-auth: instradalo verso la cosa in volo che lo
        if let Some(tok) = ie::find(&f.ies, iet::CALLTOKEN) {
            if !tok.data.is_empty() {
                let data = tok.data.clone();
                if f.dst_call == self.reg.local && self.calltoken.is_none() {
                    self.resend_reg_with_token(data, now);
                    return;
                }
                // chiamata uscente in attesa di token
                let target = self
                    .calls
                    .iter()
                    .find(|(_, c)| c.dir == Dir::Out && c.state == CallState::Trying && f.dst_call == c.leg.local)
                    .map(|(&l, _)| l);
                if let Some(local) = target {
                    self.resend_new_with_token(local, data, now);
                    return;
                }
            }
        }

        // 3) NEW in ingresso = chiamata in arrivo.
        if f.frametype == frametype::IAX && f.subclass == iax::NEW {
            self.on_incoming_new(&f, now);
            return;
        }

        // 4) Routing per gamba.
        if f.dst_call == self.reg.local {
            self.on_reg_frame(&f, now);
            return;
        }
        if self.calls.contains_key(&f.dst_call) {
            self.on_call_frame(f.dst_call, &f, now);
            return;
        }

        // 5) Fuori gamba: ACK/PONG/INVAL benigni -> silenzio.
        match (f.frametype, f.subclass) {
            (frametype::IAX, iax::ACK) | (frametype::IAX, iax::PONG) | (frametype::IAX, iax::INVAL) => {}
            (ft, sc) => self.emit(Event::Log(format!("out of leg frame dst_call={} ft=0x{ft:02x} sc=0x{sc:02x}", f.dst_call))),
        }
    }

    fn resend_reg_with_token(&mut self, token: Vec<u8>, now: Instant) {
        self.calltoken = Some(token);
        self.reg.reset();
        let ies = self.reg_ies();
        let f = self.reg.build(self.ts(), frametype::IAX, iax::REGREQ, ies);
        self.reg.track_reliable(&f, now);
        self.push(f.encode());
    }

    fn resend_new_with_token(&mut self, local: u16, token: Vec<u8>, now: Instant) {
        let ts = self.ts();
        let number = self.calls.get(&local).map(|c| c.peer.clone()).unwrap_or_default();
        if let Some(c) = self.calls.get_mut(&local) {
            c.leg.reset(); // oseq=0, iseq=0, remote=0 (dst_call=0): richiesta fresca
        }
        let mut ies = self.new_call_ies(&number);
        // rimpiazza il CALLTOKEN vuoto con quello reale
        if let Some(p) = ies.iter_mut().find(|i| i.kind == iet::CALLTOKEN) {
            *p = Ie::new(iet::CALLTOKEN, token);
        }
        if let Some(c) = self.calls.get_mut(&local) {
            let f = c.leg.build(ts, frametype::IAX, iax::NEW, ies);
            c.leg.track_reliable(&f, now);
            let bytes = f.encode();
            self.push(bytes);
        }
    }

    fn on_incoming_new(&mut self, f: &FullFrame, now: Instant) {
        // Telefono di servizio mono-utente: piu' chiamate possono coesistere
        // (call waiting), ma il driver decide la policy. Qui le ACCETTIAMO
        // tutte come "squillanti": la decisione di rispondere e' un comando.
        let called = ie::find(&f.ies, iet::CALLED_NUMBER).map(|i| i.as_str()).unwrap_or_default();
        let caller = ie::find(&f.ies, iet::CALLING_NUMBER).map(|i| i.as_str()).unwrap_or_default();

        let local = self.alloc_callno();
        let mut leg = Leg::new(local);
        leg.note_recv(f);
        self.remote_index.insert(f.src_call, local);

        let ts = self.ts();
        // ACCEPT (negozia formato) — affidabile.
        let accept = leg.build(ts, frametype::IAX, iax::ACCEPT, vec![Ie::u32(iet::FORMAT, self.cfg.audio_format)]);
        leg.track_reliable(&accept, now);
        self.push(accept.encode());
        // RINGING — segnala che stiamo squillando.
        let ringing = leg.build(ts, frametype::CONTROL, control::RINGING, vec![]);
        leg.track_reliable(&ringing, now);
        self.push(ringing.encode());

        self.calls.insert(
            local,
            CallLeg { leg, state: CallState::Ringing, dir: Dir::In, peer: caller.clone(), fmt: self.cfg.audio_format, sent_format: false, last_voice_ts: 0, voice_ts_seen: false },
        );
        self.emit(Event::Incoming { call: local, from: caller, to: called });
    }

    fn on_reg_frame(&mut self, f: &FullFrame, now: Instant) {
        self.reg.note_recv(f);
        let is_ack = f.frametype == frametype::IAX && f.subclass == iax::ACK;
        self.reg.ack_upto(f.iseq, f.timestamp, is_ack);

        match f.subclass {
            iax::REGAUTH => {
                let methods = ie::find(&f.ies, iet::AUTHMETHODS).and_then(|i| i.as_u16()).unwrap_or(0);
                let challenge = ie::find(&f.ies, iet::CHALLENGE).map(|i| i.as_str());
                if methods & authmethod::MD5 == 0 {
                    self.emit(Event::Log("REGAUTH without MD5".into()));
                    return;
                }
                let Some(chal) = challenge else {
                    self.emit(Event::Log("REGAUTH without challenge".into()));
                    return;
                };
                let md5 = crate::md5_response(&chal, &self.cfg.secret);
                let mut ies = self.reg_ies();
                ies.insert(1, Ie::str(iet::MD5_RESULT, &md5)); // ordine come nello spike provato
                let fr = self.reg.build(self.ts(), frametype::IAX, iax::REGREQ, ies);
                self.reg.track_reliable(&fr, now);
                self.push(fr.encode());
            }
            iax::REGACK => {
                if let Some(rf) = ie::find(&f.ies, iet::REFRESH).and_then(|i| i.as_u16()) {
                    if rf > 0 {
                        self.refresh = rf;
                    }
                }
                self.reg_state = RegState::Registered;
                // re-register a ~80% del refresh
                self.next_reg = now + Duration::from_secs((self.refresh as u64 * 4) / 5);
                self.next_keepalive = now + KEEPALIVE_EVERY;
                let ack = self.reg.ack_bytes(self.ts());
                self.push(ack);
                self.emit(Event::Registered);
            }
            iax::REGREJ => {
                let code = ie::find(&f.ies, iet::CAUSECODE).and_then(|i| i.data.first().copied());
                self.calltoken = None;
                self.reg_state = RegState::Idle;
                self.next_reg = now + REG_RETRY;
                let ack = self.reg.ack_bytes(self.ts());
                self.push(ack);
                self.emit(Event::RegisterLost { reason: format!("REGREJ causecode={code:?}") });
            }
            iax::ACK | iax::CALLTOKEN => {}
            other => self.emit(Event::Log(format!("reg leg subclass=0x{other:02x}"))),
        }
    }

    fn on_call_frame(&mut self, local: u16, f: &FullFrame, now: Instant) {
        let ts = self.ts();
        // aggiorna seq/finestra
        if let Some(c) = self.calls.get_mut(&local) {
            c.leg.note_recv(f);
            if f.src_call != 0 {
                self.remote_index.insert(f.src_call, local);
            }
            let is_ack = f.frametype == frametype::IAX && f.subclass == iax::ACK;
            c.leg.ack_upto(f.iseq, f.timestamp, is_ack);
        }

        let dir = self.calls.get(&local).map(|c| c.dir);
        match (f.frametype, f.subclass) {
            (frametype::VOICE, _) => {
                if let Some(c) = self.calls.get_mut(&local) {
                    let ack = c.leg.ack_bytes(ts);
                    // il full VOICE porta il timestamp completo a 32 bit:
                    // memorizzalo per ricostruire i mini-frame che seguono
                    c.last_voice_ts = f.timestamp;
                    c.voice_ts_seen = true;
                    self.push(ack);
                }
                if matches!(self.calls.get(&local).map(|c| c.state), Some(CallState::Up)) {
                    self.emit(Event::Voice { call: local, ts: f.timestamp, ulaw: f.media_payload.clone() });
                }
            }
            (frametype::IAX, iax::AUTHREQ) if dir == Some(Dir::Out) => {
                let methods = ie::find(&f.ies, iet::AUTHMETHODS).and_then(|i| i.as_u16()).unwrap_or(0);
                let challenge = ie::find(&f.ies, iet::CHALLENGE).map(|i| i.as_str());
                if methods & authmethod::MD5 == 0 {
                    self.emit(Event::Log("AUTHREQ without MD5".into()));
                    return;
                }
                let Some(chal) = challenge else {
                    self.emit(Event::Log("AUTHREQ eithout challenge".into()));
                    return;
                };
                let md5 = crate::md5_response(&chal, &self.cfg.secret);
                if let Some(c) = self.calls.get_mut(&local) {
                    let rep = c.leg.build(ts, frametype::IAX, iax::AUTHREP, vec![Ie::str(iet::MD5_RESULT, &md5)]);
                    c.leg.track_reliable(&rep, now);
                    self.push(rep.encode());
                }
            }
            (frametype::IAX, iax::ACCEPT) => {
                if let Some(fmt) = ie::find(&f.ies, iet::FORMAT).and_then(|i| i.as_u32()) {
                    if let Some(c) = self.calls.get_mut(&local) {
                        c.fmt = fmt;
                    }
                }
                if let Some(c) = self.calls.get_mut(&local) {
                    let ack = c.leg.ack_bytes(ts);
                    self.push(ack);
                }
            }
            (frametype::CONTROL, control::RINGING) => {
                if let Some(c) = self.calls.get_mut(&local) {
                    c.state = CallState::Ringing;
                    let ack = c.leg.ack_bytes(ts);
                    self.push(ack);
                }
                self.emit(Event::Ringing { call: local });
            }
            (frametype::CONTROL, control::ANSWER) => {
                if let Some(c) = self.calls.get_mut(&local) {
                    c.state = CallState::Up;
                    c.sent_format = false;
                    let ack = c.leg.ack_bytes(ts);
                    self.push(ack);
                }
                self.emit(Event::Answered { call: local });
            }
            (frametype::CONTROL, control::BUSY) => {
                if let Some(c) = self.calls.get_mut(&local) {
                    let ack = c.leg.ack_bytes(ts);
                    self.push(ack);
                }
                self.drop_call(local, "busy".into());
            }
            (frametype::IAX, iax::HANGUP) => {
                if let Some(c) = self.calls.get_mut(&local) {
                    let ack = c.leg.ack_bytes(ts);
                    self.push(ack);
                }
                self.drop_call(local, "hangup by remote".into());
            }
            (frametype::IAX, iax::REJECT) => {
                let code = ie::find(&f.ies, iet::CAUSECODE).and_then(|i| i.data.first().copied());
                if let Some(c) = self.calls.get_mut(&local) {
                    let ack = c.leg.ack_bytes(ts);
                    self.push(ack);
                }
                self.drop_call(local, format!("refused causecode={code:?}"));
            }
            (frametype::IAX, iax::ACK) | (frametype::IAX, iax::PONG) => {}
            (frametype::IAX, iax::LAGRQ) => {
                // Lag request: misura RTT sulla chiamata. Rispondi LAGRP
                // riecheggiando il timestamp della LAGRQ (come il PONG di
                // qualify), altrimenti Asterisk calcola un lag assurdo.
                // L'iseq della LAGRP ack-a implicitamente la LAGRQ.
                if let Some(c) = self.calls.get_mut(&local) {
                    let lagrp = c.leg.build(f.timestamp, frametype::IAX, iax::LAGRP, vec![]);
                    self.push(lagrp.encode());
                }
            }
            
            (frametype::IAX, iax::PING) => {
                if let Some(c) = self.calls.get_mut(&local) {
                let pong = c.leg.build(f.timestamp, frametype::IAX, iax::PONG, vec![]);
                self.push(pong.encode());
                }
            }

            (frametype::IAX, iax::LAGRP) => {} // risposta a una nostra LAGRQ (non inviata): assorbi
            (frametype::DTMF, sc) => {
                // cifra DTMF dal remoto: ACKa (full-frame) ed emetti l'evento
                if let Some(c) = self.calls.get_mut(&local) {
                    let ack = c.leg.ack_bytes(ts);
                    self.push(ack);
                }
                self.emit(Event::Dtmf { call: local, digit: sc as char });
            }
            (frametype::CNG, _) => {
                // Comfort Noise: il capo remoto e' in silenzio (silence
                // suppression/VAD attivo). sc = livello in -dBov, non e' audio.
                // E' un full-frame: si ACKa e si tratta come silenzio. Niente
                // Event::Voice, niente log (altrimenti spamma durante le pause).
                if let Some(c) = self.calls.get_mut(&local) {
                    let ack = c.leg.ack_bytes(ts);
                    self.push(ack);
                }
            }
            (frametype::CONTROL, _) => {
                if let Some(c) = self.calls.get_mut(&local) {
                    let ack = c.leg.ack_bytes(ts);
                    self.push(ack);
                }
            }
            (ft, sc) => {
                if let Some(c) = self.calls.get_mut(&local) {
                    let ack = c.leg.ack_bytes(ts);
                    self.push(ack);
                }
                self.emit(Event::Log(format!("call leg ft=0x{ft:02x} sc=0x{sc:02x}")));
            }
        }
    }

    // --- costruttori di IE ---------------------------------------------

    fn reg_ies(&self) -> Vec<Ie> {
        let mut ies = vec![Ie::str(iet::USERNAME, &self.cfg.username), Ie::u16(iet::REFRESH, self.refresh)];
        match &self.calltoken {
            Some(t) => ies.push(Ie::new(iet::CALLTOKEN, t.clone())),
            None => ies.push(Ie::empty(iet::CALLTOKEN)),
        }
        ies
    }

    fn new_call_ies(&self, called: &str) -> Vec<Ie> {
        vec![
            Ie::u16(iet::VERSION, IAX_PROTO_VERSION),
            Ie::str(iet::CALLING_NUMBER, &self.cfg.username),
            Ie::str(iet::CALLING_NAME, "iax2"),
            Ie::str(iet::USERNAME, &self.cfg.username),
            Ie::u32(iet::CAPABILITY, self.cfg.audio_format),
            Ie::u32(iet::FORMAT, self.cfg.audio_format),
            Ie::str(iet::CALLED_NUMBER, called),
            Ie::empty(iet::CALLTOKEN),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::{frametype, iax, ie as iec};

    fn cfg() -> Config {
        Config::new("Test", "10001", "segreto")
    }

    /// Decodifica l'ultimo datagramma full-frame uscito.
    fn drain_full(c: &mut PbxClient) -> Vec<FullFrame> {
        let mut v = Vec::new();
        while let Some(b) = c.poll_transmit() {
            if frame::is_full_frame(&b) {
                if let Some(f) = FullFrame::decode(&b) {
                    v.push(f);
                }
            }
        }
        v
    }

    #[test]
    fn registration_handshake_with_calltoken() {
        let mut c = PbxClient::new(cfg());
        let now = Instant::now();

        // 1) il timeout iniziale deve produrre un REGREQ
        c.handle_timeout(now);
        let out = drain_full(&mut c);
        let regreq = out.iter().find(|f| f.subclass == iax::REGREQ).expect("REGREQ");
        assert_eq!(regreq.src_call, CALLNO_REG);
        assert!(ie::find(&regreq.ies, iec::CALLTOKEN).unwrap().data.is_empty(), "first token empty");

        // 2) il PBX risponde con un CALLTOKEN -> deve rispedire REGREQ fresca col token
        let token = b"TOKEN123".to_vec();
        let tokframe = FullFrame::new(5, CALLNO_REG, 10, 0, 1, frametype::IAX, iax::CALLTOKEN, vec![Ie::new(iec::CALLTOKEN, token.clone())]);
        c.handle_input(&tokframe.encode(), now);
        let out = drain_full(&mut c);
        let regreq2 = out.iter().find(|f| f.subclass == iax::REGREQ).expect("REGREQ#2");
        assert_eq!(ie::find(&regreq2.ies, iec::CALLTOKEN).unwrap().data, token, "token incorporato");
        assert_eq!(regreq2.oseq, 0, "richiesta fresca: oseq azzerato");

        // 3) REGAUTH (MD5 + challenge) -> deve rispondere REGREQ con MD5_RESULT
        let regauth = FullFrame::new(
            5,
            CALLNO_REG,
            20,
            0,
            2,
            frametype::IAX,
            iax::REGAUTH,
            vec![Ie::u16(iec::AUTHMETHODS, authmethod::MD5), Ie::str(iec::CHALLENGE, "98765")],
        );
        c.handle_input(&regauth.encode(), now);
        let out = drain_full(&mut c);
        let authed = out.iter().find(|f| f.subclass == iax::REGREQ).expect("REGREQ con MD5");
        let md5 = ie::find(&authed.ies, iec::MD5_RESULT).expect("MD5_RESULT");
        assert_eq!(md5.as_str(), crate::md5_response("98765", "segreto"));

        // 4) REGACK -> Registered + ACK in uscita + evento
        let regack = FullFrame::new(5, CALLNO_REG, 30, 1, 3, frametype::IAX, iax::REGACK, vec![Ie::u16(iec::REFRESH, 120)]);
        c.handle_input(&regack.encode(), now);
        assert!(c.is_registered());
        let evs: Vec<_> = std::iter::from_fn(|| c.poll_event()).collect();
        assert!(evs.iter().any(|e| matches!(e, Event::Registered)), "evento Registered");
    }

    #[test]
    fn incoming_call_rings_then_answers_then_audio() {
        let now = Instant::now();
        let mut c = PbxClient::new(cfg());
        c.reg_state = RegState::Registered; // salta la registrazione

        // NEW in ingresso da Asterisk (src_call=42, dst_call=0).
        let new = FullFrame::new(
            42,
            0,
            100,
            0,
            0,
            frametype::IAX,
            iax::NEW,
            vec![Ie::str(iec::CALLING_NUMBER, "200"), Ie::str(iec::CALLED_NUMBER, "10001")],
        );
        c.handle_input(&new.encode(), now);

        // deve uscire ACCEPT + RINGING e un evento Incoming.
        let out = drain_full(&mut c);
        assert!(out.iter().any(|f| f.subclass == iax::ACCEPT), "ACCEPT");
        assert!(out.iter().any(|f| f.frametype == frametype::CONTROL && f.subclass == control::RINGING), "RINGING");
        let call = match c.poll_event() {
            Some(Event::Incoming { call, from, .. }) => {
                assert_eq!(from, "200");
                call
            }
            other => panic!("atteso Incoming, trovato {other:?}"),
        };
        assert_eq!(c.call_state(call), Some(CallState::Ringing));

        // rispondi -> ANSWER + evento Answered + stato Up
        c.handle_command(Command::Answer { call }, now);
        let out = drain_full(&mut c);
        assert!(out.iter().any(|f| f.frametype == frametype::CONTROL && f.subclass == control::ANSWER), "ANSWER");
        assert!(std::iter::from_fn(|| c.poll_event()).any(|e| matches!(e, Event::Answered { .. })));
        assert_eq!(c.call_state(call), Some(CallState::Up));

        // audio in ingresso (mini-frame con src_call del remoto) -> evento Voice
        // (il remote_index e' stato popolato dal NEW: 42 -> local)
        let mf = MiniFrame { src_call: 42, timestamp16: 20, payload: vec![0xFF; 160] };
        c.handle_input(&mf.encode(), now);
        let voice = std::iter::from_fn(|| c.poll_event()).find(|e| matches!(e, Event::Voice { .. }));
        assert!(voice.is_some(), "evento Voice dal mini-frame");

        // invio audio: primo frame full VOICE (dichiara formato), poi mini
        c.handle_command(Command::SendVoice { call, ulaw: vec![0x7F; 160] }, now);
        let out = drain_full(&mut c);
        assert!(out.iter().any(|f| f.frametype == frametype::VOICE), "primo full VOICE");
    }

    #[test]
    fn two_concurrent_calls_have_isolated_reliability() {
        // Il bug di classe da evitare: l'ACK della chiamata B che azzera la
        // finestra affidabile della chiamata A.
        let now = Instant::now();
        let mut c = PbxClient::new(cfg());
        c.reg_state = RegState::Registered;

        // due NEW in ingresso da due src_call diversi
        for (src, caller) in [(42u16, "200"), (43u16, "201")] {
            let new = FullFrame::new(src, 0, 100, 0, 0, frametype::IAX, iax::NEW, vec![Ie::str(iec::CALLING_NUMBER, caller), Ie::str(iec::CALLED_NUMBER, "10001")]);
            c.handle_input(&new.encode(), now);
        }
        // due eventi Incoming, due call leg distinte
        let calls: Vec<u16> = std::iter::from_fn(|| c.poll_event())
            .filter_map(|e| if let Event::Incoming { call, .. } = e { Some(call) } else { None })
            .collect();
        assert_eq!(calls.len(), 2, "due chiamate concorrenti");
        assert_ne!(calls[0], calls[1], "call number distinti");
        let _ = drain_full(&mut c);

        // entrambe hanno ACCEPT+RINGING affidabili in volo (finestra non vuota)
        for &call in &calls {
            assert!(c.calls.get(&call).unwrap().leg.outstanding.len() >= 1, "finestra A/B non vuota");
        }

        // un ACK cumulativo sulla chiamata calls[0] NON deve toccare calls[1]
        let leg0 = &c.calls[&calls[0]].leg;
        let (l0, r0, iseq0) = (leg0.local, leg0.remote, leg0.oseq);
        let ack = FullFrame::new(r0, l0, 5, iseq0, 0, frametype::IAX, iax::ACK, vec![]);
        let before_b = c.calls[&calls[1]].leg.outstanding.len();
        c.handle_input(&ack.encode(), now);
        let after_b = c.calls[&calls[1]].leg.outstanding.len();
        assert_eq!(before_b, after_b, "l'ACK di A non tocca la finestra di B");
    }

    #[test]
    fn qualify_pong_echoes_timestamp_and_rotates_callno() {
        let now = Instant::now();
        let mut c = PbxClient::new(cfg());
        c.reg_state = RegState::Registered;

        let mut seen = Vec::new();
        for ts_poke in [14u32, 1000, 2000] {
            let poke = FullFrame::new(7, 0, ts_poke, 0, 0, frametype::IAX, iax::POKE, vec![]);
            c.handle_input(&poke.encode(), now);
            let out = drain_full(&mut c);
            let pong = out.iter().find(|f| f.subclass == iax::PONG).expect("PONG");
            assert_eq!(pong.timestamp, ts_poke, "LEZIONE 3: PONG riecheggia il ts del POKE");
            seen.push(pong.src_call);
        }
        // call number rotante: non sempre lo stesso (niente NOTICE)
        assert!(seen[0] != seen[1] || seen[1] != seen[2], "il callno del PONG ruota");
    }

    #[test]
    fn in_call_ping_gets_pong_on_active_call_not_new_callno() {
        // Riproduce il bug: Asterisk manda PING con dst_call = scall locale
        // della call attiva (18 nel log reale). Il client NON deve cadere nel
        // ramo del qualify standalone (che alloca un nuovo scall rotante), ma
        // deve rispondere PONG sulla stessa call, con SCall=locale e
        // DCall=remoto, riecheggiando il timestamp — esattamente come fa già
        // per LAGRQ/LAGRP.
        let now = Instant::now();
        let mut c = PbxClient::new(cfg());
        c.reg_state = RegState::Registered;

        // stabilisci una call in ingresso, portala a Up (come in
        // incoming_call_rings_then_answers_then_audio)
        let new = FullFrame::new(
            42,
            0,
            100,
            0,
            0,
            frametype::IAX,
            iax::NEW,
            vec![Ie::str(iec::CALLING_NUMBER, "200"), Ie::str(iec::CALLED_NUMBER, "10001")],
        );
        c.handle_input(&new.encode(), now);
        let _ = drain_full(&mut c);
        let call = match c.poll_event() {
            Some(Event::Incoming { call, .. }) => call,
            other => panic!("atteso Incoming, trovato {other:?}"),
        };
        c.handle_command(Command::Answer { call }, now);
        let _ = drain_full(&mut c);
        let _ = std::iter::from_fn(|| c.poll_event()).count();
        assert_eq!(c.call_state(call), Some(CallState::Up));

        let local_before = c.calls[&call].leg.local; // scall nostro reale (es. 18)
        let remote = c.calls[&call].leg.remote; // scall di Asterisk per questa call (42)

        // PING in-call: dst_call = il nostro scall reale, non 0.
        let ping = FullFrame::new(remote, local_before, 5000, 3, 4, frametype::IAX, iax::PING, vec![]);
        c.handle_input(&ping.encode(), now);
        let out = drain_full(&mut c);

        let pong = out.iter().find(|f| f.subclass == iax::PONG).expect("PONG in risposta al PING in-call");
        assert_eq!(pong.timestamp, 5000, "il PONG riecheggia il ts del PING");
        assert_eq!(pong.src_call, local_before, "SCall del PONG = scall locale della call attiva, non un nuovo callno");
        assert_eq!(pong.dst_call, remote, "DCall del PONG = scall del peer");

        // la call non deve essere toccata/duplicata: stesso local, ancora Up
        assert_eq!(c.calls.len(), 1, "nessuna call fantasma creata dal PING");
        assert_eq!(c.calls[&call].leg.local, local_before, "lo scall locale della call non cambia");
        assert_eq!(c.call_state(call), Some(CallState::Up), "la call resta Up");
    }



    #[test]
    fn reconstruct_ts_handles_wrap() {
        // primo mini-frame senza riferimento: prende i 16 bit cosi' come sono
        assert_eq!(reconstruct_ts(0, 1234, false), 1234);
        // riferimento nello stesso blocco di 16 bit
        assert_eq!(reconstruct_ts(0x0001_2000, 0x2050, true), 0x0001_2050);
        // wrap in avanti: last appena sotto il confine, ts16 appena sopra zero
        assert_eq!(reconstruct_ts(0x0001_FFF0, 0x0010, true), 0x0002_0010);
        // riordino lieve all'indietro: ts16 poco prima del confine
        assert_eq!(reconstruct_ts(0x0002_0010, 0xFFF0, true), 0x0001_FFF0);
    }

    #[test]
    fn dtmf_send_and_receive() {
        let now = Instant::now();
        let mut c = PbxClient::new(cfg());
        c.reg_state = RegState::Registered;
        // chiamata in ingresso + risposta -> Up, remote=42
        let new = FullFrame::new(42, 0, 100, 0, 0, frametype::IAX, iax::NEW, vec![Ie::str(iec::CALLING_NUMBER, "200"), Ie::str(iec::CALLED_NUMBER, "10001")]);
        c.handle_input(&new.encode(), now);
        let call = std::iter::from_fn(|| c.poll_event())
            .find_map(|e| if let Event::Incoming { call, .. } = e { Some(call) } else { None })
            .expect("Incoming");
        c.handle_command(Command::Answer { call }, now);
        let _ = drain_full(&mut c);

        // invio: '5' deve uscire come full-frame DTMF con subclass = b'5'
        c.handle_command(Command::Dtmf { call, digit: '5' }, now);
        let out = drain_full(&mut c);
        let dtmf = out.iter().find(|f| f.frametype == frametype::DTMF).expect("frame DTMF");
        assert_eq!(dtmf.subclass, b'5');

        // cifra non valida: niente frame
        c.handle_command(Command::Dtmf { call, digit: 'Z' }, now);
        assert!(drain_full(&mut c).iter().all(|f| f.frametype != frametype::DTMF), "cifra invalida scartata");

        // ricezione: un frame DTMF in ingresso emette Event::Dtmf
        let rx = FullFrame::new(42, call, 200, 3, 3, frametype::DTMF, b'#', vec![]);
        c.handle_input(&rx.encode(), now);
        let got = std::iter::from_fn(|| c.poll_event())
            .find_map(|e| if let Event::Dtmf { digit, .. } = e { Some(digit) } else { None })
            .expect("Event::Dtmf");
        assert_eq!(got, '#');
    }

    #[test]
    fn lagrq_on_call_leg_replies_lagrp_echoing_timestamp() {
        let now = Instant::now();
        let mut c = PbxClient::new(cfg());
        c.reg_state = RegState::Registered;

        // chiamata in ingresso e risposta -> leg Up con remote=42
        let new = FullFrame::new(42, 0, 100, 0, 0, frametype::IAX, iax::NEW, vec![Ie::str(iec::CALLING_NUMBER, "200"), Ie::str(iec::CALLED_NUMBER, "10001")]);
        c.handle_input(&new.encode(), now);
        let call = std::iter::from_fn(|| c.poll_event())
            .find_map(|e| if let Event::Incoming { call, .. } = e { Some(call) } else { None })
            .expect("Incoming");
        c.handle_command(Command::Answer { call }, now);
        let _ = drain_full(&mut c);

        // Asterisk manda LAGRQ con un suo timestamp; ci aspettiamo LAGRP con LO STESSO ts
        let lagrq = FullFrame::new(42, call, 7777, 5, 5, frametype::IAX, iax::LAGRQ, vec![]);
        c.handle_input(&lagrq.encode(), now);
        let out = drain_full(&mut c);
        let lagrp = out.iter().find(|f| f.frametype == frametype::IAX && f.subclass == iax::LAGRP).expect("LAGRP");
        assert_eq!(lagrp.timestamp, 7777, "la LAGRP deve riecheggiare il ts della LAGRQ");
        // e ack-a implicitamente la LAGRQ via iseq
        assert_eq!(lagrp.iseq, lagrq.oseq.wrapping_add(1), "iseq della LAGRP = oseq LAGRQ + 1");
    }
}