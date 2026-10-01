//! Sample Adaptive Offset filter (H.265 Section 8.7.3)
//!
//! Applied after deblocking to reduce banding and ringing artifacts.
//! Two modes per CTB: Band Offset (BO) and Edge Offset (EO).

use alloc::vec;
use alloc::vec::Vec;

use super::picture::{DEBLOCK_FLAG_BYPASS, DecodedFrame};

/// Lookup for cu_transquant_bypass blocks in plane coordinates (H.265 8.7.3:
/// SAO leaves samples of lossless CUs unmodified).
struct BypassMap<'a> {
    flags: &'a [u8],
    stride: u32,
    sub_x: u32,
    sub_y: u32,
}

impl BypassMap<'_> {
    #[inline]
    fn is_bypass(&self, x: u32, y: u32) -> bool {
        let lx = x * self.sub_x;
        let ly = y * self.sub_y;
        let idx = ((ly / 4) * self.stride + lx / 4) as usize;
        self.flags
            .get(idx)
            .is_some_and(|f| f & DEBLOCK_FLAG_BYPASS != 0)
    }
}

/// SAO parameters for one CTB
#[derive(Clone, Copy, Debug, Default)]
pub struct SaoInfo {
    /// SAO type per component: 0=off, 1=band offset, 2=edge offset
    /// [0]=Y, [1]=Cb, [2]=Cr
    pub sao_type_idx: [u8; 3],
    /// Edge offset class per component (0-3, only used when type==2)
    /// 0=horizontal, 1=vertical, 2=diagonal 135°, 3=diagonal 45°
    pub sao_eo_class: [u8; 3],
    /// Band position per component (0-31, only used when type==1)
    pub sao_band_position: [u8; 3],
    /// Signed offset values per component, 4 values each
    /// For band offset: offsets for 4 consecutive bands starting at band_position
    /// For edge offset: offsets[0]=cat1(+), [1]=cat2(+), [2]=cat3(-), [3]=cat4(-)
    pub sao_offset_val: [[i8; 4]; 3],
}

/// SAO map for the entire frame, stored at CTB granularity
pub struct SaoMap {
    pub data: Vec<SaoInfo>,
    pub width_ctbs: u32,
    pub height_ctbs: u32,
}

impl SaoMap {
    pub fn new(width_ctbs: u32, height_ctbs: u32) -> super::Result<Self> {
        Ok(Self {
            data: super::allocation::filled(
                SaoInfo::default(),
                (width_ctbs * height_ctbs) as usize,
            )?,
            width_ctbs,
            height_ctbs,
        })
    }

    #[inline]
    pub fn get(&self, ctb_x: u32, ctb_y: u32) -> &SaoInfo {
        &self.data[((ctb_y % self.height_ctbs) * self.width_ctbs + ctb_x) as usize]
    }

    #[inline]
    pub fn get_mut(&mut self, ctb_x: u32, ctb_y: u32) -> &mut SaoInfo {
        &mut self.data[((ctb_y % self.height_ctbs) * self.width_ctbs + ctb_x) as usize]
    }
}

/// Edge offset direction lookup: (dx0, dy0, dx1, dy1) for each eo_class
/// eo_class 0: horizontal (left, right)
/// eo_class 1: vertical (above, below)
/// eo_class 2: diagonal 135° (upper-left, lower-right)
/// eo_class 3: diagonal 45° (upper-right, lower-left)
const EO_OFFSETS: [(i32, i32, i32, i32); 4] = [
    (-1, 0, 1, 0),  // class 0: horizontal
    (0, -1, 0, 1),  // class 1: vertical
    (-1, -1, 1, 1), // class 2: 135° diagonal
    (1, -1, -1, 1), // class 3: 45° diagonal
];

