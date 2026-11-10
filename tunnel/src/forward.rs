//! What flows inside the tunnel: each QUIC bidirectional stream is one TCP connection to the
//! agent's HTTP port (first byte says so); call audio goes as sealed voice packets beside QUIC
//! (see voice.rs), never inside it.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use quinn::{Connection, RecvStream, SendStream};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;

use crate::mux::Mux;
use crate::voice::{Incoming, VoiceChannel};

pub const KIND_TCP: u8 = 1;
/// Application-level hello: the router answers with the same byte. The TLS client finishes its
/// handshake before the server has checked the client certificate, so this round trip is what
/// tells the phone it was actually accepted.
pub const KIND_PING: u8 = 3;
/// Hello reply for a phone the router does not know yet: its streams only reach the
/// bootstrap port (login + pair), and there is no voice channel.
pub const KIND_PING_LIMITED: u8 = 4;

/// Where the router hands tunnel streams.
#[derive(Clone, Copy, Debug)]
pub struct DevicePorts {
    pub http: u16,
    pub bootstrap: u16,
    pub voice: u16,
}

/// Copies both directions between a QUIC stream pair and a TCP socket until both sides are
/// done. A half-close on one side becomes a half-close on the other, so HTTP keep-alive and
/// request/response boundaries survive the hop.
pub async fn pump(mut send: SendStream, mut recv: RecvStream, tcp: TcpStream) {
    let (mut tr, mut tw) = tcp.into_split();
    let to_tcp = async {
        let r = tokio::io::copy(&mut recv, &mut tw).await;
        let _ = tw.shutdown().await;
        r
    };
    let to_quic = async {
        let r = tokio::io::copy(&mut tr, &mut send).await;
        let _ = send.finish();
        r
    };
    let _ = tokio::join!(to_tcp, to_quic);
}

/// Router side: serve one phone's connection until it closes. An unpaired phone gets the
/// bootstrap port and no audio.
pub async fn device_serve(conn: Connection, mux: Arc<Mux>, ports: DevicePorts, paired: bool) {
    let peer = conn.remote_address();
    let voice = if paired {
        Some(tokio::spawn(device_voice(conn.clone(), Arc::clone(&mux), ports.voice)))
    } else {
        None
    };
    let tcp_port = if paired { ports.http } else { ports.bootstrap };
    loop {
        match conn.accept_bi().await {
            Ok((send, recv)) => {
                tokio::spawn(device_stream(send, recv, tcp_port, paired));
            }
            Err(e) => {
                log::info!("connection from {peer} closed: {e}");
                break;
            }
        }
    }
    mux.unregister_voice(peer);
    if let Some(v) = voice {
        v.abort();
    }
}

async fn device_stream(mut send: SendStream, mut recv: RecvStream, http_port: u16, paired: bool) {
    let mut kind = [0u8; 1];
    if recv.read_exact(&mut kind).await.is_err() {
        return;
    }
    if kind[0] == KIND_PING {
        let _ = send.write_all(&[if paired { KIND_PING } else { KIND_PING_LIMITED }]).await;
        let _ = send.finish();
        return;
    }
    if kind[0] != KIND_TCP {
        let _ = send.reset(quinn::VarInt::from_u32(1));
        return;
    }
    match TcpStream::connect((Ipv4Addr::LOCALHOST, http_port)).await {
        Ok(tcp) => pump(send, recv, tcp).await,
        Err(e) => {
            log::warn!("agent port {http_port} unreachable: {e}");
            let _ = send.reset(quinn::VarInt::from_u32(2));
        }
    }
}

