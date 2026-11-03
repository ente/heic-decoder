use alloc::vec;
use alloc::vec::Vec;

use super::color_convert;

pub(crate) const UNINIT_SAMPLE: u16 = u16::MAX;

pub(crate) const DEBLOCK_FLAG_VERT: u8 = 1;
pub(crate) const DEBLOCK_FLAG_HORIZ: u8 = 2;
pub(crate) const DEBLOCK_FLAG_BYPASS: u8 = 4;

#[derive(Debug)]
#[non_exhaustive]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub y_plane: Vec<u16>,
    pub cb_plane: Vec<u16>,
    pub cr_plane: Vec<u16>,
    pub bit_depth: u8,
    pub chroma_format: u8,
    pub crop_left: u32,
    pub crop_right: u32,
    pub crop_top: u32,
    pub crop_bottom: u32,
    pub alpha_plane: Option<Vec<u16>>,
    pub full_range: bool,
    pub matrix_coeffs: u8,
    pub colour_primaries: u8,
    #[doc(hidden)]
    pub deblock_flags: Vec<u8>,
    #[doc(hidden)]
    pub deblock_stride: u32,
    #[doc(hidden)]
    pub qp_map: Vec<i8>,
    pub(crate) row_origin: u32,
}

impl DecodedFrame {
    pub(crate) fn plane_origin(&self, c_idx: u8) -> u32 {
        if c_idx != 0 && self.chroma_format == 1 {
            self.row_origin / 2
        } else {
            self.row_origin
        }
    }

    pub(crate) fn advance_rows(&mut self, origin: u32) {
        let rows = (origin - self.row_origin) as usize;
        let width = self.width as usize;
        let chroma_width = self.c_stride();
        let chroma_rows = if self.chroma_format == 1 {
            rows / 2
        } else {
            rows
        };
        shift_rows(&mut self.y_plane, rows * width, UNINIT_SAMPLE);
        shift_rows(
            &mut self.cb_plane,
            chroma_rows * chroma_width,
            UNINIT_SAMPLE,
        );
        shift_rows(
            &mut self.cr_plane,
            chroma_rows * chroma_width,
            UNINIT_SAMPLE,
        );
        shift_rows(
            &mut self.deblock_flags,
            rows / 4 * self.deblock_stride as usize,
            0,
        );
        shift_rows(&mut self.qp_map, rows / 4 * self.deblock_stride as usize, 0);
        self.row_origin = origin;
    }

    pub(crate) fn copy_rows_from(&mut self, source: &Self, start: u32, rows: u32) {
        for component in 0..3 {
            let subsample = if component != 0 && self.chroma_format == 1 {
                2
            } else {
                1
            };
            let target_y = (start - self.row_origin) / subsample;
            let source_y = (start - source.row_origin) / subsample;
            let (input, stride) = source.plane(component);
            let (output, _) = self.plane_mut(component);
            let len = (rows / subsample) as usize * stride;
            output[target_y as usize * stride..][..len]
                .copy_from_slice(&input[source_y as usize * stride..][..len]);
        }
        let stride = self.deblock_stride as usize;
        let target = ((start - self.row_origin) / 4) as usize * stride;
        let source_start = ((start - source.row_origin) / 4) as usize * stride;
        let len = (rows / 4) as usize * stride;
        self.deblock_flags[target..][..len]
            .copy_from_slice(&source.deblock_flags[source_start..][..len]);
        self.qp_map[target..][..len].copy_from_slice(&source.qp_map[source_start..][..len]);
    }

    #[cfg(test)]
    pub(crate) fn with_params(width: u32, height: u32, bit_depth: u8, chroma_format: u8) -> Self {
        Self::try_with_params(width, height, bit_depth, chroma_format)
            .expect("frame allocation failed")
    }

