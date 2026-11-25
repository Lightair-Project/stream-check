//! Receiving an NDI source (`ndi://MACHINE (Source)`): NDI's own runtime does the receiving,
//! loaded at run time when it's installed (NDI Tools, the NDI SDK, or `NDI_RUNTIME_DIR_V6`/`V5`)
//! and never shipped with the lab; without it the lab says NDI isn't installed. The runtime is
//! called through the C functions and structures the NDI SDK documents. What arrives is
//! measured like a stream: picture size, frame rate and timing (the source's own timestamps,
//! and arrival), sound format, levels and A/V offset, and what the picture looks like.

use std::ffi::{CStr, CString, c_char, c_void};
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::analyze::Level;
use crate::helper::Meter;
use crate::hub::{Hub, Proto, Publisher, Session};
use crate::media::Media;
use crate::picture::{Layout, PictureCheck, RawThumbs, measure};

#[repr(C)]
struct FindCreate {
    show_local_sources: bool,
    p_groups: *const c_char,
    p_extra_ips: *const c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Source {
    p_ndi_name: *const c_char,
    p_url_address: *const c_char,
}

#[repr(C)]
struct RecvCreate {
    source_to_connect_to: Source,
    color_format: i32,
    bandwidth: i32,
    allow_video_fields: bool,
    p_ndi_recv_name: *const c_char,
}

#[repr(C)]
struct VideoFrame {
    xres: i32,
    yres: i32,
    four_cc: u32,
    frame_rate_n: i32,
    frame_rate_d: i32,
    picture_aspect_ratio: f32,
    frame_format_type: i32,
    timecode: i64,
    p_data: *mut u8,
    line_stride_in_bytes: i32,
    p_metadata: *const c_char,
    timestamp: i64,
}

#[repr(C)]
struct AudioFrame {
    sample_rate: i32,
    no_channels: i32,
    no_samples: i32,
    timecode: i64,
    p_data: *mut f32,
    channel_stride_in_bytes: i32,
    p_metadata: *const c_char,
    timestamp: i64,
}

const FRAME_VIDEO: i32 = 1;
const FRAME_AUDIO: i32 = 2;
const FRAME_ERROR: i32 = 4;
const COLOR_BGRX_BGRA: i32 = 0;
const BANDWIDTH_HIGHEST: i32 = 100;
const PROGRESSIVE: i32 = 1;
/// A timestamp the sender didn't give.
const UNDEFINED: i64 = i64::MAX;

type Ptr = *mut c_void;

struct Lib {
    _lib: libloading::Library,
    find_create_v2: unsafe extern "C" fn(*const FindCreate) -> Ptr,
    find_destroy: unsafe extern "C" fn(Ptr),
    find_wait_for_sources: unsafe extern "C" fn(Ptr, u32) -> bool,
    find_get_current_sources: unsafe extern "C" fn(Ptr, *mut u32) -> *const Source,
    recv_create_v3: unsafe extern "C" fn(*const RecvCreate) -> Ptr,
    recv_destroy: unsafe extern "C" fn(Ptr),
    recv_capture_v2: unsafe extern "C" fn(Ptr, *mut VideoFrame, *mut AudioFrame, *mut c_void, u32) -> i32,
    recv_free_video_v2: unsafe extern "C" fn(Ptr, *const VideoFrame),
    recv_free_audio_v2: unsafe extern "C" fn(Ptr, *const AudioFrame),
}

unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

/// Where NDI's runtime may be.
fn candidates() -> Vec<std::path::PathBuf> {
    let file = if cfg!(windows) {
        "Processing.NDI.Lib.x64.dll"
    } else if cfg!(target_os = "macos") {
        "libndi.dylib"
    } else {
        "libndi.so.6"
    };
    let mut out = vec![];
    for var in ["NDI_RUNTIME_DIR_V6", "NDI_RUNTIME_DIR_V5"] {
        if let Ok(dir) = std::env::var(var) {
            out.push(std::path::Path::new(&dir).join(file));
        }
    }
    if cfg!(target_os = "macos") {
        for p in [
            "/usr/local/lib/libndi.dylib",
            "/Library/NDI SDK for Apple/lib/macOS/libndi.dylib",
            "/Library/Application Support/NDI/lib/libndi.dylib",
            "/Applications/NDI Video Monitor.app/Contents/Frameworks/libndi_advanced.dylib",
        ] {
            out.push(p.into());
        }
    } else if cfg!(windows) {
        out.push(r"C:\Program Files\NDI\NDI 6 Runtime\v6\Processing.NDI.Lib.x64.dll".into());
        out.push(r"C:\Program Files\NDI\NDI 5 Runtime\v5\Processing.NDI.Lib.x64.dll".into());
    } else {
        for p in ["/usr/lib/libndi.so.6", "/usr/local/lib/libndi.so.6", "/usr/lib/x86_64-linux-gnu/libndi.so.6"] {
            out.push(p.into());
        }
    }
    out
}

fn lib() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    LIB.get_or_init(|| {
        for path in candidates().into_iter().filter(|p| p.exists()) {
            let Ok(l) = (unsafe { libloading::Library::new(&path) }) else { continue };
            let lib = unsafe {
                macro_rules! sym {
                    ($name:literal) => {
                        match l.get($name) {
                            Ok(s) => *s,
                            Err(_) => continue,
                        }
                    };
                }
                let initialize: unsafe extern "C" fn() -> bool = sym!(b"NDIlib_initialize\0");
                if !initialize() {
                    continue;
                }
                Lib {
                    find_create_v2: sym!(b"NDIlib_find_create_v2\0"),
                    find_destroy: sym!(b"NDIlib_find_destroy\0"),
                    find_wait_for_sources: sym!(b"NDIlib_find_wait_for_sources\0"),
                    find_get_current_sources: sym!(b"NDIlib_find_get_current_sources\0"),
                    recv_create_v3: sym!(b"NDIlib_recv_create_v3\0"),
                    recv_destroy: sym!(b"NDIlib_recv_destroy\0"),
                    recv_capture_v2: sym!(b"NDIlib_recv_capture_v2\0"),
                    recv_free_video_v2: sym!(b"NDIlib_recv_free_video_v2\0"),
                    recv_free_audio_v2: sym!(b"NDIlib_recv_free_audio_v2\0"),
                    _lib: l,
                }
            };
            return Some(lib);
        }
        None
    })
    .as_ref()
}

