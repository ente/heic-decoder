use heic_decoder::image_integration::register_image_decoder_hooks;
use heic_decoder::{DecodedRgbaPixels, decode_path_to_rgb8, decode_path_to_rgba};
use image::{DynamicImage, ImageReader};
use std::env;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecodeMode {
    Direct,
    Rgb,
    Adapter,
    AdapterRgb,
    VerifyRgb,
}

#[derive(Debug)]
struct CliError(String);

impl Display for CliError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for CliError {}

fn usage() -> &'static str {
    "Usage: heif-image-adapter-bench <direct|rgb|adapter|adapter-rgb|verify-rgb> <input.heic|.heif|.avif>"
}

fn parse_args() -> Result<(DecodeMode, String), CliError> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        return Err(CliError(usage().to_string()));
    }

    let mode = match args[1].as_str() {
        "direct" => DecodeMode::Direct,
        "rgb" => DecodeMode::Rgb,
        "adapter" => DecodeMode::Adapter,
        "adapter-rgb" => DecodeMode::AdapterRgb,
        "verify-rgb" => DecodeMode::VerifyRgb,
        other => return Err(CliError(format!("Unsupported mode '{other}'. {}", usage()))),
    };

    Ok((mode, args[2].clone()))
}

fn small_checksum(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }

    let first = bytes[0] as u64;
    let middle = bytes[bytes.len() / 2] as u64;
    let last = bytes[bytes.len() - 1] as u64;
    (first << 16) ^ (middle << 8) ^ last ^ bytes.len() as u64
}

fn small_checksum_u16(samples: &[u16]) -> u64 {
    if samples.is_empty() {
        return 0;
    }

    let first = samples[0] as u64;
    let middle = samples[samples.len() / 2] as u64;
    let last = samples[samples.len() - 1] as u64;
    (first << 32) ^ (middle << 16) ^ last ^ samples.len() as u64
}

fn bench_direct(input_path: &Path) -> Result<u64, Box<dyn Error>> {
    let decoded = decode_path_to_rgba(input_path)?;
    let checksum = match &decoded.pixels {
        DecodedRgbaPixels::U8(samples) => small_checksum(samples),
        DecodedRgbaPixels::U16(samples) => small_checksum_u16(samples),
    };

    Ok(((decoded.width as u64) << 32) ^ (decoded.height as u64) ^ checksum)
}

fn bench_adapter(input_path: &Path) -> Result<u64, Box<dyn Error>> {
    let _ = register_image_decoder_hooks();

    let decoded = ImageReader::open(input_path)?.decode()?;
    let (width, height) = (decoded.width(), decoded.height());
    let checksum = match decoded {
        DynamicImage::ImageRgba8(buffer) => small_checksum(buffer.as_raw()),
        DynamicImage::ImageRgba16(buffer) => small_checksum_u16(buffer.as_raw()),
        other => {
            return Err(Box::new(CliError(format!(
                "Unsupported adapter output color type: {:?}",
                other.color()
            ))));
        }
    };

    Ok(((width as u64) << 32) ^ (height as u64) ^ checksum)
}

fn bench_rgb(input_path: &Path) -> Result<u64, Box<dyn Error>> {
    let decoded = decode_path_to_rgb8(input_path)?;
    Ok(((decoded.width as u64) << 32) ^ (decoded.height as u64) ^ small_checksum(&decoded.pixels))
}

fn decode_adapter_rgb(input_path: &Path) -> Result<image::RgbImage, Box<dyn Error>> {
    let _ = register_image_decoder_hooks();
    Ok(ImageReader::open(input_path)?.decode()?.into_rgb8())
}

fn bench_adapter_rgb(input_path: &Path) -> Result<u64, Box<dyn Error>> {
    let decoded = decode_adapter_rgb(input_path)?;
    Ok(((decoded.width() as u64) << 32)
        ^ (decoded.height() as u64)
        ^ small_checksum(decoded.as_raw()))
}

fn verify_rgb(input_path: &Path) -> Result<u64, Box<dyn Error>> {
    let direct = decode_path_to_rgb8(input_path)?;
    let adapter = decode_adapter_rgb(input_path)?;
    if direct.width != adapter.width()
        || direct.height != adapter.height()
        || direct.pixels != *adapter.as_raw()
    {
        return Err(Box::new(CliError(format!(
            "Direct RGB8 output does not match adapter RGB8 output for {}",
            input_path.display()
        ))));
    }

    Ok(((direct.width as u64) << 32) ^ (direct.height as u64) ^ small_checksum(&direct.pixels))
}

fn main() -> Result<(), Box<dyn Error>> {
    let (mode, input_path) = parse_args()?;
    let input_path = Path::new(&input_path);

    let checksum = match mode {
        DecodeMode::Direct => bench_direct(input_path)?,
        DecodeMode::Rgb => bench_rgb(input_path)?,
        DecodeMode::Adapter => bench_adapter(input_path)?,
        DecodeMode::AdapterRgb => bench_adapter_rgb(input_path)?,
        DecodeMode::VerifyRgb => verify_rgb(input_path)?,
    };

    println!("{checksum}");
    Ok(())
}