    pub(crate) fn try_with_params(
        width: u32,
        height: u32,
        bit_depth: u8,
        chroma_format: u8,
    ) -> super::Result<Self> {
        let luma_size = width
            .checked_mul(height)
            .ok_or(super::HevcError::DecodingError("frame dimensions overflow"))?
            as usize;

        let (chroma_width, chroma_height) = match chroma_format {
            0 => (0, 0),
            1 => (width.div_ceil(2), height.div_ceil(2)),
            2 => (width.div_ceil(2), height),
            3 => (width, height),
            _ => (width.div_ceil(2), height.div_ceil(2)),
        };

        let chroma_size =
            chroma_width
                .checked_mul(chroma_height)
                .ok_or(super::HevcError::DecodingError(
                    "chroma dimensions overflow",
                ))? as usize;

        let deblock_stride = width.div_ceil(4);
        let deblock_height = height.div_ceil(4);
        let deblock_size =
            deblock_stride
                .checked_mul(deblock_height)
                .ok_or(super::HevcError::DecodingError(
                    "deblock dimensions overflow",
                ))? as usize;

        Ok(Self {
            width,
            row_origin: 0,
            height,
            y_plane: super::allocation::filled(UNINIT_SAMPLE, luma_size)?,
            cb_plane: super::allocation::filled(UNINIT_SAMPLE, chroma_size)?,
            cr_plane: super::allocation::filled(UNINIT_SAMPLE, chroma_size)?,
            bit_depth,
            chroma_format,
            crop_left: 0,
            crop_right: 0,
            crop_top: 0,
            crop_bottom: 0,
            deblock_flags: super::allocation::filled(0, deblock_size)?,
            deblock_stride,
            qp_map: super::allocation::filled(0, deblock_size)?,
            alpha_plane: None,
            full_range: false,
            matrix_coeffs: 2,
            colour_primaries: 2,
        })
    }

    pub(crate) fn mark_tu_boundary(&mut self, x: u32, y: u32, size: u32) {
        let bx = x / 4;
        let by = (y - self.row_origin) / 4;
        let bs = size / 4;

        if x > 0 {
            for j in 0..bs {
                let idx = ((by + j) * self.deblock_stride + bx) as usize;
                if idx < self.deblock_flags.len() {
                    self.deblock_flags[idx] |= DEBLOCK_FLAG_VERT;
                }
            }
        }

        if y > 0 {
            for i in 0..bs {
                let idx = (by * self.deblock_stride + bx + i) as usize;
                if idx < self.deblock_flags.len() {
                    self.deblock_flags[idx] |= DEBLOCK_FLAG_HORIZ;
                }
            }
        }
    }

    pub(crate) fn store_block_qp(&mut self, x: u32, y: u32, size: u32, qp: i8) {
        let bx = x / 4;
        let by = (y - self.row_origin) / 4;
        let bs = size / 4;
        for j in 0..bs {
            for i in 0..bs {
                let idx = ((by + j) * self.deblock_stride + bx + i) as usize;
                if idx < self.qp_map.len() {
                    self.qp_map[idx] = qp;
                }
            }
        }
    }

    pub(crate) fn store_block_bypass(&mut self, x: u32, y: u32, size: u32) {
        let bx = x / 4;
        let by = (y - self.row_origin) / 4;
        let bs = size / 4;
        for j in 0..bs {
            for i in 0..bs {
                let idx = ((by + j) * self.deblock_stride + bx + i) as usize;
                if idx < self.deblock_flags.len() {
                    self.deblock_flags[idx] |= DEBLOCK_FLAG_BYPASS;
                }
            }
        }
    }

    #[inline]
    pub(crate) fn is_block_bypass(&self, x: u32, y: u32) -> bool {
        let idx = (((y - self.row_origin) / 4) * self.deblock_stride + x / 4) as usize;
        self.deblock_flags
            .get(idx)
            .is_some_and(|f| f & DEBLOCK_FLAG_BYPASS != 0)
    }

    #[inline]
    pub(crate) fn has_bypass_blocks(&self) -> bool {
        self.deblock_flags
            .iter()
            .any(|f| f & DEBLOCK_FLAG_BYPASS != 0)
    }

