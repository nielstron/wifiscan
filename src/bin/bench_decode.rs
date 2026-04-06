use std::hint::black_box;
use std::time::Instant;

use anyhow::{Context, Result};
use image::DynamicImage;
use qrcode_generator::QrCodeEcc;
use wifiscan::decode::{
    decode_qr_from_image_cpu_legacy, decode_qr_from_image_current, decode_qr_from_image_parallel,
    detection_parallelism_budget,
};

fn main() -> Result<()> {
    println!("detector_parallelism_budget={}", detection_parallelism_budget());
    let real_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/Users/niels/Pictures/Photo on 04.04.2026 at 10.41.jpg".to_owned());

    let real_image =
        image::open(&real_path).with_context(|| format!("failed to open benchmark image {real_path}"))?;
    let clean_image = generated_qr_image("WIFI:T:WPA;S:BenchNet;P:swordfish;;")?;

    let cases = [
        ("generated-clean", clean_image),
        ("real-webcam-photo", real_image),
    ];

    println!(
        "{:<18} {:<14} {:<10} {:>8} {:>12} {:>12}",
        "case", "pipeline", "result", "iters", "total_ms", "avg_ms"
    );

    for (case_name, image) in cases {
        run_case(case_name, "legacy_cpu", &image, decode_qr_from_image_cpu_legacy);
        run_case(case_name, "parallel", &image, decode_qr_from_image_parallel);
        run_case(case_name, "current", &image, decode_qr_from_image_current);
    }

    Ok(())
}

fn generated_qr_image(payload: &str) -> Result<DynamicImage> {
    let png = qrcode_generator::to_png_to_vec(payload, QrCodeEcc::Medium, 512)
        .context("failed to generate benchmark QR")?;
    image::load_from_memory(&png).context("failed to decode generated benchmark QR")
}

fn run_case(
    case_name: &str,
    pipeline_name: &str,
    image: &DynamicImage,
    decode: fn(&DynamicImage) -> Option<String>,
) {
    let warmup = decode(image);
    let iterations = choose_iterations(case_name, pipeline_name);
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(decode(black_box(image)));
    }
    let elapsed = started.elapsed();
    let avg_ms = elapsed.as_secs_f64() * 1000.0 / iterations as f64;
    let result = if warmup.is_some() { "hit" } else { "miss" };

    println!(
        "{:<18} {:<14} {:<10} {:>8} {:>12.2} {:>12.3}",
        case_name,
        pipeline_name,
        result,
        iterations,
        elapsed.as_secs_f64() * 1000.0,
        avg_ms
    );
}

fn choose_iterations(case_name: &str, pipeline_name: &str) -> u32 {
    match (case_name, pipeline_name) {
        ("real-webcam-photo", "current") => 1,
        ("real-webcam-photo", _) => 1,
        _ => 100,
    }
}
