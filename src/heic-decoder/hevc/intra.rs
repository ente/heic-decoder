use super::picture::{DecodedFrame, UNINIT_SAMPLE};
use super::slice::IntraPredMode;

const MAX_INTRA_PRED_BLOCK_SIZE: usize = 32;

pub static INTRA_PRED_ANGLE: [i16; 35] = [
    0, 0, 32, 26, 21, 17, 13, 9, 5, 2, 0, -2, -5, -9, -13, -17, -21, -26, -32, -26, -21, -17, -13,
    -9, -5, -2, 0, 2, 5, 9, 13, 17, 21, 26, 32,
];

pub static INV_ANGLE: [i32; 15] = [
    -4096, -1638, -910, -630, -482, -390, -315, -256, -315, -390, -482, -630, -910, -1638, -4096,
];

fn get_inv_angle(mode: u8) -> i32 {
    if (11..=25).contains(&mode) {
        INV_ANGLE[(mode - 11) as usize]
    } else {
        0
    }
}

pub fn predict_intra(
    frame: &mut DecodedFrame,
    x: u32,
    y: u32,
    log2_size: u8,
    mode: IntraPredMode,
    c_idx: u8,
    strong_intra_smoothing_enabled: bool,
) {
    let size = 1u32 << log2_size;
    let bit_depth = frame.bit_depth;
    let chroma_format = frame.chroma_format;

    let mut border = [0i32; 4 * MAX_INTRA_PRED_BLOCK_SIZE + 1];
    let border_center = 2 * MAX_INTRA_PRED_BLOCK_SIZE;

    fill_border_samples(frame, x, y, size, c_idx, &mut border, border_center);

    if c_idx == 0 || chroma_format == 3 {
        intra_prediction_sample_filtering(
            &mut border,
            border_center,
            size as usize,
            c_idx,
            mode.as_u8(),
            strong_intra_smoothing_enabled,
            bit_depth as usize,
        );
    }

    let y = y - frame.plane_origin(c_idx);
    let (plane, stride) = frame.plane_mut(c_idx);
    let max_val = (1i32 << bit_depth) - 1;

    match mode {
        IntraPredMode::Planar => {
            predict_planar(
                plane,
                stride,
                x,
                y,
                size,
                log2_size,
                max_val,
                &border,
                border_center,
            );
        }
        IntraPredMode::Dc => {
            predict_dc(
                plane,
                stride,
                x,
                y,
                size,
                log2_size,
                c_idx,
                max_val,
                &border,
                border_center,
            );
        }
        _ => {
            let mode_val = mode.as_u8();
            predict_angular(
                plane,
                stride,
                x,
                y,
                size,
                c_idx,
                mode_val,
                max_val,
                &border,
                border_center,
            );
        }
    }
}

fn intra_prediction_sample_filtering(
    border: &mut [i32],
    center: usize,
    n_t: usize,
    c_idx: u8,
    intra_pred_mode: u8,
    strong_intra_smoothing_enabled: bool,
    bit_depth: usize,
) {
    let filter_flag = if intra_pred_mode == 1 || n_t == 4 {
        false
    } else {
        let min_dist_ver_hor = (intra_pred_mode as i32 - 26)
            .abs()
            .min((intra_pred_mode as i32 - 10).abs());

        match n_t {
            8 => min_dist_ver_hor > 7,
            16 => min_dist_ver_hor > 1,
            32 => min_dist_ver_hor > 0,
            _ => false,
        }
    };

    if !filter_flag {
        return;
    }

    let bi_int_flag = strong_intra_smoothing_enabled
        && c_idx == 0
        && n_t == 32
        && (border[center] + border[center + 64] - 2 * border[center + 32]).abs()
            < (1 << (bit_depth - 5))
        && (border[center] + border[center.wrapping_sub(64)] - 2 * border[center.wrapping_sub(32)])
            .abs()
            < (1 << (bit_depth - 5));

    let mut pf = [0i32; 4 * MAX_INTRA_PRED_BLOCK_SIZE + 1];
    let pf_center = 2 * MAX_INTRA_PRED_BLOCK_SIZE;

    if bi_int_flag {
        pf[pf_center - 2 * n_t] = border[center - 2 * n_t];
        pf[pf_center + 2 * n_t] = border[center + 2 * n_t];
        pf[pf_center] = border[center];

        let p0 = border[center];
        let p_neg64 = border[center - 64];
        let p_pos64 = border[center + 64];

        for i in 1..64i32 {
            pf[(pf_center as i32 - i) as usize] = p0 + ((i * (p_neg64 - p0) + 32) >> 6);
            pf[(pf_center as i32 + i) as usize] = p0 + ((i * (p_pos64 - p0) + 32) >> 6);
        }
    } else {
        pf[pf_center - 2 * n_t] = border[center - 2 * n_t];
        pf[pf_center + 2 * n_t] = border[center + 2 * n_t];

        for i in -(2 * n_t as i32 - 1)..=(2 * n_t as i32 - 1) {
            let idx = (center as i32 + i) as usize;
            pf[(pf_center as i32 + i) as usize] =
                (border[idx + 1] + 2 * border[idx] + border[idx - 1] + 2) >> 2;
        }
    }

    for i in 0..=(4 * n_t) {
        border[center - 2 * n_t + i] = pf[pf_center - 2 * n_t + i];
    }
}

