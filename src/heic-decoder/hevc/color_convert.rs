//! SIMD-accelerated YCbCr→RGB color conversion
//!
//! Uses archmage for safe runtime dispatch across x86 (AVX2) with
//! scalar fallback on other platforms.

use archmage::incant;
use archmage::prelude::*;

#[cfg(target_arch = "aarch64")]
use core::arch::aarch64::{
    uint8x8x3_t, uint8x8x4_t, uint16x4_t, uint16x4x4_t, uint16x8_t, vaddq_f32, vaddq_s32,
    vcombine_u16, vcvtq_f32_u32, vcvtq_s32_f32, vdup_n_u8, vdup_n_u16, vdupq_n_f32, vdupq_n_s32,
    vfmaq_n_f32, vget_high_u16, vget_low_u16, vmaxq_s32, vminq_s32, vmlaq_n_s32, vmovl_u16,
    vmulq_n_f32, vqmovn_u16, vqmovn_u32, vqmovun_s32, vreinterpretq_s32_u32, vreinterpretq_u32_s32,
    vshlq_u32, vshrq_n_s32, vst3_u8, vst4_u8, vst4_u16, vsubq_f32, vsubq_s32, vzip1_u16, vzip2_u16,
};
#[cfg(target_arch = "aarch64")]
use safe_unaligned_simd::aarch64::{vld1_u16, vld1q_u16};

/// Parameters for libheif's generic f32 matrix conversion.
///
/// Full range uses offsets `(0, midpoint)` and scales `(1, 1)`. Limited
/// range uses libheif's exact `1.1689f` luma and `1.1429f` chroma scales.
#[derive(Clone, Copy)]
pub(crate) struct FloatMatrixParams {
    pub(crate) y_offset: f32,
    pub(crate) y_scale: f32,
    pub(crate) chroma_midpoint: f32,
    pub(crate) chroma_scale: f32,
    pub(crate) r_cr: f32,
    pub(crate) g_cb: f32,
    pub(crate) g_cr: f32,
    pub(crate) b_cb: f32,
}

// Explicit imports for safe SIMD load/store (can't glob-import alongside core::arch)
#[cfg(target_arch = "x86_64")]
use safe_unaligned_simd::x86_64::{_mm_loadu_si64, _mm_loadu_si128, _mm256_storeu_si256};

/// Convert full-range 8-bit 4:2:0 planes with libheif's exact fp8 kernel.
///
/// `channels` must be 3 or 4. The caller has already validated plane and
/// output lengths; keeping this kernel focused lets the same path serve both
/// RGB grid scratch buffers and the image adapter's RGBA buffer.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_420_8bit_to_interleaved(
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    width: usize,
    height: usize,
    chroma_width: usize,
    r_cr: i32,
    g_cb: i32,
    g_cr: i32,
    b_cb: i32,
    channels: usize,
    output: &mut [u8],
) {
    convert_420_8bit_region_to_interleaved(
        y_plane,
        cb_plane,
        cr_plane,
        width,
        chroma_width,
        0,
        0,
        width,
        height,
        r_cr,
        g_cb,
        g_cr,
        b_cb,
        channels,
        output,
    );
}

/// Convert a rectangular region of full-range 8-bit 4:2:0 planes.
///
/// Source coordinates remain in the uncropped planes, so an odd `x_start`
/// retains the original chroma phase. The result is tightly packed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_420_8bit_region_to_interleaved(
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    y_stride: usize,
    chroma_stride: usize,
    x_start: usize,
    y_start: usize,
    width: usize,
    height: usize,
    r_cr: i32,
    g_cb: i32,
    g_cr: i32,
    b_cb: i32,
    channels: usize,
    output: &mut [u8],
) {
    assert!(matches!(channels, 3 | 4), "channels must be RGB or RGBA");
    let pixel_count = width
        .checked_mul(height)
        .expect("4:2:0 region pixel count must fit in usize");
    let output_len = pixel_count
        .checked_mul(channels)
        .expect("interleaved output length must fit in usize");
    let x_end = x_start
        .checked_add(width)
        .expect("4:2:0 region x extent must fit in usize");
    let y_end = y_start
        .checked_add(height)
        .expect("4:2:0 region y extent must fit in usize");
    let y_len = y_stride
        .checked_mul(y_end)
        .expect("4:2:0 luma extent must fit in usize");
    let chroma_rows = y_end.div_ceil(2);
    let chroma_len = chroma_stride
        .checked_mul(chroma_rows)
        .expect("4:2:0 chroma extent must fit in usize");
    assert_eq!(output.len(), output_len, "interleaved output length");
    assert!(x_end <= y_stride, "4:2:0 region exceeds luma row");
    assert!(y_plane.len() >= y_len, "luma plane length");
    assert!(
        chroma_stride >= x_end.div_ceil(2),
        "4:2:0 region exceeds chroma row"
    );
    assert!(cb_plane.len() >= chroma_len, "Cb plane length");
    assert!(cr_plane.len() >= chroma_len, "Cr plane length");
    incant!(
        convert_420_8bit_region_to_interleaved(
            y_plane,
            cb_plane,
            cr_plane,
            y_stride,
            chroma_stride,
            x_start,
            y_start,
            width,
            height,
            r_cr,
            g_cb,
            g_cr,
            b_cb,
            channels,
            output
        ),
        [neon, scalar]
    );
}

