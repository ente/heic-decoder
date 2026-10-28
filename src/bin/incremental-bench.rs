use heic_decoder::{BoundedDecodeOptions, BoundedInput};
use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if !(3..=4).contains(&args.len()) {
        return Err("Usage: incremental-bench bounded|normal INPUT [MAX_SIDE]".into());
    }
    let mode = &args[1];
    let input = Path::new(&args[2]);
    let max_side = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(6000);
    let start = Instant::now();
    let image = match mode.as_str() {
        "bounded" => {
            heic_decoder::decode_incremental_experiment(
                BoundedInput::Path(input),
                BoundedDecodeOptions {
                    max_side,
                    ..Default::default()
                },
            )?
            .0
            .image
        }
        "normal" => heic_decoder::decode_path_to_rgb8(input)?,
        _ => return Err("Mode must be bounded or normal".into()),
    };
    black_box(&image.pixels);
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{{\"mode\":\"{mode}\",\"width\":{},\"height\":{},\"milliseconds\":{elapsed:.6}}}",
        image.width, image.height
    );
    Ok(())
}
