use std::sync::Arc;
use std::io::Read;

use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::at_terminal;
use crate::cell;
use crate::conn_watchdog;
use crate::device_ext;
use crate::lan_test;
use crate::mms;
use crate::scheduler;
use crate::handlers::{self, AppState};
use crate::mihomo;
use crate::modem_ext;
use crate::network_ext;
use crate::oos_policy;
use crate::router;
use crate::sim;
use crate::sms;
use crate::sms_forward;
use crate::speedtest;
use crate::tailscale;
use crate::telephony;
use crate::usb;
use crate::wifi;
use crate::wifi_wan;

/// 嵌入的 Web 管理面板（web-admin vite 单文件构建产物）。
/// build.rs 保证该文件存在（未构建前端时为占位页）。
const INDEX_HTML: &str = include_str!("../web-dist/index.html");

pub fn start(bind: &str, threads: usize, state: Arc<AppState>) {
    // Bind ourselves so TCP keepalive can be set on the listener; accepted
    // sockets inherit it. tiny_http parks one thread per client connection
    // with no timeout, so a phone that vanishes without closing (Wi-Fi roam,
    // sleep) would otherwise pin a thread and two fds forever.
    let listener = match std::net::TcpListener::bind(bind) {
        Ok(l) => l,
        Err(_) => {
            std::process::exit(1);
        }
    };
    set_tcp_keepalive(&listener);
    let server = match Server::from_listener(listener, None) {
        Ok(s) => s,
        Err(_) => {
            std::process::exit(1);
        }
    };
    // Seed the CPU tracker with initial sample (speed tracker is demand-driven)
    state.cpu.sample();

    let server = Arc::new(server);
    let mut handles = Vec::new();

    for i in 0..threads {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        let handle = crate::system::spawn_named(&format!("http-{i}"), move || {
            loop {
                let request = match server.recv() {
                    Ok(r) => r,
                    Err(_) => {
                        continue;
                    }
                };
                handle_request(request, &state);
            }
        });
        handles.push(handle);
    }

    start_bootstrap(Arc::clone(&state));

    for h in handles {
        let _ = h.join();
    }
}

/// Loopback listener for phones the tunnel does not know yet (see tunnel.rs): only login and
/// pairing are served, and wrong passwords are throttled hard, because anyone who knows the
/// router's id and public key can reach this port through the relay.
fn start_bootstrap(state: Arc<AppState>) {
    let bind = std::env::var("ZTE_AGENT_BOOTSTRAP_BIND").unwrap_or_else(|_| "127.0.0.1:9092".to_string());
    let listener = match std::net::TcpListener::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[server] bootstrap bind {bind}: {e}");
            return;
        }
    };
    let server = match Server::from_listener(listener, None) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[server] bootstrap server: {e}");
            return;
        }
    };
    // Several workers: a phone keeps HTTP connections alive, and tiny_http parks a worker on
    // an idle keep-alive connection, so one thread would block every later request.
    let server = Arc::new(server);
    for i in 0..4 {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        crate::system::spawn_named(&format!("http-bootstrap-{i}"), move || loop {
            let request = match server.recv() {
                Ok(r) => r,
                Err(_) => continue,
            };
            handle_bootstrap(request, &state);
        });
    }
}

const BOOTSTRAP_MAX_FAILURES: usize = 10;
const BOOTSTRAP_WINDOW_SECS: u64 = 15 * 60;

static BOOTSTRAP_FAILURES: std::sync::Mutex<Vec<std::time::Instant>> = std::sync::Mutex::new(Vec::new());

fn handle_bootstrap(mut request: Request, state: &AppState) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or(&url).to_string();
    let mut body = Vec::new();
    let _ = request.as_reader().read_to_end(&mut body);
    let token_ok = request
        .headers()
        .iter()
        .find(|h| h.field.as_str().to_ascii_lowercase() == "authorization")
        .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
        .map(|token| state.auth.validate(token) || state.tunnel.as_ref().map(|t| t.validate_token(token)).unwrap_or(false))
        .unwrap_or(false);
    let (status, reply) = match (&method, path.as_str()) {
        (&Method::Post, "/api/auth/login") => {
            let throttled = {
                let mut fails = BOOTSTRAP_FAILURES.lock().unwrap();
                fails.retain(|t| t.elapsed().as_secs() < BOOTSTRAP_WINDOW_SECS);
                fails.len() >= BOOTSTRAP_MAX_FAILURES
            };
            if throttled {
                (429, json!({"ok": false, "error": "too many attempts, try again later"}))
            } else {
                let (status, reply) = crate::handlers::login(state, &body);
                if status != 200 {
                    BOOTSTRAP_FAILURES.lock().unwrap().push(std::time::Instant::now());
                }
                (status, reply)
            }
        }
        (&Method::Get, "/api/tunnel/info") | (&Method::Post, "/api/tunnel/pair") if token_ok => {
            crate::tunnel::route(&method, &path, state, &body).unwrap_or((404, json!({"ok": false, "error": "not found"})))
        }
        (_, "/api/tunnel/info") | (_, "/api/tunnel/pair") => (401, json!({"ok": false, "error": "unauthorized"})),
        _ => (403, json!({"ok": false, "error": "pair this device first"})),
    };
    eprintln!("[bootstrap] {} {} -> {status}", method, path);
    // Close after every reply so a worker never sits on an idle connection.
    let body_str = serde_json::to_string(&reply).unwrap_or_default();
    let response = Response::from_string(body_str)
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
        .with_header(Header::from_bytes("Connection", "close").unwrap());
    let _ = request.respond(response);
}

