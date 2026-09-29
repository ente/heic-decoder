use std::sync::Arc;

use moxcms::{
    CicpColorPrimaries, CicpProfile, ColorProfile, Layout, MatrixCoefficients, ParsingOptions,
    TransferCharacteristics, Transform8BitExecutor,
};

use super::container::Reader;
use super::grid::Color;
use super::memory::{Budget, Reservation};
use super::{BoundedDecodeError as Error, Result};

pub(super) struct ColorTransform {
    executor: Option<Arc<Transform8BitExecutor>>,
    _reservation: Option<Reservation>,
}

impl ColorTransform {
    pub(super) fn new(
        color: &Color<'_>,
        bitstream_primaries: u8,
        bitstream_transfer: u8,
        budget: &Budget,
    ) -> Result<Self> {
        if matches!(bitstream_transfer, 16 | 18)
            || color
                .nclx
                .as_ref()
                .is_some_and(|n| matches!(n.transfer_characteristics, 16 | 18))
        {
            return Err(Error::Unsupported("HDR transfer function"));
        }
        let nclx = color.nclx.as_ref().filter(|n| !n.is_undefined());
        let srgb_or_unspecified = |primaries, transfer| {
            matches!(primaries, 1 | 2) && matches!(transfer, 1 | 2 | 6 | 13..=15)
        };
        if color.icc == Some(&[])
            && nclx.is_some_and(|n| {
                !srgb_or_unspecified(n.colour_primaries, n.transfer_characteristics)
            })
        {
            return Err(Error::Unsupported("empty ICC with non-sRGB nclx"));
        }
        if color.icc.is_none()
            && nclx.is_none()
            && !srgb_or_unspecified(
                u16::from(bitstream_primaries),
                u16::from(bitstream_transfer),
            )
        {
            return Err(Error::Unsupported("unprofiled HEVC color"));
        }
        if color.icc == Some(&[])
            || (color.icc.is_none()
                && color.nclx.as_ref().is_none_or(|n| {
                    srgb_or_unspecified(n.colour_primaries, n.transfer_characteristics)
                        || n.is_undefined()
                }))
        {
            return Ok(Self {
                executor: None,
                _reservation: None,
            });
        }
        let reservation = budget.reserve(2 * 1024 * 1024, "color transform")?;
        let source = if let Some(icc) = color.icc {
            matrix_profile(icc)?
        } else {
            let nclx = color
                .nclx
                .as_ref()
                .ok_or(Error::Unsupported("color profile"))?;
            if !matches!(nclx.colour_primaries, 1 | 9 | 12)
                || !matches!(nclx.transfer_characteristics, 1 | 6 | 13..=15)
            {
                return Err(Error::Unsupported("SDR nclx profile"));
            }
            ColorProfile::new_from_cicp(CicpProfile {
                color_primaries: CicpColorPrimaries::try_from(nclx.colour_primaries as u8)
                    .map_err(|_| Error::Unsupported("color primaries"))?,
                transfer_characteristics: TransferCharacteristics::try_from(13u8).unwrap(),
                matrix_coefficients: MatrixCoefficients::Identity,
                full_range: true,
            })
        };
        if !source.is_matrix_shaper() {
            return Err(Error::Unsupported("ICC matrix/TRC profile required"));
        }
        let executor = source
            .create_transform_8bit(
                Layout::Rgb,
                &ColorProfile::new_srgb(),
                Layout::Rgb,
                Default::default(),
            )
            .map_err(|_| Error::Unsupported("ICC transform"))?;
        Ok(Self {
            executor: Some(executor),
            _reservation: Some(reservation),
        })
    }

    pub(super) fn apply(&self, input: &[u8], output: &mut [u8]) -> Result<()> {
        if let Some(executor) = &self.executor {
            executor
                .transform(input, output)
                .map_err(|_| Error::Unsupported("ICC conversion failed"))?;
        } else {
            output.copy_from_slice(input);
        }
        Ok(())
    }

    pub(super) fn is_identity(&self) -> bool {
        self.executor.is_none()
    }
}

