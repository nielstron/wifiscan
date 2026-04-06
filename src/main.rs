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
#[cfg(target_os = "macos")]
use tray_icon::{
    MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, Submenu},
};
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

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("WiFi QR Scanner")
            .with_inner_size([420.0, 720.0]),
        ..Default::default()
    };

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
    selected_camera: usize,
    frame_rx: Receiver<FrameUpdate>,
    connect_rx: Receiver<ConnectResult>,
    connect_tx: Sender<ConnectResult>,
    stop_tx: Option<Sender<()>>,
    texture: Option<TextureHandle>,
    preview_aspect_ratio: Option<f32>,
    last_scan: Option<ScanResult>,
    connect_in_flight: bool,
    connect_prompt_open: bool,
    status: String,
    last_preview_update: Instant,
    window_visible: bool,
    #[cfg(target_os = "macos")]
    tray: Option<MenuBarState>,
    #[cfg(target_os = "macos")]
    tray_event_rx: Receiver<TrayIconEvent>,
    #[cfg(target_os = "macos")]
    tray_event_tx: Sender<TrayIconEvent>,
    #[cfg(target_os = "macos")]
    menu_event_rx: Receiver<MenuEvent>,
    #[cfg(target_os = "macos")]
    menu_event_tx: Sender<MenuEvent>,
    #[cfg(target_os = "macos")]
    should_quit: bool,
}

impl WifiScanApp {
    fn new(cameras: Vec<CameraInfo>) -> Self {
        let selected_camera = default_camera_index(&cameras);
        let (connect_tx, connect_rx) = unbounded();
        #[cfg(target_os = "macos")]
        let (tray_event_tx, tray_event_rx) = unbounded();
        #[cfg(target_os = "macos")]
        let (menu_event_tx, menu_event_rx) = unbounded();
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
            selected_camera,
            frame_rx,
            connect_rx,
            connect_tx,
            stop_tx,
            texture: None,
            preview_aspect_ratio: None,
            last_scan: None,
            connect_in_flight: false,
            connect_prompt_open: false,
            status,
            last_preview_update: Instant::now(),
            window_visible: true,
            #[cfg(target_os = "macos")]
            tray: None,
            #[cfg(target_os = "macos")]
            tray_event_rx,
            #[cfg(target_os = "macos")]
            tray_event_tx,
            #[cfg(target_os = "macos")]
            menu_event_rx,
            #[cfg(target_os = "macos")]
            menu_event_tx,
            #[cfg(target_os = "macos")]
            should_quit: false,
        }
    }

    #[cfg(target_os = "macos")]
    fn ensure_menu_bar(&mut self, ctx: &egui::Context) {
        if self.tray.is_some() {
            return;
        }

        let tray_event_tx = self.tray_event_tx.clone();
        let tray_ctx = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            let _ = tray_event_tx.send(event);
            tray_ctx.request_repaint();
        }));

        let menu_event_tx = self.menu_event_tx.clone();
        let menu_ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |event| {
            let _ = menu_event_tx.send(event);
            menu_ctx.request_repaint();
        }));

        match MenuBarState::new(&self.cameras, self.selected_camera) {
            Ok(tray) => self.tray = Some(tray),
            Err(err) => {
                self.status = format!("Failed to start menu bar icon: {err:#}");
            }
        }
    }

    fn stop_camera(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        self.frame_rx = empty_receiver();
        self.texture = None;
        self.preview_aspect_ratio = None;
        self.last_preview_update = Instant::now();
    }

    fn start_camera(&mut self) {
        match start_first_available_camera(&self.cameras, self.selected_camera) {
            Ok((selected_camera, frame_rx, stop_tx)) => {
                self.selected_camera = selected_camera;
                self.frame_rx = frame_rx;
                self.stop_tx = Some(stop_tx);
                self.status = "Point the camera at a Wi-Fi QR code.".to_owned();
                self.last_preview_update = Instant::now();
            }
            Err(err) => {
                self.frame_rx = empty_receiver();
                self.stop_tx = None;
                self.status = describe_camera_error(&err);
            }
        }
    }

    fn select_camera(&mut self, index: usize) {
        if index >= self.cameras.len() {
            return;
        }

        self.selected_camera = index;
        self.last_scan = None;
        self.connect_in_flight = false;
        self.connect_prompt_open = false;

        if self.window_visible {
            self.stop_camera();
            self.start_camera();
        } else {
            self.status = format!(
                "Selected camera '{}'.",
                self.cameras[index].human_name()
            );
        }
    }

    fn set_window_visible(&mut self, ctx: &egui::Context, visible: bool) {
        if self.window_visible == visible {
            return;
        }

        self.window_visible = visible;
        if visible {
            self.start_camera();
        } else {
            self.stop_camera();
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(visible));
        if visible {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    fn sync_window_to_preview_aspect(&self, ctx: &egui::Context) {
        let Some(aspect_ratio) = self.preview_aspect_ratio else {
            return;
        };

        let current_size = ctx
            .input(|input| input.viewport().inner_rect.map(|rect| rect.size()))
            .unwrap_or_else(|| egui::vec2(420.0, 720.0));
        let target_height = current_size.y.max(360.0);
        let target_width = (target_height * aspect_ratio).clamp(480.0, 1600.0);
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            target_width,
            target_height,
        )));
    }

    fn repaint_interval(&self) -> Option<Duration> {
        if self.window_visible && self.stop_tx.is_some() {
            Some(Duration::from_millis(16))
        } else if self.connect_in_flight {
            Some(Duration::from_millis(50))
        } else if self.connect_prompt_open || self.window_visible {
            Some(Duration::from_millis(250))
        } else {
            None
        }
    }
}

