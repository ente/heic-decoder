use std::sync::Arc;

use moxcms::{
    CicpColorPrimaries, CicpProfile, ColorProfile, Layout, MatrixCoefficients, ParsingOptions,
    TransferCharacteristics, Transform8BitExecutor,
};

use super::container::Reader;
use super::grid::Color;
use super::memory::{Budget, Buffer, Reservation};
use super::{BoundedDecodeError as Error, Result};

const REFERENCE_WHITE: f64 = 203.0;
const HLG_PEAK: f64 = 1000.0;
const DEFAULT_PQ_PEAK: f64 = 1000.0;
const CURVE_SAMPLES: usize = 4096;
const OOTF_SAMPLES: usize = 65536;
const ENCODE_SAMPLES: usize = 65536;

const BT2020_TO_BT709: [[f32; 3]; 3] = [
    [1.660491, -0.587641, -0.07285],
    [-0.12455, 1.1329, -0.008349],
    [-0.018151, -0.100579, 1.11873],
];
const P3_TO_BT709: [[f32; 3]; 3] = [
    [1.22494, -0.22494, 0.0],
    [-0.042057, 1.042057, 0.0],
    [-0.019638, -0.078636, 1.098274],
];
const BT709_TO_BT709: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

pub(super) struct ColorTransform {
    stage: Stage,
    _reservation: Option<Reservation>,
}

enum Stage {
    Identity,
    Icc(Arc<Transform8BitExecutor>),
    ToneMap(ToneMap),
}

impl ColorTransform {
    pub(super) fn new(
        color: &Color<'_>,
        bitstream_primaries: u8,
        bitstream_transfer: u8,
        bit_depth: u8,
        budget: &Budget,
    ) -> Result<Self> {
        let nclx = color.nclx.as_ref().filter(|n| !n.is_undefined());
        let declared = match (color.icc, nclx) {
            (Some(icc), _) => icc_cicp(icc),
            (None, Some(n)) => Some((n.colour_primaries, n.transfer_characteristics)),
            (None, None) => Some((bitstream_primaries.into(), bitstream_transfer.into())),
        };
        let hdr = |transfer: u16| matches!(transfer, 16 | 18);
        if let Some((primaries, transfer)) = declared.filter(|&(_, transfer)| hdr(transfer)) {
            let tone_map = ToneMap::new(
                primaries,
                transfer,
                bit_depth,
                color.peak_luminance(),
                budget,
            )?;
            return Ok(Self {
                stage: Stage::ToneMap(tone_map),
                _reservation: None,
            });
        }
        if hdr(bitstream_transfer.into()) || nclx.is_some_and(|n| hdr(n.transfer_characteristics)) {
            return Err(Error::Unsupported("HDR transfer function"));
        }
        let srgb_transfer = |transfer: u16| matches!(transfer, 1 | 2 | 6 | 13..=15);
        let srgb_or_unspecified =
            |primaries, transfer| matches!(primaries, 1 | 2) && srgb_transfer(transfer);
        if color.icc == Some(&[])
            && nclx.is_some_and(|n| {
                !srgb_or_unspecified(n.colour_primaries, n.transfer_characteristics)
            })
        {
            return Err(Error::Unsupported("empty ICC with non-sRGB nclx"));
        }
        if color.icc.is_none()
            && nclx.is_none()
            && !(matches!(bitstream_primaries, 1 | 2 | 5 | 6)
                && srgb_transfer(u16::from(bitstream_transfer)))
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
                stage: Stage::Identity,
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
            stage: Stage::Icc(executor),
            _reservation: Some(reservation),
        })
    }

    pub(super) fn apply(&self, input: &[u8], output: &mut [u8]) -> Result<()> {
        match &self.stage {
            Stage::Identity => output.copy_from_slice(input),
            Stage::Icc(executor) => executor
                .transform(input, output)
                .map_err(|_| Error::Unsupported("ICC conversion failed"))?,
            Stage::ToneMap(_) => return Err(Error::Unsupported("8-bit RGB for HDR color")),
        }
        Ok(())
    }

    pub(super) fn is_identity(&self) -> bool {
        matches!(self.stage, Stage::Identity)
    }

    pub(super) fn tone_map(&self) -> Option<&ToneMap> {
        match &self.stage {
            Stage::ToneMap(tone_map) => Some(tone_map),
            _ => None,
        }
    }
}