pub const NOT_INSTALLED: &str =
    "NDI isn't installed: install NDI Tools (ndi.video) or the NDI SDK, or point NDI_RUNTIME_DIR_V6 at its runtime";

/// NDI's runtime is installed and starts.
pub fn available() -> bool {
    lib().is_some()
}

/// The NDI sources on the network, looked for for up to `wait` (blocking).
pub fn sources(wait: Duration) -> Result<Vec<String>, String> {
    let l = lib().ok_or(NOT_INSTALLED)?;
    let create = FindCreate { show_local_sources: true, p_groups: std::ptr::null(), p_extra_ips: std::ptr::null() };
    let finder = unsafe { (l.find_create_v2)(&create) };
    if finder.is_null() {
        return Err("NDI couldn't start looking for sources".into());
    }
    let start = Instant::now();
    let mut names = vec![];
    loop {
        let left = wait.saturating_sub(start.elapsed());
        unsafe { (l.find_wait_for_sources)(finder, left.as_millis().min(1000) as u32) };
        let mut n = 0u32;
        let list = unsafe { (l.find_get_current_sources)(finder, &mut n) };
        names.clear();
        if !list.is_null() {
            for i in 0..n as usize {
                let s = unsafe { *list.add(i) };
                if !s.p_ndi_name.is_null() {
                    names.push(unsafe { CStr::from_ptr(s.p_ndi_name) }.to_string_lossy().into_owned());
                }
            }
        }
        if start.elapsed() >= wait {
            break;
        }
    }
    unsafe { (l.find_destroy)(finder) };
    Ok(names)
}