    pub(crate) fn set_crop(&mut self, left: u32, right: u32, top: u32, bottom: u32) {
        self.crop_left = left;
        self.crop_right = right;
        self.crop_top = top;
        self.crop_bottom = bottom;
    }

    pub fn cropped_width(&self) -> u32 {
        self.width - self.crop_left - self.crop_right
    }

    pub fn cropped_height(&self) -> u32 {
        self.height - self.crop_top - self.crop_bottom
    }

    pub fn y_stride(&self) -> usize {
        self.width as usize
    }

    pub fn c_stride(&self) -> usize {
        match self.chroma_format {
            0 => 0,
            1 | 2 => self.width.div_ceil(2) as usize,
            3 => self.width as usize,
            _ => self.width.div_ceil(2) as usize,
        }
    }

    #[inline(always)]
    fn ycbcr_to_rgb(&self, y_val: i32, cb_val: i32, cr_val: i32) -> (u8, u8, u8) {
        let cb = cb_val - 128;
        let cr = cr_val - 128;

        if self.full_range {
            let (cr_r, cb_g, cr_g, cb_b) = match self.matrix_coeffs {
                1 => (403, -48, -120, 475),
                9 => (377, -42, -146, 482),
                _ => (359, -88, -183, 454),
            };
            let r = y_val + ((cr_r * cr + 128) >> 8);
            let g = y_val + ((cb_g * cb + cr_g * cr + 128) >> 8);
            let b = y_val + ((cb_b * cb + 128) >> 8);
            (
                r.clamp(0, 255) as u8,
                g.clamp(0, 255) as u8,
                b.clamp(0, 255) as u8,
            )
        } else {
            let (cr_r, cb_g, cr_g, cb_b) = match self.matrix_coeffs {
                1 => (14744, -1754, -4383, 17373),
                9 => (13806, -1541, -5349, 17615),
                _ => (13126, -3222, -6686, 16591),
            };
            let yv = (y_val - 16) * 9576;
            let r = (yv + cr_r * cr + 4096) >> 13;
            let g = (yv + cb_g * cb + cr_g * cr + 4096) >> 13;
            let b = (yv + cb_b * cb + 4096) >> 13;
            (
                r.clamp(0, 255) as u8,
                g.clamp(0, 255) as u8,
                b.clamp(0, 255) as u8,
            )
        }
    }

    pub fn to_rgb(&self) -> Vec<u8> {
        let out_width = self.cropped_width();
        let out_height = self.cropped_height();
        let total = (out_width * out_height) as usize;
        let mut rgb = vec![0u8; total * 3];
        let shift = self.bit_depth - 8;

        let y_start = self.crop_top;
        let y_end = self.height - self.crop_bottom;
        let x_start = self.crop_left;
        let x_end = self.width - self.crop_right;
        let w = self.width as usize;

        let mut out_idx = 0;

        if self.chroma_format == 1 {
            let c_stride = self.c_stride();
            color_convert::convert_420_to_rgb(
                &self.y_plane,
                &self.cb_plane,
                &self.cr_plane,
                w,
                c_stride,
                y_start,
                y_end,
                x_start,
                x_end,
                shift as u32,
                self.full_range,
                self.matrix_coeffs,
                &mut rgb,
            );
        } else {
            for y in y_start..y_end {
                for x in x_start..x_end {
                    let y_idx = y as usize * w + x as usize;
                    let y_val = (self.y_plane[y_idx] >> shift) as i32;
                    let (cb_val, cr_val) = self.get_chroma(x, y, shift);
                    let (r, g, b) = self.ycbcr_to_rgb(y_val, cb_val, cr_val);
                    rgb[out_idx] = r;
                    rgb[out_idx + 1] = g;
                    rgb[out_idx + 2] = b;
                    out_idx += 3;
                }
            }
        }

        rgb
    }

