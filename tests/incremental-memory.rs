#[path = "../src/bin/support/allocation.rs"]
mod allocation;
use allocation::*;
use heic_decoder::{BoundedDecodeOptions, BoundedInput, decode_incremental_experiment};
use std::path::Path;
use std::sync::atomic::Ordering;

fn measure(
    input: BoundedInput<'_>,
    side: u32,
    budget: usize,
    request_limit: usize,
    expect_sao_edges: bool,
) -> (heic_decoder::BoundedRgbImage, usize) {
    let baseline = LIVE.load(Ordering::SeqCst);
    PEAK.store(baseline, Ordering::SeqCst);
    LARGEST.store(0, Ordering::SeqCst);
    DENIED.store(0, Ordering::SeqCst);
    MAX_REQUEST.store(request_limit, Ordering::SeqCst);
    MAX_LIVE.store(baseline + budget, Ordering::SeqCst);
    ACTIVE.store(true, Ordering::SeqCst);
    let result = decode_incremental_experiment(
        input,
        BoundedDecodeOptions {
            max_side: side,
            max_memory_bytes: budget,
        },
    );
    ACTIVE.store(false, Ordering::SeqCst);
    let peak = PEAK.load(Ordering::SeqCst) - baseline;
    assert_eq!(DENIED.load(Ordering::SeqCst), 0);
    assert!(peak <= budget);
    let (image, stats) = result.unwrap();
    assert!(stats[0] > 0);
    assert_eq!(stats[1] > 0, expect_sao_edges);
    (image, peak)
}

fn reference(path: &Path, width: u32, height: u32) -> Vec<u8> {
    let golden = path.with_extension("png");
    let image = {
        let decoder = png::Decoder::new(std::io::BufReader::new(
            std::fs::File::open(golden).unwrap(),
        ));
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(info.bit_depth, png::BitDepth::Eight);
        pixels.truncate(info.buffer_size());
        assert!(pixels.chunks_exact(4).all(|p| p[3] == 255));
        heic_decoder::DecodedRgbImage {
            width: info.width,
            height: info.height,
            pixels: pixels
                .chunks_exact(4)
                .flat_map(|p| p[..3].iter().copied())
                .collect(),
            source_bit_depth: 8,
            icc_profile: None,
        }
    };
    let mut output = vec![0; (width * height * 3) as usize];
    for oy in 0..height {
        let y0 = f64::from(oy) * f64::from(image.height) / f64::from(height);
        let y1 = f64::from(oy + 1) * f64::from(image.height) / f64::from(height);
        for ox in 0..width {
            let x0 = f64::from(ox) * f64::from(image.width) / f64::from(width);
            let x1 = f64::from(ox + 1) * f64::from(image.width) / f64::from(width);
            let mut sum = [0.0; 3];
            for sy in y0.floor() as u32..y1.ceil() as u32 {
                for sx in x0.floor() as u32..x1.ceil() as u32 {
                    let weight = (x1.min(f64::from(sx + 1)) - x0.max(f64::from(sx)))
                        * (y1.min(f64::from(sy + 1)) - y0.max(f64::from(sy)));
                    let i = ((sy * image.width + sx) * 3) as usize;
                    for (c, value) in sum.iter_mut().enumerate() {
                        *value += f64::from(image.pixels[i + c]) * weight;
                    }
                }
            }
            let i = ((oy * width + ox) * 3) as usize;
            for (c, value) in sum.iter().enumerate() {
                output[i + c] = (value / ((x1 - x0) * (y1 - y0))).round() as u8;
            }
        }
    }
    output
}

