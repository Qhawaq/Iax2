//! softphone — driver multilinea / multi-PBX.
//!
//! Possiede N `iax2::PbxClient` (uno per account/PBX), N socket UDP (uno per
//! PBX: spazio call-number isolato, niente NAT tra peer sull'overlay) e UN solo
//! motore audio. Instrada il microfono verso la chiamata ATTIVA e riproduce
//! solo l'audio di quella. Le altre restano "in attesa" (call waiting).
//!
//! Tutta la logica di protocollo sta nel core sans-io `iax2::client`; qui c'e'
//! solo I/O: socket, audio, tastiera.
//!
//! ## Uso
//! Singolo account (compatibile con regdaemon):
//!   softphone <host> <user> <secret> [port] [refresh] [nome]
//! Multi-account da file:
//!   softphone accounts.conf
//!
//! Formato file (INI minimale):
//!   [Catania]
//!   host = pbxctcatania.magaldinnova.tech
//!   port = 4569
//!   user = 10001
//!   secret = xxxxx
//!   refresh = 60
//!
//!   [Salerno]
//!   host = 10.10.9.1
//!   user = 10001
//!   secret = yyyyy
//!
//! ## Comandi da tastiera (una riga + invio)
//!   a            rispondi alla prima chiamata che squilla
//!   h            riaggancia la chiamata attiva
//!   d <num>      componi <num> sul PBX selezionato
//!   t <cifre>    invia DTMF (0-9 * # A-D) sulla chiamata attiva
//!   p <n>        seleziona il PBX n (per comporre)   [1-based]
//!   <n>          rendi attiva la chiamata n della lista   [1-based]
//!   l            elenca PBX e chiamate
//!   q            esci

use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iax2::audio::AudioIo;
use iax2::client::CallState;
use iax2::g711;
use iax2::jitter::{JitterBuffer, Pull};
use iax2::{Command, Config, Event, PbxClient};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{interval, sleep};

const IAX_PORT_DEFAULT: u16 = 4569;
const FRAME_SAMPLES: usize = 160; // 20 ms @ 8 kHz

struct Account {
    cfg: Config,
    host: String,
    port: u16,
}

struct App {
    clients: Vec<PbxClient>,
    sockets: Vec<Arc<UdpSocket>>,
    audio: AudioIo,
    /// (indice PBX, call number) della chiamata il cui audio e' live.
    active: Option<(usize, u16)>,
    /// PBX su cui finiscono i `dial`.
    selected: usize,
    /// Jitter buffer della sola chiamata attiva (reset al cambio).
    jb: JitterBuffer,
    /// Ultimo frame PCM riprodotto, per il packet-loss concealment.
    last_pcm: Vec<i16>,
}

impl App {
    fn now() -> Instant {
        Instant::now()
    }

    /// Spedisce tutti i datagrammi pronti del client `idx` sul suo socket.
    async fn flush_tx(&mut self, idx: usize) {
        let mut batch = Vec::new();
        while let Some(b) = self.clients[idx].poll_transmit() {
            batch.push(b);
        }
        for b in batch {
            let _ = self.sockets[idx].send(&b).await;
        }
    }