impl eframe::App for WifiScanApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "macos")]
        self.ensure_menu_bar(ctx);

        #[cfg(target_os = "macos")]
        if let Some(tray) = &self.tray {
            let toggle_id = tray.toggle_item.id().clone();
            let quit_id = tray.quit_item.id().clone();
            let tray_id = tray.icon.id().clone();
            let mut toggle_window = false;
            let mut selected_camera = None;

            while let Ok(event) = self.menu_event_rx.try_recv() {
                if event.id == toggle_id {
                    toggle_window = true;
                } else if event.id == quit_id {
                    self.should_quit = true;
                } else if let Some((index, _)) = tray
                    .camera_items
                    .iter()
                    .enumerate()
                    .find(|(_, item)| event.id == item.id())
                {
                    selected_camera = Some(index);
                }
            }

            while let Ok(event) = self.tray_event_rx.try_recv() {
                if event.id() != &tray_id {
                    continue;
                }

                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    toggle_window = true;
                }
            }

            if toggle_window {
                self.set_window_visible(ctx, !self.window_visible);
            }

            if let Some(index) = selected_camera {
                self.select_camera(index);
            }
        }

        if ctx.input(|input| input.viewport().close_requested()) && !self.should_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.set_window_visible(ctx, false);
        }

        if self.should_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        while let Ok(update) = self.frame_rx.try_recv() {
            match update {
                FrameUpdate::Preview(image) => {
                    let aspect_ratio = image.size[0] as f32 / image.size[1] as f32;
                    let aspect_changed = self
                        .preview_aspect_ratio
                        .is_none_or(|current| (current - aspect_ratio).abs() > 0.01);
                    if aspect_changed {
                        self.preview_aspect_ratio = Some(aspect_ratio);
                        self.sync_window_to_preview_aspect(ctx);
                    }
                    let texture = self.texture.get_or_insert_with(|| {
                        ctx.load_texture("camera-preview", image.clone(), TextureOptions::LINEAR)
                    });
                    texture.set(image, TextureOptions::LINEAR);
                    self.last_preview_update = Instant::now();
                }
                FrameUpdate::Scan(scan) => {
                    self.status = format!("Found network '{}'.", scan.credentials.ssid);
                    self.last_scan = Some(scan);
                    self.connect_in_flight = false;
                    self.connect_prompt_open = true;
                    self.set_window_visible(ctx, true);
                }
                FrameUpdate::Error(message) => {
                    self.status = message;
                }
            }
        }

        while let Ok(result) = self.connect_rx.try_recv() {
            self.connect_in_flight = false;
            self.status = match result.result {
                Ok(()) => format!("Connected to '{}'.", result.ssid),
                Err(err) => format!("Connection failed: {err:#}"),
            };
            self.connect_prompt_open = false;
        }

        #[cfg(target_os = "macos")]
        if let Some(tray) = &self.tray {
            tray.toggle_item.set_text(if self.window_visible {
                "Hide Window"
            } else {
                "Show Window"
            });
            for (index, item) in tray.camera_items.iter().enumerate() {
                item.set_checked(index == self.selected_camera);
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            let panel_rect = ui.max_rect();

            if let Some(texture) = &self.texture {
                let image_size = texture.size_vec2();
                let scale = (panel_rect.width() / image_size.x)
                    .min(panel_rect.height() / image_size.y);
                let scaled_size = image_size * scale;
                let image_rect = egui::Rect::from_center_size(panel_rect.center(), scaled_size);
                ui.put(
                    image_rect,
                    egui::Image::new((texture.id(), scaled_size))
                        .corner_radius(egui::CornerRadius::same(24)),
                );
            } else {
                ui.allocate_ui_at_rect(
                    panel_rect,
                    |ui| {
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgb(24, 24, 26))
                            .corner_radius(egui::CornerRadius::same(24))
                            .show(ui, |ui| {
                                ui.with_layout(
                                    egui::Layout::centered_and_justified(egui::Direction::TopDown),
                                    |ui| {
                                        ui.label(
                                            egui::RichText::new("Opening camera…")
                                                .size(24.0)
                                                .color(egui::Color32::from_gray(220)),
                                        );
                                    },
                                );
                            });
                    },
                );
            }

            if !self.status.starts_with("Point the camera")
                || (self.stop_tx.is_some()
                    && self.last_preview_update.elapsed() > Duration::from_secs(3)
                    && !self.cameras.is_empty())
            {
                let message = if self.stop_tx.is_some()
                    && self.last_preview_update.elapsed() > Duration::from_secs(3)
                    && !self.cameras.is_empty()
                {
                    "Camera preview stalled".to_owned()
                } else {
                    self.status.clone()
                };
                let overlay_rect = egui::Rect::from_min_size(
                    panel_rect.min + egui::vec2(20.0, 20.0),
                    egui::vec2((panel_rect.width() - 40.0).min(420.0), 52.0),
                );
                ui.allocate_ui_at_rect(overlay_rect, |ui| {
                    egui::Frame::new()
                        .fill(egui::Color32::from_black_alpha(170))
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_white_alpha(24)))
                        .corner_radius(egui::CornerRadius::same(18))
                        .inner_margin(egui::Margin::symmetric(16, 12))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(message)
                                    .color(egui::Color32::WHITE)
                                    .size(15.0),
                            );
                        });
                });
            }
        });

        if self.connect_prompt_open {
            if let Some(scan) = self.last_scan.clone() {
                let mut prompt_open = true;
                let window_response = egui::Window::new("Connect To Wi-Fi?")
                    .open(&mut prompt_open)
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                    .frame(
                        egui::Frame::window(&ctx.style())
                            .corner_radius(egui::CornerRadius::same(22))
                            .shadow(egui::epaint::Shadow {
                                offset: [0, 16],
                                blur: 32,
                                spread: 0,
                                color: egui::Color32::from_black_alpha(60),
                            })
                            .inner_margin(egui::Margin::same(20)),
                    )
                    .show(ctx, |ui| {
                        ui.set_width(320.0);
                        ui.label(
                            egui::RichText::new(&scan.credentials.ssid)
                                .size(20.0)
                                .color(egui::Color32::from_rgb(32, 32, 36)),
                        );
                        ui.add_space(6.0);
                        if scan.credentials.hidden {
                            ui.label(
                                egui::RichText::new("Hidden network")
                                    .size(14.0)
                                    .color(egui::Color32::from_gray(110)),
                            );
                        }
                        ui.add_space(14.0);
                        if self.connect_in_flight {
                            ui.horizontal(|ui| {
                                ui.add(egui::Spinner::new().size(18.0));
                                ui.label(
                                    egui::RichText::new("Connecting…")
                                        .size(14.0)
                                        .color(egui::Color32::from_gray(110)),
                                );
                            });
                            ui.add_space(12.0);
                        }
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 10.0;
                            if ui
                                .add_enabled(
                                    !self.connect_in_flight,
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
                            if ui
                                .add_enabled(
                                    !self.connect_in_flight,
                                    egui::Button::new("Connect"),
                                )
                                .clicked()
                            {
                                self.connect_in_flight = true;
                                self.status = format!("Connecting to '{}'…", scan.credentials.ssid);
                                let credentials = scan.credentials.clone();
                                let connect_tx = self.connect_tx.clone();
                                thread::spawn(move || {
                                    let ssid = credentials.ssid.clone();
                                    let result = connect_to_wifi(&credentials);
                                    let _ = connect_tx.send(ConnectResult { ssid, result });
                                });
                            }
                        });
                    });

                if prompt_open {
                    if let Some(window_response) = &window_response {
                        let outside_click = ctx.input(|input| {
                            input.pointer.any_pressed()
                                && input
                                    .pointer
                                    .press_origin()
                                    .is_some_and(|pos| !window_response.response.rect.contains(pos))
                        });
                        if outside_click {
                            prompt_open = false;
                        }
                    }
                }

                self.connect_prompt_open = prompt_open;
            } else {
                self.connect_prompt_open = false;
            }
        }

        if let Some(interval) = self.repaint_interval() {
            ctx.request_repaint_after(interval);
        }
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