    pub fn to_bgra(&self) -> Vec<u8> {
        let out_width = self.cropped_width();
        let out_height = self.cropped_height();
        let mut bgra = Vec::with_capacity((out_width * out_height * 4) as usize);
        let shift = self.bit_depth - 8;

        let y_start = self.crop_top;
        let y_end = self.height - self.crop_bottom;
        let x_start = self.crop_left;
        let x_end = self.width - self.crop_right;

        for y in y_start..y_end {
            for x in x_start..x_end {
                let y_idx = (y * self.width + x) as usize;
                let y_val = (self.y_plane[y_idx] >> shift) as i32;

                let (cb_val, cr_val) = self.get_chroma(x, y, shift);

                let (r, g, b) = self.ycbcr_to_rgb(y_val, cb_val, cr_val);
                bgra.push(b);
                bgra.push(g);
                bgra.push(r);

                let alpha = if let Some(ref alpha) = self.alpha_plane {
                    if y_idx < alpha.len() {
                        (alpha[y_idx] >> shift).min(255) as u8
                    } else {
                        255
                    }
                } else {
                    255
                };
                bgra.push(alpha);
            }
        }

        bgra
    }

    pub fn to_bgr(&self) -> Vec<u8> {
        let out_width = self.cropped_width();
        let out_height = self.cropped_height();
        let mut bgr = Vec::with_capacity((out_width * out_height * 3) as usize);
        let shift = self.bit_depth - 8;

        let y_start = self.crop_top;
        let y_end = self.height - self.crop_bottom;
        let x_start = self.crop_left;
        let x_end = self.width - self.crop_right;

        for y in y_start..y_end {
            for x in x_start..x_end {
                let y_idx = (y * self.width + x) as usize;
                let y_val = (self.y_plane[y_idx] >> shift) as i32;
                let (cb_val, cr_val) = self.get_chroma(x, y, shift);

                let (r, g, b) = self.ycbcr_to_rgb(y_val, cb_val, cr_val);
                bgr.push(b);
                bgr.push(g);
                bgr.push(r);
            }
        }

        bgr
    }

    pub fn write_rgb_into(&self, output: &mut [u8]) -> usize {
        let out_width = self.cropped_width();
        let out_height = self.cropped_height();
        let shift = self.bit_depth - 8;

        let y_start = self.crop_top;
        let y_end = self.height - self.crop_bottom;
        let x_start = self.crop_left;
        let x_end = self.width - self.crop_right;
        let w = self.width as usize;

        let mut offset = 0;
        if self.chroma_format == 1 {
            let c_stride = self.c_stride();
            let needed = (out_width * out_height * 3) as usize;
            if output.len() >= needed {
                color_convert::convert_420_to_rgb(
                    &self.y_plane,
                    &self.cb_plane,
                    &self.cr_plane,
                    w,
                    c_stride,
                    y_start,
                    y_end,
                    x_start,
                    x_end,
                    shift as u32,
                    self.full_range,
                    self.matrix_coeffs,
                    output,
                );
            }
        } else {
            for y in y_start..y_end {
                for x in x_start..x_end {
                    let y_idx = y as usize * w + x as usize;
                    let y_val = (self.y_plane[y_idx] >> shift) as i32;
                    let (cb_val, cr_val) = self.get_chroma(x, y, shift);
                    let (r, g, b) = self.ycbcr_to_rgb(y_val, cb_val, cr_val);
                    if offset + 3 <= output.len() {
                        output[offset] = r;
                        output[offset + 1] = g;
                        output[offset + 2] = b;
                        offset += 3;
                    }
                }
            }
        }
        (out_width * out_height * 3) as usize
    }

    pub fn write_rgba_into(&self, output: &mut [u8]) -> usize {
        let out_width = self.cropped_width();
        let out_height = self.cropped_height();
        let shift = self.bit_depth - 8;

        let y_start = self.crop_top;
        let y_end = self.height - self.crop_bottom;
        let x_start = self.crop_left;
        let x_end = self.width - self.crop_right;

        let mut offset = 0;
        for y in y_start..y_end {
            for x in x_start..x_end {
                let y_idx = (y * self.width + x) as usize;
                let y_val = (self.y_plane[y_idx] >> shift) as i32;
                let (cb_val, cr_val) = self.get_chroma(x, y, shift);
                let (r, g, b) = self.ycbcr_to_rgb(y_val, cb_val, cr_val);
                let alpha = if let Some(ref alpha) = self.alpha_plane {
                    if y_idx < alpha.len() {
                        (alpha[y_idx] >> shift).min(255) as u8
                    } else {
                        255
                    }
                } else {
                    255
                };
                if offset + 4 <= output.len() {
                    output[offset] = r;
                    output[offset + 1] = g;
                    output[offset + 2] = b;
                    output[offset + 3] = alpha;
                    offset += 4;
                }
            }
        }
        (out_width * out_height * 4) as usize
    }

