// Embeds the router payload (agent, touchscreen program, device scripts) into the installer,
// so the release is one file. OPENU60_PAYLOAD=<dir> takes every file from one folder;
// otherwise the agent comes from the workspace's musl build and the scripts from scripts/u60/device.
use std::{env, fs, path::PathBuf};

fn main() {
    let here = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = here.join("../..");
    let one_dir = env::var("OPENU60_PAYLOAD").ok().map(PathBuf::from);
    println!("cargo:rerun-if-env-changed=OPENU60_PAYLOAD");
    let bin_dir = one_dir.clone().unwrap_or_else(|| root.join("target/aarch64-unknown-linux-musl/release"));
    let script_dir = one_dir.unwrap_or_else(|| root.join("scripts/u60/device"));
    let files: [(&str, &PathBuf, bool); 5] = [
        ("zte-agent", &bin_dir, true),
        ("openu60-screen", &bin_dir, false),
        ("zte-agent-run.sh", &script_dir, true),
        ("wifi_uplink.sh", &script_dir, true),
        ("wifi_uplink_dhcp.sh", &script_dir, true),
    ];
    let mut out = String::from("pub const PAYLOAD: &[(&str, &[u8])] = &[\n");
    for (name, dir, required) in files {
        let path = dir.join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        match fs::canonicalize(&path) {
            Ok(p) => out.push_str(&format!("    ({name:?}, include_bytes!({:?})),\n", p.display().to_string())),
            Err(_) if !required => {}
            Err(_) => panic!("payload file missing: {} (build the agent first, or set OPENU60_PAYLOAD)", path.display()),
        }
    }
    out.push_str("];\n");
    let device = here.join("device/openu60-device.sh");
    println!("cargo:rerun-if-changed={}", device.display());
    out.push_str(&format!("pub const DEVICE_SCRIPT: &[u8] = include_bytes!({:?});\n", fs::canonicalize(device).unwrap().display().to_string()));
    fs::write(PathBuf::from(env::var("OUT_DIR").unwrap()).join("payload.rs"), out).unwrap();
}
