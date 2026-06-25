//! regdaemon — IAX2 phone "alway on".
//!
//!
//! Uso: regdaemon <host> <username> <secret> [port] [refresh_sec]
//!

use std::env;
use std::time::{Duration, Instant};

use iax2::audio::AudioIo;
use iax2::consts::{authmethod, control, format, frametype, iax, ie as iet};
use iax2::frame::{self, FullFrame, MiniFrame};
use iax2::g711;
use iax2::ie::{self, Ie};
use tokio::net::UdpSocket;
use tokio::time::interval;

const IAX_PORT_DEFAULT: u16 = 4569;
const FRAME_SAMPLES: usize = 160;
const REG_LOCAL_CALL: u16 = 1;
const CALL_LOCAL_CALL: u16 = 2;
const KEEPALIVE_CALL: u16 = 4;
const RETX_AFTER: Duration = Duration::from_millis(700);
const RETX_MAX: u32 = 5;

/// Numbering/seq conversation leg.
struct Leg {
    local: u16,
    remote: u16,
    oseq: u8,
    iseq: u8,
}
impl Leg {
    fn new(local: u16) -> Self {
        Leg { local, remote: 0, oseq: 0, iseq: 0 }
    }
    fn reset(&mut self) {
        self.remote = 0;
        self.oseq = 0;
        self.iseq = 0;
    }
    fn note_recv(&mut self, f: &FullFrame) {
        if f.src_call != 0 {
            self.remote = f.src_call;
        }
        self.iseq = f.oseq.wrapping_add(1);
    }
    fn full(&mut self, ts: u32, ft: u8, sc: u8, ies: Vec<Ie>) -> FullFrame {
        let f = FullFrame::new(self.local, self.remote, ts, self.oseq, self.iseq, ft, sc, ies);
        self.oseq = self.oseq.wrapping_add(1);
        f
    }
    fn ack(&self, ts: u32) -> Vec<u8> {
        FullFrame::new(self.local, self.remote, ts, self.oseq, self.iseq, frametype::IAX, iax::ACK, vec![]).encode()
    }
}

struct Call {
    leg: Leg,
    answered: bool,
    sent_format: bool,
}

/// Full-frame trusted waiting for ACK.
struct Pending {
    bytes: Vec<u8>,
    deadline: Instant,
    tries: u32,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 {
        eprintln!("use: {} <host> <username> <secret> [port] [refresh_sec]", args[0]);
        std::process::exit(2);
    }
    let host = &args[1];
    let username = args[2].clone();
    let secret = args[3].clone();
    let port: u16 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(IAX_PORT_DEFAULT);
    let mut refresh: u16 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(60);

    let mut audio = match AudioIo::new() {
        Ok(a) => a,
        Err(e) => { eprintln!("[x] audio not available: {e}"); std::process::exit(1); }
    };
    println!("[i] audio: in {}Hz/{}ch  out {}Hz/{}ch", audio.in_rate, audio.in_ch, audio.out_rate, audio.out_ch);

    let remote = format!("{host}:{port}");
    let sock = UdpSocket::bind("0.0.0.0:0").await.expect("bind");
    sock.connect(&remote).await.expect("connect");
    println!("[i] {} -> {remote} (user {username})", sock.local_addr().unwrap());

    let start = Instant::now();
    let ts = || start.elapsed().as_millis() as u32;

    let mut reg = Leg::new(REG_LOCAL_CALL);
    let mut call: Option<Call> = None;
    let mut calltoken: Option<Vec<u8>> = None;
    let mut registered = false;
    let mut pending: Option<Pending> = None;
    let mut next_reg = Instant::now(); // registra subito

    let mut buf = [0u8; 4096];
    let mut audio_tick = interval(Duration::from_millis(20));
    let mut house = interval(Duration::from_millis(200));
    let mut keepalive = interval(Duration::from_secs(15));