    pub fn write_bgra_into(&self, output: &mut [u8]) -> usize {
        let out_width = self.cropped_width();
        let out_height = self.cropped_height();
        let shift = self.bit_depth - 8;

        let y_start = self.crop_top;
        let y_end = self.height - self.crop_bottom;
        let x_start = self.crop_left;
        let x_end = self.width - self.crop_right;

        let mut offset = 0;
        for y in y_start..y_end {
            for x in x_start..x_end {
                let y_idx = (y * self.width + x) as usize;
                let y_val = (self.y_plane[y_idx] >> shift) as i32;
                let (cb_val, cr_val) = self.get_chroma(x, y, shift);
                let (r, g, b) = self.ycbcr_to_rgb(y_val, cb_val, cr_val);
                let alpha = if let Some(ref alpha) = self.alpha_plane {
                    if y_idx < alpha.len() {
                        (alpha[y_idx] >> shift).min(255) as u8
                    } else {
                        255
                    }
                } else {
                    255
                };
                if offset + 4 <= output.len() {
                    output[offset] = b;
                    output[offset + 1] = g;
                    output[offset + 2] = r;
                    output[offset + 3] = alpha;
                    offset += 4;
                }
            }
        }
        (out_width * out_height * 4) as usize
    }

    pub fn write_bgr_into(&self, output: &mut [u8]) -> usize {
        let out_width = self.cropped_width();
        let out_height = self.cropped_height();
        let shift = self.bit_depth - 8;

        let y_start = self.crop_top;
        let y_end = self.height - self.crop_bottom;
        let x_start = self.crop_left;
        let x_end = self.width - self.crop_right;

        let mut offset = 0;
        for y in y_start..y_end {
            for x in x_start..x_end {
                let y_idx = (y * self.width + x) as usize;
                let y_val = (self.y_plane[y_idx] >> shift) as i32;
                let (cb_val, cr_val) = self.get_chroma(x, y, shift);
                let (r, g, b) = self.ycbcr_to_rgb(y_val, cb_val, cr_val);
                if offset + 3 <= output.len() {
                    output[offset] = b;
                    output[offset + 1] = g;
                    output[offset + 2] = r;
                    offset += 3;
                }
            }
        }
        (out_width * out_height * 3) as usize
    }

    pub fn to_rgba(&self) -> Vec<u8> {
        let out_width = self.cropped_width();
        let out_height = self.cropped_height();
        let mut rgba = Vec::with_capacity((out_width * out_height * 4) as usize);
        let shift = self.bit_depth - 8;

        let y_start = self.crop_top;
        let y_end = self.height - self.crop_bottom;
        let x_start = self.crop_left;
        let x_end = self.width - self.crop_right;

        for y in y_start..y_end {
            for x in x_start..x_end {
                let y_idx = (y * self.width + x) as usize;
                let y_val = (self.y_plane[y_idx] >> shift) as i32;

                let (cb_val, cr_val) = self.get_chroma(x, y, shift);

                let (r, g, b) = self.ycbcr_to_rgb(y_val, cb_val, cr_val);
                rgba.push(r);
                rgba.push(g);
                rgba.push(b);

                let alpha = if let Some(ref alpha) = self.alpha_plane {
                    if y_idx < alpha.len() {
                        (alpha[y_idx] >> shift).min(255) as u8
                    } else {
                        255
                    }
                } else {
                    255
                };
                rgba.push(alpha);
            }
        }

        rgba
    }

