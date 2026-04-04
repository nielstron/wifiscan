use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use image::DynamicImage;
use nokhwa::{
    Camera,
    pixel_format::RgbFormat,
    utils::{ApiBackend, CameraIndex, CameraInfo, RequestedFormat, RequestedFormatType},
};
use qrcode_generator::QrCodeEcc;
use wifiscan::decode::{decode_qr_from_image_current, decode_qr_from_path};

fn main() -> Result<()> {
    let cameras = nokhwa::query(ApiBackend::Auto).context("failed to enumerate cameras")?;
    let args = std::env::args().skip(1).collect::<Vec<_>>();

    if args.iter().any(|arg| arg == "--list-cameras") {
        list_cameras(&cameras);
        return Ok(());
    }

    if let Some(index) = args.iter().position(|arg| arg == "--decode-image") {
        let path = args
            .get(index + 1)
            .context("--decode-image requires a path")?;
        let payload = decode_qr_from_path(path)?;
        println!("{payload}");
        return Ok(());
    }

    if let Some(index) = args.iter().position(|arg| arg == "--generate-qr") {
        let output = args
            .get(index + 1)
            .context("--generate-qr requires an output path")?;
        let payload = args
            .get(index + 2)
            .context("--generate-qr requires a payload")?;
        write_qr_png(output, payload)?;
        println!("wrote {output}");
        return Ok(());
    }

    if let Some(index) = args.iter().position(|arg| arg == "--scan-camera") {
        let index = args
            .get(index + 1)
            .context("--scan-camera requires a camera index")?
            .parse::<usize>()
            .context("camera index must be an integer")?;
        let camera = cameras
            .get(index)
            .with_context(|| format!("camera index {index} is out of range"))?;
        match scan_camera_for_qr(camera, Duration::from_secs(5))? {
            Some(payload) => println!("{payload}"),
            None => println!("no QR detected"),
        }
        return Ok(());
    }

    let native_options = eframe::NativeOptions::default();

    eframe::run_native(
        "WiFi QR Scanner",
        native_options,
        Box::new(move |_cc| Ok(Box::new(WifiScanApp::new(cameras.clone())))),
    )
    .map_err(|err| anyhow::anyhow!("failed to start app: {err}"))?;

    Ok(())
}

struct WifiScanApp {
    cameras: Vec<CameraInfo>,
    frame_rx: Receiver<FrameUpdate>,
    stop_tx: Option<Sender<()>>,
    texture: Option<TextureHandle>,
    selected_camera: usize,
    last_scan: Option<ScanResult>,
    connect_prompt_open: bool,
    status: String,
    last_preview_update: Instant,
}

impl WifiScanApp {
    fn new(cameras: Vec<CameraInfo>) -> Self {
        let selected_camera = default_camera_index(&cameras);
        let (selected_camera, frame_rx, stop_tx, status) =
            match start_first_available_camera(&cameras, selected_camera) {
                Ok((selected_camera, frame_rx, stop_tx)) => (
                    selected_camera,
                    frame_rx,
                    Some(stop_tx),
                    "Point the camera at a Wi-Fi QR code.".to_owned(),
                ),
                Err(err) => (
                    selected_camera,
                    empty_receiver(),
                    None,
                    describe_camera_error(&err),
                ),
            };

        Self {
            cameras,
            frame_rx,
            stop_tx,
            texture: None,
            selected_camera,
            last_scan: None,
            connect_prompt_open: false,
            status,
            last_preview_update: Instant::now(),
        }
    }

    fn restart_camera(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }

        self.texture = None;
        self.last_scan = None;
        self.connect_prompt_open = false;
        self.last_preview_update = Instant::now();

        let camera = self.cameras.get(self.selected_camera).cloned();
        match start_camera_worker(camera) {
            Ok((frame_rx, stop_tx)) => {
                self.frame_rx = frame_rx;
                self.stop_tx = Some(stop_tx);
                self.status = "Point the camera at a Wi-Fi QR code.".to_owned();
            }
            Err(err) => {
                self.frame_rx = empty_receiver();
                self.status = describe_camera_error(&err);
            }
        }
    }
}

