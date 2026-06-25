//! regtest — IAX2 REGISTRATION handshake with real PBX.
//!
//! Use: regtest <host> <username> <secret> [port] [refresh_sec]
//!

use std::env;
use std::time::{Duration, Instant};

use iax2::consts::{authmethod, frametype, iax, ie as iet};
use iax2::ie::{self, Ie};
use iax2::frame::{self, FullFrame};
use tokio::net::UdpSocket;
use tokio::time::timeout;

const IAX_PORT_DEFAULT: u16 = 4569;
const RETRANSMIT_AFTER: Duration = Duration::from_millis(1000);
const MAX_ATTEMPTS: u32 = 5;


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
        Session {
            src_call: 1, // un call number qualsiasi 1..=32767
            dst_call: 0, // sconosciuto finche' il PBX non risponde
            oseq: 0,
            iseq: 0,
            start: Instant::now(),
            calltoken: None,
        }
    }

    /// Timestamp relativo in millisecondi (campo a 32 bit dei full frame).
    fn ts(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    /// Costruisce un full frame IAX in uscita consumando un numero di sequenza.
    fn outgoing(&mut self, subclass: u8, ies: Vec<Ie>) -> FullFrame {
        let f = FullFrame::new(
            self.src_call,
            self.dst_call,
            self.ts(),
            self.oseq,
            self.iseq,
            frametype::IAX,
            subclass,
            ies,
        );
        self.oseq = self.oseq.wrapping_add(1);
        f
    }

    /// ACK: NON consuma un numero di sequenza (semantica chan_iax2).
    fn ack(&self) -> FullFrame {
        FullFrame::new(
            self.src_call,
            self.dst_call,
            self.ts(),
            self.oseq,
            self.iseq,
            frametype::IAX,
            iax::ACK,
            vec![],
        )
    }

    /// IE comuni a tutti i REGREQ (con call-token se gia' ottenuto dal PBX).
    fn regreq_ies(&self, username: &str, refresh: u16, extra: Vec<Ie>) -> Vec<Ie> {
        let mut ies = vec![Ie::str(iet::USERNAME, username)];
        ies.extend(extra);
        ies.push(Ie::u16(iet::REFRESH, refresh));
        match &self.calltoken {
            Some(tok) => ies.push(Ie::new(iet::CALLTOKEN, tok.clone())),
            // primo invio: IE CALLTOKEN vuoto per dichiarare il supporto
            None => ies.push(Ie::empty(iet::CALLTOKEN)),
        }
        ies
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 {
        eprintln!("uso: {} <host> <username> <secret> [port] [refresh_sec]", args[0]);
        std::process::exit(2);
    }
    let host = &args[1];
    let username = &args[2];
    let secret = &args[3];
    let port: u16 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(IAX_PORT_DEFAULT);
    let refresh: u16 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(60);

    let remote = format!("{host}:{port}");
    let sock = UdpSocket::bind("0.0.0.0:0").await.expect("local bind");
    sock.connect(&remote).await.expect("connect (host resolution fails?)");
    println!("[i] socket {} -> {remote}", sock.local_addr().unwrap());

    let mut s = Session::new();

    // --- 1) REGREQ iniziale (non autenticato) -------------------------------
    let mut pending = s.outgoing(iax::REGREQ, s.regreq_ies(username, refresh, vec![]));
    send(&sock, &pending, "REGREQ (init)").await;

    let mut buf = [0u8; 4096];
    let mut attempts = 0u32;

    loop {
        match timeout(RETRANSMIT_AFTER, sock.recv(&mut buf)).await {
            Err(_) => {
                attempts += 1;
                if attempts >= MAX_ATTEMPTS {
                    eprintln!("[x] no answer after {MAX_ATTEMPTS} tries");
                    std::process::exit(1);
                }
                pending.retransmit = true;
                pending.timestamp = s.ts();
                send(&sock, &pending, "(retransmission)").await;
            }
            Ok(Ok(n)) => {
                attempts = 0;
                let datagram = &buf[..n];
                if !frame::is_full_frame(datagram) {
                    continue; // ignora mini frame in fase di registrazione
                }
                let Some(f) = FullFrame::decode(datagram) else {
                    eprintln!("[!] full frame not decoded ({n} byte)");
                    continue;
                };
                if f.frametype != frametype::IAX {
                    continue;
                }

                if let Some(tok) = ie::find(&f.ies, iet::CALLTOKEN) {
                    if !tok.data.is_empty() && s.calltoken.is_none() {
                        println!("[i] received call-token ricevuto ({} byte), resend REGREQ (refresh)", tok.data.len());
                        s.calltoken = Some(tok.data.clone());
                        s.oseq = 0;
                        s.iseq = 0;
                        s.dst_call = 0;
                        pending = s.outgoing(iax::REGREQ, s.regreq_ies(username, refresh, vec![]));
                        send(&sock, &pending, "REGREQ (with call-token)").await;
                        continue;
                    }
                }

                if f.src_call != 0 {
                    s.dst_call = f.src_call;
                }
                s.iseq = f.oseq.wrapping_add(1);

                match f.subclass {
                    iax::REGAUTH => {
                        let methods = ie::find(&f.ies, iet::AUTHMETHODS)
                            .and_then(|ie| ie.as_u16())
                            .unwrap_or(0);
                        let challenge = ie::find(&f.ies, iet::CHALLENGE).map(|ie| ie.as_str());
                        println!("[i] REGAUTH: methods=0x{methods:04x} challenge={challenge:?}");

                        if methods & authmethod::MD5 == 0 {
                            eprintln!("[x] il PBX non offre MD5 (questo spike fa solo MD5). methods=0x{methods:04x}");
                            std::process::exit(1);
                        }
                        let Some(chal) = challenge else {
                            eprintln!("[x] REGAUTH without CHALLENGE");
                            std::process::exit(1);
                        };
                        let md5 = iax2::md5_response(&chal, secret);
                        let extra = vec![Ie::str(iet::MD5_RESULT, &md5)];
                        pending = s.outgoing(iax::REGREQ, s.regreq_ies(username, refresh, extra));
                        send(&sock, &pending, "REGREQ (authenticated MD5)").await;
                    }
                    iax::REGACK => {
                        println!("[\u{2713}] REGACK — registration success!");
                        if let Some(dt) = ie::find(&f.ies, iet::DATETIME) {
                            println!("    datetime IE: {} byte", dt.data.len());
                        }
                        if let Some(rf) = ie::find(&f.ies, iet::REFRESH).and_then(|ie| ie.as_u16()) {
                            println!("    best refresh: {rf}s");
                        }
                        send(&sock, &s.ack(), "ACK").await;
                        return;
                    }
                    iax::REGREJ => {
                        let cause = ie::find(&f.ies, iet::CAUSE).map(|ie| ie.as_str());
                        let code = ie::find(&f.ies, iet::CAUSECODE).and_then(|ie| ie.data.first().copied());
                        eprintln!("[x] REGREJ — registration refused. cause={cause:?} causecode={code:?}");
                        eprintln!("    IE (types): {:?}", f.ies.iter().map(|ie| ie.kind).collect::<Vec<_>>());
                        send(&sock, &s.ack(), "ACK").await;
                        std::process::exit(1);
                    }
                    other => {
                        println!("[i] frame IAX subclass=0x{other:02x} (ignored)");
                    }
                }
            }
            Ok(Err(e)) => {
                eprintln!("[x] error recv: {e}");
                std::process::exit(1);
            }
        }
    }
}

async fn send(sock: &UdpSocket, f: &FullFrame, label: &str) {
    let bytes = f.encode();
    match sock.send(&bytes).await {
        Ok(_) => println!("[>] {label}: oseq={} iseq={} dst_call={} ({} byte)", f.oseq, f.iseq, f.dst_call, bytes.len()),
        Err(e) => eprintln!("[x] send {label}: {e}"),
    }
}