    /// Processa gli eventi del client `idx` (audio, stato, stampa).
    async fn handle_events(&mut self, idx: usize) {
        let name = self.clients[idx].name().to_string();
        loop {
            let Some(ev) = self.clients[idx].poll_event() else { break };
            match ev {
                Event::Registered => println!("[\u{2713}] [{name}] REGISTRATO"),
                Event::RegisterLost { reason } => println!("[!] [{name}] registrazione persa: {reason}"),
                Event::Incoming { call, from, to } => {
                    println!("[\u{260E}] [{name}] CHIAMATA IN ARRIVO #{call} da '{from}' verso '{to}'  —  premi 'a' per rispondere");
                }
                Event::Dialing { call, to } => println!("[>] [{name}] chiamo '{to}' (#{call})"),
                Event::Ringing { call } => println!("[i] [{name}] #{call} sta squillando…"),
                Event::Answered { call } => {
                    println!("[\u{2713}] [{name}] #{call} connessa — audio attivo");
                    // diventa la chiamata attiva; metti in attesa le altre Up
                    self.make_active(idx, call).await;
                }
                Event::Voice { call, ts, ulaw } => {
                    if self.active == Some((idx, call)) {
                        self.jb.push(ts, ulaw, Self::now());
                    }
                }
                Event::Ended { call, reason } => {
                    println!("[i] [{name}] #{call} terminata: {reason}");
                    if self.active == Some((idx, call)) {
                        self.active = None;
                        self.jb.reset();
                        self.last_pcm.clear();
                        self.audio.flush();
                    }
                }
                Event::Dtmf { call, digit } => println!("[#] [{name}] #{call} DTMF ricevuto: {digit}"),
                Event::Log(s) => println!("[i] [{name}] {s}"),
            }
        }
    }

    /// Rende attiva (idx,call): mette in HOLD l'eventuale chiamata attiva
    /// precedente (se ancora connessa), fa UNHOLD della target se era in
    /// attesa, resetta il jitter buffer (nuovo stream) e spinge i frame sui
    /// PBX toccati.
    async fn make_active(&mut self, idx: usize, call: u16) {
        let now = Self::now();
        let mut touched: Vec<usize> = Vec::new();
        if let Some((pidx, pcall)) = self.active {
            if (pidx, pcall) != (idx, call)
                && self.clients[pidx].call_state(pcall) == Some(CallState::Up)
            {
                self.clients[pidx].handle_command(Command::Hold { call: pcall }, now);
                touched.push(pidx);
            }
        }
        if self.clients[idx].call_state(call) == Some(CallState::Held) {
            self.clients[idx].handle_command(Command::Unhold { call }, now);
        }
        touched.push(idx);
        let switching = self.active != Some((idx, call));
        self.active = Some((idx, call));
        if switching {
            self.jb.reset();
            self.last_pcm.clear();
        }
        self.audio.flush();
        touched.sort_unstable();
        touched.dedup();
        for i in touched {
            self.flush_tx(i).await;
        }
    }

    /// Estrae un frame da 20 ms dal jitter buffer della chiamata attiva e lo
    /// manda all'altoparlante. Sui buchi (frame perso/in ritardo) fa PLC
    /// ripetendo l'ultimo frame attenuato; in starvation riproduce silenzio.
    fn pump_playout(&mut self) {
        let active_up = match self.active {
            Some((idx, call)) => self.clients[idx].call_state(call) == Some(CallState::Up),
            None => false,
        };
        if !active_up {
            return;
        }
        match self.jb.pull() {
            Pull::Play(ulaw) => {
                let mut pcm = Vec::with_capacity(ulaw.len());
                g711::decode_block(&ulaw, &mut pcm);
                self.audio.play_pcm_8k(&pcm);
                self.last_pcm = pcm;
            }
            Pull::Conceal => {
                // ripeti l'ultimo frame attenuato (PLC povero ma efficace);
                // poi sfuma verso il silenzio se i buchi continuano
                if !self.last_pcm.is_empty() {
                    for s in self.last_pcm.iter_mut() {
                        *s = (*s as i32 / 2) as i16;
                    }
                    self.audio.play_pcm_8k(&self.last_pcm);
                }
            }
            Pull::Silence => {
                // niente in coda: l'altoparlante va a silenzio da solo
            }
        }
    }

    /// Cattura microfono e invia audio alla chiamata attiva (se Up).
    fn pump_mic(&mut self, now: Instant) -> Option<usize> {
        let (idx, call) = self.active?;
        if self.clients[idx].call_state(call) != Some(CallState::Up) {
            // svuota comunque il mic per non accumulare latenza
            let _ = self.audio.take_frames_8k(FRAME_SAMPLES);
            return None;
        }
        for pcm in self.audio.take_frames_8k(FRAME_SAMPLES) {
            let mut ulaw = Vec::with_capacity(FRAME_SAMPLES);
            g711::encode_block(&pcm, &mut ulaw);
            self.clients[idx].handle_command(Command::SendVoice { call, ulaw }, now);
        }
        Some(idx)
    }