#[allow(clippy::too_many_arguments)]
fn convert_420_8bit_region_to_interleaved_scalar(
    _token: ScalarToken,
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    y_stride: usize,
    chroma_stride: usize,
    x_start: usize,
    y_start: usize,
    width: usize,
    height: usize,
    r_cr: i32,
    g_cb: i32,
    g_cr: i32,
    b_cb: i32,
    channels: usize,
    output: &mut [u8],
) {
    for output_y in 0..height {
        let source_y = y_start + output_y;
        let y_row = source_y * y_stride;
        let chroma_row = (source_y / 2) * chroma_stride;
        for output_x in 0..width {
            let source_x = x_start + output_x;
            write_420_8bit_pixel(
                y_plane[y_row + source_x],
                cb_plane[chroma_row + source_x / 2],
                cr_plane[chroma_row + source_x / 2],
                r_cr,
                g_cb,
                g_cr,
                b_cb,
                channels,
                &mut output[(output_y * width + output_x) * channels..],
            );
        }
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn write_420_8bit_pixel(
    y_sample: u16,
    cb_sample: u16,
    cr_sample: u16,
    r_cr: i32,
    g_cb: i32,
    g_cr: i32,
    b_cb: i32,
    channels: usize,
    output: &mut [u8],
) {
    let y_sample = i64::from(y_sample);
    let cb = i64::from(cb_sample) - 128;
    let cr = i64::from(cr_sample) - 128;
    let r = y_sample + ((i64::from(r_cr) * cr + 128) >> 8);
    let g = y_sample + ((i64::from(g_cb) * cb + i64::from(g_cr) * cr + 128) >> 8);
    let b = y_sample + ((i64::from(b_cb) * cb + 128) >> 8);
    output[0] = r.clamp(0, 255) as u8;
    output[1] = g.clamp(0, 255) as u8;
    output[2] = b.clamp(0, 255) as u8;
    if channels == 4 {
        output[3] = u8::MAX;
    }
}

#[cfg(target_arch = "aarch64")]
#[allow(clippy::too_many_arguments)]
#[arcane]
fn convert_420_8bit_region_to_interleaved_neon(
    _token: NeonToken,
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    y_stride: usize,
    chroma_stride: usize,
    x_start: usize,
    y_start: usize,
    width: usize,
    height: usize,
    r_cr: i32,
    g_cb: i32,
    g_cr: i32,
    b_cb: i32,
    channels: usize,
    output: &mut [u8],
) {
    // Plane samples are u16 even for an 8-bit stream. Bound coefficient
    // magnitudes before using i32 lanes so unusual public color metadata
    // retains the scalar implementation's i64 overflow behavior.
    let max_delta = i64::from(u16::MAX) - 128;
    let max_y = i64::from(u16::MAX);
    let rounded = 128_i64;
    let single_channel_fits = |coefficient: i32| {
        i64::from(coefficient).abs() * max_delta + max_y + rounded <= i64::from(i32::MAX)
    };
    let green_fits = (i64::from(g_cb).abs() + i64::from(g_cr).abs()) * max_delta + max_y + rounded
        <= i64::from(i32::MAX);
    if !single_channel_fits(r_cr) || !green_fits || !single_channel_fits(b_cb) {
        convert_420_8bit_region_to_interleaved_scalar(
            ScalarToken,
            y_plane,
            cb_plane,
            cr_plane,
            y_stride,
            chroma_stride,
            x_start,
            y_start,
            width,
            height,
            r_cr,
            g_cb,
            g_cr,
            b_cb,
            channels,
            output,
        );
        return;
    }

    let zero = vdupq_n_s32(0);
    let max_255 = vdupq_n_s32(255);
    let center = vdupq_n_s32(128);
    let rounding = vdupq_n_s32(128);
    let opaque = vdup_n_u8(u8::MAX);
    let x_end = x_start + width;
    // Chroma pairs start on even luma coordinates. Peel an odd first sample
    // so every SIMD group can duplicate four adjacent chroma samples.
    let simd_start = x_start.next_multiple_of(2).min(x_end);
    let simd_end = simd_start + (x_end - simd_start) / 8 * 8;

    for output_y in 0..height {
        let source_y = y_start + output_y;
        let y_row = source_y * y_stride;
        let chroma_row = (source_y / 2) * chroma_stride;

        for source_x in x_start..simd_start {
            let output_x = source_x - x_start;
            write_420_8bit_pixel(
                y_plane[y_row + source_x],
                cb_plane[chroma_row + source_x / 2],
                cr_plane[chroma_row + source_x / 2],
                r_cr,
                g_cb,
                g_cr,
                b_cb,
                channels,
                &mut output[(output_y * width + output_x) * channels..],
            );
        }

        let mut source_x = simd_start;
        while source_x < simd_end {
            let y_values = vld1q_u16(
                (&y_plane[y_row + source_x..y_row + source_x + 8])
                    .try_into()
                    .unwrap(),
            );
            let cb_values = vld1_u16(
                (&cb_plane[chroma_row + source_x / 2..chroma_row + source_x / 2 + 4])
                    .try_into()
                    .unwrap(),
            );
            let cr_values = vld1_u16(
                (&cr_plane[chroma_row + source_x / 2..chroma_row + source_x / 2 + 4])
                    .try_into()
                    .unwrap(),
            );
            let cb_values = vcombine_u16(
                vzip1_u16(cb_values, cb_values),
                vzip2_u16(cb_values, cb_values),
            );
            let cr_values = vcombine_u16(
                vzip1_u16(cr_values, cr_values),
                vzip2_u16(cr_values, cr_values),
            );

            let y_lo = vreinterpretq_s32_u32(vmovl_u16(vget_low_u16(y_values)));
            let y_hi = vreinterpretq_s32_u32(vmovl_u16(vget_high_u16(y_values)));
            let cb_lo = vsubq_s32(
                vreinterpretq_s32_u32(vmovl_u16(vget_low_u16(cb_values))),
                center,
            );
            let cb_hi = vsubq_s32(
                vreinterpretq_s32_u32(vmovl_u16(vget_high_u16(cb_values))),
                center,
            );
            let cr_lo = vsubq_s32(
                vreinterpretq_s32_u32(vmovl_u16(vget_low_u16(cr_values))),
                center,
            );
            let cr_hi = vsubq_s32(
                vreinterpretq_s32_u32(vmovl_u16(vget_high_u16(cr_values))),
                center,
            );

            let convert_half = |y_values, cb_values, cr_values| {
                let r = vaddq_s32(
                    y_values,
                    vshrq_n_s32(vmlaq_n_s32(rounding, cr_values, r_cr), 8),
                );
                let g_terms = vmlaq_n_s32(vmlaq_n_s32(rounding, cb_values, g_cb), cr_values, g_cr);
                let g = vaddq_s32(y_values, vshrq_n_s32(g_terms, 8));
                let b = vaddq_s32(
                    y_values,
                    vshrq_n_s32(vmlaq_n_s32(rounding, cb_values, b_cb), 8),
                );
                (
                    vminq_s32(vmaxq_s32(r, zero), max_255),
                    vminq_s32(vmaxq_s32(g, zero), max_255),
                    vminq_s32(vmaxq_s32(b, zero), max_255),
                )
            };
            let (r_lo, g_lo, b_lo) = convert_half(y_lo, cb_lo, cr_lo);
            let (r_hi, g_hi, b_hi) = convert_half(y_hi, cb_hi, cr_hi);
            let r = vqmovn_u16(vcombine_u16(vqmovun_s32(r_lo), vqmovun_s32(r_hi)));
            let g = vqmovn_u16(vcombine_u16(vqmovun_s32(g_lo), vqmovun_s32(g_hi)));
            let b = vqmovn_u16(vcombine_u16(vqmovun_s32(b_lo), vqmovun_s32(b_hi)));
            let output_x = source_x - x_start;
            let output_index = (output_y * width + output_x) * channels;

            // SAFETY: the caller-proven output length is `width * height *
            // channels`; this iteration starts at an in-row group of eight
            // pixels, and vst3/vst4 write exactly 8 * channels bytes.
            unsafe {
                let destination = output.as_mut_ptr().add(output_index);
                if channels == 4 {
                    vst4_u8(destination, uint8x8x4_t(r, g, b, opaque));
                } else {
                    vst3_u8(destination, uint8x8x3_t(r, g, b));
                }
            }
            source_x += 8;
        }

        for source_x in simd_end..x_end {
            let output_x = source_x - x_start;
            write_420_8bit_pixel(
                y_plane[y_row + source_x],
                cb_plane[chroma_row + source_x / 2],
                cr_plane[chroma_row + source_x / 2],
                r_cr,
                g_cb,
                g_cr,
                b_cb,
                channels,
                &mut output[(output_y * width + output_x) * channels..],
            );
        }
    }
}

/// Convert an 8-bit HEIC region through libheif's generic f32 matrix kernel.
///
/// This covers limited-range 4:2:0 and full/limited 4:2:2 or 4:4:4, which do
/// not use libheif's specialized fixed-point 4:2:0 operation. Source
/// coordinates remain in the uncropped planes so odd crop origins retain
/// their original chroma phase. `subsample_x`/`subsample_y` must each be one
/// or two and `channels` must be three or four.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_float_matrix_8bit_region_to_interleaved(
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    y_stride: usize,
    chroma_stride: usize,
    subsample_x: usize,
    subsample_y: usize,
    x_start: usize,
    y_start: usize,
    width: usize,
    height: usize,
    params: FloatMatrixParams,
    channels: usize,
    output: &mut [u8],
) {
    validate_float_matrix_region(
        y_plane,
        cb_plane,
        cr_plane,
        y_stride,
        chroma_stride,
        subsample_x,
        subsample_y,
        x_start,
        y_start,
        width,
        height,
        channels,
        output.len(),
    );
    incant!(
        convert_float_matrix_8bit_region_to_interleaved(
            y_plane,
            cb_plane,
            cr_plane,
            y_stride,
            chroma_stride,
            subsample_x,
            subsample_y,
            x_start,
            y_start,
            width,
            height,
            params,
            channels,
            output
        ),
        [neon, scalar]
    );
}

/// Convert a complete high-bit-depth HEIC image through libheif's generic
/// f32 matrix kernel and expand the clipped source samples to RGBA16.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_float_matrix_region_to_rgba16(
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    y_stride: usize,
    chroma_stride: usize,
    subsample_x: usize,
    subsample_y: usize,
    x_start: usize,
    y_start: usize,
    width: usize,
    height: usize,
    bit_depth: u8,
    params: FloatMatrixParams,
    output: &mut [u16],
) {
    assert!((1..=16).contains(&bit_depth), "source bit depth");
    validate_float_matrix_region(
        y_plane,
        cb_plane,
        cr_plane,
        y_stride,
        chroma_stride,
        subsample_x,
        subsample_y,
        x_start,
        y_start,
        width,
        height,
        4,
        output.len(),
    );
    incant!(
        convert_float_matrix_region_to_rgba16(
            y_plane,
            cb_plane,
            cr_plane,
            y_stride,
            chroma_stride,
            subsample_x,
            subsample_y,
            x_start,
            y_start,
            width,
            height,
            bit_depth,
            params,
            output
        ),
        [neon, scalar]
    );
}