pub(super) struct ToneMap {
    signal: Buffer<f32>,
    ootf: Option<([f32; 3], Buffer<f32>)>,
    gamut: [[f32; 3]; 3],
    curve: Buffer<f32>,
    curve_scale: f32,
    encode: Buffer<u8>,
}

impl ToneMap {
    fn new(
        primaries: u16,
        transfer: u16,
        bit_depth: u8,
        peak_luminance: Option<f32>,
        budget: &Budget,
    ) -> Result<Self> {
        let (gamut, weights) = match primaries {
            1 => (BT709_TO_BT709, [0.212639, 0.715169, 0.072192]),
            2 | 9 => (BT2020_TO_BT709, [0.2627, 0.677998, 0.059302]),
            12 => (P3_TO_BT709, [0.228975, 0.691739, 0.079287]),
            _ => return Err(Error::Unsupported("HDR color primaries")),
        };
        let hlg = transfer == 18;
        let peak = if hlg {
            HLG_PEAK
        } else {
            peak_luminance
                .map_or(DEFAULT_PQ_PEAK, f64::from)
                .clamp(REFERENCE_WHITE, 10000.0)
        };
        let max_code = f64::from((1u32 << bit_depth) - 1);
        let mut signal = budget.zeroed(1 << bit_depth, "HDR signal table")?;
        for (code, value) in signal.iter_mut().enumerate() {
            let encoded = code as f64 / max_code;
            *value = if hlg {
                hlg_to_scene(encoded)
            } else {
                pq_to_nits(encoded) / REFERENCE_WHITE
            } as f32;
        }
        let ootf = if hlg {
            let mut gains = budget.zeroed(OOTF_SAMPLES, "HLG system gamma table")?;
            for (index, gain) in gains.iter_mut().enumerate() {
                let luminance = index as f64 / (OOTF_SAMPLES - 1) as f64;
                *gain = (HLG_PEAK / REFERENCE_WHITE * luminance.powf(0.2)) as f32;
            }
            Some((weights, gains))
        } else {
            None
        };
        let relative_peak = peak / REFERENCE_WHITE;
        let mut curve = budget.zeroed(CURVE_SAMPLES, "tone curve")?;
        for (index, value) in curve.iter_mut().enumerate() {
            let level = relative_peak * index as f64 / (CURVE_SAMPLES - 1) as f64;
            *value = (eetf(level * REFERENCE_WHITE, peak) / REFERENCE_WHITE) as f32;
        }
        let mut encode = budget.zeroed(ENCODE_SAMPLES, "sRGB encoding table")?;
        for (index, value) in encode.iter_mut().enumerate() {
            let linear = index as f64 / (ENCODE_SAMPLES - 1) as f64;
            *value = (srgb_encode(linear) * 255.0).round() as u8;
        }
        Ok(Self {
            signal,
            ootf,
            gamut,
            curve,
            curve_scale: ((CURVE_SAMPLES - 1) as f64 / relative_peak) as f32,
            encode,
        })
    }

    pub(super) fn apply(&self, input: &[u16], output: &mut [u8]) {
        let last_code = self.signal.len() - 1;
        for (samples, pixel) in input
            .as_chunks::<3>()
            .0
            .iter()
            .zip(output.as_chunks_mut::<3>().0)
        {
            let mut light = [0.0; 3];
            for (light, &sample) in light.iter_mut().zip(samples) {
                *light = self.signal[usize::from(sample).min(last_code)];
            }
            if let Some((weights, gains)) = &self.ootf {
                let luminance = dot(weights, &light);
                let index = (luminance * (OOTF_SAMPLES - 1) as f32 + 0.5) as usize;
                let gain = gains[index.min(OOTF_SAMPLES - 1)];
                light = light.map(|value| value * gain);
            }
            let light = self.gamut.map(|row| dot(&row, &light).max(0.0));
            let level = light[0].max(light[1]).max(light[2]);
            let gain = if level > 0.0 {
                self.curve_at(level) / level
            } else {
                0.0
            };
            for (output, light) in pixel.iter_mut().zip(light) {
                let index = ((light * gain).min(1.0) * (ENCODE_SAMPLES - 1) as f32 + 0.5) as usize;
                *output = self.encode[index];
            }
        }
    }