    /// Prossimo risveglio (minimo tra tutti i client).
    fn next_wake(&self) -> Instant {
        let now = Self::now();
        let mut t = now + Duration::from_secs(3600);
        for c in &self.clients {
            if let Some(x) = c.poll_timeout() {
                if x < t {
                    t = x;
                }
            }
        }
        t
    }

    /// Elenco stabile (PBX idx, call) di tutte le chiamate vive, nello stesso
    /// ordine mostrato da `list()`: per PBX, poi per call-number crescente.
    fn calls_index(&self) -> Vec<(usize, u16)> {
        let mut v = Vec::new();
        for (idx, c) in self.clients.iter().enumerate() {
            for call in c.call_ids() {
                v.push((idx, call));
            }
        }
        v
    }

    fn list(&self) {
        println!("--- PBX ---");
        for (i, c) in self.clients.iter().enumerate() {
            let sel = if i == self.selected { "*" } else { " " };
            let reg = if c.is_registered() { "OK" } else { "--" };
            println!(" {sel}{}) {} [{reg}]", i + 1, c.name());
        }
        let calls = self.calls_index();
        if calls.is_empty() {
            println!("--- nessuna chiamata in corso ---");
            return;
        }
        println!("--- chiamate (premi il numero per renderla attiva) ---");
        for (n, (idx, call)) in calls.iter().enumerate() {
            let c = &self.clients[*idx];
            let st = match c.call_state(*call) {
                Some(CallState::Trying) => "in connessione",
                Some(CallState::Ringing) => "squilla",
                Some(CallState::Up) => "in linea",
                Some(CallState::Held) => "in attesa",
                None => "?",
            };
            let peer = c.call_peer(*call).unwrap_or("?");
            let here = if self.active == Some((*idx, *call)) { "  <== attiva" } else { "" };
            println!(" {}) [{}] #{call} {peer} — {st}{here}", n + 1, c.name());
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("uso:");
        eprintln!("  {} <host> <user> <secret> [port] [refresh] [nome]", args[0]);
        eprintln!("  {} accounts.conf", args[0]);
        std::process::exit(2);
    }

    let accounts = if std::path::Path::new(&args[1]).is_file() {
        match parse_accounts(&args[1]) {
            Ok(a) if !a.is_empty() => a,
            Ok(_) => {
                eprintln!("[x] nessun account nel file");
                std::process::exit(2);
            }
            Err(e) => {
                eprintln!("[x] config: {e}");
                std::process::exit(2);
            }
        }
    } else {
        if args.len() < 4 {
            eprintln!("uso singolo account: {} <host> <user> <secret> [port] [refresh] [nome]", args[0]);
            std::process::exit(2);
        }
        let host = args[1].clone();
        let user = args[2].clone();
        let secret = args[3].clone();
        let port = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(IAX_PORT_DEFAULT);
        let refresh = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(60);
        let name = args.get(6).cloned().unwrap_or_else(|| host.clone());
        let mut cfg = Config::new(name, user, secret);
        cfg.refresh = refresh;
        vec![Account { cfg, host, port }]
    };

    let audio = match AudioIo::new() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[x] audio non disponibile: {e}");
            std::process::exit(1);
        }
    };
    println!("[i] audio: in {}Hz/{}ch  out {}Hz/{}ch", audio.in_rate, audio.in_ch, audio.out_rate, audio.out_ch);

    // socket + client + reader task per PBX
    let (tx, mut rx) = mpsc::channel::<(usize, Vec<u8>)>(256);
    let mut clients = Vec::new();
    let mut sockets = Vec::new();
    for (idx, acc) in accounts.into_iter().enumerate() {
        let remote = format!("{}:{}", acc.host, acc.port);
        let sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[x] bind {remote}: {e}");
                std::process::exit(1);
            }
        };
        if let Err(e) = sock.connect(&remote).await {
            eprintln!("[x] connect {remote}: {e}");
            std::process::exit(1);
        }
        println!("[i] [{}] {} -> {remote} (utente {})", acc.cfg.name, sock.local_addr().unwrap(), acc.cfg.username);
        let sock = Arc::new(sock);
        let reader = sock.clone();
        let txc = tx.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match reader.recv(&mut buf).await {
                    Ok(n) => {
                        if txc.send((idx, buf[..n].to_vec())).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        clients.push(PbxClient::new(acc.cfg));
        sockets.push(sock);
    }
    drop(tx);

    let mut app = App { clients, sockets, audio, active: None, selected: 0, jb: JitterBuffer::new(), last_pcm: Vec::new() };

    println!("[i] pronto. comandi: a=rispondi  h=riaggancia  d <num>=chiama  p <n>=seleziona PBX  <n>=attiva chiamata  l=lista  q=esci");

    let mut audio_tick = interval(Duration::from_millis(20));
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();

    // primo giro di registrazione subito
    let now = App::now();
    for i in 0..app.clients.len() {
        app.clients[i].handle_timeout(now);
        app.flush_tx(i).await;
        app.handle_events(i).await;
    }

    loop {
        let wake = app.next_wake();
        let dur = wake.saturating_duration_since(App::now());
        tokio::select! {
            // datagrammi da uno qualsiasi dei PBX
            Some((idx, dg)) = rx.recv() => {
                let now = App::now();
                app.clients[idx].handle_input(&dg, now);
                app.flush_tx(idx).await;
                app.handle_events(idx).await;
            }

            // scadenza timer: re-register / keepalive / ritrasmissioni
            _ = sleep(dur) => {
                let now = App::now();
                for i in 0..app.clients.len() {
                    app.clients[i].handle_timeout(now);
                    app.flush_tx(i).await;
                    app.handle_events(i).await;
                }
            }

            // tick 20 ms: microfono -> chiamata attiva, e jitter buffer -> altoparlante
            _ = audio_tick.tick() => {
                let now = App::now();
                if let Some(idx) = app.pump_mic(now) {
                    app.flush_tx(idx).await;
                }
                app.pump_playout();
            }

            // tastiera
            line = stdin.next_line() => {
                let now = App::now();
                let Ok(Some(line)) = line else { break };
                let line = line.trim().to_string();
                if line.is_empty() { continue; }
                if !app.dispatch(&line, now).await { break; }
            }
        }
    }
    println!("[i] uscita.");
}