/// SO_KEEPALIVE with a 5 min idle / 60 s interval / 3 probes: a dead peer is
/// detected within ~8 min and its tiny_http thread exits. Linux only; the
/// values are inherited by accepted sockets.
#[cfg(target_os = "linux")]
fn set_tcp_keepalive(listener: &std::net::TcpListener) {
    use std::os::unix::io::AsRawFd;
    let fd = listener.as_raw_fd();
    let opts: [(libc::c_int, libc::c_int, libc::c_int); 4] = [
        (libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1),
        (libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 300),
        (libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 60),
        (libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3),
    ];
    for (level, name, value) in opts {
        // SAFETY: fd is a valid open socket owned by `listener`; `value` outlives the call.
        let rc = unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                &value as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            eprintln!("[server] setsockopt({level},{name}) failed: {}", std::io::Error::last_os_error());
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn set_tcp_keepalive(_listener: &std::net::TcpListener) {}

fn handle_request(mut request: Request, state: &AppState) {
    let method = request.method().clone();
    let url = request.url().to_string();
    // Strip query string for routing
    let path = url.split('?').next().unwrap_or(&url);

    // Web 管理面板：非 /api 路径一律返回嵌入的 SPA（含前端路由回退）。
    // 静态资源无需鉴权（登录页本身也要加载）；API 鉴权在下方进行。
    if !path.starts_with("/api") {
        match method {
            Method::Get => {
                serve_index(request);
            }
            _ => {
                respond(request, 405, json!({"ok": false, "error": "method not allowed"}));
            }
        }
        return;
    }

    if method == Method::Post && matches!(path, "/api/call/hangup" | "/api/call/answer" | "/api/call/dial" | "/api/call/audio/start") {
        let ua = request
            .headers()
            .iter()
            .find(|h| h.field.as_str().to_ascii_lowercase() == "user-agent")
            .map(|h| h.value.as_str().chars().take(60).collect::<String>())
            .unwrap_or_default();
        eprintln!(
            "[call] {} from {} ({ua}) at {}",
            path.trim_start_matches("/api/call/"),
            request.remote_addr().map(|a| a.to_string()).unwrap_or_else(|| "?".into()),
            crate::conn_watchdog::unix_now()
        );
    }

    // Auth check — skip for login endpoint
    let needs_auth = path != "/api/auth/login";
    if needs_auth {
        let authorized = request
            .headers()
            .iter()
            .find(|h| h.field.as_str().to_ascii_lowercase() == "authorization")
            .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
            .map(|token| {
                state.auth.validate(token)
                    || state.tunnel.as_ref().map(|t| t.validate_token(token)).unwrap_or(false)
                    || (state.auth.is_local_token(token)
                        && request.remote_addr().map(|a| a.ip().is_loopback()).unwrap_or(false))
            })
            .unwrap_or(false);

        if !state.auth.has_password() {
            // No password set — allow unauthenticated access
        } else if !authorized {
            respond(request, 401, json!({"ok": false, "error": "unauthorized"}));
            return;
        }
    }

    // LAN test: download streams raw bytes, upload reads body incrementally.
    // Handle BEFORE the body is fully read into memory.
    match (&method, path) {
        (&Method::Get, "/api/lan/download") => {
            let size = parse_query_usize(&url, "size").unwrap_or(50 * 1024 * 1024);
            let size = size.min(200 * 1024 * 1024); // cap at 200 MB
            lan_test::download(request, size);
            return;
        }
        (&Method::Post, "/api/lan/upload") => {
            let (status, body_json) = lan_test::upload(&mut request);
            respond(request, status, body_json);
            return;
        }
        // Needs the query string (long-poll parameters), which route() does not see.
        (&Method::Get, "/api/call/status") => {
            let wait = parse_query_usize(&url, "wait").map(|ms| std::time::Duration::from_millis(ms as u64));
            let since = parse_query_usize(&url, "since").map(|v| v as u64);
            if let Some(device) = parse_query_str(&url, "device") {
                state.push.note_poll(&device);
            }
            let (status, body_json) = state.call_monitor.status(state, wait, since);
            respond(request, status, body_json);
            return;
        }
        (&Method::Delete, "/api/push/register") => {
            let (status, body_json) = state.push.unregister(parse_query_str(&url, "device").as_deref());
            respond(request, status, body_json);
            return;
        }
        _ => {}
    }

    if path.starts_with("/api/live-activity") && !state.auth.has_password() {
        respond(request, 403, json!({"ok": false, "error": "agent password required"}));
        return;
    }

    // Mihomo binary upload streams straight to disk; handle before the body
    // is read. Logs need the query string, which `route` does not receive.
    if method == Method::Post && path == "/api/mihomo/install/upload" {
        let (status, body_json) = mihomo::handlers::install_upload(&mut request, state);
        respond(request, status, body_json);
        return;
    }
    if method == Method::Get && path == "/api/mihomo/logs" {
        let lines = parse_query_usize(&url, "lines").unwrap_or(200);
        let (status, body_json) = mihomo::handlers::logs(state, lines);
        respond(request, status, body_json);
        return;
    }
    if method == Method::Get && path == "/api/mihomo/capabilities" {
        let (status, body_json) = if url.contains("force=1") {
            mihomo::handlers::capabilities_force(state)
        } else {
            mihomo::handlers::capabilities(state)
        };
        respond(request, status, body_json);
        return;
    }

    // MMS part bytes stream straight from disk.
    if method == Method::Get && mms::handlers::is_part_path(path) {
        mms::handlers::serve_part(request, state, path);
        return;
    }

    // Read body, with tight limits for the paths that accept raw YAML.
    let mut body = Vec::new();
    if path.starts_with("/api/live-activity") {
        let _ = std::io::Read::read_to_end(&mut request.as_reader().take(4097), &mut body);
        if body.len() > 4096 {
            respond(request, 413, json!({"ok": false, "error": "body too large"}));
            return;
        }
    } else if path == "/api/mihomo/config" || path == "/api/mihomo/config/validate" {
        let limit = mihomo::config::MAX_CONFIG_BYTES;
        let _ = std::io::Read::read_to_end(&mut request.as_reader().take((limit + 1) as u64), &mut body);
        if body.len() > limit {
            respond(request, 413, json!({"ok": false, "error": "config too large"}));
            return;
        }
    } else {
        let _ = std::io::Read::read_to_end(&mut request.as_reader(), &mut body);
    }

    let (status, body_json) = route(&method, path, state, &body);
    respond(request, status, body_json);
}

fn parse_query_str(url: &str, key: &str) -> Option<String> {
    let query = url.split('?').nth(1)?;
    for pair in query.split('&') {
        let mut kv = pair.splitn(2, '=');
        if kv.next()? == key {
            let v: String = kv.next()?.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(64).collect();
            return (!v.is_empty()).then_some(v);
        }
    }
    None
}

fn parse_query_usize(url: &str, key: &str) -> Option<usize> {
    let query = url.split('?').nth(1)?;
    for pair in query.split('&') {
        let mut kv = pair.splitn(2, '=');
        if kv.next()? == key {
            return kv.next()?.parse().ok();
        }
    }
    None
}

pub fn route(method: &Method, path: &str, state: &AppState, body: &[u8]) -> (u16, Value) {
    if let Some(r) = mms::handlers::route(method, path, state, body) {
        return r;
    }
    if let Some(r) = crate::tunnel::route(method, path, state, body) {
        return r;
    }
    if let Some(r) = crate::watch_link::route(method, path, state) {
        return r;
    }
    match (method, path) {
        (&Method::Get, "/api/live-activity") => state.live_activity.status(),
        (&Method::Put, "/api/live-activity") => state.live_activity.register(body),
        (&Method::Delete, "/api/live-activity") => state.live_activity.unregister(),
        // Mihomo
        (&Method::Get, "/api/mihomo/status") => mihomo::handlers::status(state),
        (&Method::Get, "/api/mihomo/proxies") => mihomo::handlers::proxies(state),
        (&Method::Put, "/api/mihomo/proxies") => mihomo::handlers::proxy_select(state, body),
        (&Method::Get, "/api/mihomo/config") => mihomo::handlers::config_get(state),
        (&Method::Put, "/api/mihomo/config") => mihomo::handlers::config_set(state, body),
        (&Method::Post, "/api/mihomo/config/validate") => mihomo::handlers::config_validate(state, body),
        (&Method::Get, "/api/mihomo/settings") => mihomo::handlers::settings_get(state),
        (&Method::Put, "/api/mihomo/settings") => mihomo::handlers::settings_set(state, body),
        (&Method::Post, "/api/mihomo/start") => mihomo::handlers::start(state),
        (&Method::Post, "/api/mihomo/stop") => mihomo::handlers::stop(state),
        (&Method::Post, "/api/mihomo/restart") => mihomo::handlers::restart(state),
        (&Method::Post, "/api/mihomo/panic") => mihomo::handlers::panic(state),
        (&Method::Post, "/api/mihomo/safe-mode/clear") => mihomo::handlers::safe_mode_clear(state),
        (&Method::Post, "/api/mihomo/logs/clear") => mihomo::handlers::logs_clear(state),
        (&Method::Post, "/api/mihomo/install/download") => mihomo::handlers::install_download(state, body),
        // Auth
        (&Method::Post, "/api/auth/login") => handlers::login(state, body),
        // Device info (sysfs)
        (&Method::Get, "/api/device") => handlers::device(state),
        (&Method::Get, "/api/battery") => handlers::battery(state),
        (&Method::Get, "/api/cpu") => handlers::cpu(state),
        (&Method::Get, "/api/memory") => handlers::memory(state),
        // Network
        (&Method::Get, "/api/network/signal") => handlers::network_signal(state),
        (&Method::Get, "/api/network/traffic") => handlers::network_traffic(state),
        (&Method::Get, "/api/network/speed") => handlers::network_speed(state),
        (&Method::Get, "/api/network/wan") => network_ext::network_wan(state),
        (&Method::Get, "/api/network/wan6") => network_ext::network_wan6(state),
        (&Method::Get, "/api/network/lan-status") => network_ext::network_lan_status(state),
        (&Method::Get, "/api/network/clients") => network_ext::network_clients(state),
        (&Method::Get, "/api/network/speeds") => network_ext::network_speeds(state),
        (&Method::Get, "/api/network/rmnet") => network_ext::network_rmnet(state),
        // Device (ubus)
        (&Method::Get, "/api/device/battery-info") => network_ext::network_battery_ubus(state),
        (&Method::Get, "/api/device/thermal") => device_ext::device_thermal(state),
        (&Method::Get, "/api/device/charger") => device_ext::device_charger(state),
        (&Method::Get, "/api/device/system") => device_ext::device_system(state),
        (&Method::Post, "/api/device/reboot") => device_ext::device_reboot(state),
        (&Method::Post, "/api/device/factory-reset") => device_ext::device_factory_reset(state),
        (&Method::Get, "/api/device/charge-control") => device_ext::charge_control_get(state),
        (&Method::Put, "/api/device/charge-control") => device_ext::charge_control_set(state, body),
        (&Method::Post, "/api/device/power-save") => device_ext::device_power_save_get(state, body),
        (&Method::Put, "/api/device/power-save") => device_ext::device_power_save_set(state, body),
        (&Method::Get, "/api/device/fast-boot") => device_ext::device_fast_boot_get(state),
        (&Method::Put, "/api/device/fast-boot") => device_ext::device_fast_boot_set(state, body),
        (&Method::Get, "/api/device/auto-sleep") => device_ext::device_auto_sleep_get(state),
        (&Method::Put, "/api/device/auto-sleep") => device_ext::device_auto_sleep_set(state, body),
        // System — process monitor
        (&Method::Get, "/api/system/top") => handlers::system_top(state),
        (&Method::Post, "/api/system/kill-bloat") => handlers::system_kill_bloat(state, body),
        // WiFi
        (&Method::Get, "/api/wifi/status") => wifi::wifi_status(state),
        (&Method::Put, "/api/wifi/settings") => wifi::wifi_set(state, body),
        (&Method::Get, "/api/wifi/guest") => wifi::guest_status(state),
        (&Method::Put, "/api/wifi/guest") => wifi::guest_set(state, body),
        // WiFi uplink (station / repeater mode)
        (&Method::Get, "/api/wifi/wan/status") => wifi_wan::status(state),
        (&Method::Post, "/api/wifi/wan/scan") => wifi_wan::scan_start(state),
        (&Method::Get, "/api/wifi/wan/scan") => wifi_wan::scan_results(state),
        (&Method::Post, "/api/wifi/wan/connect") => wifi_wan::connect(state, body),
        (&Method::Post, "/api/wifi/wan/disconnect") => wifi_wan::disconnect(state),
        (&Method::Put, "/api/wifi/wan/mode") => wifi_wan::mode_set(state, body),
        // Modem
        (&Method::Get, "/api/data-usage") => handlers::data_usage(state),
        (&Method::Get, "/api/system/time") => crate::clock::get(),
        (&Method::Put, "/api/system/time") => crate::clock::set(body),
        (&Method::Get, "/api/system/timezones") => crate::clock::zones(),
        (&Method::Get, "/api/data-plan") => state.data_plan.get(),
        (&Method::Put, "/api/data-plan") => state.data_plan.set(body),
        (&Method::Get, "/api/modem/status") => handlers::modem_status(state),
        (&Method::Post, "/api/modem/online") => modem_ext::modem_online_user(state),
        (&Method::Get, "/api/modem/data") => modem_ext::modem_data_get(state),
        (&Method::Put, "/api/modem/data") => modem_ext::modem_data_set(state, body),
        (&Method::Post, "/api/modem/airplane") => modem_ext::modem_airplane(state, body),
        (&Method::Put, "/api/modem/network-mode") => modem_ext::modem_network_mode_set(state, body),
        (&Method::Post, "/api/modem/scan") => modem_ext::modem_scan(state),
        (&Method::Get, "/api/modem/scan/status") => modem_ext::modem_scan_status(state),
        (&Method::Get, "/api/modem/scan/results") => modem_ext::modem_scan_results(state),
        (&Method::Post, "/api/modem/register") => modem_ext::modem_register(state, body),
        (&Method::Get, "/api/modem/register/result") => modem_ext::modem_register_result(state),
        (&Method::Get, "/api/modem/oos-policy") => oos_policy::config_get(&state.oos_policy),
        (&Method::Put, "/api/modem/oos-policy") => oos_policy::config_set(&state.oos_policy, body),
        (&Method::Get, "/api/modem/conn-watchdog") => {
            conn_watchdog::config_get(&state.conn_watchdog)
        }
        (&Method::Put, "/api/modem/conn-watchdog") => {
            conn_watchdog::config_set(&state.conn_watchdog, body)
        }
        // SMS
        (&Method::Post, "/api/sms/list") => sms::sms_list(state, body),
        (&Method::Get, "/api/sms/capacity") => sms::sms_capacity(state),
        (&Method::Post, "/api/sms/send") => sms::sms_send(state, body),
        (&Method::Post, "/api/sms/delete") => sms::sms_delete(state, body),
        (&Method::Post, "/api/sms/read") => sms::sms_mark_read(state, body),
        // SMS forwarding
        (&Method::Get, "/api/sms/forward/config") => sms_forward::config_get(state),
        (&Method::Put, "/api/sms/forward/config") => sms_forward::config_set(state, body),
        (&Method::Post, "/api/sms/forward/rules") => sms_forward::rules_create(state, body),
        (&Method::Put, "/api/sms/forward/rules") => sms_forward::rules_update(state, body),
        (&Method::Delete, "/api/sms/forward/rules") => sms_forward::rules_delete(state, body),
        (&Method::Put, "/api/sms/forward/rules/toggle") => sms_forward::rules_toggle(state, body),
        (&Method::Post, "/api/sms/forward/test") => sms_forward::test_forward(state, body),
        (&Method::Get, "/api/sms/forward/log") => sms_forward::log_get(state),
        (&Method::Post, "/api/sms/forward/log/clear") => sms_forward::log_clear(state),
        (&Method::Post, "/api/sms/forward/retry") => sms_forward::retry_forward(state, body),
        // SIM
        (&Method::Get, "/api/sim/info") => sim::sim_info(state),
        (&Method::Get, "/api/sim/imei") => sim::sim_imei(state),
        (&Method::Post, "/api/sim/pin/verify") => sim::sim_pin_verify(state, body),
        (&Method::Post, "/api/sim/pin/change") => sim::sim_pin_change(state, body),
        (&Method::Post, "/api/sim/pin/mode") => sim::sim_pin_mode(state, body),
        (&Method::Post, "/api/sim/unlock") => sim::sim_unlock(state, body),
        (&Method::Get, "/api/sim/lock-trials") => sim::sim_lock_trials(state),
        // Cell
        (&Method::Post, "/api/cell/lock/nr") => cell::cell_lock_nr(state, body),
        (&Method::Post, "/api/cell/lock/lte") => cell::cell_lock_lte(state, body),
        (&Method::Post, "/api/cell/lock/reset") => cell::cell_lock_reset(state),
        (&Method::Post, "/api/cell/neighbors/scan") => cell::cell_neighbors_scan(state),
        (&Method::Get, "/api/cell/neighbors/nr") => cell::cell_neighbors_nr(state),
        (&Method::Get, "/api/cell/neighbors/lte") => cell::cell_neighbors_lte(state),
        (&Method::Post, "/api/cell/band/nr") => cell::cell_band_nr(state, body),
        (&Method::Post, "/api/cell/band/lte") => cell::cell_band_lte(state, body),
        (&Method::Post, "/api/cell/band/reset") => cell::cell_band_reset(state),
        (&Method::Get, "/api/cell/stc/params") => cell::cell_stc_params_get(state),
        (&Method::Put, "/api/cell/stc/params") => cell::cell_stc_params_set(state, body),
        (&Method::Get, "/api/cell/stc/status") => cell::cell_stc_status(state),
        (&Method::Post, "/api/cell/stc/enable") => cell::cell_stc_enable(state),
        (&Method::Post, "/api/cell/stc/disable") => cell::cell_stc_disable(state),
        (&Method::Post, "/api/cell/stc/reset") => cell::cell_stc_reset(state),
        (&Method::Post, "/api/cell/signal-detect/start") => cell::cell_signal_detect_start(state),
        (&Method::Post, "/api/cell/signal-detect/stop") => cell::cell_signal_detect_stop(state),
        (&Method::Get, "/api/cell/signal-detect/results") => cell::cell_signal_detect_results(state),
        (&Method::Get, "/api/cell/signal-detect/progress") => cell::cell_signal_detect_progress(state),
        // Router
        (&Method::Get, "/api/router/dns") => router::router_dns_get(state),
        (&Method::Put, "/api/router/dns") => router::router_dns_set(state, body),
        (&Method::Get, "/api/router/lan") => router::router_lan_get(state),
        (&Method::Put, "/api/router/lan") => router::router_lan_set(state, body),
        (&Method::Get, "/api/router/firewall") => router::router_firewall_get(state),
        (&Method::Put, "/api/router/firewall/switch") => router::router_firewall_switch_set(state, body),
        (&Method::Put, "/api/router/firewall/level") => router::router_firewall_level_set(state, body),
        (&Method::Put, "/api/router/firewall/nat") => router::router_firewall_nat_set(state, body),
        (&Method::Put, "/api/router/firewall/dmz") => router::router_firewall_dmz_set(state, body),
        (&Method::Get, "/api/router/firewall/upnp") => router::router_firewall_upnp_get(state),
        (&Method::Put, "/api/router/firewall/upnp") => router::router_firewall_upnp_set(state, body),
        (&Method::Get, "/api/router/firewall/port-forward") => router::router_firewall_port_forward_get(state),
        (&Method::Post, "/api/router/firewall/port-forward") => router::router_firewall_port_forward_set(state, body),
        (&Method::Put, "/api/router/firewall/port-forward/switch") => router::router_firewall_port_forward_switch(state, body),
        (&Method::Get, "/api/router/firewall/filter-rules") => router::router_firewall_filter_rules(state),
        (&Method::Get, "/api/router/vpn") => router::router_vpn_get(state),
        (&Method::Put, "/api/router/vpn") => router::router_vpn_set(state, body),
        (&Method::Get, "/api/router/qos") => router::router_qos_get(state),
        (&Method::Put, "/api/router/qos") => router::router_qos_set(state, body),
        (&Method::Get, "/api/router/domain-filter") => router::router_domain_filter_get(state),
        (&Method::Put, "/api/router/domain-filter") => router::router_domain_filter_set(state, body),
        (&Method::Get, "/api/router/apn/mode") => router::router_apn_mode_get(state),
        (&Method::Put, "/api/router/apn/mode") => router::router_apn_mode_set(state, body),
        (&Method::Get, "/api/router/apn/profiles") => router::router_apn_profiles_get(state),
        (&Method::Post, "/api/router/apn/profiles") => router::router_apn_profiles_add(state, body),
        (&Method::Put, "/api/router/apn/profiles") => router::router_apn_profiles_modify(state, body),
        (&Method::Get, "/api/router/apn/auto-profiles") => router::router_apn_auto_profiles(state),
        (&Method::Post, "/api/router/apn/profiles/delete") => router::router_apn_profiles_delete(state, body),
        (&Method::Post, "/api/router/apn/profiles/activate") => router::router_apn_profiles_activate(state, body),
        // USB
        (&Method::Get, "/api/usb/status") => usb::usb_status(state),
        (&Method::Put, "/api/usb/mode") => usb::usb_mode_set(state, body),
        (&Method::Put, "/api/usb/powerbank") => usb::usb_powerbank_set(state, body),
        // Telephony — calls
        (&Method::Post, "/api/call/dial") => telephony::call_dial(state, body),
        (&Method::Post, "/api/call/hangup") => telephony::call_hangup(state),
        (&Method::Post, "/api/call/answer") => telephony::call_answer(state),
        (&Method::Post, "/api/call/dtmf") => telephony::call_dtmf(state, body),
        (&Method::Post, "/api/call/mute") => telephony::call_mute(state, body),
        (&Method::Post, "/api/call/audio/start") => match state.voice_bridge.start() {
            Ok(v) => (200, json!({"ok": true, "data": v})),
            Err(e) => (503, json!({"ok": false, "error": e})),
        },
        (&Method::Post, "/api/call/audio/stop") => (200, json!({"ok": true, "data": state.voice_bridge.stop()})),
        (&Method::Get, "/api/call/audio/status") => (200, json!({"ok": true, "data": state.voice_bridge.status()})),
        (&Method::Put, "/api/call/audio/settings") => {
            let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            match state.voice_bridge.set_mic_mode(parsed["mic_mode"].as_str().unwrap_or("")) {
                Ok(v) => (200, json!({"ok": true, "data": v})),
                Err(e) => (400, json!({"ok": false, "error": e})),
            }
        }
        (&Method::Get, "/api/push/status") => state.push.status(),
        (&Method::Put, "/api/push/register") => state.push.register(body),
        (&Method::Delete, "/api/push/register") => state.push.unregister(None),
        (&Method::Post, "/api/push/test") => state.push.test(),
        // Telephony — USSD
        (&Method::Post, "/api/ussd/send") => telephony::ussd_send(state, body),
        (&Method::Post, "/api/ussd/respond") => telephony::ussd_respond(state, body),
        (&Method::Post, "/api/ussd/cancel") => telephony::ussd_cancel(state),
        // Telephony — STK
        (&Method::Get, "/api/stk/menu") => telephony::stk_menu(state),
        (&Method::Post, "/api/stk/select") => telephony::stk_select(state, body),
        (&Method::Get, "/api/stk/session") => state.stk.request("status", &[]),
        (&Method::Post, "/api/stk/respond") => state.stk.request("respond", body),
        (&Method::Post, "/api/stk/cancel") => state.stk.request("cancel", body),
        (&Method::Post, "/api/stk/back") => state.stk.request("back", body),
        // DoH
        (&Method::Get, "/api/call/missed") => crate::missed::list(),
        (&Method::Post, "/api/call/missed/clear") => crate::missed::clear(),
        (&Method::Get, "/api/screen/settings") => crate::screen_api::get(),
        (&Method::Put, "/api/screen/settings") => crate::screen_api::put(body),
        (&Method::Put, "/api/screen/wallpaper") => crate::screen_api::put_wallpaper(body),
        (&Method::Delete, "/api/screen/wallpaper") => crate::screen_api::delete_wallpaper(),
        (&Method::Post, "/api/screen/open") => crate::screen_api::open(),
        (&Method::Get, "/api/adblock/status") => state.adblock.status(),
        (&Method::Put, "/api/adblock/config") => state.adblock.config_set(body),
        (&Method::Post, "/api/adblock/pause") => state.adblock.pause(body),
        (&Method::Post, "/api/adblock/resume") => state.adblock.resume(),
        (&Method::Post, "/api/adblock/update") => state.adblock.update(),
        (&Method::Get, "/api/doh/status") => doh_status(state),
        (&Method::Put, "/api/doh/config") => doh_config_set(state, body),
        (&Method::Post, "/api/doh/enable") => doh_enable(state),
        (&Method::Post, "/api/doh/disable") => doh_disable(state),
        (&Method::Get, "/api/doh/cache") => doh_cache_list(state),
        (&Method::Post, "/api/doh/cache/clear") => doh_cache_clear(state),
        // Tailscale remote management
        (&Method::Get, "/api/tailscale/status") => tailscale::status(state),
        (&Method::Get, "/api/tailscale/release") => tailscale::release::handler(),
        (&Method::Post, "/api/tailscale/setup") => tailscale::setup(state, body),
        (&Method::Post, "/api/tailscale/login-url") => tailscale::login_url(state),
        (&Method::Post, "/api/tailscale/enable") => tailscale::enable(state),
        (&Method::Post, "/api/tailscale/disable") => tailscale::disable(state),
        (&Method::Put, "/api/tailscale/config") => tailscale::config_set(state, body),
        (&Method::Post, "/api/tailscale/logout") => tailscale::logout(state),
        (&Method::Post, "/api/tailscale/update") => tailscale::update(state, body),
        // LAN test (download/upload handled above before body read)
        (&Method::Get, "/api/lan/ping") => lan_test::ping(),
        // Speed test
        (&Method::Get, "/api/speedtest/servers") => speedtest::servers(state),
        (&Method::Post, "/api/speedtest/start") => speedtest::start(state, body),
        (&Method::Get, "/api/speedtest/progress") => speedtest::progress(state),
        (&Method::Post, "/api/speedtest/stop") => speedtest::stop(state, body),
        // Scheduler
        (&Method::Get, "/api/scheduler/jobs") => scheduler::jobs_list(state),
        (&Method::Post, "/api/scheduler/jobs") => scheduler::jobs_create(state, body),
        (&Method::Put, "/api/scheduler/jobs") => scheduler::jobs_update(state, body),
        (&Method::Delete, "/api/scheduler/jobs") => scheduler::jobs_delete(state, body),
        (&Method::Put, "/api/scheduler/jobs/toggle") => scheduler::jobs_toggle(state, body),
        // AT terminal
        (&Method::Post, "/api/at/send") => at_terminal::at_send(state, body),
        (&Method::Get, "/api/at/port") => at_terminal::at_port(state),
        // Fallback
        _ => (404, json!({"ok": false, "error": "not found"})),
    }
}

// --- DoH handlers ---

fn doh_status(state: &AppState) -> (u16, Value) {
    (200, json!({"ok": true, "data": state.doh.status()}))
}

fn doh_config_set(state: &AppState, body: &[u8]) -> (u16, Value) {
    match state.doh.update_config(body) {
        Ok(()) => (200, json!({"ok": true})),
        Err(e) => (400, json!({"ok": false, "error": e})),
    }
}

fn doh_enable(state: &AppState) -> (u16, Value) {
    if state.mihomo.dns_override_active() {
        return (
            409,
            json!({"ok": false, "error": "Mihomo transparent DNS is active; stop it first"}),
        );
    }
    if let Err(e) = state.doh.start() {
        return (500, json!({"ok": false, "error": e}));
    }
    // Write DoH forwarding config to dnsmasq.d drop-in
    // (UCI `set` creates a plain option, but dnsmasq init only reads `server` as a list — drop-in is reliable)
    let _ = std::fs::write("/tmp/dnsmasq.d/doh.conf", "server=127.0.0.1#5353\nno-resolv\n");
    let _ = std::process::Command::new("sh")
        .args(["-c", "/etc/init.d/dnsmasq restart"])
        .output();
    // Save config
    state.doh.set_enabled(true);
    (200, json!({"ok": true, "data": {"status": "enabled"}}))
}

fn doh_disable(state: &AppState) -> (u16, Value) {
    state.mihomo.discard_paused_doh();
    state.doh.stop();
    dnsmasq_restore_defaults();
    state.doh.set_enabled(false);
    (200, json!({"ok": true, "data": {"status": "disabled"}}))
}

/// Restore dnsmasq to default DNS resolution (remove DoH forwarding).
/// Safe to call even if dnsmasq isn't forwarding to DoH.
pub fn dnsmasq_restore_defaults() {
    let _ = std::process::Command::new("sh")
        .args(["-c", "rm -f /tmp/dnsmasq.d/doh.conf; uci delete dhcp.lan_dns.server 2>/dev/null; uci delete dhcp.lan_dns.noresolv 2>/dev/null; uci commit dhcp; /etc/init.d/dnsmasq restart"])
        .output();
}

fn doh_cache_list(state: &AppState) -> (u16, Value) {
    (200, json!({"ok": true, "data": state.doh.cache_entries()}))
}

fn doh_cache_clear(state: &AppState) -> (u16, Value) {
    state.doh.clear_cache();
    (200, json!({"ok": true}))
}

/// Serve the embedded admin SPA (single-file build).
fn serve_index(request: Request) {
    let response = Response::from_string(INDEX_HTML)
        .with_status_code(200)
        .with_header(Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap())
        .with_header(Header::from_bytes("Cache-Control", "no-cache").unwrap());
    let _ = request.respond(response);
}

fn respond(request: Request, status: u16, body: Value) {
    let body_str = serde_json::to_string(&body).unwrap_or_default();
    let content_type = Header::from_bytes("Content-Type", "application/json").unwrap();
    let response = Response::from_string(body_str)
        .with_status_code(status)
        .with_header(content_type);
    let _ = request.respond(response);
}
