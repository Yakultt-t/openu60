//! DRM atomic KMS scanout for the 320x480 panel: two RGB565 dumb buffers, flipped on every
//! present. Legacy SETCRTC must never be used on this hardware (it reboots the unit).

use std::fs::OpenOptions;
use std::os::unix::io::{AsFd, BorrowedFd};

use drm::buffer::{Buffer as _, DrmFourcc};
use drm::control::{atomic, connector, crtc, dumbbuffer::DumbBuffer, framebuffer, plane, property, AtomicCommitFlags, Device as ControlDevice};
use drm::Device;

use crate::gfx::{Canvas, H, W};

struct Card(std::fs::File);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl Device for Card {}
impl ControlDevice for Card {}

pub struct Display {
    card: Card,
    con: connector::Handle,
    crtc: crtc::Handle,
    plane: plane::Handle,
    bufs: Vec<(DumbBuffer, framebuffer::Handle)>,
    props: Props,
    blob: property::Value<'static>,
    frame: usize,
    modeset_done: bool,
    pub rotate: bool,
}

struct Props {
    con_crtc: property::Handle,
    mode_id: property::Handle,
    active: property::Handle,
    fb_id: property::Handle,
    p_crtc: property::Handle,
    src: [property::Handle; 4],
    dst: [property::Handle; 4],
}

impl Display {
    pub fn open() -> Result<Self, String> {
        let card = Card(OpenOptions::new().read(true).write(true).open("/dev/dri/card0").map_err(|e| format!("open card0: {e}"))?);
        card.set_client_capability(drm::ClientCapability::UniversalPlanes, true).map_err(|e| format!("cap: {e}"))?;
        card.set_client_capability(drm::ClientCapability::Atomic, true).map_err(|e| format!("atomic: {e}"))?;
        let res = card.resource_handles().map_err(|e| format!("resources: {e}"))?;
        let con = res
            .connectors()
            .iter()
            .filter_map(|h| card.get_connector(*h, true).ok())
            .find(|c| c.state() == connector::State::Connected)
            .ok_or("no connected panel")?;
        let mode = *con.modes().first().ok_or("panel has no mode")?;
        let crtc = *res.crtcs().first().ok_or("no crtc")?;
        let plane = *card
            .plane_handles()
            .map_err(|e| format!("planes: {e}"))?
            .iter()
            .find(|p| card.get_plane(**p).map(|i| res.filter_crtcs(i.possible_crtcs()).contains(&crtc)).unwrap_or(false))
            .ok_or("no plane")?;
        let mut bufs = Vec::new();
        for _ in 0..2 {
            let db = card.create_dumb_buffer((W, H), DrmFourcc::Rgb565, 16).map_err(|e| format!("dumb: {e}"))?;
            let fb = card.add_framebuffer(&db, 16, 16).map_err(|e| format!("fb: {e}"))?;
            bufs.push((db, fb));
        }
        let find = |set: std::collections::HashMap<String, property::Info>, name: &str| -> Result<property::Handle, String> {
            set.get(name).map(|i| i.handle()).ok_or(format!("property {name} missing"))
        };
        let cp = card.get_properties(con.handle()).map_err(|e| e.to_string())?.as_hashmap(&card).map_err(|e| e.to_string())?;
        let rp = card.get_properties(crtc).map_err(|e| e.to_string())?.as_hashmap(&card).map_err(|e| e.to_string())?;
        let pp = card.get_properties(plane).map_err(|e| e.to_string())?.as_hashmap(&card).map_err(|e| e.to_string())?;
        let props = Props {
            con_crtc: find(cp, "CRTC_ID")?,
            mode_id: find(rp.clone(), "MODE_ID")?,
            active: find(rp, "ACTIVE")?,
            fb_id: find(pp.clone(), "FB_ID")?,
            p_crtc: find(pp.clone(), "CRTC_ID")?,
            src: [find(pp.clone(), "SRC_X")?, find(pp.clone(), "SRC_Y")?, find(pp.clone(), "SRC_W")?, find(pp.clone(), "SRC_H")?],
            dst: [find(pp.clone(), "CRTC_X")?, find(pp.clone(), "CRTC_Y")?, find(pp.clone(), "CRTC_W")?, find(pp, "CRTC_H")?],
        };
        let blob = card.create_property_blob(&mode).map_err(|e| format!("blob: {e}"))?;
        Ok(Self { card, con: con.handle(), crtc, plane, bufs, props, blob, frame: 0, modeset_done: false, rotate: true })
    }

    pub fn present(&mut self, canvas: &Canvas) -> Result<(), String> {
        let idx = self.frame % 2;
        {
            let (db, _) = &mut self.bufs[idx];
            let pitch = db.pitch() as usize;
            let mut map = self.card.map_dumb_buffer(db).map_err(|e| format!("map: {e}"))?;
            canvas.to_rgb565(map.as_mut(), pitch, self.rotate);
        }
        let fb = self.bufs[idx].1;
        let p = &self.props;
        let mut req = atomic::AtomicModeReq::new();
        if !self.modeset_done {
            req.add_property(self.con, p.con_crtc, property::Value::CRTC(Some(self.crtc)));
            req.add_property(self.crtc, p.mode_id, self.blob);
            req.add_property(self.crtc, p.active, property::Value::Boolean(true));
            req.add_property(self.plane, p.p_crtc, property::Value::CRTC(Some(self.crtc)));
            for (h, v) in p.src.iter().zip([0u64, 0, (W as u64) << 16, (H as u64) << 16]) {
                req.add_property(self.plane, *h, property::Value::UnsignedRange(v));
            }
            req.add_property(self.plane, p.dst[0], property::Value::SignedRange(0));
            req.add_property(self.plane, p.dst[1], property::Value::SignedRange(0));
            req.add_property(self.plane, p.dst[2], property::Value::UnsignedRange(W as u64));
            req.add_property(self.plane, p.dst[3], property::Value::UnsignedRange(H as u64));
        }
        req.add_property(self.plane, p.fb_id, property::Value::Framebuffer(Some(fb)));
        let flags = if self.modeset_done { AtomicCommitFlags::empty() } else { AtomicCommitFlags::ALLOW_MODESET };
        self.card.atomic_commit(flags, req).map_err(|e| format!("commit: {e}"))?;
        self.modeset_done = true;
        self.frame += 1;
        Ok(())
    }
}

const BACKLIGHT: &str = "/sys/class/leds/led:lcd/brightness";

pub fn set_backlight(level: u8) {
    let _ = std::fs::write(BACKLIGHT, level.to_string());
}

pub fn backlight() -> u8 {
    std::fs::read_to_string(BACKLIGHT).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}