impl App {
    /// Esegue un comando da tastiera. Ritorna false per uscire.
    async fn dispatch(&mut self, line: &str, now: Instant) -> bool {
        let mut it = line.split_whitespace();
        let cmd = it.next().unwrap_or("");
        match cmd {
            "q" => return false,
            "l" => self.list(),
            "p" => {
                if let Some(n) = it.next().and_then(|s| s.parse::<usize>().ok()) {
                    if n >= 1 && n <= self.clients.len() {
                        self.selected = n - 1;
                        println!("[i] PBX selezionato: {}", self.clients[self.selected].name());
                    }
                }
            }
            "d" => {
                let number = it.collect::<Vec<_>>().join(" ");
                if number.is_empty() {
                    println!("[!] uso: d <numero>");
                } else {
                    let idx = self.selected;
                    if !self.clients[idx].is_registered() {
                        println!("[!] [{}] non registrato; provo lo stesso", self.clients[idx].name());
                    }
                    self.clients[idx].handle_command(Command::Dial { number }, now);
                    self.flush_tx(idx).await;
                    self.handle_events(idx).await;
                }
            }
            "a" => {
                // prima chiamata che squilla, in qualsiasi PBX
                if let Some((idx, call)) = self.find_ringing() {
                    self.clients[idx].handle_command(Command::Answer { call }, now);
                    self.flush_tx(idx).await;
                    self.handle_events(idx).await;
                } else {
                    println!("[!] nessuna chiamata che squilla");
                }
            }
            "t" => {
                // invia cifre DTMF sulla chiamata attiva
                let digits = it.collect::<Vec<_>>().join("");
                if digits.is_empty() {
                    println!("[!] uso: t <cifre>   (0-9 * # A-D)");
                } else if let Some((idx, call)) = self.active {
                    for d in digits.chars() {
                        self.clients[idx].handle_command(Command::Dtmf { call, digit: d }, now);
                    }
                    self.flush_tx(idx).await;
                    self.handle_events(idx).await;
                    println!("[i] DTMF inviati: {digits}");
                } else {
                    println!("[!] nessuna chiamata attiva");
                }
            }
            "h" => {
                if let Some((idx, call)) = self.active {
                    self.clients[idx].handle_command(Command::Hangup { call }, now);
                    self.flush_tx(idx).await;
                    self.handle_events(idx).await;
                    self.active = None;
                    self.audio.flush();
                } else {
                    println!("[!] nessuna chiamata attiva");
                }
            }
            other => {
                // <n> (anche "#n") -> rendi attiva la chiamata n della lista
                let key = other.strip_prefix('#').unwrap_or(other);
                if let Ok(n) = key.parse::<usize>() {
                    let calls = self.calls_index();
                    if calls.is_empty() {
                        println!("[!] nessuna chiamata in corso");
                    } else if n < 1 || n > calls.len() {
                        println!("[!] indice fuori range: usa 1..{} ('l' per la lista)", calls.len());
                    } else {
                        let (idx, call) = calls[n - 1];
                        match self.clients[idx].call_state(call) {
                            Some(CallState::Ringing) => {
                                // squilla ancora: rispondi (Answered -> make_active)
                                self.clients[idx].handle_command(Command::Answer { call }, now);
                                self.flush_tx(idx).await;
                                self.handle_events(idx).await;
                            }
                            Some(_) => {
                                self.make_active(idx, call).await;
                                println!("[i] attiva: [{}] #{call}", self.clients[idx].name());
                            }
                            None => println!("[!] la chiamata {n} non c'e' piu'"),
                        }
                    }
                } else {
                    println!("[!] comando sconosciuto: {other}");
                }
            }
        }
        true
    }

