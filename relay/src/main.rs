//! OpenU60 relay: introduces phones to routers and forwards their encrypted packets when a
//! direct path cannot be punched. It never holds a key that could decrypt anything.
//!
//!   openu60-relay [--listen 0.0.0.0:443] [--log info]
//!   openu60-relay --probe relay.example.com[:443]     check that a relay answers
//!   openu60-relay --version

use std::net::SocketAddr;
use std::time::Duration;

use openu60_tunnel::relay::Relay;
use tokio::net::UdpSocket;

const USAGE: &str = "usage: openu60-relay [--listen 0.0.0.0:443] [--log info|debug]
       openu60-relay --probe HOST[:PORT]   (exit 0 if the relay answers)
       openu60-relay --version";

enum Cmd {
    Serve(SocketAddr, log::LevelFilter),
    Probe(String),
}

fn parse_args() -> Cmd {
    let mut listen: SocketAddr = "0.0.0.0:443".parse().unwrap();
    let mut level = log::LevelFilter::Info;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--listen" => {
                listen = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| fail("--listen needs addr:port, e.g. 0.0.0.0:443"))
            }
            "--log" => level = args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| fail("--log needs a level")),
            "--probe" => return Cmd::Probe(args.next().unwrap_or_else(|| fail("--probe needs HOST[:PORT]"))),
            "-V" | "--version" => {
                println!("openu60-relay {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => fail(&format!("unknown argument {other}")),
        }
    }
    Cmd::Serve(listen, level)
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}\n{USAGE}");
    std::process::exit(2);
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let (listen, level) = match parse_args() {
        Cmd::Serve(l, v) => (l, v),
        Cmd::Probe(target) => {
            let r = openu60_tunnel::probe::probe_relay(&target, Duration::from_secs(4)).await;
            if r.ok {
                println!(
                    "ok: {} ({}) answered in {} ms; it sees this machine as {}",
                    r.relay,
                    r.addr.unwrap_or_default(),
                    r.rtt_ms.unwrap_or(0),
                    r.public_addr.unwrap_or_default()
                );
                std::process::exit(0);
            }
            eprintln!("failed: {}: {}", r.relay, r.error.unwrap_or_default());
            std::process::exit(1);
        }
    };
    openu60_tunnel::logger::init_stderr(level);
    let sock = UdpSocket::bind(listen).await.unwrap_or_else(|e| {
        eprintln!("bind {listen}: {e}");
        std::process::exit(1);
    });
    log::info!("openu60-relay v{} listening on udp {listen}", env!("CARGO_PKG_VERSION"));
    let relay = Relay::new(sock);
    tokio::select! {
        _ = relay.serve() => {},
        _ = tokio::signal::ctrl_c() => {},
    }
    log::info!("bye");
}
