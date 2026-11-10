//! Command-line "phone": brings a tunnel up from this machine and keeps it open.
//!
//!   cargo run -p openu60-tunnel --example phone -- --key            # print this machine's public key
//!   cargo run -p openu60-tunnel --example phone -- <device_id> <device_key> [--relay host:port] [--force-relay]
//!
//! The identity lives in ~/.openu60/phone.key. Pair it first:
//!   curl -X POST http://<router>:9090/api/tunnel/pair -H 'Authorization: Bearer …' \
//!        -d '{"public_key":"<hex>","name":"Mac"}'

use std::sync::Arc;

use openu60_tunnel::identity::{self, Identity};
use openu60_tunnel::phone::{PhoneParams, PhoneTunnel};

#[tokio::main]
async fn main() {
    openu60_tunnel::logger::init_stderr(log::LevelFilter::Info);
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let path = std::path::PathBuf::from(home).join(".openu60").join("phone.key");
    let identity = Arc::new(Identity::load_or_create(&path).expect("identity"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--key" {
        println!("{}", hex::encode(identity.public));
        return;
    }
    let device_id = identity::parse_device_id(&args[0]).expect("device id: 16 hex chars");
    let device_key = identity::parse_pubkey(&args[1]).expect("device key: 64 hex chars");
    let mut relays = Vec::new();
    let mut force_relay = false;
    let mut probe = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--relay" => {
                relays.push(args[i + 1].clone());
                i += 1;
            }
            "--force-relay" => force_relay = true,
            "--probe" => probe = true,
            other => panic!("unknown argument {other}"),
        }
        i += 1;
    }
    if relays.is_empty() {
        relays = openu60_tunnel::default_relays();
    }
    let params = PhoneParams { relays, device_id, device_key, tcp_port: 19090, udp_port: 19091, force_relay };
    let t = match PhoneTunnel::connect(identity, params).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("connect failed: {e}");
            std::process::exit(1);
        }
    };
    println!("{}", serde_json::to_string_pretty(&t.status()).unwrap());
    println!("agent: http://127.0.0.1:{}  voice: udp 127.0.0.1:{}", t.info().tcp_port, t.info().udp_port);
    if probe {
        for round in 1..=3 {
            let r = t.probe_voice(250, std::time::Duration::from_millis(20)).await;
            println!("voice probe {round}: {}", serde_json::to_string(&r).unwrap());
        }
        return;
    }
    loop {
        tokio::select! {
            _ = t.closed() => { eprintln!("tunnel closed: {:?}", t.status()["close_reason"]); break; }
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                eprintln!("rtt {} ms, path {}", t.status()["rtt_ms"], t.info().path);
            }
        }
    }
}