/// No frame for this long ends the session.
const SILENT_END: Duration = Duration::from_secs(10);

/// Connects to an NDI source by name; the session starts with its first frame.
pub async fn start(hub: &Arc<Hub>, name: &str, key: Option<String>, profile: &str) -> Result<Arc<Session>, String> {
    if lib().is_none() {
        return Err(NOT_INSTALLED.into());
    }
    let wanted = name.to_string();
    let found = tokio::task::spawn_blocking(move || sources(Duration::from_secs(3))).await.map_err(|e| e.to_string())??;
    if !found.contains(&wanted) {
        return Err(format!(
            "no NDI source named '{name}'{}",
            if found.is_empty() { " (none found)".to_string() } else { format!("; there are: {}", found.join(", ")) }
        ));
    }
    let key = key.unwrap_or_else(|| name.to_string());
    let (hub, name, profile) = (hub.clone(), name.to_string(), profile.to_string());
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || receive(hub, name, key, profile, tx));
    rx.await.map_err(|_| "the NDI receiver stopped".to_string())?
}

struct Recv(Ptr);
impl Drop for Recv {
    fn drop(&mut self) {
        if let Some(l) = lib() {
            unsafe { (l.recv_destroy)(self.0) };
        }
    }
}

/// The receiving thread: waits for the first frame (then the session starts), then measures
/// every frame until the source stops or the session is ended.
fn receive(hub: Arc<Hub>, name: String, key: String, profile: String, ready: tokio::sync::oneshot::Sender<Result<Arc<Session>, String>>) {
    let l = lib().unwrap();
    let cname = CString::new(name.clone()).unwrap_or_default();
    let recv_name = CString::new("Stream Check").unwrap();
    let create = RecvCreate {
        source_to_connect_to: Source { p_ndi_name: cname.as_ptr(), p_url_address: std::ptr::null() },
        color_format: COLOR_BGRX_BGRA,
        bandwidth: BANDWIDTH_HIGHEST,
        allow_video_fields: false,
        p_ndi_recv_name: recv_name.as_ptr(),
    };
    let ptr = unsafe { (l.recv_create_v3)(&create) };
    if ptr.is_null() {
        let _ = ready.send(Err("NDI couldn't make a receiver".into()));
        return;
    }
    let recv = Recv(ptr);
    let mut ready = Some(ready);
    let mut p: Option<Publisher> = None;
    let mut st = State::default();
    let waited = Instant::now();
    let mut last_frame = Instant::now();
    let reason = loop {
        if let Some(publisher) = p.as_ref().filter(|p| p.is_killed()) {
            break publisher.session.stop_reason();
        }
        let mut v: VideoFrame = unsafe { std::mem::zeroed() };
        let mut a: AudioFrame = unsafe { std::mem::zeroed() };
        let kind = unsafe { (l.recv_capture_v2)(recv.0, &mut v, &mut a, std::ptr::null_mut(), 500) };
        match kind {
            FRAME_VIDEO | FRAME_AUDIO => {
                last_frame = Instant::now();
                if p.is_none() {
                    match hub.authorize_pull(Proto::Ndi, &profile, &key, SocketAddr::from(([0, 0, 0, 0], 0))) {
                        Ok(publisher) => {
                            publisher.session.set_source(json!({ "kind": "ndi", "name": name }));
                            st.thumbs = Some(RawThumbs::new(&publisher.session.dir.join("thumbs")));
                            if let Some(r) = ready.take() {
                                let _ = r.send(Ok(publisher.session.clone()));
                            }
                            p = Some(publisher);
                        }
                        Err(e) => {
                            free(l, &recv, kind, &v, &a);
                            if let Some(r) = ready.take() {
                                let _ = r.send(Err(e.msg));
                            }
                            return;
                        }
                    }
                }
                let publisher = p.as_ref().unwrap();
                if kind == FRAME_VIDEO {
                    st.video(publisher, &v);
                } else {
                    st.audio(publisher, &a);
                }
                free(l, &recv, kind, &v, &a);
            }
            FRAME_ERROR => break "the NDI connection failed".into(),
            _ => {
                if p.is_none() && waited.elapsed() > Duration::from_secs(8) {
                    if let Some(r) = ready.take() {
                        let _ = r.send(Err(format!("'{name}' sent nothing in 8 s")));
                    }
                    return;
                }
                if p.is_some() && last_frame.elapsed() > SILENT_END {
                    break format!("nothing came from the NDI source for {} s", SILENT_END.as_secs());
                }
            }
        }
        if let Some(publisher) = &p {
            st.update(&publisher.session);
        }
    };
    drop(recv);
    if let Some(publisher) = p {
        if let Some(t) = &mut st.thumbs {
            t.close();
        }
        crate::camera::finish_picture(&publisher.session, &st.check);
        publisher.end(&reason);
    }
}

