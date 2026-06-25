//! calltest — Start a call to *43 ( Asterisk Echo Test ) to test audio echo.
//!
//! Use: calltest <host> <username> <secret> [called] [port] [secondi]

use std::env;
use std::time::{Duration, Instant};

use iax2::audio::{AudioIo, SAMPLE_RATE};
use iax2::consts::{authmethod, control, format, frametype, iax, ie as iet};
use iax2::frame::{self, FullFrame, MiniFrame};
use iax2::g711;
use iax2::ie::{self, Ie};
use tokio::net::UdpSocket;
use tokio::time::{interval, sleep};

const IAX_PORT_DEFAULT: u16 = 4569;
const FRAME_SAMPLES: usize = 160; // 20 ms @ 8 kHz

struct Session {
    src_call: u16,
    dst_call: u16,
    oseq: u8,
    iseq: u8,
    start: Instant,
    calltoken: Option<Vec<u8>>,
}
impl Session {
    fn new() -> Self {
        Session { src_call: 1, dst_call: 0, oseq: 0, iseq: 0, start: Instant::now(), calltoken: None }
    }
    fn ts(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }
    fn outgoing(&mut self, ft: u8, sc: u8, ies: Vec<Ie>) -> FullFrame {
        let f = FullFrame::new(self.src_call, self.dst_call, self.ts(), self.oseq, self.iseq, ft, sc, ies);
        self.oseq = self.oseq.wrapping_add(1);
        f
    }
    fn ack_bytes(&self) -> Vec<u8> {
        FullFrame::new(self.src_call, self.dst_call, self.ts(), self.oseq, self.iseq, frametype::IAX, iax::ACK, vec![]).encode()
    }
    fn new_call_ies(&self, username: &str, called: &str) -> Vec<Ie> {
        let mut ies = vec![
            Ie::u16(iet::VERSION, iax2::consts::IAX_PROTO_VERSION),
            Ie::str(iet::CALLING_NUMBER, username),
            Ie::str(iet::CALLING_NAME, "iax2-spike"),
            Ie::str(iet::USERNAME, username),
            Ie::u32(iet::CAPABILITY, format::ULAW | format::ALAW),
            Ie::u32(iet::FORMAT, format::ULAW),
            Ie::str(iet::CALLED_NUMBER, called),
        ];
        match &self.calltoken {
            Some(t) => ies.push(Ie::new(iet::CALLTOKEN, t.clone())),
            None => ies.push(Ie::empty(iet::CALLTOKEN)),
        }
        ies
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 {
        eprintln!("uso: {} <host> <username> <secret> [called] [port] [seconds]", args[0]);
        std::process::exit(2);
    }
    let host = &args[1];
    let username = &args[2];
    let secret = &args[3];
    let called = args.get(4).cloned().unwrap_or_else(|| "*43".to_string());
    let port: u16 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(IAX_PORT_DEFAULT);
    let seconds: u64 = args.get(6).and_then(|s| s.parse().ok()).unwrap_or(15);

    let mut audio = match AudioIo::new() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[x] audio not available: {e}");
            std::process::exit(1);
        }
    };
    println!("[i] audio: in {}Hz/{}ch  out {}Hz/{}ch", audio.in_rate, audio.in_ch, audio.out_rate, audio.out_ch);
    let _ = SAMPLE_RATE;

    let remote = format!("{host}:{port}");
    let sock = UdpSocket::bind("0.0.0.0:0").await.expect("bind");
    sock.connect(&remote).await.expect("connect");
    println!("[i] {} -> {remote}, calling {called}", sock.local_addr().unwrap());

    let mut s = Session::new();
    let mut pending = s.outgoing(frametype::IAX, iax::NEW, s.new_call_ies(username, &called));
    send(&sock, &pending.encode(), "NEW (initial)").await;

    let mut answered = false;
    let mut sent_format = false;
    let mut buf = [0u8; 4096];
    let mut tick = interval(Duration::from_millis(20));
    let deadline = sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                if !answered { continue; }
                for pcm in audio.take_frames_8k(FRAME_SAMPLES) {
                    let mut payload = Vec::with_capacity(FRAME_SAMPLES);
                    g711::encode_block(&pcm, &mut payload);
                    if !sent_format {
                        let vf = s.outgoing(frametype::VOICE, format::ULAW as u8, vec![]);
                        let _ = sock.send(&vf.encode_media(&payload)).await;
                        sent_format = true;
                    } else {
                        let mf = MiniFrame { src_call: s.src_call, timestamp16: s.ts() as u16, payload };
                        let _ = sock.send(&mf.encode()).await;
                    }
                }
            }
            r = sock.recv(&mut buf) => {
                let n = match r { Ok(n) => n, Err(e) => { eprintln!("[x] recv: {e}"); break; } };
                let dg = &buf[..n];
                if !frame::is_full_frame(dg) {
                    if let Some(m) = MiniFrame::decode(dg) { audio.play_ulaw(&m.payload); }
                    continue;
                }
                let Some(f) = FullFrame::decode(dg) else { continue; };
                if let Some(tok) = ie::find(&f.ies, iet::CALLTOKEN) {
                    if !tok.data.is_empty() && s.calltoken.is_none() {
                        println!("[i] call-token, resend NEW (refresh)");
                        s.calltoken = Some(tok.data.clone());
                        s.oseq = 0; s.iseq = 0; s.dst_call = 0;
                        pending = s.outgoing(frametype::IAX, iax::NEW, s.new_call_ies(username, &called));
                        send(&sock, &pending.encode(), "NEW (with call-token)").await;
                        continue;
                    }
                }
                if f.src_call != 0 { s.dst_call = f.src_call; }
                s.iseq = f.oseq.wrapping_add(1);

                match (f.frametype, f.subclass) {
                    (frametype::VOICE, fmt) => {
                        if fmt as u32 != format::ULAW { eprintln!("[!] voice non-ULAW: 0x{fmt:02x}"); }
                        audio.play_ulaw(&f.media_payload);
                        let _ = sock.send(&s.ack_bytes()).await;
                    }
                    (frametype::IAX, iax::AUTHREQ) => {
                        let methods = ie::find(&f.ies, iet::AUTHMETHODS).and_then(|i| i.as_u16()).unwrap_or(0);
                        let challenge = ie::find(&f.ies, iet::CHALLENGE).map(|i| i.as_str());
                        if methods & authmethod::MD5 == 0 { eprintln!("[x] no MD5"); break; }
                        let Some(chal) = challenge else { eprintln!("[x] no challenge"); break; };
                        let md5 = iax2::md5_response(&chal, secret);
                        let rep = s.outgoing(frametype::IAX, iax::AUTHREP, vec![Ie::str(iet::MD5_RESULT, &md5)]);
                        send(&sock, &rep.encode(), "AUTHREP (MD5)").await;
                    }
                    (frametype::IAX, iax::ACCEPT) => {
                        let fmt = ie::find(&f.ies, iet::FORMAT).and_then(|i| i.as_u32());
                        println!("[\u{2713}] ACCEPT format={fmt:?}");
                        let _ = sock.send(&s.ack_bytes()).await;
                    }
                    (frametype::CONTROL, control::RINGING) => { println!("[i] RINGING…"); let _ = sock.send(&s.ack_bytes()).await; }
                    (frametype::CONTROL, control::ANSWER) => {
                        println!("[\u{2713}] ANSWER — speak please, you must be hear your echo");
                        answered = true;
                        let _ = sock.send(&s.ack_bytes()).await;
                    }
                    (frametype::CONTROL, control::BUSY) => { println!("[x] BUSY"); let _ = sock.send(&s.ack_bytes()).await; break; }
                    (frametype::IAX, iax::PING) | (frametype::IAX, iax::POKE) => {
                        // PONG riecheggia il timestamp del PING/POKE (RTT qualify).
                        let pong = FullFrame::new(s.src_call, s.dst_call, f.timestamp, s.oseq, s.iseq,
                                                  frametype::IAX, iax::PONG, vec![]).encode();
                        let _ = sock.send(&pong).await;
                    }
                    (frametype::IAX, iax::HANGUP) => { println!("[i] HANGUP"); let _ = sock.send(&s.ack_bytes()).await; break; }
                    (frametype::IAX, iax::REJECT) => {
                        let code = ie::find(&f.ies, iet::CAUSECODE).and_then(|i| i.data.first().copied());
                        eprintln!("[x] REJECT causecode={code:?}");
                        let _ = sock.send(&s.ack_bytes()).await; break;
                    }
                    (frametype::IAX, iax::ACK) | (frametype::IAX, iax::PONG) => {}
                    (ft, sc) => { println!("[i] not processed ft=0x{ft:02x} sc=0x{sc:02x}"); let _ = sock.send(&s.ack_bytes()).await; }
                }
            }
            _ = &mut deadline => {
                println!("[i] time out, HANGUP");
                let h = s.outgoing(frametype::IAX, iax::HANGUP, vec![Ie::str(iet::CAUSE, "Normal Clearing")]);
                let _ = sock.send(&h.encode()).await;
                break;
            }
        }
    }
    println!("[i] End.");
}

async fn send(sock: &UdpSocket, bytes: &[u8], label: &str) {
    match sock.send(bytes).await {
        Ok(_) => println!("[>] {label} ({} byte)", bytes.len()),
        Err(e) => eprintln!("[x] send {label}: {e}"),
    }
}