    fn get_chroma(&self, x: u32, y: u32, shift: u8) -> (i32, i32) {
        match self.chroma_format {
            0 => (128, 128),
            1 => {
                let cx = x / 2;
                let cy = y / 2;
                let c_stride = self.c_stride();
                let c_idx = (cy as usize) * c_stride + (cx as usize);
                let cb = if c_idx < self.cb_plane.len() {
                    (self.cb_plane[c_idx] >> shift) as i32
                } else {
                    128
                };
                let cr = if c_idx < self.cr_plane.len() {
                    (self.cr_plane[c_idx] >> shift) as i32
                } else {
                    128
                };
                (cb, cr)
            }
            2 => {
                let cx = x / 2;
                let c_stride = self.c_stride();
                let c_idx = (y as usize) * c_stride + (cx as usize);
                let cb = if c_idx < self.cb_plane.len() {
                    (self.cb_plane[c_idx] >> shift) as i32
                } else {
                    128
                };
                let cr = if c_idx < self.cr_plane.len() {
                    (self.cr_plane[c_idx] >> shift) as i32
                } else {
                    128
                };
                (cb, cr)
            }
            3 => {
                let c_idx = (y * self.width + x) as usize;
                let cb = if c_idx < self.cb_plane.len() {
                    (self.cb_plane[c_idx] >> shift) as i32
                } else {
                    128
                };
                let cr = if c_idx < self.cr_plane.len() {
                    (self.cr_plane[c_idx] >> shift) as i32
                } else {
                    128
                };
                (cb, cr)
            }
            _ => (128, 128),
        }
    }

    #[inline]
    pub fn get_y(&self, x: u32, y: u32) -> u16 {
        let idx = (y * self.width + x) as usize;
        if idx < self.y_plane.len() {
            self.y_plane[idx]
        } else {
            0
        }
    }

    #[inline]
    pub fn get_cb(&self, x: u32, y: u32) -> u16 {
        let stride = self.c_stride();
        let idx = (y as usize) * stride + (x as usize);
        if idx < self.cb_plane.len() {
            self.cb_plane[idx]
        } else {
            128 << (self.bit_depth - 8)
        }
    }

    #[inline]
    pub fn get_cr(&self, x: u32, y: u32) -> u16 {
        let stride = self.c_stride();
        let idx = (y as usize) * stride + (x as usize);
        if idx < self.cr_plane.len() {
            self.cr_plane[idx]
        } else {
            128 << (self.bit_depth - 8)
        }
    }

    #[inline]
    pub(crate) fn plane_mut(&mut self, c_idx: u8) -> (&mut [u16], usize) {
        match c_idx {
            0 => (&mut self.y_plane, self.width as usize),
            1 => {
                let stride = self.c_stride();
                (&mut self.cb_plane, stride)
            }
            2 => {
                let stride = self.c_stride();
                (&mut self.cr_plane, stride)
            }
            _ => (&mut self.y_plane, self.width as usize),
        }
    }

    #[inline]
    pub fn plane(&self, c_idx: u8) -> (&[u16], usize) {
        match c_idx {
            0 => (&self.y_plane, self.width as usize),
            1 => {
                let stride = self.c_stride();
                (&self.cb_plane, stride)
            }
            2 => {
                let stride = self.c_stride();
                (&self.cr_plane, stride)
            }
            _ => (&self.y_plane, self.width as usize),
        }
    }

    pub(crate) fn chroma_dims(&self) -> (u32, u32) {
        match self.chroma_format {
            0 => (0, 0),
            1 => (self.width.div_ceil(2), self.height.div_ceil(2)),
            2 => (self.width.div_ceil(2), self.height),
            3 => (self.width, self.height),
            _ => (self.width.div_ceil(2), self.height.div_ceil(2)),
        }
    }
}

pub(crate) fn shift_rows<T: Copy>(data: &mut [T], count: usize, value: T) {
    let count = count.min(data.len());
    data.copy_within(count.., 0);
    let keep = data.len() - count;
    data[keep..].fill(value);
}