fn matrix_profile(icc: &[u8]) -> Result<ColorProfile> {
    if icc.len() < 132 || icc.len() > 1024 * 1024 {
        return Err(Error::LimitExceeded("ICC bytes"));
    }
    if &icc[16..20] != b"RGB " || &icc[20..24] != b"XYZ " || &icc[36..40] != b"acsp" {
        return Err(Error::Unsupported("RGB matrix ICC required"));
    }
    let mut r = Reader::new(icc);
    let declared = r.uint(4)? as usize;
    if declared != icc.len() {
        return Err(Error::Malformed("ICC size"));
    }
    r.pos = 128;
    let count = r.uint(4)? as usize;
    if count > 128 {
        return Err(Error::LimitExceeded("ICC tag count"));
    }
    let table_end = 132 + count * 12;
    let signatures = [
        *b"rXYZ", *b"gXYZ", *b"bXYZ", *b"rTRC", *b"gTRC", *b"bTRC", *b"wtpt", *b"chad", *b"cicp",
    ];
    let mut tags = [None; 9];
    for _ in 0..count {
        let signature: [u8; 4] = r.take(4)?.try_into().unwrap();
        let offset = r.uint(4)? as usize;
        let len = r.uint(4)? as usize;
        if offset < table_end || offset.checked_add(len).is_none_or(|end| end > icc.len()) {
            return Err(Error::Malformed("ICC tag range"));
        }
        let data = &icc[offset..offset + len];
        if signature.starts_with(b"A2B")
            || signature.starts_with(b"B2A")
            || signature.starts_with(b"D2B")
            || signature.starts_with(b"B2D")
            || signature == *b"kTRC"
        {
            return Err(Error::Unsupported("ICC lookup tables or grayscale"));
        }
        if let Some(index) = signatures.iter().position(|s| s == &signature) {
            if tags[index].replace(data).is_some() {
                return Err(Error::Malformed("duplicate ICC tag"));
            }
            let valid = match index {
                0..=2 | 6 => len == 20 && &data[..4] == b"XYZ ",
                3..=5 => {
                    if len < 12 {
                        false
                    } else {
                        let mut curve = Reader::new(data);
                        let kind = curve.take(4)?;
                        curve.take(4)?;
                        match kind {
                            b"curv" => {
                                let entries = curve.uint(4)? as usize;
                                entries <= 4096 && len == 12 + entries * 2
                            }
                            b"para" => {
                                let function = curve.uint(2)? as usize;
                                function < 5 && len == 12 + [1, 3, 4, 5, 7][function] * 4
                            }
                            _ => false,
                        }
                    }
                }
                7 => len == 44 && &data[..4] == b"sf32",
                8 => len == 12 && &data[..4] == b"cicp" && !matches!(data[9], 16 | 18),
                _ => false,
            };
            if !valid {
                return Err(Error::Unsupported("ICC matrix/TRC tag"));
            }
        }
    }
    if tags[..7].iter().any(Option::is_none) {
        return Err(Error::Malformed("missing ICC matrix/TRC tag"));
    }
    let count = tags.iter().flatten().count();
    let mut clean = [0u8; 32768];
    clean[..128].copy_from_slice(&icc[..128]);
    clean[128..132].copy_from_slice(&(count as u32).to_be_bytes());
    let mut entry = 132;
    let mut offset = 132 + count * 12;
    for (signature, data) in signatures.iter().zip(tags) {
        let Some(data) = data else {
            continue;
        };
        clean[entry..entry + 4].copy_from_slice(signature);
        clean[entry + 4..entry + 8].copy_from_slice(&(offset as u32).to_be_bytes());
        clean[entry + 8..entry + 12].copy_from_slice(&(data.len() as u32).to_be_bytes());
        clean[offset..offset + data.len()].copy_from_slice(data);
        offset = (offset + data.len() + 3) & !3;
        entry += 12;
    }
    clean[..4].copy_from_slice(&(offset as u32).to_be_bytes());
    ColorProfile::new_from_slice_with_options(
        &clean[..offset],
        ParsingOptions {
            max_profile_size: 32769,
            max_allowed_clut_size: 0,
            max_allowed_trc_size: 4096,
        },
    )
    .map_err(|_| Error::Malformed("ICC profile"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p3_profile() -> Vec<u8> {
        crate::nclx_to_icc_profile(&crate::isobmff::NclxColorProfile {
            colour_primaries: 12,
            transfer_characteristics: 13,
            matrix_coefficients: 1,
            full_range_flag: true,
        })
        .unwrap()
    }

    #[test]
    fn bounded_matrix_profile_matches_regular_color_conversion() {
        let icc = p3_profile();
        let color = Color {
            nclx: None,
            icc: Some(&icc),
        };
        let budget = Budget::new(4 * 1024 * 1024);
        let bounded = ColorTransform::new(&color, 2, 2, &budget).unwrap();
        let normal = ColorProfile::new_from_slice(&icc)
            .unwrap()
            .create_transform_8bit(
                Layout::Rgb,
                &ColorProfile::new_srgb(),
                Layout::Rgb,
                Default::default(),
            )
            .unwrap();
        let input: Vec<u8> = (0..256 * 3).map(|i| (i * 73) as u8).collect();
        let mut expected = vec![0; input.len()];
        let mut actual = vec![0; input.len()];
        bounded.apply(&input, &mut actual).unwrap();
        normal.transform(&input, &mut expected).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn rejects_hdr_gray_lut_and_unbounded_icc_tag_counts() {
        let original = p3_profile();
        let budget = Budget::new(4 * 1024 * 1024);
        assert!(matches!(
            ColorTransform::new(&Color::default(), 2, 16, &budget),
            Err(Error::Unsupported(_))
        ));
        let mut gray = original.clone();
        gray[16..20].copy_from_slice(b"GRAY");
        let mut lut = original.clone();
        lut[132..136].copy_from_slice(b"A2B0");
        let mut excessive = original;
        excessive[128..132].copy_from_slice(&u32::MAX.to_be_bytes());
        for icc in [gray, lut, excessive] {
            assert!(matches!(
                ColorTransform::new(
                    &Color {
                        nclx: None,
                        icc: Some(&icc)
                    },
                    2,
                    2,
                    &budget
                ),
                Err(Error::Unsupported(_) | Error::LimitExceeded(_))
            ));
        }
    }

    #[test]
    fn rejects_unprofiled_vui_color_and_preserves_container_precedence() {
        let budget = Budget::new(4 * 1024 * 1024);
        let undefined = crate::isobmff::NclxColorProfile {
            colour_primaries: 2,
            transfer_characteristics: 2,
            matrix_coefficients: 2,
            full_range_flag: true,
        };
        for icc in [None, Some(&[][..])] {
            for nclx in [None, Some(undefined.clone())] {
                let color = Color { icc, nclx };
                for (primaries, transfer) in [(9, 13), (12, 1), (1, 8), (2, 17)] {
                    let result = ColorTransform::new(&color, primaries, transfer, &budget);
                    if icc.is_some() {
                        assert!(result.unwrap().is_identity());
                    } else {
                        assert!(matches!(
                            result,
                            Err(Error::Unsupported("unprofiled HEVC color"))
                        ));
                    }
                }
                for primaries in [1, 2] {
                    for transfer in [1, 2, 6, 13, 14, 15] {
                        assert!(
                            ColorTransform::new(&color, primaries, transfer, &budget)
                                .unwrap()
                                .is_identity()
                        );
                    }
                }
            }
        }
        let nclx = crate::isobmff::NclxColorProfile {
            colour_primaries: 9,
            transfer_characteristics: 13,
            matrix_coefficients: 9,
            full_range_flag: false,
        };
        let mut color = Color {
            icc: None,
            nclx: Some(nclx),
        };
        assert!(
            !ColorTransform::new(&color, 9, 13, &budget)
                .unwrap()
                .is_identity()
        );
        color.icc = Some(&[]);
        assert!(matches!(
            ColorTransform::new(&color, 9, 13, &budget),
            Err(Error::Unsupported("empty ICC with non-sRGB nclx"))
        ));
        let icc = p3_profile();
        color.icc = Some(&icc);
        assert!(
            !ColorTransform::new(&color, 9, 13, &budget)
                .unwrap()
                .is_identity()
        );
    }

    #[test]
    fn nclx_conversion_matches_normal_icc_normalization() {
        let budget = Budget::new(4 * 1024 * 1024);
        let input: Vec<u8> = (0..4096 * 3).map(|i| (i * 73 + i / 256) as u8).collect();
        for (primaries, transfer) in [(9, 1), (9, 13), (12, 13)] {
            let nclx = crate::isobmff::NclxColorProfile {
                colour_primaries: primaries,
                transfer_characteristics: transfer,
                matrix_coefficients: 1,
                full_range_flag: true,
            };
            let icc = crate::nclx_to_icc_profile(&nclx).unwrap();
            let normal = ColorProfile::new_from_slice(&icc)
                .unwrap()
                .create_transform_8bit(
                    Layout::Rgb,
                    &ColorProfile::new_srgb(),
                    Layout::Rgb,
                    Default::default(),
                )
                .unwrap();
            let color = Color {
                nclx: Some(nclx),
                icc: None,
            };
            let bounded = ColorTransform::new(&color, 2, 2, &budget).unwrap();
            let mut actual = vec![0; input.len()];
            let mut expected = vec![0; input.len()];
            bounded.apply(&input, &mut actual).unwrap();
            normal.transform(&input, &mut expected).unwrap();
            assert!(
                actual
                    .iter()
                    .zip(&expected)
                    .all(|(a, b)| a.abs_diff(*b) <= 1)
            );
        }
    }

    #[test]
    fn partially_unspecified_nclx_matches_normal_srgb_policy() {
        let budget = Budget::new(4 * 1024 * 1024);
        for (primaries, transfer) in [(2, 13), (1, 2), (2, 2), (9, 2), (12, 2)] {
            let nclx = crate::isobmff::NclxColorProfile {
                colour_primaries: primaries,
                transfer_characteristics: transfer,
                matrix_coefficients: 6,
                full_range_flag: false,
            };
            assert!(crate::nclx_to_icc_profile(&nclx).is_none());
            let color = Color {
                nclx: Some(nclx),
                icc: None,
            };
            let transform = ColorTransform::new(&color, 2, 2, &budget);
            if matches!(primaries, 1 | 2) {
                let mut output = [0; 6];
                transform
                    .unwrap()
                    .apply(&[0, 57, 255, 130, 2, 91], &mut output)
                    .unwrap();
                assert_eq!(output, [0, 57, 255, 130, 2, 91]);
            } else {
                assert!(matches!(
                    transform,
                    Err(Error::Unsupported("SDR nclx profile"))
                ));
            }
        }
    }
}