impl eframe::App for WifiScanApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(update) = self.frame_rx.try_recv() {
            match update {
                FrameUpdate::Preview(image) => {
                    let texture = self.texture.get_or_insert_with(|| {
                        ctx.load_texture("camera-preview", image.clone(), TextureOptions::LINEAR)
                    });
                    texture.set(image, TextureOptions::LINEAR);
                    self.last_preview_update = Instant::now();
                }
                FrameUpdate::Scan(scan) => {
                    self.status = format!(
                        "Found network '{}' using {} security.",
                        scan.credentials.ssid, scan.credentials.auth_type
                    );
                    self.last_scan = Some(scan);
                    self.connect_prompt_open = true;
                }
                FrameUpdate::Error(message) => {
                    self.status = message;
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Wi-Fi QR Scanner");
            ui.separator();

            if self.cameras.is_empty() {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "No cameras detected. Connect a camera and restart the app.",
                );
            } else {
                let selected_name = self
                    .cameras
                    .get(self.selected_camera)
                    .map(CameraInfo::human_name)
                    .unwrap_or("Unknown camera".to_owned());

                egui::ComboBox::from_label("Camera")
                    .selected_text(selected_name)
                    .show_ui(ui, |ui| {
                        let mut changed = false;
                        for (index, camera) in self.cameras.iter().enumerate() {
                            changed |= ui
                                .selectable_value(
                                    &mut self.selected_camera,
                                    index,
                                    camera.human_name(),
                                )
                                .changed();
                        }
                        if changed {
                            self.restart_camera();
                        }
                    });
            }

            ui.separator();

            if let Some(texture) = &self.texture {
                let available = ui.available_width();
                let image_size = texture.size_vec2();
                let scale = (available / image_size.x).min(1.0);
                ui.image((texture.id(), image_size * scale));
            } else {
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 320.0),
                    egui::Layout::centered_and_justified(egui::Direction::TopDown),
                    |ui| {
                        ui.label("Waiting for camera frames...");
                    },
                );
            }

            ui.separator();
            ui.label(&self.status);

            if self.stop_tx.is_some()
                && self.last_preview_update.elapsed() > Duration::from_secs(3)
                && !self.cameras.is_empty()
            {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "The camera preview has stalled. Re-select the camera to restart it.",
                );
            }

            if let Some(scan) = &self.last_scan {
                ui.separator();
                ui.monospace(format!("SSID: {}", scan.credentials.ssid));
                ui.monospace(format!(
                    "Password: {}",
                    if scan.credentials.password.is_empty() {
                        "<empty>"
                    } else {
                        &scan.credentials.password
                    }
                ));
                if scan.credentials.hidden {
                    ui.monospace("Hidden network: true");
                }

                ui.horizontal(|ui| {
                    if ui.button("Connect").clicked() {
                        let result = connect_to_wifi(&scan.credentials);
                        self.status = match result {
                            Ok(()) => format!("Connected to '{}'.", scan.credentials.ssid),
                            Err(err) => format!("Connection failed: {err:#}"),
                        };
                        self.connect_prompt_open = false;
                    }

                    if ui
                        .add_enabled(
                            !scan.credentials.password.is_empty(),
                            egui::Button::new("Copy Password"),
                        )
                        .clicked()
                    {
                        ctx.copy_text(scan.credentials.password.clone());
                        self.status =
                            format!("Copied password for '{}' to the clipboard.", scan.credentials.ssid);
                    }
                });
            }
        });

        if self.connect_prompt_open {
            if let Some(scan) = self.last_scan.clone() {
                egui::Window::new("Connect To Wi-Fi?")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .show(ctx, |ui| {
                        ui.label(format!("Connect to '{}'? ", scan.credentials.ssid));
                        if scan.credentials.hidden {
                            ui.monospace("Hidden network: true");
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Connect").clicked() {
                                let result = connect_to_wifi(&scan.credentials);
                                self.status = match result {
                                    Ok(()) => format!("Connected to '{}'.", scan.credentials.ssid),
                                    Err(err) => format!("Connection failed: {err:#}"),
                                };
                                self.connect_prompt_open = false;
                            }
                            if ui
                                .add_enabled(
                                    !scan.credentials.password.is_empty(),
                                    egui::Button::new("Copy Password"),
                                )
                                .clicked()
                            {
                                ctx.copy_text(scan.credentials.password.clone());
                                self.status = format!(
                                    "Copied password for '{}' to the clipboard.",
                                    scan.credentials.ssid
                                );
                            }
                            if ui.button("Not now").clicked() {
                                self.connect_prompt_open = false;
                            }
                        });
                    });
            } else {
                self.connect_prompt_open = false;
            }
        }

        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

