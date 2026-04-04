use anyhow::{Context, Result};
use image::DynamicImage;
use zxingcpp::{BarcodeFormat, Binarizer};

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: cargo run --bin zxing_probe -- <image-path>")?;
    let image = image::open(&path).with_context(|| format!("failed to open {path}"))?;

    match decode_with_zxing(&image) {
        Some(text) => println!("{text}"),
        None => println!("no QR detected"),
    }

    Ok(())
}

fn decode_with_zxing(image: &DynamicImage) -> Option<String> {
    let reader = zxingcpp::read()
        .formats(&[BarcodeFormat::QRCode])
        .try_harder(true)
        .try_rotate(true)
        .try_invert(true)
        .try_downscale(true)
        .binarizer(Binarizer::LocalAverage)
        .max_number_of_symbols(1);

    let barcodes = reader.from(image).ok()?;
    barcodes
        .into_iter()
        .find(|barcode| barcode.is_valid() && barcode.format() == BarcodeFormat::QRCode)
        .map(|barcode| barcode.text())
}
