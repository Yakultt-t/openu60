//! OpenU60 relay: introduces phones to routers and forwards their encrypted packets when a
//! direct path cannot be punched. It never holds a key that could decrypt anything.
//!
//!   openu60-relay [--listen 0.0.0.0:443] [--log info]

use std::net::SocketAddr;

use openu60_tunnel::relay::Relay;
use tokio::net::UdpSocket;

fn parse_args() -> (SocketAddr, log::LevelFilter) {
    let mut listen: SocketAddr = "0.0.0.0:443".parse().unwrap();
    let mut level = log::LevelFilter::Info;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--listen" => listen = args.next().and_then(|v| v.parse().ok()).expect("--listen addr:port"),
            "--log" => level = args.next().and_then(|v| v.parse().ok()).expect("--log level"),
            "-h" | "--help" => {
                eprintln!("usage: openu60-relay [--listen 0.0.0.0:443] [--log info|debug]");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    (listen, level)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let (listen, level) = parse_args();
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