    fn curve_at(&self, level: f32) -> f32 {
        let position = level * self.curve_scale;
        let last = CURVE_SAMPLES - 1;
        if position >= last as f32 {
            return self.curve[last];
        }
        let index = position as usize;
        let fraction = position - index as f32;
        self.curve[index] + (self.curve[index + 1] - self.curve[index]) * fraction
    }
}

fn dot(a: &[f32; 3], b: &[f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

const PQ_M1: f64 = 2610.0 / 16384.0;
const PQ_M2: f64 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f64 = 3424.0 / 4096.0;
const PQ_C2: f64 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f64 = 2392.0 / 4096.0 * 32.0;

fn pq_to_nits(encoded: f64) -> f64 {
    let power = encoded.max(0.0).powf(1.0 / PQ_M2);
    ((power - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * power)).powf(1.0 / PQ_M1) * 10000.0
}

fn nits_to_pq(nits: f64) -> f64 {
    let power = (nits / 10000.0).max(0.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * power) / (1.0 + PQ_C3 * power)).powf(PQ_M2)
}

fn hlg_to_scene(encoded: f64) -> f64 {
    const A: f64 = 0.17883277;
    const B: f64 = 0.28466892;
    const C: f64 = 0.55991073;
    if encoded <= 0.5 {
        encoded * encoded / 3.0
    } else {
        (((encoded - C) / A).exp() + B) / 12.0
    }
}

fn eetf(nits: f64, peak: f64) -> f64 {
    let source = nits_to_pq(peak);
    let level = (nits_to_pq(nits) / source).min(1.0);
    let target = nits_to_pq(REFERENCE_WHITE) / source;
    let knee = 1.5 * target - 0.5;
    let mapped = if level < knee || knee >= 1.0 {
        level
    } else {
        let t = (level - knee) / (1.0 - knee);
        let (t2, t3) = (t * t, t * t * t);
        (2.0 * t3 - 3.0 * t2 + 1.0) * knee
            + (t3 - 2.0 * t2 + t) * (1.0 - knee)
            + (-2.0 * t3 + 3.0 * t2) * target
    };
    pq_to_nits(mapped * source)
}

fn srgb_encode(linear: f64) -> f64 {
    if linear <= 0.0031308 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

fn icc_cicp(icc: &[u8]) -> Option<(u16, u16)> {
    if icc.len() < 132 || &icc[16..20] != b"RGB " || &icc[36..40] != b"acsp" {
        return None;
    }
    let mut r = Reader::new(icc);
    if r.uint(4).ok()? as usize != icc.len() {
        return None;
    }
    r.pos = 128;
    let count = r.uint(4).ok()? as usize;
    if count > 128 || 132 + count * 12 > icc.len() {
        return None;
    }
    let table_end = 132 + count * 12;
    (0..count).find_map(|_| {
        let signature = r.take(4).ok()?;
        let offset = r.uint(4).ok()? as usize;
        let len = r.uint(4).ok()?;
        if signature != b"cicp" || len != 12 || offset < table_end {
            return None;
        }
        let data = icc.get(offset..offset.checked_add(12)?)?;
        (&data[..4] == b"cicp").then(|| (data[8].into(), data[9].into()))
    })
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
            icc: Some(&icc),
            ..Default::default()
        };
        let budget = Budget::new(4 * 1024 * 1024);
        let bounded = ColorTransform::new(&color, 2, 2, 8, &budget).unwrap();
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
    fn rejects_gray_lut_and_unbounded_icc_tag_counts() {
        let original = p3_profile();
        let budget = Budget::new(4 * 1024 * 1024);
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
                        icc: Some(&icc),
                        ..Default::default()
                    },
                    2,
                    2,
                    8,
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
                let color = Color {
                    icc,
                    nclx,
                    ..Default::default()
                };
                for (primaries, transfer) in [(9, 13), (12, 1), (1, 8), (2, 17), (5, 8), (7, 6)] {
                    let result = ColorTransform::new(&color, primaries, transfer, 8, &budget);
                    if icc.is_some() {
                        assert!(result.unwrap().is_identity());
                    } else {
                        assert!(matches!(
                            result,
                            Err(Error::Unsupported("unprofiled HEVC color"))
                        ));
                    }
                }
                for primaries in [1, 2, 5, 6] {
                    for transfer in [1, 2, 6, 13, 14, 15] {
                        assert!(
                            ColorTransform::new(&color, primaries, transfer, 8, &budget)
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
            nclx: Some(nclx),
            ..Default::default()
        };
        assert!(
            !ColorTransform::new(&color, 9, 13, 8, &budget)
                .unwrap()
                .is_identity()
        );
        color.icc = Some(&[]);
        assert!(matches!(
            ColorTransform::new(&color, 9, 13, 8, &budget),
            Err(Error::Unsupported("empty ICC with non-sRGB nclx"))
        ));
        let icc = p3_profile();
        color.icc = Some(&icc);
        assert!(
            !ColorTransform::new(&color, 9, 13, 8, &budget)
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
                ..Default::default()
            };
            let bounded = ColorTransform::new(&color, 2, 2, 8, &budget).unwrap();
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
                ..Default::default()
            };
            let transform = ColorTransform::new(&color, 2, 2, 8, &budget);
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

    fn nclx(primaries: u16, transfer: u16) -> crate::isobmff::NclxColorProfile {
        crate::isobmff::NclxColorProfile {
            colour_primaries: primaries,
            transfer_characteristics: transfer,
            matrix_coefficients: 9,
            full_range_flag: true,
        }
    }

    fn tone_map_gray(tone_map: &ToneMap, code: u16) -> u8 {
        let mut output = [0; 3];
        tone_map.apply(&[code; 3], &mut output);
        assert!(output.iter().all(|&value| value == output[0]));
        output[0]
    }

    #[cfg(not(feature = "decoder-tracing"))]
    fn reference_tone_map(samples: [u16; 3], hlg: bool) -> [u8; 3] {
        let mut light = samples.map(|sample| {
            let encoded = f64::from(sample) / 1023.0;
            if hlg {
                hlg_to_scene(encoded)
            } else {
                pq_to_nits(encoded) / REFERENCE_WHITE
            }
        });
        if hlg {
            let luminance = 0.2627 * light[0] + 0.677998 * light[1] + 0.059302 * light[2];
            light = light.map(|value| value * HLG_PEAK / REFERENCE_WHITE * luminance.powf(0.2));
        }
        let light = BT2020_TO_BT709.map(|row| {
            (f64::from(row[0]) * light[0]
                + f64::from(row[1]) * light[1]
                + f64::from(row[2]) * light[2])
                .max(0.0)
        });
        let level = light[0].max(light[1]).max(light[2]);
        let gain = if level > 0.0 {
            eetf(level * REFERENCE_WHITE, 1000.0) / REFERENCE_WHITE / level
        } else {
            0.0
        };
        light.map(|value| (srgb_encode((value * gain).min(1.0)) * 255.0).round() as u8)
    }

    #[cfg(not(feature = "decoder-tracing"))]
    #[test]
    fn hdr_fixtures_match_a_float_tone_mapping_reference() {
        for (bytes, hlg) in [
            (
                include_bytes!("testdata/apple-pq-64x48.heic").as_slice(),
                false,
            ),
            (
                include_bytes!("testdata/apple-hlg-64x48.heic").as_slice(),
                true,
            ),
        ] {
            let bounded =
                crate::decode_bounded(crate::BoundedInput::Bytes(bytes), Default::default())
                    .unwrap()
                    .image;
            assert_eq!(
                (bounded.width, bounded.height, bounded.source_bit_depth),
                (64, 48, 10)
            );
            assert_eq!(bounded.icc_profile, None);
            let normal = crate::decode_bytes_to_rgba(bytes).unwrap();
            let samples = normal.pixels.as_rgba16().unwrap();
            let mut highlights = 0;
            for (index, (pixel, actual)) in samples
                .as_chunks::<4>()
                .0
                .iter()
                .zip(bounded.pixels.as_chunks::<3>().0)
                .enumerate()
            {
                let codes = [pixel[0] >> 6, pixel[1] >> 6, pixel[2] >> 6];
                let expected = reference_tone_map(codes, hlg);
                assert!(
                    expected
                        .iter()
                        .zip(actual)
                        .all(|(e, a)| e.abs_diff(*a) <= 1),
                    "hlg={hlg} pixel {index}: {codes:?} -> {actual:?}, expected {expected:?}"
                );
                highlights += usize::from(actual.contains(&255));
            }
            assert!(highlights > 0);
        }
    }

    #[test]
    fn transfer_functions_match_published_reference_levels() {
        for (nits, encoded) in [
            (100.0, 0.5081),
            (203.0, 0.5807),
            (1000.0, 0.7518),
            (10000.0, 1.0),
        ] {
            assert!((nits_to_pq(nits) - encoded).abs() < 1e-4, "{nits}");
            assert!((pq_to_nits(nits_to_pq(nits)) - nits).abs() < 1e-6 * nits);
        }
        assert!((hlg_to_scene(0.5) - 1.0 / 12.0).abs() < 1e-12);
        assert!((hlg_to_scene(1.0) - 1.0).abs() < 1e-6);
        assert!((HLG_PEAK * hlg_to_scene(0.75).powf(1.2) - REFERENCE_WHITE).abs() < 0.5);
        assert_eq!(eetf(50.0, 1000.0), pq_to_nits(nits_to_pq(50.0)));
        assert!((eetf(1000.0, 1000.0) - REFERENCE_WHITE).abs() < 1e-9);
        assert!((eetf(5000.0, 1000.0) - REFERENCE_WHITE).abs() < 1e-9);
        assert!((eetf(150.0, 203.0) - 150.0).abs() < 1e-9);
        assert!((eetf(400.0, 203.0) - REFERENCE_WHITE).abs() < 1e-9);
        let curve: Vec<f64> = (0..=1000)
            .map(|nits| eetf(f64::from(nits), 1000.0))
            .collect();
        assert!(curve.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn hlg_and_pq_reference_white_meet_at_the_same_level() {
        let budget = Budget::new(4 * 1024 * 1024);
        let pq = ToneMap::new(9, 16, 12, None, &budget).unwrap();
        let hlg = ToneMap::new(9, 18, 12, None, &budget).unwrap();
        let white = (nits_to_pq(REFERENCE_WHITE) * 4095.0).round() as u16;
        assert_eq!(tone_map_gray(&pq, white), 229);
        assert_eq!(tone_map_gray(&hlg, 3071), 229);
        assert_eq!(tone_map_gray(&pq, 0), 0);
        assert_eq!(tone_map_gray(&hlg, 0), 0);
        assert_eq!(tone_map_gray(&pq, 4095), 255);
        assert_eq!(tone_map_gray(&hlg, 4095), 255);
        let levels: Vec<u8> = (0..4096).map(|code| tone_map_gray(&pq, code)).collect();
        assert!(levels.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn content_light_level_sets_the_highlight_roll_off() {
        let level = |content, mastering| Color {
            content_light_level: content,
            mastering_luminance: mastering,
            ..Default::default()
        };
        assert_eq!(
            level(Some(880), Some(10_000_000)).peak_luminance(),
            Some(880.0)
        );
        assert_eq!(
            level(Some(0), Some(40_000_000)).peak_luminance(),
            Some(4000.0)
        );
        assert_eq!(level(None, Some(0)).peak_luminance(), None);
        let budget = Budget::new(4 * 1024 * 1024);
        let white = (nits_to_pq(REFERENCE_WHITE) * 4095.0).round() as u16;
        let gray = |transfer, peak| {
            let tone_map = ToneMap::new(9, transfer, 12, peak, &budget).unwrap();
            let code = if transfer == 18 { 3071 } else { white };
            tone_map_gray(&tone_map, code)
        };
        assert_eq!(gray(16, None), 229);
        assert_eq!(gray(16, Some(1000.0)), 229);
        assert_eq!(gray(16, Some(100.0)), 255);
        assert_eq!(gray(16, Some(203.0)), 255);
        assert_eq!(gray(16, Some(4000.0)), 208);
        assert_eq!(gray(18, Some(100.0)), 229);
    }

    #[test]
    fn hdr_signalling_follows_container_precedence() {
        let budget = Budget::new(4 * 1024 * 1024);
        let tone_maps = |color: &Color<'_>, primaries, transfer| {
            ColorTransform::new(color, primaries, transfer, 10, &budget)
                .map(|transform| transform.tone_map().is_some())
        };
        let unspecified = crate::isobmff::NclxColorProfile {
            colour_primaries: 2,
            transfer_characteristics: 2,
            matrix_coefficients: 2,
            full_range_flag: true,
        };
        for nclx in [None, Some(unspecified)] {
            let color = Color {
                nclx,
                ..Default::default()
            };
            for (primaries, transfer) in [(9, 16), (9, 18), (1, 16), (12, 18), (2, 16)] {
                assert!(tone_maps(&color, primaries, transfer).unwrap());
            }
            assert!(matches!(
                tone_maps(&color, 5, 16),
                Err(Error::Unsupported("HDR color primaries"))
            ));
        }
        let color = |nclx, icc| Color {
            nclx,
            icc,
            ..Default::default()
        };
        assert!(tone_maps(&color(Some(nclx(9, 16)), None), 1, 1).unwrap());
        assert!(tone_maps(&color(Some(nclx(12, 18)), None), 2, 2).unwrap());
        assert!(matches!(
            tone_maps(&color(Some(nclx(1, 13)), None), 9, 16),
            Err(Error::Unsupported("HDR transfer function"))
        ));
        let hdr_icc = crate::nclx_to_icc_profile(&nclx(9, 16)).unwrap();
        assert_eq!(icc_cicp(&hdr_icc), Some((9, 16)));
        assert!(tone_maps(&color(None, Some(&hdr_icc)), 1, 1).unwrap());
        let sdr_icc = p3_profile();
        assert!(!tone_maps(&color(None, Some(&sdr_icc)), 9, 13).unwrap());
        assert!(matches!(
            tone_maps(&color(None, Some(&sdr_icc)), 9, 16),
            Err(Error::Unsupported("HDR transfer function"))
        ));
        for icc in [&sdr_icc[..], &[]] {
            assert!(matches!(
                tone_maps(&color(Some(nclx(9, 16)), Some(icc)), 9, 13),
                Err(Error::Unsupported("HDR transfer function"))
            ));
        }
    }

    #[test]
    fn hdr_cicp_requires_a_valid_rgb_profile_header() {
        let budget = Budget::new(4 * 1024 * 1024);
        let hdr_icc = crate::nclx_to_icc_profile(&nclx(9, 16)).unwrap();
        for (range, value) in [(0..4, [0; 4]), (16..20, *b"GRAY"), (36..40, *b"xxxx")] {
            let mut invalid = hdr_icc.clone();
            invalid[range].copy_from_slice(&value);
            assert_eq!(icc_cicp(&invalid), None);
            let color = Color {
                icc: Some(&invalid),
                ..Default::default()
            };
            assert!(ColorTransform::new(&color, 1, 1, 10, &budget).is_err());
        }
        assert_eq!(icc_cicp(&hdr_icc[..hdr_icc.len() - 1]), None);
        let count = u32::from_be_bytes(hdr_icc[128..132].try_into().unwrap()) as usize;
        let entry = (0..count)
            .map(|index| 132 + index * 12)
            .find(|&at| &hdr_icc[at..at + 4] == b"cicp")
            .unwrap();
        let mut overlapping = hdr_icc.clone();
        overlapping[entry + 4..entry + 8].copy_from_slice(&80u32.to_be_bytes());
        overlapping[80..92].copy_from_slice(b"cicp\0\0\0\0\x09\x10\0\x01");
        assert_eq!(icc_cicp(&overlapping), None);
        let mut oversized = hdr_icc.clone();
        oversized[128..132].copy_from_slice(&129u32.to_be_bytes());
        assert_eq!(icc_cicp(&oversized), None);
    }
}