struct ConnectResult {
    ssid: String,
    result: Result<()>,
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
        loop {
            let mut frame_image = crossbeam_channel::select! {
                recv(stop_rx) -> _ => break,
                recv(detect_rx) -> message => {
                    let Ok(frame_image) = message else {
                        break;
                    };
                    frame_image
                }
            };

            while let Ok(newer_frame) = detect_rx.try_recv() {
                frame_image = newer_frame;
            }

            if let Some(payload) = decode_qr_from_image_current(&frame_image) {
                match parse_wifi_qr(&payload) {
                    Ok(credentials) => {
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

#[cfg(target_os = "macos")]
struct MenuBarState {
    icon: TrayIcon,
    toggle_item: MenuItem,
    camera_items: Vec<CheckMenuItem>,
    quit_item: MenuItem,
}

#[cfg(target_os = "macos")]
impl MenuBarState {
    fn new(cameras: &[CameraInfo], selected_camera: usize) -> Result<Self> {
        let menu = Menu::new();
        let toggle_item = MenuItem::new("Hide Window", true, None);
        let camera_menu = Submenu::new("Camera", true);
        let mut camera_items = Vec::with_capacity(cameras.len());
        for (index, camera) in cameras.iter().enumerate() {
            let item = CheckMenuItem::new(camera.human_name(), true, index == selected_camera, None);
            camera_menu.append(&item)?;
            camera_items.push(item);
        }
        let quit_item = MenuItem::new("Quit", true, None);
        menu.append_items(&[&toggle_item, &camera_menu, &quit_item])?;

        let icon = TrayIconBuilder::new()
            .with_tooltip("WiFi QR Scanner")
            .with_icon(menu_bar_icon()?)
            .with_icon_as_template(true)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;

        Ok(Self {
            icon,
            toggle_item,
            camera_items,
            quit_item,
        })
    }
}

#[cfg(target_os = "macos")]
fn menu_bar_icon() -> Result<tray_icon::Icon> {
    const SIZE: u32 = 18;
    let mut rgba = vec![0_u8; (SIZE * SIZE * 4) as usize];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let idx = ((y * SIZE + x) * 4) as usize;
            let alpha = if camera_glyph_alpha(x as i32, y as i32, SIZE as i32) {
                255
            } else {
                0
            };
            rgba[idx] = 0;
            rgba[idx + 1] = 0;
            rgba[idx + 2] = 0;
            rgba[idx + 3] = alpha;
        }
    }

    tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).context("failed to build menu bar icon")
}

#[cfg(target_os = "macos")]
fn camera_glyph_alpha(x: i32, y: i32, size: i32) -> bool {
    let body = (3..=14).contains(&x) && (5..=13).contains(&y);
    let top = (6..=11).contains(&x) && (3..=5).contains(&y);
    let lens_dx = x - (size / 2);
    let lens_dy = y - 9;
    let lens = lens_dx * lens_dx + lens_dy * lens_dy <= 9;
    body || top || lens
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