fn free(l: &Lib, recv: &Recv, kind: i32, v: &VideoFrame, a: &AudioFrame) {
    unsafe {
        match kind {
            FRAME_VIDEO => (l.recv_free_video_v2)(recv.0, v),
            FRAME_AUDIO => (l.recv_free_audio_v2)(recv.0, a),
            _ => {}
        }
    }
}

/// What the receiving thread keeps between frames.
#[derive(Default)]
struct State {
    /// The source's first timestamp (100 ns units) and that moment in the session (µs).
    first: Option<(i64, i64)>,
    format: Option<(i32, i32, u32, i32, i32, i32)>,
    check: PictureCheck,
    thumbs: Option<RawThumbs>,
    next_thumb: f64,
    meter: Option<(Meter, i32, i32)>,
    last_update: Option<Instant>,
    audio_format: Option<(i32, i32)>,
}

fn fourcc(f: u32) -> String {
    f.to_le_bytes().iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '?' }).collect()
}

impl State {
    /// A frame's moment in microseconds of the session: the source's own timestamps, when it gives them.
    fn pts(&mut self, s: &Session, timestamp: i64, timecode: i64) -> i64 {
        let now_us = s.t0.elapsed().as_micros() as i64;
        let ts = if timestamp != UNDEFINED && timestamp != 0 { timestamp } else { timecode };
        if ts == UNDEFINED {
            return now_us;
        }
        let (first, base) = *self.first.get_or_insert((ts, now_us));
        base + (ts - first) / 10
    }

