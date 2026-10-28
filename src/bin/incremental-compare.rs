use std::error::Error;
use std::fs::File;
use std::io::BufReader;

struct Reference {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
    transformed: bool,
}

fn read_reference(path: &str) -> Result<Reference, Box<dyn Error>> {
    let mut decoder = png::Decoder::new(BufReader::new(File::open(path)?));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info()?;
    let icc = reader.info().icc_profile.as_ref().map(|p| p.to_vec());
    let mut bytes = vec![0; reader.output_buffer_size().ok_or("PNG size overflow")?];
    let info = reader.next_frame(&mut bytes)?;
    if info.bit_depth != png::BitDepth::Eight {
        return Err("Expected an 8-bit reference".into());
    }
    bytes.truncate(info.buffer_size());
    let pixels = match info.color_type {
        png::ColorType::Rgb => bytes,
        png::ColorType::Rgba => {
            if bytes.chunks_exact(4).any(|p| p[3] != 255) {
                return Err("Reference has nonopaque alpha".into());
            }
            bytes
                .chunks_exact(4)
                .flat_map(|p| p[..3].iter().copied())
                .collect()
        }
        _ => return Err("Expected RGB reference".into()),
    };
    let transformed = icc.is_some();
    let pixels = if let Some(icc) = icc {
        let profile = moxcms::ColorProfile::new_from_slice(&icc)?;
        let transform = profile.create_transform_8bit(
            moxcms::Layout::Rgb,
            &moxcms::ColorProfile::new_srgb(),
            moxcms::Layout::Rgb,
            Default::default(),
        )?;
        let mut output = vec![0; pixels.len()];
        transform.transform(&pixels, &mut output)?;
        output
    } else {
        pixels
    };
    Ok(Reference {
        width: info.width as usize,
        height: info.height as usize,
        pixels,
        transformed,
    })
}

fn area_reference(source: &[u8], sw: usize, sh: usize, w: usize, h: usize) -> Vec<u8> {
    if (sw, sh) == (w, h) {
        return source.to_vec();
    }
    let mut output = vec![0; w * h * 3];
    for y in 0..h {
        let y0 = y as f64 * sh as f64 / h as f64;
        let y1 = (y + 1) as f64 * sh as f64 / h as f64;
        for x in 0..w {
            let x0 = x as f64 * sw as f64 / w as f64;
            let x1 = (x + 1) as f64 * sw as f64 / w as f64;
            let mut sum = [0.0; 3];
            for sy in y0.floor() as usize..(y1.ceil() as usize).min(sh) {
                let wy = y1.min((sy + 1) as f64) - y0.max(sy as f64);
                for sx in x0.floor() as usize..(x1.ceil() as usize).min(sw) {
                    let weight = wy * (x1.min((sx + 1) as f64) - x0.max(sx as f64));
                    for c in 0..3 {
                        sum[c] += f64::from(source[(sy * sw + sx) * 3 + c]) * weight;
                    }
                }
            }
            for c in 0..3 {
                output[(y * w + x) * 3 + c] = (sum[c] / ((x1 - x0) * (y1 - y0))).round() as u8;
            }
        }
    }
    output
}

fn metrics(expected: &[u8], actual: &[u8], width: usize, channels: usize) -> String {
    assert_eq!(expected.len(), actual.len());
    let mut histogram = [0u64; 256];
    let mut signed = [0i64; 3];
    let mut square = 0u64;
    let mut pixel_count = 0u64;
    let mut alpha_errors = 0u64;
    let mut border_above_one = 0u64;
    let height = expected.len() / channels / width;
    let mut tile_sums =
        vec![0u64; width.div_ceil(16) * (expected.len() / channels / width).div_ceil(16)];
    let mut tile_counts = vec![0u64; tile_sums.len()];
    for (i, (a, b)) in expected
        .chunks_exact(channels)
        .zip(actual.chunks_exact(channels))
        .enumerate()
    {
        let tile = (i / width / 16) * width.div_ceil(16) + (i % width / 16);
        pixel_count += u64::from(a[..3] != b[..3]);
        if channels == 4 {
            alpha_errors += u64::from(a[3] != b[3]);
        }
        for c in 0..3 {
            let error = a[c].abs_diff(b[c]);
            if error > 1
                && (i % width == 0
                    || i % width == width - 1
                    || i / width == 0
                    || i / width == height - 1)
            {
                border_above_one += 1;
            }
            histogram[usize::from(error)] += 1;
            signed[c] += i64::from(b[c]) - i64::from(a[c]);
            square += u64::from(error).pow(2);
            tile_sums[tile] += u64::from(error);
            tile_counts[tile] += 1;
        }
    }
    let count: u64 = histogram.iter().sum();
    let sum: u64 = histogram
        .iter()
        .enumerate()
        .map(|(e, &n)| e as u64 * n)
        .sum();
    let max = histogram.iter().rposition(|&n| n > 0).unwrap_or(0);
    let p999 = histogram
        .iter()
        .scan(0u64, |n, &v| {
            *n += v;
            Some(*n)
        })
        .position(|n| n as f64 >= count as f64 * 0.999)
        .unwrap_or(0);
    let worst_tile_mae = tile_sums
        .iter()
        .zip(&tile_counts)
        .map(|(&s, &n)| s as f64 / n as f64)
        .fold(0.0, f64::max);
    let psnr = if square == 0 {
        "null".to_string()
    } else {
        format!(
            "{:.8}",
            10.0 * (255.0f64.powi(2) * count as f64 / square as f64).log10()
        )
    };
    let above_one: u64 = histogram[2..].iter().sum();
    let bias = signed.map(|s| s as f64 / (count / 3) as f64);
    format!(
        "{{\"max_error\":{max},\"mean_error\":{:.10},\"p999_error\":{p999},\"changed_pixels\":{pixel_count},\"above_one\":{above_one},\"border_above_one\":{border_above_one},\"psnr_db\":{psnr},\"worst_16x16_mae\":{worst_tile_mae:.10},\"channel_bias\":{bias:?},\"alpha_errors\":{alpha_errors},\"samples\":{count}}}",
        sum as f64 / count as f64
    )
}