impl Drop for WifiScanApp {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WifiCredentials {
    ssid: String,
    password: String,
    auth_type: String,
    hidden: bool,
}

#[derive(Clone, Debug)]
struct ScanResult {
    credentials: WifiCredentials,
}

enum FrameUpdate {
    Preview(ColorImage),
    Scan(ScanResult),
    Error(String),
}

fn start_camera_worker(
    camera: Option<CameraInfo>,
) -> Result<(Receiver<FrameUpdate>, Sender<()>)> {
    let camera = camera.context("no camera available")?;
    let (frame_tx, frame_rx) = bounded(2);
    let (detect_tx, detect_rx) = bounded(1);
    let (stop_tx, stop_rx) = unbounded();

    spawn_detection_worker(frame_tx.clone(), detect_rx, stop_rx.clone());

    thread::spawn(move || {
        let index = match camera.index().as_index() {
            Ok(index) => CameraIndex::Index(index),
            Err(err) => {
                let _ = frame_tx.send(FrameUpdate::Error(format!(
                    "Failed to resolve camera index: {err:#}"
                )));
                return;
            }
        };
        let requested =
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestResolution);
        let mut camera = match Camera::new(index, requested) {
            Ok(camera) => camera,
            Err(err) => {
                let _ = frame_tx.send(FrameUpdate::Error(format!(
                    "Failed to create camera: {err:#}"
                )));
                return;
            }
        };
        if let Err(err) = camera.open_stream() {
            let _ = frame_tx.send(FrameUpdate::Error(format!(
                "Failed to open camera stream: {err:#}"
            )));
            return;
        }

        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }

            let frame = match camera.frame() {
                Ok(frame) => frame,
                Err(err) => {
                    let _ = frame_tx.send(FrameUpdate::Error(format!(
                        "Failed to capture camera frame: {err:#}"
                    )));
                    thread::sleep(Duration::from_millis(250));
                    continue;
                }
            };

            let rgb = match frame.decode_image::<RgbFormat>() {
                Ok(image) => image,
                Err(err) => {
                    let _ = frame_tx.send(FrameUpdate::Error(format!(
                        "Failed to decode camera frame: {err:#}"
                    )));
                    thread::sleep(Duration::from_millis(250));
                    continue;
                }
            };

            let color = ColorImage::from_rgb(
                [rgb.width() as usize, rgb.height() as usize],
                rgb.as_raw(),
            );
            let _ = frame_tx.try_send(FrameUpdate::Preview(color));

            let frame_image = DynamicImage::ImageRgb8(rgb);
            match detect_tx.try_send(frame_image) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
    });

    Ok((frame_rx, stop_tx))
}

fn spawn_detection_worker(
    frame_tx: Sender<FrameUpdate>,
    detect_rx: Receiver<DynamicImage>,
    stop_rx: Receiver<()>,
) {
    thread::spawn(move || {
        let mut last_payload = None;

        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }

            let Ok(mut frame_image) = detect_rx.recv_timeout(Duration::from_millis(100)) else {
                continue;
            };

            while let Ok(newer_frame) = detect_rx.try_recv() {
                frame_image = newer_frame;
            }

            if let Some(payload) = decode_qr_from_image_current(&frame_image) {
                if last_payload.as_deref() != Some(payload.as_str()) {
                    match parse_wifi_qr(&payload) {
                        Ok(credentials) => {
                            last_payload = Some(payload);
                            let _ = frame_tx.try_send(FrameUpdate::Scan(ScanResult { credentials }));
                        }
                        Err(err) => {
                            let _ = frame_tx.try_send(FrameUpdate::Error(format!(
                                "QR code found, but it is not a valid Wi-Fi payload: {err:#}"
                            )));
                        }
                    }
                }
            }
        }
    });
}