#[allow(clippy::too_many_arguments)]
fn validate_float_matrix_region(
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    y_stride: usize,
    chroma_stride: usize,
    subsample_x: usize,
    subsample_y: usize,
    x_start: usize,
    y_start: usize,
    width: usize,
    height: usize,
    channels: usize,
    output_len: usize,
) {
    assert!(matches!(subsample_x, 1 | 2), "horizontal subsampling");
    assert!(matches!(subsample_y, 1 | 2), "vertical subsampling");
    assert!(matches!(channels, 3 | 4), "channels must be RGB or RGBA");
    let x_end = x_start
        .checked_add(width)
        .expect("float-matrix region x extent must fit in usize");
    let y_end = y_start
        .checked_add(height)
        .expect("float-matrix region y extent must fit in usize");
    let expected_output_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(channels))
        .expect("float-matrix output length must fit in usize");
    let y_len = y_stride
        .checked_mul(y_end)
        .expect("float-matrix luma extent must fit in usize");
    let chroma_rows = y_end.div_ceil(subsample_y);
    let chroma_len = chroma_stride
        .checked_mul(chroma_rows)
        .expect("float-matrix chroma extent must fit in usize");
    assert_eq!(
        output_len, expected_output_len,
        "float-matrix output length"
    );
    assert!(x_end <= y_stride, "float-matrix region exceeds luma row");
    assert!(y_plane.len() >= y_len, "float-matrix luma plane length");
    assert!(
        chroma_stride >= x_end.div_ceil(subsample_x),
        "float-matrix region exceeds chroma row"
    );
    assert!(cb_plane.len() >= chroma_len, "float-matrix Cb plane length");
    assert!(cr_plane.len() >= chroma_len, "float-matrix Cr plane length");
}

#[inline(always)]
fn float_matrix_fma(a: f32, b: f32, c: f32) -> f32 {
    #[cfg(any(
        target_arch = "aarch64",
        all(target_arch = "x86_64", target_feature = "fma")
    ))]
    {
        a.mul_add(b, c)
    }
    #[cfg(not(any(
        target_arch = "aarch64",
        all(target_arch = "x86_64", target_feature = "fma")
    )))]
    {
        a * b + c
    }
}

#[inline(always)]
fn convert_float_matrix_pixel(
    y_sample: u16,
    cb_sample: u16,
    cr_sample: u16,
    bit_depth: u8,
    params: FloatMatrixParams,
) -> (u16, u16, u16) {
    let y = (f32::from(y_sample) - params.y_offset) * params.y_scale;
    let cb = (f32::from(cb_sample) - params.chroma_midpoint) * params.chroma_scale;
    let cr = (f32::from(cr_sample) - params.chroma_midpoint) * params.chroma_scale;
    let r = float_matrix_fma(params.r_cr, cr, y);
    let g = float_matrix_fma(params.g_cr, cr, float_matrix_fma(params.g_cb, cb, y));
    let b = float_matrix_fma(params.b_cb, cb, y);
    let max = ((1_i32 << bit_depth) - 1).max(0);
    let clip = |value: f32| ((value + 0.5) as i32).clamp(0, max) as u16;
    (clip(r), clip(g), clip(b))
}

#[inline(always)]
fn scale_float_matrix_sample_to_u16(sample: u16, bit_depth: u8) -> u16 {
    if bit_depth >= 16 {
        return sample;
    }
    let shift = 16 - u32::from(bit_depth);
    let value = u32::from(sample);
    ((value << shift) | (value >> u32::from(bit_depth).saturating_sub(shift)))
        .min(u32::from(u16::MAX)) as u16
}

