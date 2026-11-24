use crate::{
    DecodeHeicError, DecodedHeicImage, HeicPixelLayout, HeicPlane, PreparedYcbcrToRgb,
    YCbCrMatrixCoefficients, YCbCrRange,
};

const DEFAULT_RGB_TO_YCBCR: [[f32; 3]; 3] = [
    [0.299, 0.587, 0.114],
    [-0.168_735, -0.331_264, 0.5],
    [0.5, -0.418_688, -0.081_312],
];

pub(crate) fn bilinear_chroma_neighbour(luma: usize, lowest: usize, highest: usize) -> usize {
    let chroma = luma / 2;
    if luma.is_multiple_of(2) {
        chroma.saturating_sub(1).max(lowest)
    } else {
        (chroma + 1).min(highest)
    }
}

pub(crate) fn bilinear_chroma_sample(
    nearest: u16,
    horizontal: u16,
    vertical: u16,
    diagonal: u16,
) -> u16 {
    ((9 * u32::from(nearest)
        + 3 * u32::from(horizontal)
        + 3 * u32::from(vertical)
        + u32::from(diagonal)
        + 8)
        >> 4) as u16
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeicRegion {
    pub(crate) left: u32,
    pub(crate) top: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

pub(crate) fn upsample_heic_to_yuv444(
    decoded: &DecodedHeicImage,
) -> Result<DecodedHeicImage, DecodeHeicError> {
    upsample_heic_region_to_yuv444(
        decoded,
        HeicRegion {
            left: 0,
            top: 0,
            width: decoded.width,
            height: decoded.height,
        },
    )
}

pub(crate) fn upsample_heic_region_to_yuv444(
    decoded: &DecodedHeicImage,
    region: HeicRegion,
) -> Result<DecodedHeicImage, DecodeHeicError> {
    let (subsample_x, subsample_y) = crate::heic_chroma_subsampling(decoded.layout);
    if subsample_x == 1 && subsample_y == 1 {
        return Err(DecodeHeicError::InvalidDecodedFrame {
            detail: format!(
                "bilinear chroma upsampling requires subsampled chroma, got {:?}",
                decoded.layout
            ),
        });
    }
    if region
        .left
        .checked_add(region.width)
        .is_none_or(|right| right > decoded.width)
        || region
            .top
            .checked_add(region.height)
            .is_none_or(|bottom| bottom > decoded.height)
    {
        return Err(DecodeHeicError::InvalidDecodedFrame {
            detail: format!(
                "{}x{} upsampling region at ({}, {}) exceeds {}x{} image",
                region.width, region.height, region.left, region.top, decoded.width, decoded.height
            ),
        });
    }
    crate::validate_heic_plane_dimensions(&decoded.y_plane, decoded.width, decoded.height, "Y")?;
    let (u_plane, v_plane, chroma_width, chroma_height) =
        crate::require_heic_chroma_planes(decoded)?;
    crate::validate_heic_plane_dimensions(u_plane, chroma_width, chroma_height, "U")?;
    crate::validate_heic_plane_dimensions(v_plane, chroma_width, chroma_height, "V")?;
    let y_plane = crate::extract_cropped_heic_plane(
        &decoded.y_plane.samples,
        decoded.y_plane.width as usize,
        region.left,
        region.top,
        region.width,
        region.height,
        "Y",
    )?;
    let upsampled_u = upsample_plane(u_plane, region, subsample_y, "U")?;
    let upsampled_v = upsample_plane(v_plane, region, subsample_y, "V")?;
    let mut upsampled = DecodedHeicImage {
        width: region.width,
        height: region.height,
        bit_depth_luma: decoded.bit_depth_luma,
        bit_depth_chroma: decoded.bit_depth_chroma,
        layout: HeicPixelLayout::Yuv444,
        ycbcr_range: decoded.ycbcr_range,
        ycbcr_matrix: with_libheif_srgb_defaults(decoded.ycbcr_matrix),
        y_plane,
        u_plane: Some(upsampled_u),
        v_plane: Some(upsampled_v),
    };
    if upsampled.ycbcr_range == YCbCrRange::Limited {
        requantize_yuv444_to_full_range(&mut upsampled)?;
    }
    Ok(upsampled)
}

fn upsample_plane(
    plane: &HeicPlane,
    region: HeicRegion,
    subsample_y: u32,
    label: &'static str,
) -> Result<HeicPlane, DecodeHeicError> {
    let chroma_width = plane.width as usize;
    let chroma_height = plane.height as usize;
    if plane.samples.len() != chroma_width * chroma_height {
        return Err(DecodeHeicError::InvalidDecodedFrame {
            detail: format!(
                "{label} plane has {} samples, expected {chroma_width}x{chroma_height}",
                plane.samples.len()
            ),
        });
    }
    let sample_count = crate::heic_sample_count(region.width, region.height, label)?;
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(sample_count)
        .map_err(|_| DecodeHeicError::AllocationFailed {
            stage: "bilinear chroma upsampling",
        })?;
    let (left, top) = (region.left as usize, region.top as usize);
    for y in top..top + region.height as usize {
        let (row, neighbour_row) = if subsample_y == 2 {
            (y / 2, bilinear_chroma_neighbour(y, 0, chroma_height - 1))
        } else {
            (y, y)
        };
        let current = &plane.samples[row * chroma_width..(row + 1) * chroma_width];
        let neighbour =
            &plane.samples[neighbour_row * chroma_width..(neighbour_row + 1) * chroma_width];
        for x in left..left + region.width as usize {
            let column = x / 2;
            let neighbour_column = bilinear_chroma_neighbour(x, 0, chroma_width - 1);
            samples.push(bilinear_chroma_sample(
                current[column],
                current[neighbour_column],
                neighbour[column],
                neighbour[neighbour_column],
            ));
        }
    }
    Ok(HeicPlane {
        width: region.width,
        height: region.height,
        samples,
    })
}

fn with_libheif_srgb_defaults(matrix: YCbCrMatrixCoefficients) -> YCbCrMatrixCoefficients {
    YCbCrMatrixCoefficients {
        matrix_coefficients: if matrix.matrix_coefficients == 2 {
            6
        } else {
            matrix.matrix_coefficients
        },
        colour_primaries: if matrix.colour_primaries == 2 {
            1
        } else {
            matrix.colour_primaries
        },
    }
}

fn requantize_yuv444_to_full_range(image: &mut DecodedHeicImage) -> Result<(), DecodeHeicError> {
    let bit_depth = crate::heic_bit_depth_for_png_conversion(image)?;
    let transform =
        crate::ycbcr_transform_from_matrix(image.ycbcr_matrix).map_err(|matrix_coefficients| {
            DecodeHeicError::UnsupportedMatrixCoefficients {
                matrix_coefficients,
            }
        })?;
    let to_rgb = PreparedYcbcrToRgb::new(bit_depth, YCbCrRange::Limited, transform, false);
    let to_ycbcr = RgbToFullRangeYcbcr::new(bit_depth, image.ycbcr_matrix);
    let DecodedHeicImage {
        y_plane,
        u_plane: Some(u_plane),
        v_plane: Some(v_plane),
        ..
    } = image
    else {
        return Err(DecodeHeicError::InvalidDecodedFrame {
            detail: "4:4:4 requantization requires chroma planes".to_string(),
        });
    };
    if y_plane.samples.len() != u_plane.samples.len()
        || y_plane.samples.len() != v_plane.samples.len()
    {
        return Err(DecodeHeicError::InvalidDecodedFrame {
            detail: format!(
                "4:4:4 planes have {}/{}/{} samples",
                y_plane.samples.len(),
                u_plane.samples.len(),
                v_plane.samples.len()
            ),
        });
    }
    for ((y, u), v) in y_plane
        .samples
        .iter_mut()
        .zip(u_plane.samples.iter_mut())
        .zip(v_plane.samples.iter_mut())
    {
        let (red, green, blue) = to_rgb.convert(i32::from(*y), i32::from(*u), i32::from(*v));
        (*y, *u, *v) = to_ycbcr.convert(red, green, blue);
    }
    image.ycbcr_range = YCbCrRange::Full;
    Ok(())
}

#[derive(Clone, Copy)]
enum RgbToFullRangeYcbcr {
    Identity,
    Matrix {
        coefficients: [[f32; 3]; 3],
        midpoint: f32,
        bit_depth: u8,
    },
}

impl RgbToFullRangeYcbcr {
    fn new(bit_depth: u8, matrix: YCbCrMatrixCoefficients) -> Self {
        if matrix.matrix_coefficients == 0 {
            return Self::Identity;
        }
        Self::Matrix {
            coefficients: rgb_to_ycbcr_coefficients(matrix),
            midpoint: f32::from(1_u16 << (bit_depth - 1)),
            bit_depth,
        }
    }

    fn convert(self, red: u16, green: u16, blue: u16) -> (u16, u16, u16) {
        match self {
            Self::Identity => (green, blue, red),
            Self::Matrix {
                coefficients,
                midpoint,
                bit_depth,
            } => {
                let (red, green, blue) = (f32::from(red), f32::from(green), f32::from(blue));
                let weigh = |row: [f32; 3]| {
                    crate::fmaf_parity(
                        blue,
                        row[2],
                        crate::fmaf_parity(red, row[0], green * row[1]),
                    )
                };
                (
                    crate::clip_f32_to_bit_depth(weigh(coefficients[0]), bit_depth),
                    crate::clip_f32_to_bit_depth(weigh(coefficients[1]) + midpoint, bit_depth),
                    crate::clip_f32_to_bit_depth(weigh(coefficients[2]) + midpoint, bit_depth),
                )
            }
        }
    }
}

fn rgb_to_ycbcr_coefficients(matrix: YCbCrMatrixCoefficients) -> [[f32; 3]; 3] {
    match crate::kr_kb_from_matrix(matrix.matrix_coefficients, matrix.colour_primaries) {
        Some((kr, kb)) if kr != 0.0 || kb != 0.0 => {
            let kg = 1.0 - kr - kb;
            [
                [kr, kg, kb],
                [-kr / (1.0 - kb) / 2.0, -kg / (1.0 - kb) / 2.0, 0.5],
                [0.5, -kg / (1.0 - kr) / 2.0, -kb / (1.0 - kr) / 2.0],
            ]
        }
        _ => DEFAULT_RGB_TO_YCBCR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(
        layout: HeicPixelLayout,
        width: u32,
        height: u32,
        chroma: impl Fn(u32, u32) -> u16,
        range: YCbCrRange,
    ) -> DecodedHeicImage {
        let (chroma_width, chroma_height) = crate::heic_chroma_dimensions(width, height, layout);
        let plane = |offset: u16| HeicPlane {
            width: chroma_width,
            height: chroma_height,
            samples: (0..chroma_height)
                .flat_map(|y| (0..chroma_width).map(move |x| (x, y)))
                .map(|(x, y)| chroma(x, y) + offset)
                .collect(),
        };
        DecodedHeicImage {
            width,
            height,
            bit_depth_luma: 8,
            bit_depth_chroma: 8,
            layout,
            ycbcr_range: range,
            ycbcr_matrix: YCbCrMatrixCoefficients {
                matrix_coefficients: 2,
                colour_primaries: 2,
            },
            y_plane: HeicPlane {
                width,
                height,
                samples: (0..width * height)
                    .map(|i| 16 + (i * 7 % 200) as u16)
                    .collect(),
            },
            u_plane: Some(plane(0)),
            v_plane: Some(plane(3)),
        }
    }

    fn naive_bilinear(plane: &HeicPlane, x: u32, y: u32, subsample_y: u32) -> u16 {
        let at = |cx: i64, cy: i64| {
            let cx = cx.clamp(0, i64::from(plane.width) - 1) as u32;
            let cy = cy.clamp(0, i64::from(plane.height) - 1) as u32;
            f64::from(plane.samples[(cy * plane.width + cx) as usize])
        };
        let position_x = (f64::from(x) - 0.5) / 2.0;
        let position_y = if subsample_y == 2 {
            (f64::from(y) - 0.5) / 2.0
        } else {
            f64::from(y)
        };
        let (left, top) = (position_x.floor(), position_y.floor());
        let (fx, fy) = (position_x - left, position_y - top);
        let (left, top) = (left as i64, top as i64);
        let value = at(left, top) * (1.0 - fx) * (1.0 - fy)
            + at(left + 1, top) * fx * (1.0 - fy)
            + at(left, top + 1) * (1.0 - fx) * fy
            + at(left + 1, top + 1) * fx * fy;
        (value + 0.5).floor() as u16
    }

    #[test]
    fn upsampling_matches_clamped_quarter_phase_bilinear_interpolation() {
        for (layout, subsample_y) in [(HeicPixelLayout::Yuv420, 2), (HeicPixelLayout::Yuv422, 1)] {
            for (width, height) in [(6, 4), (7, 5), (1, 1), (2, 3)] {
                let source = image(
                    layout,
                    width,
                    height,
                    |x, y| (x * 37 + y * 91) as u16 % 240,
                    YCbCrRange::Full,
                );
                let upsampled = upsample_heic_to_yuv444(&source).unwrap();
                assert_eq!(upsampled.layout, HeicPixelLayout::Yuv444);
                assert_eq!(upsampled.y_plane, source.y_plane);
                for (original, converted) in [
                    (&source.u_plane, &upsampled.u_plane),
                    (&source.v_plane, &upsampled.v_plane),
                ] {
                    let original = original.as_ref().unwrap();
                    let converted = converted.as_ref().unwrap();
                    for y in 0..height {
                        for x in 0..width {
                            assert_eq!(
                                converted.samples[(y * width + x) as usize],
                                naive_bilinear(original, x, y, subsample_y),
                                "{layout:?} {width}x{height} at ({x},{y})"
                            );
                        }
                    }
                }
            }
        }
    }

    fn libheif_420_kernel(input: &[u16], width: usize, height: usize) -> Vec<u16> {
        let stride = width.div_ceil(2);
        let mut out = vec![0_u16; width * height];
        let mix = |a: u16, b: u16| ((3 * u32::from(a) + u32::from(b) + 2) / 4) as u16;
        let upsample_row = |out: &mut [u16], input_row: usize, output_row: usize| {
            let row = &input[input_row * stride..];
            out[output_row * width] = row[0];
            for cx in 0..(width - 1) / 2 {
                out[output_row * width + 2 * cx + 1] = mix(row[cx], row[cx + 1]);
                out[output_row * width + 2 * cx + 2] = mix(row[cx + 1], row[cx]);
            }
            if width.is_multiple_of(2) {
                out[output_row * width + width - 1] = row[width / 2 - 1];
            }
        };
        let upsample_column = |out: &mut [u16], input_column: usize, output_column: usize| {
            for cy in 0..(height - 1) / 2 {
                let a = input[cy * stride + input_column];
                let b = input[(cy + 1) * stride + input_column];
                out[(2 * cy + 1) * width + output_column] = mix(a, b);
                out[(2 * cy + 2) * width + output_column] = mix(b, a);
            }
        };
        upsample_row(&mut out, 0, 0);
        upsample_column(&mut out, 0, 0);
        if width.is_multiple_of(2) {
            upsample_column(&mut out, width / 2 - 1, width - 1);
        }
        if height.is_multiple_of(2) {
            upsample_row(&mut out, height / 2 - 1, height - 1);
        }
        for y in (1..height.saturating_sub(1)).step_by(2) {
            for x in (1..width.saturating_sub(1)).step_by(2) {
                let (cx, cy) = (x / 2, y / 2);
                let c00 = u32::from(input[cy * stride + cx]);
                let c01 = u32::from(input[cy * stride + cx + 1]);
                let c10 = u32::from(input[(cy + 1) * stride + cx]);
                let c11 = u32::from(input[(cy + 1) * stride + cx + 1]);
                out[y * width + x] = ((c00 * 9 + c01 * 3 + c10 * 3 + c11 + 8) / 16) as u16;
                out[y * width + x + 1] = ((c00 * 3 + c01 * 9 + c10 + c11 * 3 + 8) / 16) as u16;
                out[(y + 1) * width + x] = ((c00 * 3 + c01 + c10 * 9 + c11 * 3 + 8) / 16) as u16;
                out[(y + 1) * width + x + 1] =
                    ((c00 + c01 * 3 + c10 * 3 + c11 * 9 + 8) / 16) as u16;
            }
        }
        out
    }

    #[test]
    fn upsampling_matches_libheif_border_fixed_420_kernel() {
        for (width, height) in [(4, 4), (6, 4), (7, 5), (5, 8), (2, 2), (3, 3)] {
            let source = image(
                HeicPixelLayout::Yuv420,
                width,
                height,
                |x, y| ((x * 113 + y * 59) % 256) as u16,
                YCbCrRange::Full,
            );
            let original = source.u_plane.clone().unwrap();
            let upsampled = upsample_heic_to_yuv444(&source).unwrap();
            assert_eq!(
                upsampled.u_plane.unwrap().samples,
                libheif_420_kernel(&original.samples, width as usize, height as usize),
                "{width}x{height}"
            );
        }
    }

    #[test]
    fn full_range_upsampling_applies_libheif_matrix_defaults_only() {
        let source = image(
            HeicPixelLayout::Yuv420,
            5,
            3,
            |x, y| (x * 50 + y * 20) as u16,
            YCbCrRange::Full,
        );
        let upsampled = upsample_heic_to_yuv444(&source).unwrap();
        assert_eq!(upsampled.ycbcr_range, YCbCrRange::Full);
        assert_eq!(
            upsampled.ycbcr_matrix,
            YCbCrMatrixCoefficients {
                matrix_coefficients: 6,
                colour_primaries: 1,
            }
        );
        assert_eq!(upsampled.y_plane, source.y_plane);
    }

    #[test]
    fn limited_range_upsampling_requantizes_through_rgb_to_full_range() {
        let source = image(
            HeicPixelLayout::Yuv420,
            4,
            2,
            |x, _| [16, 128, 240, 200][x as usize % 4],
            YCbCrRange::Limited,
        );
        let mut expected = upsample_heic_to_yuv444(&DecodedHeicImage {
            ycbcr_range: YCbCrRange::Full,
            ..source.clone()
        })
        .unwrap();
        let transform = crate::ycbcr_transform_from_matrix(expected.ycbcr_matrix).unwrap();
        let to_rgb = PreparedYcbcrToRgb::new(8, YCbCrRange::Limited, transform, false);
        let to_ycbcr = RgbToFullRangeYcbcr::new(8, expected.ycbcr_matrix);
        for index in 0..expected.y_plane.samples.len() {
            let (red, green, blue) = to_rgb.convert(
                i32::from(expected.y_plane.samples[index]),
                i32::from(expected.u_plane.as_ref().unwrap().samples[index]),
                i32::from(expected.v_plane.as_ref().unwrap().samples[index]),
            );
            let (y, u, v) = to_ycbcr.convert(red, green, blue);
            expected.y_plane.samples[index] = y;
            expected.u_plane.as_mut().unwrap().samples[index] = u;
            expected.v_plane.as_mut().unwrap().samples[index] = v;
        }

        let upsampled = upsample_heic_to_yuv444(&source).unwrap();
        assert_eq!(upsampled, expected);
        assert_ne!(upsampled.y_plane, source.y_plane);
    }

    #[test]
    fn region_upsampling_matches_cropping_the_whole_upsampled_image() {
        for layout in [HeicPixelLayout::Yuv420, HeicPixelLayout::Yuv422] {
            for range in [YCbCrRange::Full, YCbCrRange::Limited] {
                let source = image(layout, 9, 7, |x, y| ((x * 41 + y * 23) % 220) as u16, range);
                let whole = upsample_heic_to_yuv444(&source).unwrap();
                for (left, top, width, height) in [
                    (0, 0, 9, 7),
                    (1, 1, 7, 5),
                    (3, 2, 1, 1),
                    (8, 6, 1, 1),
                    (0, 3, 9, 2),
                ] {
                    let region = HeicRegion {
                        left,
                        top,
                        width,
                        height,
                    };
                    let crop = |plane: &HeicPlane| HeicPlane {
                        width,
                        height,
                        samples: (top..top + height)
                            .flat_map(|y| {
                                (left..left + width)
                                    .map(move |x| plane.samples[(y * plane.width + x) as usize])
                            })
                            .collect(),
                    };
                    let expected = DecodedHeicImage {
                        width,
                        height,
                        y_plane: crop(&whole.y_plane),
                        u_plane: whole.u_plane.as_ref().map(crop),
                        v_plane: whole.v_plane.as_ref().map(crop),
                        ..whole.clone()
                    };
                    assert_eq!(
                        upsample_heic_region_to_yuv444(&source, region).unwrap(),
                        expected,
                        "{layout:?} {range:?} {region:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn region_upsampling_rejects_regions_outside_the_image() {
        let source = image(HeicPixelLayout::Yuv420, 4, 4, |_, _| 128, YCbCrRange::Full);
        for (left, top, width, height) in [(1, 0, 4, 4), (0, 1, 4, 4), (u32::MAX, 0, 2, 1)] {
            let region = HeicRegion {
                left,
                top,
                width,
                height,
            };
            assert!(upsample_heic_region_to_yuv444(&source, region).is_err());
        }
    }

    #[test]
    fn rgb_to_full_range_ycbcr_round_trips_primaries_and_grey() {
        let converter = RgbToFullRangeYcbcr::new(
            8,
            YCbCrMatrixCoefficients {
                matrix_coefficients: 6,
                colour_primaries: 1,
            },
        );
        assert_eq!(converter.convert(0, 0, 0), (0, 128, 128));
        assert_eq!(converter.convert(255, 255, 255), (255, 128, 128));
        assert_eq!(converter.convert(255, 0, 0), (76, 85, 255));
        assert_eq!(converter.convert(0, 0, 255), (29, 255, 107));
        let identity = RgbToFullRangeYcbcr::new(
            8,
            YCbCrMatrixCoefficients {
                matrix_coefficients: 0,
                colour_primaries: 2,
            },
        );
        assert_eq!(identity.convert(10, 20, 30), (20, 30, 10));
    }
}