fn main() {
    if cfg!(feature = "decoder-tracing") {
        return;
    }
    let directory = std::env::var_os("INCREMENTAL_FIXTURES_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join(".heic-test-assets/incremental-corpus"),
        std::path::PathBuf::from,
    );
    assert!(
        directory.join("manifest.json").is_file(),
        "Generate the external corpus with scripts/incremental/generate.rb; see TESTING.md"
    );
    let mut scratch = Vec::new();
    let mut odd_scratch = Vec::new();
    for name in [
        "direct",
        "tall",
        "grid",
        "grid-pipeline",
        "undefined-grid-nclx",
        "crop",
        "oriented",
        "pipeline",
        "odd-short",
        "odd-tall",
        "odd-pipeline",
        "odd-width-grid",
        "odd-height-grid",
    ] {
        let path = directory.join(format!("{name}.heic"));
        let bytes = std::fs::read(&path).unwrap();
        let budget = if name.ends_with("pipeline") {
            8 * 1024 * 1024
        } else {
            4 * 1024 * 1024
        };
        let request_limit = if name.ends_with("pipeline") {
            512 * 1024
        } else {
            128 * 1024
        };
        let expect_sao_edges = name != "odd-height-grid";
        let before = THREADS.load(Ordering::SeqCst);
        let (file_image, path_peak) = measure(
            BoundedInput::Path(&path),
            65,
            budget,
            request_limit,
            expect_sao_edges,
        );
        let path_workers = THREADS.load(Ordering::SeqCst) - before;
        let before = THREADS.load(Ordering::SeqCst);
        let (byte_image, byte_peak) = measure(
            BoundedInput::Bytes(&bytes),
            65,
            budget,
            request_limit,
            expect_sao_edges,
        );
        let byte_workers = THREADS.load(Ordering::SeqCst) - before;
        assert_eq!(file_image.image.pixels, byte_image.image.pixels);
        assert!(path_peak.abs_diff(byte_peak) < 64 * 1024);
        let image = &file_image.image;
        let expected = reference(&path, image.width, image.height);
        assert!(
            image
                .pixels
                .iter()
                .zip(expected)
                .all(|(&a, b)| a.abs_diff(b) <= 1),
            "{name}: area-filter parity"
        );
        if name.ends_with("pipeline") {
            assert!(path_workers > 0);
            assert!(byte_workers > 0);
        }
        if name == "direct" || name == "tall" {
            scratch.push(path_peak - image.pixels.len());
        }
        if name == "odd-short" || name == "odd-tall" {
            odd_scratch.push(path_peak - image.pixels.len());
        }
        println!(
            "{name}: {}x{}, heap_peak={path_peak}, largest_request={}",
            image.width,
            image.height,
            LARGEST.load(Ordering::SeqCst)
        );
    }
    assert!(scratch[0].abs_diff(scratch[1]) < 16 * 1024);
    assert!(odd_scratch[0].abs_diff(odd_scratch[1]) < 16 * 1024);
    let path = directory.join("invalid-grid-references.bin");
    let bytes = std::fs::read(&path).unwrap();
    for input in [BoundedInput::Path(&path), BoundedInput::Bytes(&bytes)] {
        let budget = 256 * 1024;
        let baseline = LIVE.load(Ordering::SeqCst);
        PEAK.store(baseline, Ordering::SeqCst);
        LARGEST.store(0, Ordering::SeqCst);
        DENIED.store(0, Ordering::SeqCst);
        MAX_REQUEST.store(budget, Ordering::SeqCst);
        MAX_LIVE.store(baseline + budget, Ordering::SeqCst);
        ACTIVE.store(true, Ordering::SeqCst);
        let result = decode_incremental_experiment(
            input,
            BoundedDecodeOptions {
                max_side: 65,
                max_memory_bytes: budget,
            },
        );
        ACTIVE.store(false, Ordering::SeqCst);
        assert!(matches!(
            result,
            Err(heic_decoder::BoundedDecodeError::Malformed("tile count"))
        ));
        assert_eq!(DENIED.load(Ordering::SeqCst), 0);
        assert!(PEAK.load(Ordering::SeqCst) - baseline <= budget);
        println!(
            "invalid grid count: peak={}, largest_request={}",
            PEAK.load(Ordering::SeqCst) - baseline,
            LARGEST.load(Ordering::SeqCst)
        );
    }
    let path = directory.join("tall.heic");
    let baseline = LIVE.load(Ordering::SeqCst);
    PEAK.store(baseline, Ordering::SeqCst);
    LARGEST.store(0, Ordering::SeqCst);
    MAX_REQUEST.store(64 * 1024, Ordering::SeqCst);
    MAX_LIVE.store(baseline + 3 * 1024 * 1024, Ordering::SeqCst);
    ACTIVE.store(true, Ordering::SeqCst);
    let result = decode_incremental_experiment(
        BoundedInput::Path(&path),
        BoundedDecodeOptions {
            max_side: 6000,
            max_memory_bytes: 3 * 1024 * 1024,
        },
    );
    ACTIVE.store(false, Ordering::SeqCst);
    assert!(matches!(
        result,
        Err(heic_decoder::BoundedDecodeError::MemoryBudgetExceeded { .. })
    ));
    assert!(LARGEST.load(Ordering::SeqCst) < 65536);
    assert_eq!(DENIED.load(Ordering::SeqCst), 0);
    println!(
        "under-budget rejection: peak={}, largest_request={}",
        PEAK.load(Ordering::SeqCst) - baseline,
        LARGEST.load(Ordering::SeqCst)
    );
}