#[allow(clippy::too_many_arguments)]
fn convert_float_matrix_8bit_region_to_interleaved_scalar(
    _token: ScalarToken,
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    y_stride: usize,
    chroma_stride: usize,
    subsample_x: usize,
    subsample_y: usize,
    x_start: usize,
    y_start: usize,
    width: usize,
    height: usize,
    params: FloatMatrixParams,
    channels: usize,
    output: &mut [u8],
) {
    for output_y in 0..height {
        let source_y = y_start + output_y;
        let y_row = source_y * y_stride;
        let chroma_row = (source_y / subsample_y) * chroma_stride;
        for output_x in 0..width {
            let source_x = x_start + output_x;
            let chroma_index = chroma_row + source_x / subsample_x;
            let (r, g, b) = convert_float_matrix_pixel(
                y_plane[y_row + source_x],
                cb_plane[chroma_index],
                cr_plane[chroma_index],
                8,
                params,
            );
            let output_index = (output_y * width + output_x) * channels;
            output[output_index] = r as u8;
            output[output_index + 1] = g as u8;
            output[output_index + 2] = b as u8;
            if channels == 4 {
                output[output_index + 3] = u8::MAX;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn convert_float_matrix_region_to_rgba16_scalar(
    _token: ScalarToken,
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    y_stride: usize,
    chroma_stride: usize,
    subsample_x: usize,
    subsample_y: usize,
    x_start: usize,
    y_start: usize,
    width: usize,
    height: usize,
    bit_depth: u8,
    params: FloatMatrixParams,
    output: &mut [u16],
) {
    for output_y in 0..height {
        let source_y = y_start + output_y;
        let y_row = source_y * y_stride;
        let chroma_row = (source_y / subsample_y) * chroma_stride;
        for output_x in 0..width {
            let source_x = x_start + output_x;
            let chroma_index = chroma_row + source_x / subsample_x;
            let (r, g, b) = convert_float_matrix_pixel(
                y_plane[y_row + source_x],
                cb_plane[chroma_index],
                cr_plane[chroma_index],
                bit_depth,
                params,
            );
            let output_index = (output_y * width + output_x) * 4;
            output[output_index] = scale_float_matrix_sample_to_u16(r, bit_depth);
            output[output_index + 1] = scale_float_matrix_sample_to_u16(g, bit_depth);
            output[output_index + 2] = scale_float_matrix_sample_to_u16(b, bit_depth);
            output[output_index + 3] = u16::MAX;
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn float_matrix_neon_safe(params: FloatMatrixParams) -> bool {
    let values = [
        params.y_offset,
        params.y_scale,
        params.chroma_midpoint,
        params.chroma_scale,
        params.r_cr,
        params.g_cb,
        params.g_cr,
        params.b_cb,
    ];
    if !values.iter().all(|value| value.is_finite()) {
        return false;
    }

    // FCVTZS does not have Rust's saturating float-to-int semantics for
    // infinities/out-of-range values. Public matrix metadata can derive
    // unusual coefficients, so conservatively retain the scalar path unless
    // every possible u16 input remains in the ordinary i32 conversion range.
    let input_max = f64::from(u16::MAX);
    let y_bound = (input_max + f64::from(params.y_offset).abs()) * f64::from(params.y_scale).abs();
    let chroma_bound = (input_max + f64::from(params.chroma_midpoint).abs())
        * f64::from(params.chroma_scale).abs();
    let channel_bound = y_bound
        + chroma_bound
            * f64::from(
                params
                    .r_cr
                    .abs()
                    .max(params.g_cb.abs() + params.g_cr.abs())
                    .max(params.b_cb.abs()),
            );
    channel_bound.is_finite() && channel_bound + 1.0 < f64::from(i32::MAX)
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn load_float_matrix_chroma_8(plane: &[u16], index: usize, subsample_x: usize) -> uint16x8_t {
    // SAFETY: only called from the runtime-dispatched NEON kernel; slices are
    // bounds-checked before the unaligned vector loads.
    unsafe {
        if subsample_x == 1 {
            return vld1q_u16((&plane[index..index + 8]).try_into().unwrap());
        }
        let values = vld1_u16((&plane[index..index + 4]).try_into().unwrap());
        vcombine_u16(vzip1_u16(values, values), vzip2_u16(values, values))
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn load_float_matrix_chroma_4(plane: &[u16], index: usize, subsample_x: usize) -> uint16x4_t {
    // SAFETY: only called from the runtime-dispatched NEON kernel; slices are
    // bounds-checked before the unaligned vector loads.
    unsafe {
        if subsample_x == 1 {
            return vld1_u16((&plane[index..index + 4]).try_into().unwrap());
        }
        let duplicated = [
            plane[index],
            plane[index],
            plane[index + 1],
            plane[index + 1],
        ];
        vld1_u16(&duplicated)
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn convert_float_matrix_neon_4(
    y: uint16x4_t,
    cb: uint16x4_t,
    cr: uint16x4_t,
    params: FloatMatrixParams,
    max: core::arch::aarch64::int32x4_t,
) -> (
    core::arch::aarch64::int32x4_t,
    core::arch::aarch64::int32x4_t,
    core::arch::aarch64::int32x4_t,
) {
    // SAFETY: only called from the runtime-dispatched NEON kernels.
    unsafe {
        let y = vmulq_n_f32(
            vsubq_f32(vcvtq_f32_u32(vmovl_u16(y)), vdupq_n_f32(params.y_offset)),
            params.y_scale,
        );
        let cb = vmulq_n_f32(
            vsubq_f32(
                vcvtq_f32_u32(vmovl_u16(cb)),
                vdupq_n_f32(params.chroma_midpoint),
            ),
            params.chroma_scale,
        );
        let cr = vmulq_n_f32(
            vsubq_f32(
                vcvtq_f32_u32(vmovl_u16(cr)),
                vdupq_n_f32(params.chroma_midpoint),
            ),
            params.chroma_scale,
        );
        let r = vfmaq_n_f32(y, cr, params.r_cr);
        let g = vfmaq_n_f32(vfmaq_n_f32(y, cb, params.g_cb), cr, params.g_cr);
        let b = vfmaq_n_f32(y, cb, params.b_cb);
        let zero = vdupq_n_s32(0);
        let half = vdupq_n_f32(0.5);
        let clip = |value| vminq_s32(vmaxq_s32(vcvtq_s32_f32(vaddq_f32(value, half)), zero), max);
        (clip(r), clip(g), clip(b))
    }
}

#[cfg(target_arch = "aarch64")]
#[allow(clippy::too_many_arguments)]
#[arcane]
fn convert_float_matrix_8bit_region_to_interleaved_neon(
    _token: NeonToken,
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    y_stride: usize,
    chroma_stride: usize,
    subsample_x: usize,
    subsample_y: usize,
    x_start: usize,
    y_start: usize,
    width: usize,
    height: usize,
    params: FloatMatrixParams,
    channels: usize,
    output: &mut [u8],
) {
    if !float_matrix_neon_safe(params) {
        convert_float_matrix_8bit_region_to_interleaved_scalar(
            ScalarToken,
            y_plane,
            cb_plane,
            cr_plane,
            y_stride,
            chroma_stride,
            subsample_x,
            subsample_y,
            x_start,
            y_start,
            width,
            height,
            params,
            channels,
            output,
        );
        return;
    }

    let x_end = x_start + width;
    let simd_start = if subsample_x == 2 {
        x_start.next_multiple_of(2).min(x_end)
    } else {
        x_start
    };
    let simd_end = simd_start + (x_end - simd_start) / 8 * 8;
    let max = vdupq_n_s32(255);
    let opaque = vdup_n_u8(u8::MAX);

    for output_y in 0..height {
        let source_y = y_start + output_y;
        let y_row = source_y * y_stride;
        let chroma_row = (source_y / subsample_y) * chroma_stride;

        for source_x in x_start..simd_start {
            let output_x = source_x - x_start;
            let chroma_index = chroma_row + source_x / subsample_x;
            let (r, g, b) = convert_float_matrix_pixel(
                y_plane[y_row + source_x],
                cb_plane[chroma_index],
                cr_plane[chroma_index],
                8,
                params,
            );
            let output_index = (output_y * width + output_x) * channels;
            output[output_index] = r as u8;
            output[output_index + 1] = g as u8;
            output[output_index + 2] = b as u8;
            if channels == 4 {
                output[output_index + 3] = u8::MAX;
            }
        }

        let mut source_x = simd_start;
        while source_x < simd_end {
            let y_values = vld1q_u16(
                (&y_plane[y_row + source_x..y_row + source_x + 8])
                    .try_into()
                    .unwrap(),
            );
            let chroma_index = chroma_row + source_x / subsample_x;
            let cb_values = load_float_matrix_chroma_8(cb_plane, chroma_index, subsample_x);
            let cr_values = load_float_matrix_chroma_8(cr_plane, chroma_index, subsample_x);
            let (r_lo, g_lo, b_lo) = convert_float_matrix_neon_4(
                vget_low_u16(y_values),
                vget_low_u16(cb_values),
                vget_low_u16(cr_values),
                params,
                max,
            );
            let (r_hi, g_hi, b_hi) = convert_float_matrix_neon_4(
                vget_high_u16(y_values),
                vget_high_u16(cb_values),
                vget_high_u16(cr_values),
                params,
                max,
            );
            let r = vqmovn_u16(vcombine_u16(vqmovun_s32(r_lo), vqmovun_s32(r_hi)));
            let g = vqmovn_u16(vcombine_u16(vqmovun_s32(g_lo), vqmovun_s32(g_hi)));
            let b = vqmovn_u16(vcombine_u16(vqmovun_s32(b_lo), vqmovun_s32(b_hi)));
            let output_x = source_x - x_start;
            let output_index = (output_y * width + output_x) * channels;
            // SAFETY: validation proves the full tightly packed output size;
            // this group is eight in-row pixels, so the stores write exactly
            // eight times `channels` bytes within it.
            unsafe {
                let destination = output.as_mut_ptr().add(output_index);
                if channels == 4 {
                    vst4_u8(destination, uint8x8x4_t(r, g, b, opaque));
                } else {
                    vst3_u8(destination, uint8x8x3_t(r, g, b));
                }
            }
            source_x += 8;
        }

        for source_x in simd_end..x_end {
            let output_x = source_x - x_start;
            let chroma_index = chroma_row + source_x / subsample_x;
            let (r, g, b) = convert_float_matrix_pixel(
                y_plane[y_row + source_x],
                cb_plane[chroma_index],
                cr_plane[chroma_index],
                8,
                params,
            );
            let output_index = (output_y * width + output_x) * channels;
            output[output_index] = r as u8;
            output[output_index + 1] = g as u8;
            output[output_index + 2] = b as u8;
            if channels == 4 {
                output[output_index + 3] = u8::MAX;
            }
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn scale_float_matrix_neon_to_u16(
    values: core::arch::aarch64::int32x4_t,
    bit_depth: u8,
) -> uint16x4_t {
    // SAFETY: only called from the runtime-dispatched NEON RGBA16 kernel.
    unsafe {
        let values = vreinterpretq_u32_s32(values);
        if bit_depth >= 16 {
            return vqmovn_u32(values);
        }
        let left = 16_i32 - i32::from(bit_depth);
        let right = i32::from(bit_depth).saturating_sub(left);
        let expanded = core::arch::aarch64::vorrq_u32(
            vshlq_u32(values, vdupq_n_s32(left)),
            vshlq_u32(values, vdupq_n_s32(-right)),
        );
        vqmovn_u32(expanded)
    }
}

#[cfg(target_arch = "aarch64")]
#[allow(clippy::too_many_arguments)]
#[arcane]
fn convert_float_matrix_region_to_rgba16_neon(
    _token: NeonToken,
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    y_stride: usize,
    chroma_stride: usize,
    subsample_x: usize,
    subsample_y: usize,
    x_start: usize,
    y_start: usize,
    width: usize,
    height: usize,
    bit_depth: u8,
    params: FloatMatrixParams,
    output: &mut [u16],
) {
    if !float_matrix_neon_safe(params) {
        convert_float_matrix_region_to_rgba16_scalar(
            ScalarToken,
            y_plane,
            cb_plane,
            cr_plane,
            y_stride,
            chroma_stride,
            subsample_x,
            subsample_y,
            x_start,
            y_start,
            width,
            height,
            bit_depth,
            params,
            output,
        );
        return;
    }

    let max_sample = ((1_i32 << bit_depth) - 1).max(0);
    let max = vdupq_n_s32(max_sample);
    let opaque = vdup_n_u16(u16::MAX);
    let x_end = x_start + width;
    let simd_start = if subsample_x == 2 {
        x_start.next_multiple_of(2).min(x_end)
    } else {
        x_start
    };
    let simd_end = simd_start + (x_end - simd_start) / 4 * 4;
    for output_y in 0..height {
        let source_y = y_start + output_y;
        let y_row = source_y * y_stride;
        let chroma_row = (source_y / subsample_y) * chroma_stride;
        for source_x in x_start..simd_start {
            let output_x = source_x - x_start;
            let chroma_index = chroma_row + source_x / subsample_x;
            let (r, g, b) = convert_float_matrix_pixel(
                y_plane[y_row + source_x],
                cb_plane[chroma_index],
                cr_plane[chroma_index],
                bit_depth,
                params,
            );
            let output_index = (output_y * width + output_x) * 4;
            output[output_index] = scale_float_matrix_sample_to_u16(r, bit_depth);
            output[output_index + 1] = scale_float_matrix_sample_to_u16(g, bit_depth);
            output[output_index + 2] = scale_float_matrix_sample_to_u16(b, bit_depth);
            output[output_index + 3] = u16::MAX;
        }

        let mut source_x = simd_start;
        while source_x < simd_end {
            let y_values = vld1_u16(
                (&y_plane[y_row + source_x..y_row + source_x + 4])
                    .try_into()
                    .unwrap(),
            );
            let chroma_index = chroma_row + source_x / subsample_x;
            let cb_values = load_float_matrix_chroma_4(cb_plane, chroma_index, subsample_x);
            let cr_values = load_float_matrix_chroma_4(cr_plane, chroma_index, subsample_x);
            let (r, g, b) =
                convert_float_matrix_neon_4(y_values, cb_values, cr_values, params, max);
            let r = scale_float_matrix_neon_to_u16(r, bit_depth);
            let g = scale_float_matrix_neon_to_u16(g, bit_depth);
            let b = scale_float_matrix_neon_to_u16(b, bit_depth);
            let output_x = source_x - x_start;
            let output_index = (output_y * width + output_x) * 4;
            // SAFETY: validation proves `width * height * 4` samples; this
            // group is four in-row pixels and vst4 writes exactly 16 samples.
            unsafe {
                vst4_u16(
                    output.as_mut_ptr().add(output_index),
                    uint16x4x4_t(r, g, b, opaque),
                );
            }
            source_x += 4;
        }
        for source_x in simd_end..x_end {
            let output_x = source_x - x_start;
            let chroma_index = chroma_row + source_x / subsample_x;
            let (r, g, b) = convert_float_matrix_pixel(
                y_plane[y_row + source_x],
                cb_plane[chroma_index],
                cr_plane[chroma_index],
                bit_depth,
                params,
            );
            let output_index = (output_y * width + output_x) * 4;
            output[output_index] = scale_float_matrix_sample_to_u16(r, bit_depth);
            output[output_index + 1] = scale_float_matrix_sample_to_u16(g, bit_depth);
            output[output_index + 2] = scale_float_matrix_sample_to_u16(b, bit_depth);
            output[output_index + 3] = u16::MAX;
        }
    }
}

/// Get color matrix coefficients for YCbCr→RGB conversion.
///
/// Returns (cr_r, cb_g, cr_g, cb_b, y_bias, y_scale, rounding, shift_bits).
/// Full-range uses ×256 fixed-point, limited-range uses ×8192.
#[inline]
fn get_coefficients(
    full_range: bool,
    matrix_coeffs: u8,
) -> (i32, i32, i32, i32, i32, i32, i32, i32) {
    if full_range {
        let (cr_r, cb_g, cr_g, cb_b) = match matrix_coeffs {
            1 => (403, -48, -120, 475), // BT.709
            9 => (377, -42, -146, 482), // BT.2020
            _ => (359, -88, -183, 454), // BT.601
        };
        (cr_r, cb_g, cr_g, cb_b, 0, 256, 128, 8)
    } else {
        let (cr_r, cb_g, cr_g, cb_b) = match matrix_coeffs {
            1 => (14744, -1754, -4383, 17373), // BT.709
            9 => (13806, -1541, -5349, 17615), // BT.2020
            _ => (13126, -3222, -6686, 16591), // BT.601
        };
        (cr_r, cb_g, cr_g, cb_b, 16, 9576, 4096, 13)
    }
}

/// Convert 4:2:0 YCbCr planes to interleaved RGB bytes.
///
/// Dispatches to AVX2 when available, scalar fallback otherwise.
/// Writes exactly `(y_end - y_start) * (x_end - x_start) * 3` bytes to `rgb`.
#[allow(clippy::too_many_arguments)]
pub fn convert_420_to_rgb(
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    y_stride: usize,
    c_stride: usize,
    y_start: u32,
    y_end: u32,
    x_start: u32,
    x_end: u32,
    shift: u32,
    full_range: bool,
    matrix_coeffs: u8,
    rgb: &mut [u8],
) {
    incant!(
        convert_420_to_rgb(
            y_plane,
            cb_plane,
            cr_plane,
            y_stride,
            c_stride,
            y_start,
            y_end,
            x_start,
            x_end,
            shift,
            full_range,
            matrix_coeffs,
            rgb
        ),
        [v3]
    )
}

/// Scalar YCbCr→RGB conversion (fallback for all platforms)
#[allow(clippy::too_many_arguments)]
fn convert_420_to_rgb_scalar(
    _token: ScalarToken,
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    y_stride: usize,
    c_stride: usize,
    y_start: u32,
    y_end: u32,
    x_start: u32,
    x_end: u32,
    shift: u32,
    full_range: bool,
    matrix_coeffs: u8,
    rgb: &mut [u8],
) {
    let (cr_r, cb_g, cr_g, cb_b, y_bias, y_scale, rnd, shr) =
        get_coefficients(full_range, matrix_coeffs);

    let mut out_idx = 0;
    for y in y_start..y_end {
        let y_row = y as usize * y_stride;
        let c_row = (y as usize / 2) * c_stride;
        for x in x_start..x_end {
            let y_val = (y_plane[y_row + x as usize] >> shift) as i32;
            let cx = x as usize / 2;
            let c_idx = c_row + cx;
            let cb_val = (cb_plane[c_idx] >> shift) as i32;
            let cr_val = (cr_plane[c_idx] >> shift) as i32;

            let cb = cb_val - 128;
            let cr = cr_val - 128;
            let yv = (y_val - y_bias) * y_scale;
            let r = (yv + cr_r * cr + rnd) >> shr;
            let g = (yv + cb_g * cb + cr_g * cr + rnd) >> shr;
            let b = (yv + cb_b * cb + rnd) >> shr;

            rgb[out_idx] = r.clamp(0, 255) as u8;
            rgb[out_idx + 1] = g.clamp(0, 255) as u8;
            rgb[out_idx + 2] = b.clamp(0, 255) as u8;
            out_idx += 3;
        }
    }
}

/// AVX2 YCbCr→RGB conversion — processes 8 pixels per iteration
#[arcane]
#[allow(clippy::too_many_arguments)]
fn convert_420_to_rgb_v3(
    _token: X64V3Token,
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    y_stride: usize,
    c_stride: usize,
    y_start: u32,
    y_end: u32,
    x_start: u32,
    x_end: u32,
    shift: u32,
    full_range: bool,
    matrix_coeffs: u8,
    rgb: &mut [u8],
) {
    let (cr_r, cb_g, cr_g, cb_b, y_bias, y_scale, rnd, shr) =
        get_coefficients(full_range, matrix_coeffs);

    // Coefficient vectors (hoisted out of loop)
    let cr_r_v = _mm256_set1_epi32(cr_r);
    let cb_g_v = _mm256_set1_epi32(cb_g);
    let cr_g_v = _mm256_set1_epi32(cr_g);
    let cb_b_v = _mm256_set1_epi32(cb_b);
    let y_bias_v = _mm256_set1_epi32(y_bias);
    let y_scale_v = _mm256_set1_epi32(y_scale);
    let rnd_v = _mm256_set1_epi32(rnd);
    let bias128_v = _mm256_set1_epi32(128);
    let zero = _mm256_setzero_si256();
    let max255 = _mm256_set1_epi32(255);
    let shr_v = _mm_cvtsi32_si128(shr);
    let shift_v = _mm_cvtsi32_si128(shift as i32);
    let needs_shift = shift > 0;

    // Shuffle mask: interleave packed [R0..R3, G0..G3, B0..B3, 0000] per lane
    // into [R0,G0,B0, R1,G1,B1, R2,G2,B2, R3,G3,B3, 0000]
    let shuffle = _mm256_setr_epi8(
        0, 4, 8, 1, 5, 9, 2, 6, 10, 3, 7, 11, -1, -1, -1, -1, 0, 4, 8, 1, 5, 9, 2, 6, 10, 3, 7, 11,
        -1, -1, -1, -1,
    );

    // Align SIMD start to even x for 4:2:0 chroma alignment
    let x_simd_start = x_start.next_multiple_of(2);
    let row_pixels = x_end.saturating_sub(x_simd_start) as usize;
    let simd_count = (row_pixels / 8) * 8;
    let x_simd_end = x_simd_start + simd_count as u32;

    let mut out_idx = 0;

    for y in y_start..y_end {
        let y_row = y as usize * y_stride;
        let c_row = (y as usize / 2) * c_stride;

        // Scalar prefix: handle odd x_start (0 or 1 pixel)
        for x in x_start..x_simd_start.min(x_end) {
            scalar_pixel(
                y_plane,
                cb_plane,
                cr_plane,
                y_row,
                c_row,
                x as usize,
                shift,
                y_bias,
                y_scale,
                cr_r,
                cb_g,
                cr_g,
                cb_b,
                rnd,
                shr,
                rgb,
                &mut out_idx,
            );
        }

        // SIMD: 8 pixels per iteration
        let mut x = x_simd_start as usize;
        let x_end_simd = x_simd_end as usize;
        while x < x_end_simd {
            let cx = x / 2;

            // Load 8 Y values (u16) → zero-extend to 8×i32
            let y_arr: &[u16; 8] = (&y_plane[y_row + x..y_row + x + 8]).try_into().unwrap();
            let y_raw = _mm_loadu_si128(y_arr);
            let mut y_i32 = _mm256_cvtepu16_epi32(y_raw);

            // Load 4 Cb/Cr values, duplicate each for 4:2:0 → 8×i32
            let cb_arr: &[u16; 4] = (&cb_plane[c_row + cx..c_row + cx + 4]).try_into().unwrap();
            let cr_arr: &[u16; 4] = (&cr_plane[c_row + cx..c_row + cx + 4]).try_into().unwrap();
            let cb_raw = _mm_loadu_si64(cb_arr);
            let cr_raw = _mm_loadu_si64(cr_arr);
            let cb_dup = _mm_unpacklo_epi16(cb_raw, cb_raw);
            let cr_dup = _mm_unpacklo_epi16(cr_raw, cr_raw);
            let mut cb_i32 = _mm256_cvtepu16_epi32(cb_dup);
            let mut cr_i32 = _mm256_cvtepu16_epi32(cr_dup);

            // 10-bit → 8-bit shift
            if needs_shift {
                y_i32 = _mm256_srl_epi32(y_i32, shift_v);
                cb_i32 = _mm256_srl_epi32(cb_i32, shift_v);
                cr_i32 = _mm256_srl_epi32(cr_i32, shift_v);
            }

            // Fixed-point YCbCr → RGB
            let yv = _mm256_mullo_epi32(_mm256_sub_epi32(y_i32, y_bias_v), y_scale_v);
            let cb_adj = _mm256_sub_epi32(cb_i32, bias128_v);
            let cr_adj = _mm256_sub_epi32(cr_i32, bias128_v);

            let r = _mm256_sra_epi32(
                _mm256_add_epi32(
                    _mm256_add_epi32(yv, _mm256_mullo_epi32(cr_r_v, cr_adj)),
                    rnd_v,
                ),
                shr_v,
            );
            let g = _mm256_sra_epi32(
                _mm256_add_epi32(
                    _mm256_add_epi32(
                        _mm256_add_epi32(yv, _mm256_mullo_epi32(cb_g_v, cb_adj)),
                        _mm256_mullo_epi32(cr_g_v, cr_adj),
                    ),
                    rnd_v,
                ),
                shr_v,
            );
            let b = _mm256_sra_epi32(
                _mm256_add_epi32(
                    _mm256_add_epi32(yv, _mm256_mullo_epi32(cb_b_v, cb_adj)),
                    rnd_v,
                ),
                shr_v,
            );

            // Clamp [0, 255]
            let r = _mm256_min_epi32(_mm256_max_epi32(r, zero), max255);
            let g = _mm256_min_epi32(_mm256_max_epi32(g, zero), max255);
            let b = _mm256_min_epi32(_mm256_max_epi32(b, zero), max255);

            // Pack i32→i16→u8: each lane gets [r0-3, g0-3, b0-3, 0000]
            let rg = _mm256_packs_epi32(r, g);
            let bz = _mm256_packs_epi32(b, zero);
            let packed = _mm256_packus_epi16(rg, bz);
            let interleaved = _mm256_shuffle_epi8(packed, shuffle);

            // Extract 12 bytes from each 128-bit lane → 24 bytes total
            let mut buf = [0u8; 32];
            _mm256_storeu_si256(&mut buf, interleaved);
            rgb[out_idx..out_idx + 12].copy_from_slice(&buf[..12]);
            rgb[out_idx + 12..out_idx + 24].copy_from_slice(&buf[16..28]);
            out_idx += 24;

            x += 8;
        }

        // Scalar tail: remaining 0–7 pixels
        for x in x_simd_end..x_end {
            scalar_pixel(
                y_plane,
                cb_plane,
                cr_plane,
                y_row,
                c_row,
                x as usize,
                shift,
                y_bias,
                y_scale,
                cr_r,
                cb_g,
                cr_g,
                cb_b,
                rnd,
                shr,
                rgb,
                &mut out_idx,
            );
        }
    }
}

/// Convert a single 4:2:0 pixel (shared between SIMD prefix/tail and scalar path)
#[inline(always)]
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)] // only used from #[arcane] AVX2 path
fn scalar_pixel(
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    y_row: usize,
    c_row: usize,
    x: usize,
    shift: u32,
    y_bias: i32,
    y_scale: i32,
    cr_r: i32,
    cb_g: i32,
    cr_g: i32,
    cb_b: i32,
    rnd: i32,
    shr: i32,
    rgb: &mut [u8],
    out_idx: &mut usize,
) {
    let y_val = (y_plane[y_row + x] >> shift) as i32;
    let cx = x / 2;
    let c_idx = c_row + cx;
    let cb_val = (cb_plane[c_idx] >> shift) as i32;
    let cr_val = (cr_plane[c_idx] >> shift) as i32;

    let cb = cb_val - 128;
    let cr = cr_val - 128;
    let yv = (y_val - y_bias) * y_scale;
    let r = (yv + cr_r * cr + rnd) >> shr;
    let g = (yv + cb_g * cb + cr_g * cr + rnd) >> shr;
    let b = (yv + cb_b * cb + rnd) >> shr;

    rgb[*out_idx] = r.clamp(0, 255) as u8;
    rgb[*out_idx + 1] = g.clamp(0, 255) as u8;
    rgb[*out_idx + 2] = b.clamp(0, 255) as u8;
    *out_idx += 3;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_range_420_dispatch_matches_scalar_for_rgb_and_rgba() {
        let mut state = 0x7a31_4c95_u32;
        let mut sample = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 16) as u16
        };

        for (width, height) in [(1_usize, 1_usize), (7, 3), (8, 2), (9, 5), (24, 4)] {
            let chroma_width = width.div_ceil(2);
            let chroma_height = height.div_ceil(2);
            let y_plane = (0..width * height).map(|_| sample()).collect::<Vec<_>>();
            let cb_plane = (0..chroma_width * chroma_height)
                .map(|_| sample())
                .collect::<Vec<_>>();
            let cr_plane = (0..chroma_width * chroma_height)
                .map(|_| sample())
                .collect::<Vec<_>>();

            for channels in [3, 4] {
                let mut expected = vec![0_u8; width * height * channels];
                convert_420_8bit_region_to_interleaved_scalar(
                    ScalarToken,
                    &y_plane,
                    &cb_plane,
                    &cr_plane,
                    width,
                    chroma_width,
                    0,
                    0,
                    width,
                    height,
                    403,
                    -48,
                    -120,
                    475,
                    channels,
                    &mut expected,
                );

                let mut actual = vec![0_u8; expected.len()];
                convert_420_8bit_to_interleaved(
                    &y_plane,
                    &cb_plane,
                    &cr_plane,
                    width,
                    height,
                    chroma_width,
                    403,
                    -48,
                    -120,
                    475,
                    channels,
                    &mut actual,
                );
                assert_eq!(actual, expected, "{width}x{height}, {channels} channels");
            }
        }
    }

    #[test]
    fn full_range_420_cropped_region_preserves_odd_chroma_phase() {
        let y_stride = 19_usize;
        let source_height = 7_usize;
        let chroma_stride = y_stride.div_ceil(2);
        let mut state = 0x6d28_1b45_u32;
        let mut sample = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 16) as u16
        };
        let y_plane = (0..y_stride * source_height)
            .map(|_| sample())
            .collect::<Vec<_>>();
        let cb_plane = (0..chroma_stride * source_height.div_ceil(2))
            .map(|_| sample())
            .collect::<Vec<_>>();
        let cr_plane = (0..chroma_stride * source_height.div_ceil(2))
            .map(|_| sample())
            .collect::<Vec<_>>();
        let (x_start, y_start, width, height) = (1, 1, 17, 5);

        for channels in [3, 4] {
            let mut expected = vec![0_u8; width * height * channels];
            convert_420_8bit_region_to_interleaved_scalar(
                ScalarToken,
                &y_plane,
                &cb_plane,
                &cr_plane,
                y_stride,
                chroma_stride,
                x_start,
                y_start,
                width,
                height,
                403,
                -48,
                -120,
                475,
                channels,
                &mut expected,
            );

            let mut actual = vec![0_u8; expected.len()];
            convert_420_8bit_region_to_interleaved(
                &y_plane,
                &cb_plane,
                &cr_plane,
                y_stride,
                chroma_stride,
                x_start,
                y_start,
                width,
                height,
                403,
                -48,
                -120,
                475,
                channels,
                &mut actual,
            );
            assert_eq!(actual, expected, "{channels} channels");
        }
    }

    fn full_float_params(bit_depth: u8) -> FloatMatrixParams {
        FloatMatrixParams {
            y_offset: 0.0,
            y_scale: 1.0,
            chroma_midpoint: (1_u32 << (bit_depth - 1)) as f32,
            chroma_scale: 1.0,
            r_cr: 1.5748,
            g_cb: -0.187_324,
            g_cr: -0.468_124,
            b_cb: 1.8556,
        }
    }

    fn limited_float_params(bit_depth: u8) -> FloatMatrixParams {
        FloatMatrixParams {
            y_offset: (16_u32 << bit_depth.saturating_sub(8)) as f32,
            y_scale: 1.1689,
            chroma_midpoint: (1_u32 << (bit_depth - 1)) as f32,
            chroma_scale: 1.1429,
            r_cr: 1.402,
            g_cb: -0.344_136,
            g_cr: -0.714_136,
            b_cb: 1.772,
        }
    }

    #[test]
    fn float_matrix_8bit_dispatch_matches_scalar_for_layouts_ranges_and_tails() {
        let source_width = 23_usize;
        let source_height = 9_usize;
        let mut state = 0xa1d3_7e29_u32;
        let mut sample = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((state >> 16) & 255) as u16
        };
        let y_plane = (0..source_width * source_height)
            .map(|_| sample())
            .collect::<Vec<_>>();

        for (subsample_x, subsample_y) in [(2_usize, 2_usize), (2, 1), (1, 1)] {
            let chroma_stride = source_width.div_ceil(subsample_x);
            let chroma_height = source_height.div_ceil(subsample_y);
            let cb_plane = (0..chroma_stride * chroma_height)
                .map(|_| sample())
                .collect::<Vec<_>>();
            let cr_plane = (0..chroma_stride * chroma_height)
                .map(|_| sample())
                .collect::<Vec<_>>();

            for params in [full_float_params(8), limited_float_params(8)] {
                for width in [1_usize, 3, 4, 7, 8, 9, 15, 16, 17] {
                    let (x_start, y_start, height) = (1_usize, 1_usize, 7_usize);
                    for channels in [3_usize, 4_usize] {
                        let mut expected = vec![0_u8; width * height * channels];
                        convert_float_matrix_8bit_region_to_interleaved_scalar(
                            ScalarToken,
                            &y_plane,
                            &cb_plane,
                            &cr_plane,
                            source_width,
                            chroma_stride,
                            subsample_x,
                            subsample_y,
                            x_start,
                            y_start,
                            width,
                            height,
                            params,
                            channels,
                            &mut expected,
                        );
                        let mut actual = vec![0_u8; expected.len()];
                        convert_float_matrix_8bit_region_to_interleaved(
                            &y_plane,
                            &cb_plane,
                            &cr_plane,
                            source_width,
                            chroma_stride,
                            subsample_x,
                            subsample_y,
                            x_start,
                            y_start,
                            width,
                            height,
                            params,
                            channels,
                            &mut actual,
                        );
                        assert_eq!(
                            actual, expected,
                            "subsampling={subsample_x}x{subsample_y}, width={width}, channels={channels}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn float_matrix_rgba16_dispatch_matches_scalar_and_exact_bit_replication() {
        let source_width = 19_usize;
        let source_height = 8_usize;
        for bit_depth in [10_u8, 12_u8] {
            let sample_max = (1_u16 << bit_depth) - 1;
            let mut state = 0x59c8_0f13_u32 ^ u32::from(bit_depth);
            let mut sample = || {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((state >> 16) as u16) & sample_max
            };
            let y_plane = (0..source_width * source_height)
                .map(|_| sample())
                .collect::<Vec<_>>();

            for (subsample_x, subsample_y) in [(2_usize, 2_usize), (2, 1), (1, 1)] {
                let chroma_stride = source_width.div_ceil(subsample_x);
                let chroma_height = source_height.div_ceil(subsample_y);
                let cb_plane = (0..chroma_stride * chroma_height)
                    .map(|_| sample())
                    .collect::<Vec<_>>();
                let cr_plane = (0..chroma_stride * chroma_height)
                    .map(|_| sample())
                    .collect::<Vec<_>>();

                for params in [
                    full_float_params(bit_depth),
                    limited_float_params(bit_depth),
                ] {
                    for width in [1_usize, 3, 4, 5, 7, 8, 9, 13, 16, 17] {
                        let (x_start, y_start, height) = (1_usize, 1_usize, 6_usize);
                        let mut expected = vec![0_u16; width * height * 4];
                        convert_float_matrix_region_to_rgba16_scalar(
                            ScalarToken,
                            &y_plane,
                            &cb_plane,
                            &cr_plane,
                            source_width,
                            chroma_stride,
                            subsample_x,
                            subsample_y,
                            x_start,
                            y_start,
                            width,
                            height,
                            bit_depth,
                            params,
                            &mut expected,
                        );
                        let mut actual = vec![0_u16; expected.len()];
                        convert_float_matrix_region_to_rgba16(
                            &y_plane,
                            &cb_plane,
                            &cr_plane,
                            source_width,
                            chroma_stride,
                            subsample_x,
                            subsample_y,
                            x_start,
                            y_start,
                            width,
                            height,
                            bit_depth,
                            params,
                            &mut actual,
                        );
                        assert_eq!(
                            actual, expected,
                            "depth={bit_depth}, subsampling={subsample_x}x{subsample_y}, width={width}"
                        );
                        assert!(actual.chunks_exact(4).all(|pixel| pixel[3] == u16::MAX));
                    }
                }
            }

            for sample in [0_u16, 1, sample_max / 2, sample_max - 1, sample_max] {
                let shift = 16 - u32::from(bit_depth);
                let expected = ((u32::from(sample) << shift)
                    | (u32::from(sample) >> u32::from(bit_depth).saturating_sub(shift)))
                    as u16;
                assert_eq!(
                    scale_float_matrix_sample_to_u16(sample, bit_depth),
                    expected
                );
            }
        }
    }

    #[test]
    fn float_matrix_dispatch_falls_back_for_non_finite_and_extreme_coefficients() {
        let y_plane = [0_u16, 1, 16, 128, 254, 255, 65_534, 65_535];
        let cb_plane = [0_u16, 16, 128, 255, 512, 1023, 4095, 65_535];
        let cr_plane = [65_535_u16, 4095, 1023, 512, 255, 128, 16, 0];
        let mut variants = [full_float_params(8); 4];
        variants[0].r_cr = f32::NAN;
        variants[1].g_cb = f32::INFINITY;
        variants[2].g_cr = f32::NEG_INFINITY;
        variants[3].b_cb = f32::MAX;

        for params in variants {
            let mut expected = [0_u8; 8 * 4];
            convert_float_matrix_8bit_region_to_interleaved_scalar(
                ScalarToken,
                &y_plane,
                &cb_plane,
                &cr_plane,
                8,
                8,
                1,
                1,
                0,
                0,
                8,
                1,
                params,
                4,
                &mut expected,
            );
            let mut actual = [0_u8; 8 * 4];
            convert_float_matrix_8bit_region_to_interleaved(
                &y_plane,
                &cb_plane,
                &cr_plane,
                8,
                8,
                1,
                1,
                0,
                0,
                8,
                1,
                params,
                4,
                &mut actual,
            );
            assert_eq!(actual, expected);
        }

        #[cfg(target_arch = "aarch64")]
        {
            assert!(float_matrix_neon_safe(full_float_params(8)));
            assert!(float_matrix_neon_safe(limited_float_params(12)));
            assert!(!float_matrix_neon_safe(variants[0]));
            assert!(!float_matrix_neon_safe(variants[1]));
            assert!(!float_matrix_neon_safe(variants[2]));
            assert!(!float_matrix_neon_safe(variants[3]));
        }
    }
}