    fn video(&mut self, p: &Publisher, v: &VideoFrame) {
        let s = &p.session;
        let f = (v.xres, v.yres, v.four_cc, v.frame_rate_n, v.frame_rate_d, v.frame_format_type);
        if self.format != Some(f) {
            self.format = Some(f);
            let fps = (v.frame_rate_d > 0).then(|| v.frame_rate_n as f64 / v.frame_rate_d as f64);
            let fcc = fourcc(v.four_cc);
            s.with_stats(|st| {
                st.has_video = true;
                st.video.sps = Some(crate::nal::Sps {
                    profile: format!("{fcc} (uncompressed)"),
                    width: v.xres.max(0) as u32,
                    height: v.yres.max(0) as u32,
                    declared_fps: fps,
                    interlaced: v.frame_format_type != PROGRESSIVE,
                    ..Default::default()
                });
            });
            s.event(
                Level::Info,
                "video-config",
                format!(
                    "{}x{} {fcc}, {} fps declared{}",
                    v.xres,
                    v.yres,
                    fps.map_or("?".into(), |f| format!("{f:.3}")),
                    if v.frame_format_type != PROGRESSIVE { ", interlaced" } else { "" }
                ),
            );
            s.update_source(|x| {
                x["video"] = json!({ "width": v.xres, "height": v.yres, "fourcc": fcc, "frame_rate": format!("{}/{}", v.frame_rate_n, v.frame_rate_d), "aspect": v.picture_aspect_ratio, "progressive": v.frame_format_type == PROGRESSIVE });
            });
        }
        let pts = self.pts(s, v.timestamp, v.timecode);
        p.media(Media::Video { pts_us: pts, dts_us: pts, key: false, data: Arc::new(vec![]) });
        let (w, h) = (v.xres.max(0) as usize, v.yres.max(0) as usize);
        let layout = match &v.four_cc.to_le_bytes() {
            b"BGRA" | b"BGRX" => Some((Layout::Bgra, "bgra")),
            b"RGBA" | b"RGBX" => Some((Layout::Rgba, "rgba")),
            b"UYVY" => Some((Layout::Uyvy, "uyvy422")),
            _ => None,
        };
        let stride = v.line_stride_in_bytes.max(0) as usize;
        if let (Some((layout, pix)), false) = (layout, v.p_data.is_null()) {
            let data = unsafe { std::slice::from_raw_parts(v.p_data, stride * h) };
            let m = measure(data, w, h, stride, layout);
            let t = s.t0.elapsed().as_secs_f64();
            for n in self.check.frame(t, &m) {
                s.event(n.level, n.kind, n.msg);
            }
            if t >= self.next_thumb {
                self.next_thumb = t + crate::helper::THUMB_EVERY_S;
                let row = if layout == Layout::Uyvy { w * 2 } else { w * 4 };
                let mut packed = Vec::with_capacity(row * h);
                for y in 0..h {
                    packed.extend_from_slice(&data[y * stride..y * stride + row.min(stride)]);
                }
                if let Some(th) = &mut self.thumbs {
                    th.frame(&packed, w, h, pix);
                }
            }
        }
    }

    fn audio(&mut self, p: &Publisher, a: &AudioFrame) {
        let s = &p.session;
        let (rate, ch, n) = (a.sample_rate.max(1), a.no_channels.max(1), a.no_samples.max(0));
        if self.audio_format != Some((rate, ch)) {
            self.audio_format = Some((rate, ch));
            s.event(Level::Info, "audio-config", format!("{rate} Hz, {ch} channel(s), 32-bit float"));
            s.update_source(|x| x["audio"] = json!({ "sample_rate": rate, "channels": ch, "format": "float planar" }));
        }
        let frame_ms = n as f64 * 1000.0 / rate as f64;
        s.with_stats(|st| {
            st.has_audio = true;
            st.audio.sample_rate = rate as u32;
            st.audio.channels = ch.min(255) as u8;
            st.audio.frame_ms = frame_ms;
        });
        let pts = self.pts(s, a.timestamp, a.timecode);
        p.media(Media::Audio { pts_us: pts, data: Arc::new(vec![]) });
        if a.p_data.is_null() || n == 0 {
            return;
        }
        // interleaved, for the meter
        let stride = (a.channel_stride_in_bytes.max(0) / 4) as usize;
        let planes = unsafe { std::slice::from_raw_parts(a.p_data, stride * ch as usize) };
        let mut inter = Vec::with_capacity(n as usize * ch as usize);
        for i in 0..n as usize {
            for c in 0..ch as usize {
                inter.push(planes.get(c * stride + i).copied().unwrap_or(0.0));
            }
        }
        if self.meter.as_ref().is_none_or(|m| (m.1, m.2) != (ch, rate)) {
            self.meter = Some((Meter::new(ch as u32, rate as u32), ch, rate));
        }
        let levels = self.meter.as_mut().unwrap().0.push(&inter);
        for lv in levels {
            s.set_levels(lv);
        }
    }

    fn update(&mut self, s: &Session) {
        if self.last_update.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
            return;
        }
        self.last_update = Some(Instant::now());
        let stats = self.check.stats.clone();
        s.update_source(|v| v["picture"] = json!(stats));
    }
}