#[inline(always)]
fn write_sample(plane: &mut [u16], stride: usize, x: u32, y: u32, value: u16) {
    let idx = y as usize * stride + x as usize;
    if idx < plane.len() {
        plane[idx] = value;
    }
}

#[inline(always)]
fn read_plane(plane: &[u16], stride: usize, x: u32, y: u32) -> u16 {
    let idx = y as usize * stride + x as usize;
    if idx < plane.len() { plane[idx] } else { 0 }
}

fn fill_border_samples(
    frame: &DecodedFrame,
    x: u32,
    y: u32,
    size: u32,
    c_idx: u8,
    border: &mut [i32],
    center: usize,
) {
    let mut avail = [false; 4 * MAX_INTRA_PRED_BLOCK_SIZE + 1];
    let mut avail_count = 0u32;
    let total_samples = 4 * size + 1;

    let (plane, stride) = frame.plane(c_idx);

    let (frame_w, frame_h) = if c_idx == 0 {
        (frame.width, frame.height)
    } else {
        match frame.chroma_format {
            3 => (frame.width, frame.height),
            2 => (frame.width / 2, frame.height),
            _ => (frame.width / 2, frame.height / 2),
        }
    };

    let origin = frame.plane_origin(c_idx);
    let y = y - origin;
    let frame_h = frame_h - origin;
    let avail_left = x > 0;
    let avail_top = y > 0;
    let avail_top_left = avail_left && avail_top;

    let default_val = 1i32 << (frame.bit_depth - 1);

    let mut corner_avail = false;
    if avail_top_left {
        let raw = read_plane(plane, stride, x - 1, y - 1);
        if raw != UNINIT_SAMPLE {
            border[center] = raw as i32;
            corner_avail = true;
            avail_count += 1;
        }
    }
    if !corner_avail && avail_top {
        let raw = read_plane(plane, stride, x, y - 1);
        if raw != UNINIT_SAMPLE {
            border[center] = raw as i32;
            corner_avail = true;
            avail_count += 1;
        }
    }
    if !corner_avail && avail_left {
        let raw = read_plane(plane, stride, x - 1, y);
        if raw != UNINIT_SAMPLE {
            border[center] = raw as i32;
            corner_avail = true;
            avail_count += 1;
        }
    }
    if !corner_avail {
        border[center] = default_val;
    }
    avail[center] = corner_avail;

    if avail_top {
        let top_row_start = (y - 1) as usize * stride;
        let top_count = (2 * size).min(frame_w.saturating_sub(x)) as usize;
        let top_row_end = top_row_start + x as usize + top_count;
        if top_row_end <= plane.len() {
            let row_base = top_row_start + x as usize;
            for i in 0..top_count {
                let raw = plane[row_base + i];
                if raw != UNINIT_SAMPLE {
                    let idx = center + 1 + i;
                    border[idx] = raw as i32;
                    avail[idx] = true;
                    avail_count += 1;
                }
            }
        } else {
            for i in 0..top_count {
                let plane_idx = top_row_start + x as usize + i;
                if plane_idx < plane.len() {
                    let raw = plane[plane_idx];
                    if raw != UNINIT_SAMPLE {
                        let idx = center + 1 + i;
                        border[idx] = raw as i32;
                        avail[idx] = true;
                        avail_count += 1;
                    }
                }
            }
        }
    }

    if avail_left {
        let left_x = (x - 1) as usize;
        let left_count = (2 * size).min(frame_h.saturating_sub(y)) as usize;
        let last_left_idx = (y as usize + left_count.saturating_sub(1)) * stride + left_x;
        if last_left_idx < plane.len() {
            for i in 0..left_count {
                let raw = plane[(y as usize + i) * stride + left_x];
                if raw != UNINIT_SAMPLE {
                    let idx = center - 1 - i;
                    border[idx] = raw as i32;
                    avail[idx] = true;
                    avail_count += 1;
                }
            }
        } else {
            for i in 0..left_count {
                let plane_idx = (y as usize + i) * stride + left_x;
                if plane_idx < plane.len() {
                    let raw = plane[plane_idx];
                    if raw != UNINIT_SAMPLE {
                        let idx = center - 1 - i;
                        border[idx] = raw as i32;
                        avail[idx] = true;
                        avail_count += 1;
                    }
                }
            }
        }
    }

    if avail_count < total_samples {
        reference_sample_substitution(border, &avail, center, size as usize, default_val);
    }
}

