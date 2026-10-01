#[path = "support/allocation.rs"]
mod allocation;
use allocation::*;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if !(3..=6).contains(&args.len()) || !matches!(args[1].as_str(), "bounded" | "bytes" | "normal")
    {
        eprintln!(
            "Usage: incremental-allocation bounded|bytes|normal INPUT [MAX_SIDE] [MIB] [RGB_OUTPUT]"
        );
        std::process::exit(2);
    }
    let mode = args[1].as_str();
    let path = Path::new(&args[2]);
    let side = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(6000);
    let mib = args
        .get(4)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(128);
    if let Ok(value) = std::env::var("DENY_REQUESTS_ABOVE") {
        MAX_REQUEST.store(value.parse().unwrap(), Ordering::Relaxed);
    }
    let borrowed = if mode == "bytes" {
        Some(std::fs::read(path).unwrap())
    } else {
        None
    };
    let baseline = LIVE.load(Ordering::Relaxed);
    if !mode.starts_with("normal") {
        MAX_LIVE.store(baseline + mib * 1024 * 1024, Ordering::Relaxed);
    }
    PEAK.store(baseline, Ordering::Relaxed);
    ACTIVE.store(true, Ordering::Relaxed);
    let start = Instant::now();
    let result = if mode == "normal" {
        heic_decoder::decode_path_to_rgb8(path).map_err(|e| e.to_string())
    } else {
        let input = borrowed
            .as_ref()
            .map_or(heic_decoder::BoundedInput::Path(path), |bytes| {
                heic_decoder::BoundedInput::Bytes(bytes)
            });
        heic_decoder::decode_incremental_experiment(
            input,
            heic_decoder::BoundedDecodeOptions {
                max_side: side,
                max_memory_bytes: mib * 1024 * 1024,
            },
        )
        .map(|(image, stats)| {
            eprintln!(
                "decoded_rows={} sao_edge_components={} sao_band_components={}",
                stats[0], stats[1], stats[2]
            );
            image.image
        })
        .map_err(|e| e.to_string())
    };
    let elapsed = start.elapsed();
    ACTIVE.store(false, Ordering::Relaxed);
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    eprintln!(
        "denied_allocation_attempts={} borrowed_input_bytes={}",
        DENIED.load(Ordering::Relaxed),
        borrowed.as_ref().map_or(0, Vec::len)
    );
    let largest = LARGEST.load(Ordering::Relaxed);
    let workers = THREADS.load(Ordering::Relaxed).saturating_sub(1);
    match result {
        Ok(image) => {
            println!(
                "{{\"mode\":\"{mode}\",\"side\":{side},\"budget_mib\":{mib},\"width\":{},\"height\":{},\"milliseconds\":{:.3},\"peak_bytes\":{peak},\"largest_bytes\":{largest},\"allocating_workers\":{workers}}}",
                image.width,
                image.height,
                elapsed.as_secs_f64() * 1000.0
            );
            if !mode.starts_with("normal") {
                assert!(peak <= mib * 1024 * 1024);
            }
            if let Some(output) = args.get(5) {
                std::fs::write(output, image.pixels).unwrap();
            }
        }
        Err(error) => {
            println!(
                "{{\"error\":\"{error}\",\"milliseconds\":{:.3},\"peak_bytes\":{peak},\"largest_bytes\":{largest}}}",
                elapsed.as_secs_f64() * 1000.0
            );
            std::process::exit(1);
        }
    }
}