/// Router side: the phone's audio reaches the voice bridge from a private socket, and whatever
/// the bridge sends back is sealed and sent to the phone. The bridge learns "the phone" from
/// the source address, which here is our private socket, so it needs no tunnel awareness.
async fn device_voice(conn: Connection, mux: Arc<Mux>, voice_port: u16) {
    let peer = conn.remote_address();
    let chan = match VoiceChannel::from_connection(&conn, false) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            log::warn!("voice keys: {e}");
            return;
        }
    };
    let mut rx = mux.register_voice(peer);
    let sock = match UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            log::warn!("voice relay socket: {e}");
            return;
        }
    };
    if let Err(e) = sock.connect((Ipv4Addr::LOCALHOST, voice_port)).await {
        log::warn!("voice relay connect: {e}");
        return;
    }
    let up = {
        let sock = Arc::clone(&sock);
        let chan = Arc::clone(&chan);
        let mux = Arc::clone(&mux);
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok(n) = sock.recv(&mut buf).await else { break };
                let packet = chan.seal_audio(&buf[..n]);
                if mux.send_raw(peer, &packet).await.is_err() {
                    break;
                }
            }
        })
    };
    while let Some(pkt) = rx.recv().await {
        match chan.open(&pkt) {
            Some(Incoming::Audio(frames)) => {
                for f in frames {
                    let _ = sock.send(&f).await;
                }
            }
            Some(Incoming::EchoRequest(p)) => {
                let _ = mux.send_raw(peer, &chan.seal_echo(true, &p)).await;
            }
            _ => {}
        }
    }
    up.abort();
}

/// Where the phone side hands echo replies while a probe runs.
pub type EchoSink = Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>>;

/// Phone side: a loopback TCP listener and UDP socket the app talks to as if they were the
/// router. Runs until the connection closes.
pub async fn phone_serve(conn: Connection, mux: Arc<Mux>, voice: Arc<VoiceChannel>, echo: EchoSink, tcp: TcpListener, udp: UdpSocket) {
    let peer = conn.remote_address();
    let udp = Arc::new(udp);
    let accept = {
        let conn = conn.clone();
        async move {
            loop {
                let Ok((stream, _)) = tcp.accept().await else { break };
                let _ = stream.set_nodelay(true);
                let conn = conn.clone();
                tokio::spawn(async move {
                    match conn.open_bi().await {
                        Ok((mut send, recv)) => {
                            if send.write_all(&[KIND_TCP]).await.is_ok() {
                                pump(send, recv, stream).await;
                            }
                        }
                        Err(e) => log::warn!("open_bi: {e}"),
                    }
                });
            }
        }
    };
    let app_addr: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
    let udp_up = {
        let udp = Arc::clone(&udp);
        let voice = Arc::clone(&voice);
        let mux = Arc::clone(&mux);
        let app_addr = Arc::clone(&app_addr);
        async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = udp.recv_from(&mut buf).await else { break };
                *app_addr.lock().unwrap() = Some(from);
                let packet = voice.seal_audio(&buf[..n]);
                if mux.send_raw(peer, &packet).await.is_err() {
                    break;
                }
            }
        }
    };
    let udp_down = {
        let udp = Arc::clone(&udp);
        let voice = Arc::clone(&voice);
        let app_addr = Arc::clone(&app_addr);
        let mut rx = mux.register_voice(peer);
        async move {
            while let Some(pkt) = rx.recv().await {
                match voice.open(&pkt) {
                    Some(Incoming::Audio(frames)) => {
                        let to = *app_addr.lock().unwrap();
                        if let Some(to) = to {
                            for f in frames {
                                let _ = udp.send_to(&f, to).await;
                            }
                        }
                    }
                    Some(Incoming::EchoReply(p)) => {
                        if let Some(tx) = echo.lock().unwrap().as_ref() {
                            let _ = tx.send(p);
                        }
                    }
                    _ => {}
                }
            }
        }
    };
    tokio::select! {
        _ = accept => {},
        _ = udp_up => {},
        _ = udp_down => {},
        _ = conn.closed() => {},
    }
    mux.unregister_voice(peer);
}

/// Keeps `Bytes` in the public API for callers that build packets themselves.
pub fn bytes(payload: &[u8]) -> Bytes {
    Bytes::copy_from_slice(payload)
}