#[derive(Clone, Copy)]
enum Profile {
    Exact,
    Rgb8Rounding,
}

impl Profile {
    fn parse(value: &str) -> Result<Self, Box<dyn Error>> {
        match value {
            "exact" => Ok(Self::Exact),
            "rgb8-rounding" => Ok(Self::Rgb8Rounding),
            _ => Err("Profile must be exact or rgb8-rounding".into()),
        }
    }

    fn accepts(self, expected: &[u8], actual: &[u8], channels: usize) -> bool {
        if expected.is_empty()
            || expected.len() != actual.len()
            || !matches!(channels, 3 | 4)
            || !expected.len().is_multiple_of(channels)
        {
            return false;
        }
        expected
            .iter()
            .zip(actual)
            .enumerate()
            .all(|(i, (&a, &b))| {
                let allowance = match self {
                    Self::Exact => 0,
                    Self::Rgb8Rounding => u8::from(i % channels < 3),
                };
                a.abs_diff(b) <= allowance
            })
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 7 {
        return Err("Usage: comparator raw EXPECTED ACTUAL WIDTH CHANNELS PROFILE | png REFERENCE ACTUAL WIDTH HEIGHT SIDE PROFILE".into());
    }
    if args[1] == "raw" {
        let expected = std::fs::read(&args[2])?;
        let actual = std::fs::read(&args[3])?;
        let width: usize = args[4].parse()?;
        let channels: usize = args[5].parse()?;
        let profile = Profile::parse(&args[6])?;
        if width == 0
            || !matches!(channels, 3 | 4)
            || expected.is_empty()
            || expected.len() != actual.len()
            || !expected.len().is_multiple_of(width * channels)
        {
            return Err("Raw buffer geometry differs".into());
        }
        println!("{}", metrics(&expected, &actual, width, channels));
        if !profile.accepts(&expected, &actual, channels) {
            return Err("Pixel comparison failed".into());
        }
    } else if args[1] == "png" && args.len() == 8 {
        let profile = Profile::parse(&args[7])?;
        let Reference {
            width: sw,
            height: sh,
            pixels: source,
            transformed,
        } = read_reference(&args[2])?;
        let actual = std::fs::read(&args[3])?;
        let width: usize = args[4].parse()?;
        let height: usize = args[5].parse()?;
        let side: usize = args[6].parse()?;
        let longest = sw.max(sh);
        let expected_dimensions = if longest <= side {
            (sw, sh)
        } else {
            ((sw * side / longest).max(1), (sh * side / longest).max(1))
        };
        if side == 0 || (width, height) != expected_dimensions || actual.len() != width * height * 3
        {
            return Err(format!(
                "Dimensions differ: reference={expected_dimensions:?}, actual={width}x{height}"
            )
            .into());
        }
        let expected = area_reference(&source, sw, sh, width, height);
        eprintln!("reference={sw}x{sh} output={width}x{height} icc_transformed={transformed}");
        println!("{}", metrics(&expected, &actual, width, 3));
        if !profile.accepts(&expected, &actual, 3) {
            return Err("Pixel comparison failed".into());
        }
    } else {
        return Err("Unknown comparison mode or argument count".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_and_local_corruption_remain_distinct() {
        let zero = vec![0; 32 * 32 * 3];
        let one = vec![1; zero.len()];
        let mut corruption = zero.clone();
        corruption[0] = 255;
        assert!(metrics(&zero, &one, 32, 3).contains("\"max_error\":1,"));
        let stats = metrics(&zero, &corruption, 32, 3);
        assert!(stats.contains("\"max_error\":255,"));
        assert!(stats.contains("\"above_one\":1,"));
        assert!(metrics(&[0, 0, 0, 255], &[0, 0, 0, 254], 1, 4).contains("\"alpha_errors\":1,"));
    }

    #[test]
    fn independent_area_average() {
        assert_eq!(
            area_reference(&[0, 0, 0, 100, 100, 100, 200, 200, 200], 3, 1, 2, 1),
            [33, 33, 33, 167, 167, 167]
        );
    }

    #[test]
    fn display_rounding_does_not_relax_reconstruction_or_alpha() {
        let expected = [100, 100, 100, 255];
        let rounded = [101, 99, 100, 255];
        assert!(Profile::Rgb8Rounding.accepts(&expected, &rounded, 4));
        assert!(!Profile::Exact.accepts(&expected, &rounded, 4));
        assert!(!Profile::Rgb8Rounding.accepts(&expected, &[100, 100, 100, 254], 4));
        assert!(!Profile::Rgb8Rounding.accepts(&expected, &rounded[..3], 4));
    }

    #[test]
    fn local_defects_cannot_hide_in_an_average() {
        let expected = vec![100; 64 * 64 * 3];
        let mut actual = expected.clone();
        actual[0] = 102;
        assert!(!Profile::Rgb8Rounding.accepts(&expected, &actual, 3));
        actual[..64 * 3].fill(102);
        assert!(!Profile::Rgb8Rounding.accepts(&expected, &actual, 3));
        let colors = [255, 0, 0, 0, 0, 255];
        assert!(!Profile::Rgb8Rounding.accepts(&colors, &[0, 0, 255, 255, 0, 0], 3));
    }
}