fn empty_receiver<T>() -> Receiver<T> {
    let (_tx, rx) = bounded(0);
    rx
}

fn start_first_available_camera(
    cameras: &[CameraInfo],
    preferred_index: usize,
) -> Result<(usize, Receiver<FrameUpdate>, Sender<()>)> {
    if cameras.is_empty() {
        bail!("no camera available");
    }

    for index in camera_attempt_order(cameras.len(), preferred_index) {
        if let Ok((frame_rx, stop_tx)) = start_camera_worker(cameras.get(index).cloned()) {
            return Ok((index, frame_rx, stop_tx));
        }
    }

    let preferred = cameras
        .get(preferred_index)
        .map(CameraInfo::human_name)
        .unwrap_or_else(|| "Unknown camera".to_owned());
    let fallback_errors = camera_attempt_order(cameras.len(), preferred_index)
        .filter_map(|index| {
            let camera = cameras.get(index)?;
            let err = start_camera_worker(Some(camera.clone())).err()?;
            Some(format!("{}: {}", camera.human_name(), describe_camera_error(&err)))
        })
        .collect::<Vec<_>>()
        .join(" | ");

    bail!("failed to open '{preferred}'. {fallback_errors}");
}

fn camera_attempt_order(count: usize, preferred_index: usize) -> impl Iterator<Item = usize> {
    std::iter::once(preferred_index).chain((0..count).filter(move |index| *index != preferred_index))
}

fn default_camera_index(cameras: &[CameraInfo]) -> usize {
    cameras
        .iter()
        .position(|camera| {
            let name = camera.human_name().to_ascii_lowercase();
            !name.contains("virtual")
        })
        .unwrap_or(0)
}

fn describe_camera_error(err: &anyhow::Error) -> String {
    let message = format!("{err:#}");
    if message.contains("Lock Rejected") {
        "The selected camera is in use by another app. Close OBS/Zoom/Photo Booth or pick a different camera.".to_owned()
    } else if message.contains("no camera available") {
        "No camera available.".to_owned()
    } else {
        format!("Camera startup failed: {message}")
    }
}

fn scan_camera_for_qr(camera: &CameraInfo, timeout: Duration) -> Result<Option<String>> {
    let index = CameraIndex::Index(camera.index().as_index()?);
    let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestResolution);
    let mut camera = Camera::new(index, requested).context("failed to create camera")?;
    camera.open_stream().context("failed to open camera stream")?;

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let frame = camera.frame().context("failed to capture camera frame")?;
        let rgb = frame
            .decode_image::<RgbFormat>()
            .context("failed to decode camera frame")?;
        let frame_image = DynamicImage::ImageRgb8(rgb);
        if let Some(payload) = decode_qr_from_image_current(&frame_image) {
            return Ok(Some(payload));
        }
    }

    Ok(None)
}

fn write_qr_png(path: &str, payload: &str) -> Result<()> {
    let bytes = qrcode_generator::to_png_to_vec(payload, QrCodeEcc::Medium, 512)
        .context("failed to generate QR PNG")?;
    std::fs::write(path, bytes).with_context(|| format!("failed to write QR PNG to {path}"))?;
    Ok(())
}

fn list_cameras(cameras: &[CameraInfo]) {
    if cameras.is_empty() {
        println!("No cameras detected.");
        return;
    }

    for (index, camera) in cameras.iter().enumerate() {
        println!("{index}: {}", camera.human_name());
    }
}

fn parse_wifi_qr(payload: &str) -> Result<WifiCredentials> {
    let Some(body) = payload.strip_prefix("WIFI:") else {
        bail!("missing WIFI: prefix");
    };

    let mut ssid = None;
    let mut password = String::new();
    let mut auth_type = "nopass".to_owned();
    let mut hidden = false;

    for field in split_wifi_fields(body) {
        if let Some(value) = field.strip_prefix("S:") {
            ssid = Some(unescape_wifi_value(value));
            continue;
        }
        if let Some(value) = field.strip_prefix("P:") {
            password = unescape_wifi_value(value);
            continue;
        }
        if let Some(value) = field.strip_prefix("T:") {
            auth_type = unescape_wifi_value(value);
            continue;
        }
        if let Some(value) = field.strip_prefix("H:") {
            hidden = value.eq_ignore_ascii_case("true");
        }
    }

    let ssid = ssid.context("missing SSID")?;
    if auth_type.eq_ignore_ascii_case("nopass") {
        password.clear();
    }

    Ok(WifiCredentials {
        ssid,
        password,
        auth_type,
        hidden,
    })
}