fn reference_sample_substitution(
    border: &mut [i32],
    avail: &[bool],
    center: usize,
    size: usize,
    default_val: i32,
) {
    let mut first_avail_val = None;

    for i in (0..(2 * size)).rev() {
        let idx = center - 1 - i;
        if avail[idx] {
            first_avail_val = Some(border[idx]);
            break;
        }
    }

    if first_avail_val.is_none() && avail[center] {
        first_avail_val = Some(border[center]);
    }

    if first_avail_val.is_none() {
        for i in 0..(2 * size) {
            let idx = center + 1 + i;
            if avail[idx] {
                first_avail_val = Some(border[idx]);
                break;
            }
        }
    }

    let first_val = first_avail_val.unwrap_or(default_val);

    let mut current = first_val;

    for i in (0..(2 * size)).rev() {
        let idx = center - 1 - i;
        if avail[idx] {
            current = border[idx];
        } else {
            border[idx] = current;
        }
    }

    if avail[center] {
        current = border[center];
    } else {
        border[center] = current;
    }

    for i in 0..(2 * size) {
        let idx = center + 1 + i;
        if avail[idx] {
            current = border[idx];
        } else {
            border[idx] = current;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn predict_planar(
    plane: &mut [u16],
    stride: usize,
    x: u32,
    y: u32,
    size: u32,
    log2_size: u8,
    max_val: i32,
    border: &[i32],
    center: usize,
) {
    let n = size as usize;
    let n_i = size as i32;
    let right = border[center + 1 + n];
    let bottom = border[center - 1 - n];

    let last_row_end = (y as usize + n - 1) * stride + x as usize + n;
    let block_fits = last_row_end <= plane.len();

    for py in 0..n {
        let py_i = py as i32;
        let left = border[center - 1 - py];
        let row_start = (y as usize + py) * stride + x as usize;

        if block_fits {
            let row = &mut plane[row_start..row_start + n];
            for px in 0..n {
                let px_i = px as i32;
                let top = border[center + 1 + px];
                let pred = ((n_i - 1 - px_i) * left
                    + (px_i + 1) * right
                    + (n_i - 1 - py_i) * top
                    + (py_i + 1) * bottom
                    + n_i)
                    >> (log2_size + 1);
                row[px] = pred.clamp(0, max_val) as u16;
            }
        } else {
            for px in 0..n {
                let idx = row_start + px;
                if idx < plane.len() {
                    let px_i = px as i32;
                    let top = border[center + 1 + px];
                    let pred = ((n_i - 1 - px_i) * left
                        + (px_i + 1) * right
                        + (n_i - 1 - py_i) * top
                        + (py_i + 1) * bottom
                        + n_i)
                        >> (log2_size + 1);
                    plane[idx] = pred.clamp(0, max_val) as u16;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn predict_dc(
    plane: &mut [u16],
    stride: usize,
    x: u32,
    y: u32,
    size: u32,
    log2_size: u8,
    c_idx: u8,
    max_val: i32,
    border: &[i32],
    center: usize,
) {
    let n = size as i32;

    let mut dc_val = 0i32;
    for i in 0..size {
        dc_val += border[center + 1 + i as usize];
        dc_val += border[center - 1 - i as usize];
    }
    dc_val = (dc_val + n) >> (log2_size + 1);

    if c_idx == 0 && size < 32 {
        let corner = (border[center - 1] + 2 * dc_val + border[center + 1] + 2) >> 2;
        write_sample(plane, stride, x, y, corner.clamp(0, max_val) as u16);

        let row_start = y as usize * stride + x as usize;
        let top_row = &mut plane[row_start + 1..row_start + size as usize];
        for px in 1..size as usize {
            let pred = (border[center + 1 + px] + 3 * dc_val + 2) >> 2;
            top_row[px - 1] = pred.clamp(0, max_val) as u16;
        }

        for py in 1..size {
            let pred = (border[center - 1 - py as usize] + 3 * dc_val + 2) >> 2;
            write_sample(plane, stride, x, y + py, pred.clamp(0, max_val) as u16);
        }

        let dc_u16 = dc_val.clamp(0, max_val) as u16;
        for py in 1..size {
            let row_start = (y + py) as usize * stride + (x as usize + 1);
            let row_end = row_start + (size as usize - 1);
            if row_end <= plane.len() {
                plane[row_start..row_end].fill(dc_u16);
            }
        }
    } else {
        let dc_u16 = dc_val.clamp(0, max_val) as u16;
        for py in 0..size {
            let row_start = (y + py) as usize * stride + x as usize;
            let row_end = row_start + size as usize;
            if row_end <= plane.len() {
                plane[row_start..row_end].fill(dc_u16);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn predict_angular(
    plane: &mut [u16],
    stride: usize,
    x: u32,
    y: u32,
    size: u32,
    c_idx: u8,
    mode: u8,
    max_val: i32,
    border: &[i32],
    center: usize,
) {
    let n = size as i32;
    let n_u = n as usize;
    let intra_pred_angle = INTRA_PRED_ANGLE[mode as usize] as i32;

    let last_row_end = (y as usize + n_u - 1) * stride + x as usize + n_u;
    let block_fits = last_row_end <= plane.len();

    let mut ref_arr = [0i32; 4 * MAX_INTRA_PRED_BLOCK_SIZE + 1];
    let ref_center = 2 * MAX_INTRA_PRED_BLOCK_SIZE;

    if mode >= 18 {
        ref_arr[ref_center..ref_center + n as usize + 1]
            .copy_from_slice(&border[center..center + n as usize + 1]);

        if intra_pred_angle < 0 {
            let inv_angle = get_inv_angle(mode);
            let ext = (n * intra_pred_angle) >> 5;

            if ext < -1 {
                for xx in ext..=-1 {
                    let idx = (xx * inv_angle + 128) >> 8;
                    if idx >= 0 && idx <= (2 * n) {
                        ref_arr[(ref_center as i32 + xx) as usize] =
                            border[(center as i32 - idx) as usize];
                    }
                }
            }
        } else {
            let src_start = center + n as usize + 1;
            let dst_start = ref_center + n as usize + 1;
            let count = n as usize;
            ref_arr[dst_start..dst_start + count]
                .copy_from_slice(&border[src_start..src_start + count]);
        }

        for py in 0..n {
            let i_idx = ((py + 1) * intra_pred_angle) >> 5;
            let i_fact = ((py + 1) * intra_pred_angle) & 31;
            let row_start = (y as i32 + py) as usize * stride + x as usize;
            let base_idx = (ref_center as i32 + i_idx + 1) as usize;

            if block_fits {
                let row = &mut plane[row_start..row_start + n_u];
                if i_fact != 0 {
                    let w0 = 32 - i_fact;
                    for (px, out) in row.iter_mut().enumerate() {
                        let idx = base_idx + px;
                        let pred = (w0 * ref_arr[idx] + i_fact * ref_arr[idx + 1] + 16) >> 5;
                        *out = pred.clamp(0, max_val) as u16;
                    }
                } else {
                    for (px, out) in row.iter_mut().enumerate() {
                        *out = ref_arr[base_idx + px].clamp(0, max_val) as u16;
                    }
                }
            } else {
                let w0 = 32 - i_fact;
                for px in 0..n_u {
                    let out_idx = row_start + px;
                    if out_idx < plane.len() {
                        let idx = base_idx + px;
                        let pred = if i_fact != 0 {
                            (w0 * ref_arr[idx] + i_fact * ref_arr[idx + 1] + 16) >> 5
                        } else {
                            ref_arr[idx]
                        };
                        plane[out_idx] = pred.clamp(0, max_val) as u16;
                    }
                }
            }
        }

        if mode == 26 && c_idx == 0 && size < 32 {
            for py in 0..n {
                let pred =
                    border[center + 1] + ((border[center - 1 - py as usize] - border[center]) >> 1);
                write_sample(
                    plane,
                    stride,
                    x,
                    y + py as u32,
                    pred.clamp(0, max_val) as u16,
                );
            }
        }
    } else {
        for i in 0..=n {
            ref_arr[ref_center + i as usize] = border[center - i as usize];
        }

        if intra_pred_angle < 0 {
            let inv_angle = get_inv_angle(mode);
            let ext = (n * intra_pred_angle) >> 5;

            if ext < -1 {
                for xx in ext..=-1 {
                    let idx = (xx * inv_angle + 128) >> 8;
                    if idx >= 0 && idx <= (2 * n) {
                        ref_arr[(ref_center as i32 + xx) as usize] =
                            border[(center as i32 + idx) as usize];
                    }
                }
            }
        } else {
            for xx in (n + 1)..=(2 * n) {
                ref_arr[ref_center + xx as usize] = border[center - xx as usize];
            }
        }

        for py in 0..n {
            let row_start = (y as i32 + py) as usize * stride + x as usize;
            let row_base = (ref_center as i32 + py + 1) as usize;

            if block_fits {
                let row = &mut plane[row_start..row_start + n_u];
                for (px, out) in row.iter_mut().enumerate() {
                    let i_idx = ((px as i32 + 1) * intra_pred_angle) >> 5;
                    let i_fact = ((px as i32 + 1) * intra_pred_angle) & 31;
                    let idx = (row_base as i32 + i_idx) as usize;
                    let pred = if i_fact != 0 {
                        ((32 - i_fact) * ref_arr[idx] + i_fact * ref_arr[idx + 1] + 16) >> 5
                    } else {
                        ref_arr[idx]
                    };
                    *out = pred.clamp(0, max_val) as u16;
                }
            } else {
                for px in 0..n_u {
                    let out_idx = row_start + px;
                    if out_idx < plane.len() {
                        let i_idx = ((px as i32 + 1) * intra_pred_angle) >> 5;
                        let i_fact = ((px as i32 + 1) * intra_pred_angle) & 31;
                        let idx = (row_base as i32 + i_idx) as usize;
                        let pred = if i_fact != 0 {
                            ((32 - i_fact) * ref_arr[idx] + i_fact * ref_arr[idx + 1] + 16) >> 5
                        } else {
                            ref_arr[idx]
                        };
                        plane[out_idx] = pred.clamp(0, max_val) as u16;
                    }
                }
            }
        }

        if mode == 10 && c_idx == 0 && size < 32 {
            for px in 0..n {
                let pred =
                    border[center - 1] + ((border[center + 1 + px as usize] - border[center]) >> 1);
                write_sample(
                    plane,
                    stride,
                    x + px as u32,
                    y,
                    pred.clamp(0, max_val) as u16,
                );
            }
        }
    }
}

pub fn fill_mpm_candidates(cand_a: IntraPredMode, cand_b: IntraPredMode) -> [IntraPredMode; 3] {
    if cand_a == cand_b {
        if cand_a.as_u8() < 2 {
            [
                IntraPredMode::Planar,
                IntraPredMode::Dc,
                IntraPredMode::Angular26,
            ]
        } else {
            let mode = cand_a.as_u8();
            let left = 2 + ((mode - 2).wrapping_sub(1) % 32);
            let right = 2 + ((mode - 2) + 1) % 32;
            [
                cand_a,
                IntraPredMode::from_u8(left).unwrap_or(IntraPredMode::Dc),
                IntraPredMode::from_u8(right).unwrap_or(IntraPredMode::Dc),
            ]
        }
    } else {
        let third = if cand_a != IntraPredMode::Planar && cand_b != IntraPredMode::Planar {
            IntraPredMode::Planar
        } else if cand_a != IntraPredMode::Dc && cand_b != IntraPredMode::Dc {
            IntraPredMode::Dc
        } else {
            IntraPredMode::Angular26
        };
        [cand_a, cand_b, third]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mpm_candidates_same_dc() {
        let mpm = fill_mpm_candidates(IntraPredMode::Dc, IntraPredMode::Dc);
        assert_eq!(mpm[0], IntraPredMode::Planar);
        assert_eq!(mpm[1], IntraPredMode::Dc);
        assert_eq!(mpm[2], IntraPredMode::Angular26);
    }

    #[test]
    fn test_mpm_candidates_different() {
        let mpm = fill_mpm_candidates(IntraPredMode::Dc, IntraPredMode::Planar);
        assert_eq!(mpm[0], IntraPredMode::Dc);
        assert_eq!(mpm[1], IntraPredMode::Planar);
        assert_eq!(mpm[2], IntraPredMode::Angular26);
    }

    #[test]
    fn test_intra_angles() {
        assert_eq!(INTRA_PRED_ANGLE[10], 0);
        assert_eq!(INTRA_PRED_ANGLE[26], 0);
        assert_eq!(INTRA_PRED_ANGLE[2], 32);
        assert_eq!(INTRA_PRED_ANGLE[34], 32);
    }
}
