use std::sync::mpsc;
use std::thread;
use std::io::Cursor;

use anyhow::{Context, Result};
use image::{DynamicImage, GrayImage};
use zxingcpp::{BarcodeFormat, Binarizer};

#[cfg(target_os = "macos")]
use objc2::AnyThread;
#[cfg(target_os = "macos")]
use objc2_foundation::{NSArray, NSData, NSDictionary};
#[cfg(target_os = "macos")]
use objc2_vision::{
    VNBarcodeSymbologyQR, VNDetectBarcodesRequest, VNImageRequestHandler, VNRequest,
};

pub fn decode_qr_from_path(path: &str) -> Result<String> {
    let image = image::open(path).with_context(|| format!("failed to open image at {path}"))?;
    decode_qr_from_image_current(&image).context("no QR code detected in image")
}

pub fn decode_qr_from_image_current(image: &DynamicImage) -> Option<String> {
    decode_qr_from_image_parallel(image)
}

pub fn decode_qr_from_image_parallel(image: &DynamicImage) -> Option<String> {
    let detector_count = 3;
    let (tx, rx) = mpsc::channel();
    thread::scope(|scope| {
        let tx_vision = tx.clone();
        scope.spawn(move || {
            let _ = tx_vision.send(decode_with_vision(image));
        });

        let tx_quircs = tx.clone();
        scope.spawn(move || {
            let _ = tx_quircs.send(decode_with_quircs_image(image));
        });

        let tx_zxing = tx;
        scope.spawn(move || {
            let _ = tx_zxing.send(decode_with_zxing(image));
        });

        for _ in 0..detector_count {
            let Ok(result) = rx.recv() else {
                break;
            };
            if let Some(payload) = result {
                return Some(payload);
            }
        }

        None
    })
}

pub fn decode_qr_from_image_cpu_legacy(image: &DynamicImage) -> Option<String> {
    if let Some(payload) = decode_with_quircs_image(image) {
        return Some(payload);
    }

    decode_with_zxing(image)
}

pub fn detection_parallelism_budget() -> usize {
    let available = thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    let fraction = std::env::var("WIFISCAN_DETECTOR_CORE_FRACTION")
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(0.5)
        .clamp(0.1, 1.0);
    ((available as f32 * fraction).ceil() as usize).max(1)
}

pub fn decode_with_quircs_image(image: &DynamicImage) -> Option<String> {
    let gray = image.to_luma8();
    decode_with_quircs(&gray)
}

pub fn decode_with_quircs(image: &GrayImage) -> Option<String> {
    let mut decoder = quircs::Quirc::default();
    let codes = decoder.identify(image.width() as usize, image.height() as usize, image.as_raw());

    for code in codes {
        let Ok(code) = code else {
            continue;
        };
        let Ok(decoded) = code.decode() else {
            continue;
        };
        if let Ok(payload) = String::from_utf8(decoded.payload) {
            return Some(payload);
        }
    }

    None
}

#[cfg(target_os = "macos")]
pub fn decode_with_vision(image: &DynamicImage) -> Option<String> {
    let mut png_bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        .ok()?;

    let image_data = NSData::from_vec(png_bytes);
    let options = NSDictionary::<objc2_foundation::NSString, objc2::runtime::AnyObject>::new();
    let request = unsafe { VNDetectBarcodesRequest::init(VNDetectBarcodesRequest::alloc()) };
    let qr_symbology = unsafe { VNBarcodeSymbologyQR }?;
    let symbologies = NSArray::from_slice(&[qr_symbology]);
    unsafe {
        request.setSymbologies(&symbologies);
    }

    let requests = NSArray::from_slice(&[request.as_ref() as &VNRequest]);
    let handler =
        VNImageRequestHandler::initWithData_options(VNImageRequestHandler::alloc(), &image_data, &options);
    handler.performRequests_error(&requests).ok()?;

    let results = unsafe { request.results() }?;
    for observation in results.iter() {
        let payload = unsafe { observation.payloadStringValue() }?;
        let value = payload.to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }

    None
}

#[cfg(not(target_os = "macos"))]
pub fn decode_with_vision(_image: &DynamicImage) -> Option<String> {
    None
}

pub fn decode_with_zxing(image: &DynamicImage) -> Option<String> {
    for is_pure in [false, true] {
        for binarizer in [
            Binarizer::LocalAverage,
            Binarizer::GlobalHistogram,
            Binarizer::FixedThreshold,
            Binarizer::BoolCast,
        ] {
            let reader = zxingcpp::read()
                .formats(&[BarcodeFormat::QRCode])
                .try_harder(true)
                .try_rotate(true)
                .try_invert(true)
                .try_downscale(true)
                .is_pure(is_pure)
                .return_errors(true)
                .binarizer(binarizer)
                .max_number_of_symbols(1);

            let Ok(barcodes) = reader.from(image) else {
                continue;
            };
            if let Some(payload) = barcodes
                .into_iter()
                .find(|barcode| barcode.is_valid() && barcode.format() == BarcodeFormat::QRCode)
                .map(|barcode| barcode.text())
            {
                return Some(payload);
            }
        }
    }

    None
}