    loop {
        tokio::select! {
            // ---- invio audio della chiamata in corso ----------------------
            _ = audio_tick.tick() => {
                if let Some(c) = call.as_mut() {
                    if c.answered {
                        for pcm in audio.take_frames_8k(FRAME_SAMPLES) {
                            let mut payload = Vec::with_capacity(FRAME_SAMPLES);
                            g711::encode_block(&pcm, &mut payload);
                            if !c.sent_format {
                                let vf = c.leg.full(ts(), frametype::VOICE, format::ULAW as u8, vec![]);
                                let _ = sock.send(&vf.encode_media(&payload)).await;
                                c.sent_format = true;
                            } else {
                                let mf = MiniFrame { src_call: c.leg.local, timestamp16: ts() as u16, payload };
                                let _ = sock.send(&mf.encode()).await;
                            }
                        }
                    }
                }
            }

            // ---- keepalive: periodic POKE against NAT closing ---
            _ = keepalive.tick() => {
                if registered {
                    let poke = FullFrame::new(KEEPALIVE_CALL, 0, ts(), 0, 0, frametype::IAX, iax::POKE, vec![]).encode();
                    let _ = sock.send(&poke).await;
                }
            }

            // ---- housekeeping: re-register + retrasmission ---------------
            _ = house.tick() => {
                // re-register (o primo register)
                if pending.is_none() && Instant::now() >= next_reg {
                    reg.reset();
                    calltoken = None; // i call-token scadono: ogni ciclo riparte fresco
                    let ies = reg_ies(&username, refresh, &calltoken);
                    let f = reg.full(ts(), frametype::IAX, iax::REGREQ, ies);
                    let bytes = f.encode();
                    println!("[>] REGREQ ({})", if registered { "refresh" } else { "initial" });
                    let _ = sock.send(&bytes).await;
                    pending = Some(Pending { bytes, deadline: Instant::now() + RETX_AFTER, tries: 0 });
                    next_reg = Instant::now() + Duration::from_secs(10); // guardia anti-spam
                }
                // ritrasmissione del frame affidabile in volo
                if let Some(p) = pending.as_mut() {
                    if Instant::now() >= p.deadline {
                        if p.tries >= RETX_MAX {
                            eprintln!("[!] no ACK after {RETX_MAX} retransmissions");
                            pending = None;
                            registered = false;
                            next_reg = Instant::now() + Duration::from_secs(3);
                        } else {
                            let _ = sock.send(&p.bytes).await;
                            p.tries += 1;
                            p.deadline = Instant::now() + RETX_AFTER;
                        }
                    }
                }
            }

            // ---- ricezione -------------------------------------------------
            r = sock.recv(&mut buf) => {
                let n = match r { Ok(n) => n, Err(e) => { eprintln!("[x] recv: {e}"); break; } };
                let dg = &buf[..n];

                // Mini-frame audio della chiamata.
                if !frame::is_full_frame(dg) {
                    if call.is_some() {
                        if let Some(m) = MiniFrame::decode(dg) { audio.play_ulaw(&m.payload); }
                    }
                    continue;
                }
                let Some(f) = FullFrame::decode(dg) else { continue; };

                let on_real_leg = f.dst_call == reg.local
                    || call.as_ref().map_or(false, |c| f.dst_call == c.leg.local);
                if on_real_leg {
                    pending = None;
                }

                // POKE/PING standalone -> PONG (qualify).
                if f.frametype == frametype::IAX && (f.subclass == iax::POKE || f.subclass == iax::PING) {
                    println!("[i] qualify {} dal PBX (src_call={}) -> PONG",
                        if f.subclass == iax::POKE { "POKE" } else { "PING" }, f.src_call);
                    let pong = FullFrame::new(f.dst_call.max(3), f.src_call, f.timestamp, 0, f.oseq.wrapping_add(1),
                                              frametype::IAX, iax::PONG, vec![]).encode();
                    let _ = sock.send(&pong).await;
                    continue;
                }

                // NEW incoming = incoming call.
                if f.frametype == frametype::IAX && f.subclass == iax::NEW {
                    if call.is_some() {
                        // gia' occupato: rifiuta.
                        let rej = FullFrame::new(99, f.src_call, ts(), 0, f.oseq.wrapping_add(1),
                            frametype::IAX, iax::REJECT, vec![Ie::str(iet::CAUSE, "Busy")]).encode();
                        let _ = sock.send(&rej).await;
                        continue;
                    }
                    let called = ie::find(&f.ies, iet::CALLED_NUMBER).map(|i| i.as_str()).unwrap_or_default();
                    let caller = ie::find(&f.ies, iet::CALLING_NUMBER).map(|i| i.as_str()).unwrap_or_default();
                    println!("[\u{260E}] INCOMING CALL FROM '{caller}' to '{called}'");

                    let mut leg = Leg::new(CALL_LOCAL_CALL);
                    leg.note_recv(&f);
                    // ACCEPT (format ULAW) — trusted.
                    let accept = leg.full(ts(), frametype::IAX, iax::ACCEPT, vec![Ie::u32(iet::FORMAT, format::ULAW)]);
                    let ab = accept.encode();
                    let _ = sock.send(&ab).await;
                    // ANSWER (auto-answer).
                    let answer = leg.full(ts(), frametype::CONTROL, control::ANSWER, vec![]);
                    let nb = answer.encode();
                    let _ = sock.send(&nb).await;
                    pending = Some(Pending { bytes: nb, deadline: Instant::now() + RETX_AFTER, tries: 0 });
                    audio.flush();
                    call = Some(Call { leg, answered: true, sent_format: false });
                    println!("[\u{2713}] ANSWERED — audio on");
                    continue;
                }

                if let Some(tok) = ie::find(&f.ies, iet::CALLTOKEN) {
                    if !tok.data.is_empty() && calltoken.is_none() {
                        calltoken = Some(tok.data.clone());
                        reg.reset();
                        let ies = reg_ies(&username, refresh, &calltoken);
                        let fr = reg.full(ts(), frametype::IAX, iax::REGREQ, ies);
                        let b = fr.encode();
                        let _ = sock.send(&b).await;
                        pending = Some(Pending { bytes: b, deadline: Instant::now() + RETX_AFTER, tries: 0 });
                        continue;
                    }
                }

                // Leg REGISTRATION frame.
                if f.dst_call == reg.local {
                    reg.note_recv(&f);
                    match f.subclass {
                        iax::REGAUTH => {
                            let methods = ie::find(&f.ies, iet::AUTHMETHODS).and_then(|i| i.as_u16()).unwrap_or(0);
                            let challenge = ie::find(&f.ies, iet::CHALLENGE).map(|i| i.as_str());
                            if methods & authmethod::MD5 == 0 { eprintln!("[x] no MD5 in REGAUTH"); continue; }
                            let Some(chal) = challenge else { eprintln!("[x] REGAUTH without challenge"); continue; };
                            let md5 = iax2::md5_response(&chal, &secret);
                            let mut ies = reg_ies(&username, refresh, &calltoken);
                            ies.insert(1, Ie::str(iet::MD5_RESULT, &md5));
                            let fr = reg.full(ts(), frametype::IAX, iax::REGREQ, ies);
                            let b = fr.encode();
                            let _ = sock.send(&b).await;
                            pending = Some(Pending { bytes: b, deadline: Instant::now() + RETX_AFTER, tries: 0 });
                        }
                        iax::REGACK => {
                            if let Some(rf) = ie::find(&f.ies, iet::REFRESH).and_then(|i| i.as_u16()) {
                                if rf > 0 { refresh = rf; }
                            }
                            registered = true;
                            next_reg = Instant::now() + Duration::from_secs((refresh as u64 * 4) / 5); // ~80%
                            let _ = sock.send(&reg.ack(ts())).await;
                            println!("[\u{2713}] REGISTERED (refresh {refresh}s, next ~{}s)", (refresh * 4) / 5);
                        }
                        iax::REGREJ => {
                            let code = ie::find(&f.ies, iet::CAUSECODE).and_then(|i| i.data.first().copied());
                            eprintln!("[x] REGREJ causecode={code:?} — I will retry ");
                            calltoken = None;
                            registered = false;
                            next_reg = Instant::now() + Duration::from_secs(5);
                            let _ = sock.send(&reg.ack(ts())).await;
                        }
                        iax::ACK => {}
                        iax::CALLTOKEN => {} // gia' gestito sopra; eventuali duplicati: silenzio
                        other => println!("[i] reg leg subclass=0x{other:02x}"),
                    }
                    continue;
                }

                // CALLING Leg frame.
                if let Some(c) = call.as_mut() {
                    if f.dst_call == c.leg.local {
                        c.leg.note_recv(&f);
                        match (f.frametype, f.subclass) {
                            (frametype::VOICE, _) => {
                                audio.play_ulaw(&f.media_payload);
                                let _ = sock.send(&c.leg.ack(ts())).await;
                            }
                            (frametype::IAX, iax::ACK) => {}
                            (frametype::IAX, iax::HANGUP) => {
                                println!("[i] Call closed by remote party");
                                let _ = sock.send(&c.leg.ack(ts())).await;
                                audio.flush();
                                call = None;
                            }
                            (frametype::CONTROL, _) => { let _ = sock.send(&c.leg.ack(ts())).await; }
                            (ft, sc) => {
                                println!("[i] call leg ft=0x{ft:02x} sc=0x{sc:02x}");
                                let _ = sock.send(&c.leg.ack(ts())).await;
                            }
                        }
                        continue;
                    }
                }

                match (f.frametype, f.subclass) {
                    // ACK/PONG di qualify su gamba sconosciuta: benigni, silenzio.
                    (frametype::IAX, iax::ACK)
                    | (frametype::IAX, iax::PONG)
                    | (frametype::IAX, iax::INVAL) => {}
                    _ => println!(
                        "[i] Out of leg frame: dst_call={} ft=0x{:02x} sc=0x{:02x}",
                        f.dst_call, f.frametype, f.subclass
                    ),
                }
            }
        }
    }
}


fn reg_ies(username: &str, refresh: u16, calltoken: &Option<Vec<u8>>) -> Vec<Ie> {
    let mut ies = vec![Ie::str(iet::USERNAME, username), Ie::u16(iet::REFRESH, refresh)];
    match calltoken {
        Some(t) => ies.push(Ie::new(iet::CALLTOKEN, t.clone())),
        None => ies.push(Ie::empty(iet::CALLTOKEN)),
    }
    ies
}
