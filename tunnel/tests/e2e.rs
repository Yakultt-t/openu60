//! Relay + router + phone in one process: an HTTP request and a voice datagram make the full
//! round trip, once hole-punched (loopback always punches) and once forced through the relay.

use std::sync::Arc;
use std::time::Duration;

use openu60_tunnel::device::DeviceService;
use openu60_tunnel::identity::Identity;
use openu60_tunnel::phone::{PhoneParams, PhoneTunnel};
use openu60_tunnel::relay::Relay;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

async fn fake_agent(tag: &'static str) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if req.starts_with("GET /api/device") { format!("{{\"ok\":true,\"tag\":\"{tag}\"}}") } else { "nope".to_string() };
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    port
}

async fn fake_voice() -> u16 {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = s.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        loop {
            let (n, from) = s.recv_from(&mut buf).await.unwrap();
            let mut reply = b"echo:".to_vec();
            reply.extend_from_slice(&buf[..n]);
            let _ = s.send_to(&reply, from).await;
        }
    });
    port
}

async fn run_case(force_relay: bool) {
    let relay_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let relay = Relay::new(relay_sock);
    let relay_addr = relay.local_addr().unwrap();
    tokio::spawn(relay.serve());

    let http_port = fake_agent("main").await;
    let bootstrap_port = fake_agent("bootstrap").await;
    let voice_port = fake_voice().await;

    let dir = std::env::temp_dir().join(format!("ou60-e2e-{}-{}", std::process::id(), force_relay));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.json"),
        format!("{{\"enabled\":true,\"relays\":[\"{relay_addr}\"],\"http_port\":{http_port},\"voice_port\":{voice_port},\"bootstrap_port\":{bootstrap_port}}}"),
    )
    .unwrap();
    let device = DeviceService::new(dir.clone()).unwrap();

    let phone = Arc::new(Identity::generate().unwrap());
    device.pair(&hex::encode(phone.public), "test phone").unwrap();
    device.start();

    let params = PhoneParams {
        relays: vec![relay_addr.to_string()],
        device_id: device.identity().device_id(),
        device_key: device.identity().public,
        tcp_port: 0,
        udp_port: 0,
        force_relay,
    };
    let tunnel = tokio::time::timeout(Duration::from_secs(20), PhoneTunnel::connect(phone, params))
        .await
        .expect("connect timed out")
        .expect("connect failed");
    let info = tunnel.info().clone();
    assert_eq!(info.path, if force_relay { "relay" } else { "direct" });

    // HTTP through the loopback proxy.
    let mut s = TcpStream::connect(("127.0.0.1", info.tcp_port)).await.unwrap();
    s.write_all(b"GET /api/device HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await.unwrap().unwrap();
    let text = String::from_utf8_lossy(&out);
    assert!(text.ends_with("{\"ok\":true,\"tag\":\"main\"}"), "got {text}");
    assert!(info.paired);

    // Voice datagram and its echo.
    let app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    app.connect(("127.0.0.1", info.udp_port)).await.unwrap();
    app.send(b"U60A-frame").await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), app.recv(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"echo:U60A-frame");

    let status = tunnel.status();
    assert_eq!(status["connected"], true);
    let probe = tunnel.probe_voice(50, Duration::from_millis(5)).await;
    assert!(probe.received >= 45, "voice probe: {probe:?}");
    let dev_status = device.status();
    assert_eq!(dev_status["connections"].as_array().unwrap().len(), 1);
    assert_eq!(dev_status["connections"][0]["name"], "test phone");

    // An unpaired phone gets a tunnel that only reaches the bootstrap port.
    let stranger = Arc::new(Identity::generate().unwrap());
    let params = PhoneParams {
        relays: vec![relay_addr.to_string()],
        device_id: device.identity().device_id(),
        device_key: device.identity().public,
        tcp_port: 0,
        udp_port: 0,
        force_relay,
    };
    let limited = tokio::time::timeout(Duration::from_secs(20), PhoneTunnel::connect(stranger, params))
        .await
        .unwrap()
        .expect("stranger should get a limited tunnel");
    assert!(!limited.info().paired);
    let mut s = TcpStream::connect(("127.0.0.1", limited.info().tcp_port)).await.unwrap();
    s.write_all(b"GET /api/device HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await.unwrap().unwrap();
    let text = String::from_utf8_lossy(&out);
    assert!(text.ends_with("{\"ok\":true,\"tag\":\"bootstrap\"}"), "stranger got {text}");
    let dev_status = device.status();
    assert!(dev_status["connections"].as_array().unwrap().iter().any(|c| c["paired"] == false));
    limited.close();

    tunnel.close();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_path() {
    openu60_tunnel::logger::init_stderr(log::LevelFilter::Info);
    run_case(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relayed_path() {
    openu60_tunnel::logger::init_stderr(log::LevelFilter::Info);
    run_case(true).await;
}

/// Switching relays: the router keeps the old relay when the new one does not answer, and
/// moves (old session closed shortly after) when it does; the phone then reaches it via the new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switch_relay() {
    let mut relay_addrs = Vec::new();
    for _ in 0..2 {
        let relay = Relay::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        relay_addrs.push(relay.local_addr().unwrap());
        tokio::spawn(relay.serve());
    }
    let (a, b) = (relay_addrs[0].to_string(), relay_addrs[1].to_string());
    let dead = {
        let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        s.local_addr().unwrap().to_string()
    };
    let http_port = fake_agent("main").await;
    let bootstrap_port = fake_agent("bootstrap").await;
    let voice_port = fake_voice().await;
    let dir = std::env::temp_dir().join(format!("ou60-e2e-switch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.json"),
        format!("{{\"enabled\":true,\"relays\":[\"{a}\"],\"http_port\":{http_port},\"voice_port\":{voice_port},\"bootstrap_port\":{bootstrap_port}}}"),
    )
    .unwrap();
    let device = DeviceService::new(dir.clone()).unwrap();
    let phone = Arc::new(Identity::generate().unwrap());
    device.pair(&hex::encode(phone.public), "test phone").unwrap();
    device.start();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let set = |patch: serde_json::Value| {
        let device = Arc::clone(&device);
        tokio::task::spawn_blocking(move || device.set_config(&patch))
    };
    // A relay that never answers is refused and the old one stays.
    let r = set(serde_json::json!({"relays": [dead.clone()]})).await.unwrap();
    assert!(r.is_err(), "{r:?}");
    assert_eq!(device.info()["relays"], serde_json::json!([a.clone()]));

    // Garbage is rejected before anything is tried.
    assert!(set(serde_json::json!({"relays": ["not a host/"]})).await.unwrap().is_err());

    // A working one is taken.
    let v = set(serde_json::json!({"relays": [b.clone()]})).await.unwrap().expect("switch to b");
    assert_eq!(v["relays"], serde_json::json!([b.clone()]));
    assert_eq!(v["registered"], serde_json::json!([b.clone()]));
    tokio::time::sleep(Duration::from_secs(4)).await;
    let status = device.status();
    let live: Vec<String> = status["relays_status"].as_array().unwrap().iter().map(|r| r["relay"].as_str().unwrap().to_string()).collect();
    assert_eq!(live, vec![b.clone()], "old relay session should be gone");

    let params = PhoneParams {
        relays: vec![b.clone()],
        device_id: device.identity().device_id(),
        device_key: device.identity().public,
        tcp_port: 0,
        udp_port: 0,
        force_relay: true,
    };
    let tunnel = tokio::time::timeout(Duration::from_secs(20), PhoneTunnel::connect(phone, params))
        .await
        .expect("connect timed out")
        .expect("connect via the new relay");
    let mut s = TcpStream::connect(("127.0.0.1", tunnel.info().tcp_port)).await.unwrap();
    s.write_all(b"GET /api/device HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
    let mut out = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_string(&mut out)).await;
    assert!(out.contains("\"tag\":\"main\""), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}
