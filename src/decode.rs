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
    if let Some(payload) = decode_with_vision(image) {
        return Some(payload);
    }

    decode_qr_from_image_cpu_legacy(image)
}

pub fn decode_qr_from_image_cpu_legacy(image: &DynamicImage) -> Option<String> {
    let grayscale = image.to_luma8();
    if let Some(payload) = decode_with_quircs(&grayscale) {
        return Some(payload);
    }

    for prepared in prepared_images_for_zxing(image) {
        if let Some(payload) = decode_with_zxing(&prepared) {
            return Some(payload);
        }
    }

    None
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

pub fn prepared_images_for_zxing(image: &DynamicImage) -> Vec<DynamicImage> {
    vec![
        image.clone(),
        image.brighten(20),
        image.adjust_contrast(20.0),
        image.adjust_contrast(35.0),
        image.resize(
            image.width().saturating_mul(2),
            image.height().saturating_mul(2),
            image::imageops::FilterType::CatmullRom,
        ),
    ]
}