fn split_wifi_fields(payload: &str) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut escaped = false;

    for (index, ch) in payload.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == ';' {
            fields.push(&payload[start..index]);
            start = index + ch.len_utf8();
        }
    }

    if start < payload.len() {
        fields.push(&payload[start..]);
    }

    fields
}

fn unescape_wifi_value(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut escaped = false;

    for ch in value.chars() {
        if escaped {
            result.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            result.push(ch);
        }
    }

    result
}

fn connect_to_wifi(credentials: &WifiCredentials) -> Result<()> {
    let device = wifi_device_name()?;

    let status = Command::new("networksetup")
        .arg("-setairportnetwork")
        .arg(&device)
        .arg(&credentials.ssid)
        .arg(&credentials.password)
        .status()
        .context("failed to execute networksetup")?;

    if !status.success() {
        bail!("networksetup exited with status {status}");
    }

    Ok(())
}

fn wifi_device_name() -> Result<String> {
    let output = Command::new("networksetup")
        .arg("-listallhardwareports")
        .output()
        .context("failed to list macOS hardware ports")?;

    if !output.status.success() {
        bail!("networksetup exited with status {}", output.status);
    }

    let stdout = String::from_utf8(output.stdout).context("hardware port output was not utf-8")?;
    let mut lines = stdout.lines();

    while let Some(line) = lines.next() {
        if line.trim() == "Hardware Port: Wi-Fi" {
            if let Some(device_line) = lines.next() {
                if let Some(device) = device_line.trim().strip_prefix("Device: ") {
                    return Ok(device.to_owned());
                }
            }
        }
    }

    bail!("could not find the macOS Wi-Fi hardware device");
}

#[cfg(test)]
mod tests {
    use super::{WifiCredentials, parse_wifi_qr, split_wifi_fields, unescape_wifi_value};
    use image::load_from_memory;
    use qrcode_generator::QrCodeEcc;
    use wifiscan::decode::decode_qr_from_image_current;

    #[test]
    fn parses_standard_wifi_qr() {
        let parsed = parse_wifi_qr("WIFI:T:WPA;S:OfficeNet;P:swordfish;;").unwrap();
        assert_eq!(
            parsed,
            WifiCredentials {
                ssid: "OfficeNet".into(),
                password: "swordfish".into(),
                auth_type: "WPA".into(),
                hidden: false,
            }
        );
    }

    #[test]
    fn parses_escaped_wifi_qr_fields() {
        let parsed =
            parse_wifi_qr(r"WIFI:T:WPA2;S:My\:Wifi\;Guest;P:p\;ass\\word;H:true;;").unwrap();
        assert_eq!(parsed.ssid, "My:Wifi;Guest");
        assert_eq!(parsed.password, r"p;ass\word");
        assert!(parsed.hidden);
    }

    #[test]
    fn split_fields_respects_escaped_semicolons() {
        let fields = split_wifi_fields(r"T:WPA;S:Hello\;World;P:test;;");
        assert_eq!(fields, vec!["T:WPA", r"S:Hello\;World", "P:test", ""]);
    }

    #[test]
    fn unescape_keeps_plain_text() {
        assert_eq!(unescape_wifi_value("plain-text"), "plain-text");
    }

    #[test]
    fn generated_wifi_qr_decodes_end_to_end() {
        let payload = "WIFI:T:WPA;S:MockNet;P:swordfish;;";
        let png = qrcode_generator::to_png_to_vec(payload, QrCodeEcc::Medium, 512).unwrap();
        let image = load_from_memory(&png).unwrap();
        let decoded = decode_qr_from_image_current(&image).unwrap();
        assert_eq!(decoded, payload);
    }
}