    /// Trova la prima chiamata in stato Ringing (in ingresso) tra tutti i PBX.
    fn find_ringing(&self) -> Option<(usize, u16)> {
        for (idx, c) in self.clients.iter().enumerate() {
            for call in c.call_ids() {
                if c.call_state(call) == Some(CallState::Ringing) {
                    return Some((idx, call));
                }
            }
        }
        None
    }
}

/// Parser INI minimale per il file account.
fn parse_accounts(path: &str) -> Result<Vec<Account>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut accounts = Vec::new();
    let mut cur: Option<(String, HashMap<String, String>)> = None;

    let flush = |cur: &mut Option<(String, HashMap<String, String>)>, out: &mut Vec<Account>| -> Result<(), String> {
        if let Some((name, kv)) = cur.take() {
            let host = kv.get("host").cloned().ok_or_else(|| format!("[{name}] manca host"))?;
            let user = kv.get("user").cloned().ok_or_else(|| format!("[{name}] manca user"))?;
            let secret = kv.get("secret").cloned().ok_or_else(|| format!("[{name}] manca secret"))?;
            let port = kv.get("port").and_then(|s| s.parse().ok()).unwrap_or(IAX_PORT_DEFAULT);
            let refresh = kv.get("refresh").and_then(|s| s.parse().ok()).unwrap_or(60);
            let mut cfg = Config::new(name, user, secret);
            cfg.refresh = refresh;
            out.push(Account { cfg, host, port });
        }
        Ok(())
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            flush(&mut cur, &mut accounts)?;
            cur = Some((name.trim().to_string(), HashMap::new()));
        } else if let Some((k, v)) = line.split_once('=') {
            if let Some((_, kv)) = cur.as_mut() {
                kv.insert(k.trim().to_lowercase(), v.trim().to_string());
            }
        }
    }
    flush(&mut cur, &mut accounts)?;
    Ok(accounts)
}