/// Apply SAO filter to the entire frame
pub fn apply_sao(frame: &mut DecodedFrame, sao_map: &SaoMap, ctb_size: u32) -> super::Result<()> {
    let width = frame.width;
    let height = frame.height;
    let bit_depth = frame.bit_depth;

    // Only clone planes that have edge offset (type 2), since edge offset
    // reads neighbors that may be modified. Band offset (type 1) is in-place.
    let mut need_y_clone = false;
    let mut need_cb_clone = false;
    let mut need_cr_clone = false;
    for sao in &sao_map.data {
        if sao.sao_type_idx[0] == 2 {
            need_y_clone = true;
        }
        if sao.sao_type_idx[1] == 2 {
            need_cb_clone = true;
        }
        if sao.sao_type_idx[2] == 2 {
            need_cr_clone = true;
        }
        if need_y_clone && need_cb_clone && need_cr_clone {
            break;
        }
    }

    let orig_y = if need_y_clone {
        super::allocation::copy(&frame.y_plane)?
    } else {
        Vec::new()
    };
    let orig_cb = if need_cb_clone {
        super::allocation::copy(&frame.cb_plane)?
    } else {
        Vec::new()
    };
    let orig_cr = if need_cr_clone {
        super::allocation::copy(&frame.cr_plane)?
    } else {
        Vec::new()
    };

    let y_stride = frame.y_stride();
    let c_stride = frame.c_stride();

    let (sub_x, sub_y) = match frame.chroma_format {
        1 => (2u32, 2u32),
        2 => (2, 1),
        3 => (1, 1),
        _ => (1, 1),
    };

    // Snapshot the bypass flags so lossless CUs can be exempted while the
    // sample planes are mutably borrowed. Empty when no CU uses bypass.
    let bypass_flags: Vec<u8> = if frame.has_bypass_blocks() {
        super::allocation::copy(&frame.deblock_flags)?
    } else {
        Vec::new()
    };
    let bypass_stride = frame.deblock_stride;
    let luma_bypass = (!bypass_flags.is_empty()).then_some(BypassMap {
        flags: &bypass_flags,
        stride: bypass_stride,
        sub_x: 1,
        sub_y: 1,
    });
    let chroma_bypass = (!bypass_flags.is_empty()).then_some(BypassMap {
        flags: &bypass_flags,
        stride: bypass_stride,
        sub_x,
        sub_y,
    });

    // Process each CTB
    for ctb_y in 0..sao_map.height_ctbs {
        for ctb_x in 0..sao_map.width_ctbs {
            let sao = sao_map.get(ctb_x, ctb_y);
            let ctb_x_px = ctb_x * ctb_size;
            let ctb_y_px = ctb_y * ctb_size;

            // Luma
            match sao.sao_type_idx[0] {
                1 => {
                    let x_end = (ctb_x_px + ctb_size).min(width);
                    let y_end = (ctb_y_px + ctb_size).min(height);
                    apply_sao_band_inplace(
                        &mut frame.y_plane,
                        y_stride as u32,
                        ctb_x_px,
                        ctb_y_px,
                        x_end,
                        y_end,
                        sao.sao_band_position[0],
                        &sao.sao_offset_val[0],
                        bit_depth,
                        luma_bypass.as_ref(),
                    );
                }
                2 => {
                    let x_end = (ctb_x_px + ctb_size).min(width);
                    let y_end = (ctb_y_px + ctb_size).min(height);
                    apply_sao_edge(
                        &orig_y,
                        &mut frame.y_plane,
                        y_stride as u32,
                        width,
                        height,
                        ctb_x_px,
                        ctb_y_px,
                        x_end,
                        y_end,
                        sao.sao_eo_class[0],
                        &sao.sao_offset_val[0],
                        bit_depth,
                        luma_bypass.as_ref(),
                    );
                }
                _ => {}
            }

            // Chroma (4:2:0: halved coordinates)
            if frame.chroma_format > 0 {
                let cx_start = ctb_x_px / sub_x;
                let cy_start = ctb_y_px / sub_y;
                let cx_end = ((ctb_x_px + ctb_size) / sub_x).min(width / sub_x);
                let cy_end = ((ctb_y_px + ctb_size) / sub_y).min(height / sub_y);
                let c_w = width / sub_x;
                let c_h = height / sub_y;

                // Cb
                match sao.sao_type_idx[1] {
                    1 => {
                        apply_sao_band_inplace(
                            &mut frame.cb_plane,
                            c_stride as u32,
                            cx_start,
                            cy_start,
                            cx_end,
                            cy_end,
                            sao.sao_band_position[1],
                            &sao.sao_offset_val[1],
                            bit_depth,
                            chroma_bypass.as_ref(),
                        );
                    }
                    2 => {
                        apply_sao_edge(
                            &orig_cb,
                            &mut frame.cb_plane,
                            c_stride as u32,
                            c_w,
                            c_h,
                            cx_start,
                            cy_start,
                            cx_end,
                            cy_end,
                            sao.sao_eo_class[1],
                            &sao.sao_offset_val[1],
                            bit_depth,
                            chroma_bypass.as_ref(),
                        );
                    }
                    _ => {}
                }

                // Cr
                match sao.sao_type_idx[2] {
                    1 => {
                        apply_sao_band_inplace(
                            &mut frame.cr_plane,
                            c_stride as u32,
                            cx_start,
                            cy_start,
                            cx_end,
                            cy_end,
                            sao.sao_band_position[2],
                            &sao.sao_offset_val[2],
                            bit_depth,
                            chroma_bypass.as_ref(),
                        );
                    }
                    2 => {
                        apply_sao_edge(
                            &orig_cr,
                            &mut frame.cr_plane,
                            c_stride as u32,
                            c_w,
                            c_h,
                            cx_start,
                            cy_start,
                            cx_end,
                            cy_end,
                            sao.sao_eo_class[2],
                            &sao.sao_offset_val[2],
                            bit_depth,
                            chroma_bypass.as_ref(),
                        );
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// Apply SAO edge offset to a single pixel with bounds checking
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn apply_sao_edge_pixel(
    src: &[u16],
    dst: &mut [u16],
    row: usize,
    x: u32,
    dx0: i32,
    dy0: i32,
    dx1: i32,
    dy1: i32,
    stride: u32,
    plane_w: u32,
    plane_h: u32,
    max_val: i32,
    offset_table: &[i32; 5],
) {
    let nx0 = x as i32 + dx0;
    let ny0 = (row / stride as usize) as i32 + dy0;
    let nx1 = x as i32 + dx1;
    let ny1 = (row / stride as usize) as i32 + dy1;

    if nx0 < 0
        || nx0 >= plane_w as i32
        || ny0 < 0
        || ny0 >= plane_h as i32
        || nx1 < 0
        || nx1 >= plane_w as i32
        || ny1 < 0
        || ny1 >= plane_h as i32
    {
        return;
    }

    let idx = row + x as usize;
    let sample = src[idx] as i32;
    let n0 = src[(ny0 as u32 * stride + nx0 as u32) as usize] as i32;
    let n1 = src[(ny1 as u32 * stride + nx1 as u32) as usize] as i32;

    let sign0 = (sample - n0).signum();
    let sign1 = (sample - n1).signum();
    let edge_idx = (2 + sign0 + sign1) as usize;

    let offset = offset_table[edge_idx];
    if offset != 0 {
        dst[idx] = (sample + offset).clamp(0, max_val) as u16;
    }
}

/// Apply SAO band offset in-place (type 1). Reads and writes same buffer.
#[allow(clippy::too_many_arguments)]
fn apply_sao_band_inplace(
    plane: &mut [u16],
    stride: u32,
    x_start: u32,
    y_start: u32,
    x_end: u32,
    y_end: u32,
    band_position: u8,
    offsets: &[i8; 4],
    bit_depth: u8,
    bypass: Option<&BypassMap<'_>>,
) {
    let max_val = (1i32 << bit_depth) - 1;
    let band_shift = bit_depth - 5;

    // Build lookup table for the 32 bands
    let mut band_table = [0i8; 32];
    for k in 0..4u8 {
        let band_idx = (band_position + k) & 31;
        band_table[band_idx as usize] = offsets[k as usize];
    }

    for y in y_start..y_end {
        let row = (y * stride) as usize;
        for x in x_start..x_end {
            if bypass.is_some_and(|b| b.is_bypass(x, y)) {
                continue;
            }
            let idx = row + x as usize;
            let sample = (plane[idx] as i32).min(max_val);
            let band = (sample >> band_shift) as usize;
            let offset = band_table[band] as i32;
            if offset != 0 {
                plane[idx] = (sample + offset).clamp(0, max_val) as u16;
            }
        }
    }
}

/// Apply SAO edge offset (type 2). Reads from pre-cloned src, writes to dst.
#[allow(clippy::too_many_arguments)]
fn apply_sao_edge(
    src: &[u16],
    dst: &mut [u16],
    stride: u32,
    plane_w: u32,
    plane_h: u32,
    x_start: u32,
    y_start: u32,
    x_end: u32,
    y_end: u32,
    eo_class: u8,
    offsets: &[i8; 4],
    bit_depth: u8,
    bypass: Option<&BypassMap<'_>>,
) {
    let max_val = (1i32 << bit_depth) - 1;
    let (dx0, dy0, dx1, dy1) = EO_OFFSETS[eo_class as usize & 3];

    let offset_table: [i32; 5] = [
        offsets[0] as i32,
        offsets[1] as i32,
        0,
        -(offsets[2] as i32),
        -(offsets[3] as i32),
    ];

    // Compute safe interior bounds where neighbor access never goes out of frame.
    let safe_x_start = x_start.max((-dx0).max(-dx1).max(0) as u32);
    let safe_x_end = x_end.min(plane_w - dx0.max(dx1).max(0) as u32);
    let safe_y_start = y_start.max((-dy0).max(-dy1).max(0) as u32);
    let safe_y_end = y_end.min(plane_h - dy0.max(dy1).max(0) as u32);

    let stride_u = stride as usize;
    let dx0_u = dx0 as isize;
    let dy0_s = dy0 as isize * stride_u as isize;
    let dx1_u = dx1 as isize;
    let dy1_s = dy1 as isize * stride_u as isize;

    // Interior: no bounds checks needed
    for y in safe_y_start..safe_y_end {
        let row = y as usize * stride_u;
        for x in safe_x_start..safe_x_end {
            if bypass.is_some_and(|b| b.is_bypass(x, y)) {
                continue;
            }
            let idx = row + x as usize;
            let sample = src[idx] as i32;
            let n0_idx = (idx as isize + dy0_s + dx0_u) as usize;
            let n1_idx = (idx as isize + dy1_s + dx1_u) as usize;
            let n0 = src[n0_idx] as i32;
            let n1 = src[n1_idx] as i32;

            let sign0 = (sample - n0).signum();
            let sign1 = (sample - n1).signum();
            let edge_idx = (2 + sign0 + sign1) as usize;

            let offset = offset_table[edge_idx];
            if offset != 0 {
                dst[idx] = (sample + offset).clamp(0, max_val) as u16;
            }
        }
    }

    // Border rows/columns: with bounds checks
    for y in y_start..y_end {
        if y >= safe_y_start && y < safe_y_end {
            let row = y as usize * stride_u;
            for x in x_start..safe_x_start.min(x_end) {
                if bypass.is_some_and(|b| b.is_bypass(x, y)) {
                    continue;
                }
                apply_sao_edge_pixel(
                    src,
                    dst,
                    row,
                    x,
                    dx0,
                    dy0,
                    dx1,
                    dy1,
                    stride,
                    plane_w,
                    plane_h,
                    max_val,
                    &offset_table,
                );
            }
            for x in safe_x_end.max(x_start)..x_end {
                if bypass.is_some_and(|b| b.is_bypass(x, y)) {
                    continue;
                }
                apply_sao_edge_pixel(
                    src,
                    dst,
                    row,
                    x,
                    dx0,
                    dy0,
                    dx1,
                    dy1,
                    stride,
                    plane_w,
                    plane_h,
                    max_val,
                    &offset_table,
                );
            }
        } else {
            let row = y as usize * stride_u;
            for x in x_start..x_end {
                if bypass.is_some_and(|b| b.is_bypass(x, y)) {
                    continue;
                }
                apply_sao_edge_pixel(
                    src,
                    dst,
                    row,
                    x,
                    dx0,
                    dy0,
                    dx1,
                    dy1,
                    stride,
                    plane_w,
                    plane_h,
                    max_val,
                    &offset_table,
                );
            }
        }
    }
}

pub(crate) fn write_filtered_rows(
    frame: &DecodedFrame,
    map: &SaoMap,
    ctb_size: u32,
    start: u32,
    rows: u32,
    output: &mut DecodedFrame,
) {
    let bypass = frame.has_bypass_blocks();
    let maximum = (1i32 << frame.bit_depth) - 1;
    for component in 0..3 {
        let sub = if component == 0 { 1 } else { 2 };
        let origin = frame.plane_origin(component);
        let (source, stride) = frame.plane(component);
        let (destination, _) = output.plane_mut(component);
        let width = frame.width / sub;
        let height = frame.height / sub;
        let first_y = start / sub;
        let last_y = (start + rows) / sub;
        for ctb_x in 0..map.width_ctbs {
            let first_x = ctb_x * ctb_size / sub;
            let last_x = ((ctb_x + 1) * ctb_size / sub).min(width);
            let info = map.get(ctb_x, start / ctb_size);
            let c = component as usize;
            let offsets = info.sao_offset_val[c];
            for y in first_y..last_y {
                let input = (y - origin) as usize * stride + first_x as usize;
                let target = (y - first_y) as usize * stride + first_x as usize;
                let count = (last_x - first_x) as usize;
                destination[target..target + count].copy_from_slice(&source[input..input + count]);
            }
            match info.sao_type_idx[c] {
                1 => {
                    let mut table = [0i32; 32];
                    for (i, &offset) in offsets.iter().enumerate() {
                        table[(usize::from(info.sao_band_position[c]) + i) & 31] =
                            i32::from(offset);
                    }
                    for y in first_y..last_y {
                        let input = (y - origin) as usize * stride;
                        let target = (y - first_y) as usize * stride;
                        for x in first_x..last_x {
                            if bypass && frame.is_block_bypass(x * sub, y * sub) {
                                continue;
                            }
                            let sample = i32::from(source[input + x as usize]);
                            let offset = table[(sample >> (frame.bit_depth - 5)) as usize];
                            destination[target + x as usize] =
                                (sample + offset).clamp(0, maximum) as u16;
                        }
                    }
                }
                2 => {
                    let (dx0, dy0, dx1, dy1) = EO_OFFSETS[info.sao_eo_class[c] as usize & 3];
                    let x0 = first_x.max((-dx0).max(-dx1).max(0) as u32);
                    let x1 = last_x.min(width - dx0.max(dx1).max(0) as u32);
                    let y0 = first_y.max((-dy0).max(-dy1).max(0) as u32);
                    let y1 = last_y.min(height - dy0.max(dy1).max(0) as u32);
                    let table = [
                        i32::from(offsets[0]),
                        i32::from(offsets[1]),
                        0,
                        -i32::from(offsets[2]),
                        -i32::from(offsets[3]),
                    ];
                    for y in y0..y1 {
                        let input = (y - origin) as usize * stride;
                        let target = (y - first_y) as usize * stride;
                        let a = ((y as i32 + dy0 - origin as i32) as usize * stride) as isize
                            + dx0 as isize;
                        let b = ((y as i32 + dy1 - origin as i32) as usize * stride) as isize
                            + dx1 as isize;
                        if !bypass {
                            let first = x0 as usize;
                            let last = x1 as usize;
                            edge_row(
                                &source[input + first..input + last],
                                &source
                                    [(a + first as isize) as usize..(a + last as isize) as usize],
                                &source
                                    [(b + first as isize) as usize..(b + last as isize) as usize],
                                &mut destination[target + first..target + last],
                                offsets,
                                maximum,
                            );
                            continue;
                        }
                        for x in x0..x1 {
                            if bypass && frame.is_block_bypass(x * sub, y * sub) {
                                continue;
                            }
                            let sample = i32::from(source[input + x as usize]);
                            let left = i32::from(source[(a + x as isize) as usize]);
                            let right = i32::from(source[(b + x as isize) as usize]);
                            let category =
                                (2 + (sample - left).signum() + (sample - right).signum()) as usize;
                            destination[target + x as usize] =
                                (sample + table[category]).clamp(0, maximum) as u16;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn edge_row(
    input: &[u16],
    a: &[u16],
    b: &[u16],
    output: &mut [u16],
    offsets: [i8; 4],
    maximum: i32,
) {
    #[cfg(target_arch = "aarch64")]
    let done = {
        use archmage::SimdToken;
        archmage::NeonToken::summon()
            .filter(|_| maximum <= 255)
            .map_or(0, |token| {
                edge_row_neon(token, input, a, b, output, offsets, maximum as i16)
            })
    };
    #[cfg(not(target_arch = "aarch64"))]
    let done = 0;
    let table = [
        i32::from(offsets[0]),
        i32::from(offsets[1]),
        0,
        -i32::from(offsets[2]),
        -i32::from(offsets[3]),
    ];
    for i in done..output.len() {
        let sample = i32::from(input[i]);
        let category =
            2 + (sample - i32::from(a[i])).signum() + (sample - i32::from(b[i])).signum();
        output[i] = (sample + table[category as usize]).clamp(0, maximum) as u16;
    }
}

#[cfg(target_arch = "aarch64")]
#[archmage::arcane]
fn edge_row_neon(
    _token: archmage::NeonToken,
    input: &[u16],
    a: &[u16],
    b: &[u16],
    output: &mut [u16],
    offsets: [i8; 4],
    maximum: i16,
) -> usize {
    use core::arch::aarch64::*;
    use safe_unaligned_simd::aarch64::{vld1q_u16, vst1q_u16};
    let zero = vdupq_n_s16(0);
    let maximum = vdupq_n_s16(maximum);
    let values = [
        vdupq_n_s16(i16::from(offsets[0])),
        vdupq_n_s16(i16::from(offsets[1])),
        vdupq_n_s16(-i16::from(offsets[2])),
        vdupq_n_s16(-i16::from(offsets[3])),
    ];
    let mut done = 0;
    for chunk in output.chunks_exact_mut(8) {
        let sample = vld1q_u16(input[done..done + 8].try_into().unwrap());
        let left = vld1q_u16(a[done..done + 8].try_into().unwrap());
        let right = vld1q_u16(b[done..done + 8].try_into().unwrap());
        let sign0 = vsubq_u16(vcltq_u16(sample, left), vcgtq_u16(sample, left));
        let sign1 = vsubq_u16(vcltq_u16(sample, right), vcgtq_u16(sample, right));
        let category = vreinterpretq_s16_u16(vaddq_u16(sign0, sign1));
        let mut offset = zero;
        for (value, index) in values.iter().zip([-2, -1, 1, 2]) {
            offset = vbslq_s16(vceqq_s16(category, vdupq_n_s16(index)), *value, offset);
        }
        let result = vminq_s16(
            maximum,
            vmaxq_s16(zero, vaddq_s16(vreinterpretq_s16_u16(sample), offset)),
        );
        vst1q_u16(chunk.try_into().unwrap(), vreinterpretq_u16_s16(result));
        done += 8;
    }
    done
}

#[cfg(test)]
mod incremental_tests {
    use super::edge_row;

    #[test]
    fn edge_rows_match_scalar_categories_clipping_and_tails() {
        for maximum in [255, 1023] {
            for length in 0..=67 {
                let input: Vec<u16> = (0..length)
                    .map(|i| match i % 7 {
                        0 => 0,
                        1 => maximum as u16,
                        _ => ((i * 71) % (maximum as usize + 1)) as u16,
                    })
                    .collect();
                let a: Vec<u16> = input
                    .iter()
                    .enumerate()
                    .map(|(i, &v)| match i % 3 {
                        0 => v.saturating_sub(1),
                        1 => v,
                        _ => v.saturating_add(1).min(maximum as u16),
                    })
                    .collect();
                let b: Vec<u16> = input
                    .iter()
                    .enumerate()
                    .map(|(i, &v)| match i / 3 % 3 {
                        0 => v.saturating_sub(2),
                        1 => v,
                        _ => v.saturating_add(2).min(maximum as u16),
                    })
                    .collect();
                for offsets in [[0, 0, 0, 0], [1, 2, 3, 4], [7, 6, 5, 4], [-7, 7, -7, 7]] {
                    let table = [
                        i32::from(offsets[0]),
                        i32::from(offsets[1]),
                        0,
                        -i32::from(offsets[2]),
                        -i32::from(offsets[3]),
                    ];
                    let expected: Vec<u16> = input
                        .iter()
                        .zip(&a)
                        .zip(&b)
                        .map(|((&v, &a), &b)| {
                            let category = 2
                                + (i32::from(v) - i32::from(a)).signum()
                                + (i32::from(v) - i32::from(b)).signum();
                            (i32::from(v) + table[category as usize]).clamp(0, maximum) as u16
                        })
                        .collect();
                    let mut actual = vec![0; length];
                    edge_row(&input, &a, &b, &mut actual, offsets, maximum);
                    assert_eq!(actual, expected);
                }
            }
        }
    }
}
