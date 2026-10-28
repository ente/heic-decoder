use crate::RgbaTransformPlan;
use crate::isobmff::PrimaryItemTransformProperty as Transform;

use super::grid::Grid;
use super::memory::{Budget, Buffer};
use super::{BoundedDecodeError as Error, Result};

#[derive(Clone, Copy)]
pub(super) struct Layout {
    pub(super) left: u32,
    pub(super) top: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) output_width: u32,
    pub(super) output_height: u32,
    pub(super) display_width: u32,
    pub(super) display_height: u32,
    pub(super) original_dimensions: (u32, u32),
    matrix: [i8; 4],
}

impl Layout {
    pub(super) fn new(grid: &Grid<'_>, max_side: u32) -> Result<Self> {
        let mut transforms = Vec::with_capacity(18);
        transforms.extend(grid.properties.transforms.iter().flatten().copied());
        if !crate::transforms_include_orientation(&transforms)
            && let Some(exif) = grid
                .exif
                .and_then(|n| crate::exif_orientation_to_primary_item_transforms(u16::from(n)))
        {
            transforms.extend(exif);
        }
        let plan = RgbaTransformPlan::from_primary_transforms(grid.width, grid.height, &transforms)
            .map_err(|_| Error::Malformed("display transform"))?;
        let mut left = usize::MAX;
        let mut top = usize::MAX;
        let mut right = 0;
        let mut bottom = 0;
        for (x, y) in [
            (0, 0),
            (plan.destination_width - 1, 0),
            (0, plan.destination_height - 1),
            (plan.destination_width - 1, plan.destination_height - 1),
        ] {
            let (x, y) = plan
                .map_destination_pixel(x as usize, y as usize)
                .map_err(|_| Error::Malformed("display crop"))?;
            left = left.min(x);
            top = top.min(y);
            right = right.max(x);
            bottom = bottom.max(y);
        }
        let width = (right - left + 1) as u32;
        let height = (bottom - top + 1) as u32;
        let longest = width.max(height);
        let output_width = if longest <= max_side {
            width
        } else {
            (u64::from(width) * u64::from(max_side) / u64::from(longest)).max(1) as u32
        };
        let output_height = if longest <= max_side {
            height
        } else {
            (u64::from(height) * u64::from(max_side) / u64::from(longest)).max(1) as u32
        };
        let mut matrix = [1, 0, 0, 1];
        for transform in transforms {
            let [a, b, c, d] = matrix;
            matrix = match transform {
                Transform::Rotation(rotation) => match rotation.rotation_ccw_degrees % 360 {
                    90 => [c, d, -a, -b],
                    180 => [-a, -b, -c, -d],
                    270 => [-c, -d, a, b],
                    _ => matrix,
                },
                Transform::Mirror(mirror) => match mirror.direction {
                    crate::isobmff::ImageMirrorDirection::Horizontal => [-a, -b, c, d],
                    crate::isobmff::ImageMirrorDirection::Vertical => [a, b, -c, -d],
                },
                Transform::CleanAperture(_) => matrix,
            };
        }
        let (display_width, display_height) = if matrix[0] == 0 {
            (output_height, output_width)
        } else {
            (output_width, output_height)
        };
        Ok(Self {
            left: left as u32,
            top: top as u32,
            width,
            height,
            output_width,
            output_height,
            display_width,
            display_height,
            original_dimensions: (plan.destination_width, plan.destination_height),
            matrix,
        })
    }

    pub(super) fn is_unscaled(&self) -> bool {
        self.width == self.output_width && self.height == self.output_height
    }

    #[cfg(feature = "incremental-experiment")]
    pub(super) fn write_row(&self, x: u32, y: u32, pixels: &[u8], output: &mut [u8]) {
        if self.matrix[0] == 1 {
            let start = self.pixel_index(x, y);
            output[start..start + pixels.len()].copy_from_slice(pixels);
        } else {
            for (offset, pixel) in pixels.chunks_exact(3).enumerate() {
                let start = self.pixel_index(x + offset as u32, y);
                output[start..start + 3].copy_from_slice(pixel);
            }
        }
    }

    pub(super) fn pixel_index(&self, x: u32, y: u32) -> usize {
        let [a, b, c, d] = self.matrix;
        let dx = i64::from(a) * i64::from(x)
            + i64::from(b) * i64::from(y)
            + if a < 0 || b < 0 {
                i64::from(self.display_width - 1)
            } else {
                0
            };
        let dy = i64::from(c) * i64::from(x)
            + i64::from(d) * i64::from(y)
            + if c < 0 || d < 0 {
                i64::from(self.display_height - 1)
            } else {
                0
            };
        (dy as usize * self.display_width as usize + dx as usize) * 3
    }

    pub(super) fn region(
        &self,
        tile_x: u32,
        tile_y: u32,
        tile_width: u32,
        tile_height: u32,
    ) -> Region {
        let left = tile_x.max(self.left).min(self.left + self.width) - self.left;
        let top = tile_y.max(self.top).min(self.top + self.height) - self.top;
        let right = (tile_x + tile_width)
            .min(self.left + self.width)
            .max(self.left)
            - self.left;
        let bottom = (tile_y + tile_height)
            .min(self.top + self.height)
            .max(self.top)
            - self.top;
        let output_left =
            (u64::from(left) * u64::from(self.output_width) / u64::from(self.width)) as u32;
        let output_top =
            (u64::from(top) * u64::from(self.output_height) / u64::from(self.height)) as u32;
        let output_right = (u64::from(right) * u64::from(self.output_width))
            .div_ceil(u64::from(self.width)) as u32;
        let output_bottom = (u64::from(bottom) * u64::from(self.output_height))
            .div_ceil(u64::from(self.height)) as u32;
        Region {
            left,
            top,
            right,
            bottom,
            output_left,
            output_top,
            output_right,
            output_bottom,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Region {
    pub(super) left: u32,
    pub(super) top: u32,
    pub(super) right: u32,
    pub(super) bottom: u32,
    pub(super) output_left: u32,
    pub(super) output_top: u32,
    pub(super) output_right: u32,
    pub(super) output_bottom: u32,
}

impl Region {
    pub(super) fn len(&self) -> usize {
        if self.left >= self.right || self.top >= self.bottom {
            0
        } else {
            (self.output_right - self.output_left) as usize
                * (self.output_bottom - self.output_top) as usize
        }
    }
}

pub(super) fn zeroed<T: Default + Clone>(len: usize) -> Result<Vec<T>> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(len)
        .map_err(|_| Error::AllocationFailed)?;
    result.resize(len, T::default());
    Ok(result)
}

#[derive(Default)]
pub(super) struct Contribution {
    pixels: Vec<u8>,
    edges: Vec<[u64; 3]>,
    width: usize,
    height: usize,
}

impl Contribution {
    pub(super) fn is_empty(&self) -> bool {
        self.pixels.is_empty()
    }
    fn edge_index(&self, x: usize, y: usize) -> Option<usize> {
        if y == 0 {
            Some(x)
        } else if y + 1 == self.height {
            Some(self.width + x)
        } else if x == 0 {
            Some(2 * self.width + y)
        } else if x + 1 == self.width {
            Some(2 * self.width + self.height + y)
        } else {
            None
        }
    }
    pub(super) fn workspace_bytes(region: Region) -> usize {
        if region.len() == 0 {
            return 0;
        }
        let w = (region.output_right - region.output_left) as usize;
        let h = (region.output_bottom - region.output_top) as usize;
        region.len() * 3 + (5 * w + 2 * h) * 24
    }
}

pub(super) fn contributions(
    layout: Layout,
    region: Region,
    pixels: &[u8],
    tile_x: u32,
    tile_y: u32,
    tile_width: u32,
) -> Result<Contribution> {
    if region.len() == 0 {
        return Ok(Contribution::default());
    }
    let out_width = (region.output_right - region.output_left) as usize;
    let out_height = (region.output_bottom - region.output_top) as usize;
    let mut output = Contribution {
        pixels: zeroed(region.len() * 3)?,
        edges: zeroed((out_width + out_height) * 2)?,
        width: out_width,
        height: out_height,
    };
    let mut rows = zeroed::<[u64; 3]>(out_width * 2)?;
    let denominator = u64::from(layout.width) * u64::from(layout.height);
    let mut horizontal = zeroed::<[u64; 3]>(out_width)?;
    let scale_x = u64::from(layout.output_width);
    let scale_y = u64::from(layout.output_height);
    for sy in region.top..region.bottom {
        horizontal.fill([0; 3]);
        for ox in region.output_left..region.output_right {
            let start = u64::from(ox) * u64::from(layout.width);
            let end = u64::from(ox + 1) * u64::from(layout.width);
            let first = (start / scale_x).max(u64::from(region.left)) as u32;
            let last = end.div_ceil(scale_x).min(u64::from(region.right)) as u32;
            let sum = &mut horizontal[(ox - region.output_left) as usize];
            for sx in first..last {
                let weight =
                    end.min(u64::from(sx + 1) * scale_x) - start.max(u64::from(sx) * scale_x);
                let index = ((sy + layout.top - tile_y) as usize * tile_width as usize
                    + (sx + layout.left - tile_x) as usize)
                    * 3;
                for channel in 0..3 {
                    sum[channel] += u64::from(pixels[index + channel]) * weight;
                }
            }
        }
        let first = (u64::from(sy) * scale_y / u64::from(layout.height)) as u32;
        let last = (u64::from(sy + 1) * scale_y).div_ceil(u64::from(layout.height)) as u32;
        for oy in first..last {
            let weight = (u64::from(sy + 1) * scale_y)
                .min(u64::from(oy + 1) * u64::from(layout.height))
                - (u64::from(sy) * scale_y).max(u64::from(oy) * u64::from(layout.height));
            let row = &mut rows[(oy % 2) as usize * out_width..][..out_width];
            for (sum, partial) in row.iter_mut().zip(horizontal.iter()) {
                for channel in 0..3 {
                    sum[channel] += partial[channel] * weight;
                }
            }
            if u64::from(sy + 1) * scale_y >= u64::from(oy + 1) * u64::from(layout.height)
                || sy + 1 == region.bottom
            {
                let y = (oy - region.output_top) as usize;
                for (x, sum) in row.iter_mut().enumerate() {
                    if let Some(index) = output.edge_index(x, y) {
                        output.edges[index] = *sum;
                    } else {
                        let index = (y * out_width + x) * 3;
                        for (channel, &value) in sum.iter().enumerate() {
                            output.pixels[index + channel] =
                                ((value + denominator / 2) / denominator) as u8;
                        }
                    }
                    *sum = [0; 3];
                }
            }
        }
    }
    Ok(output)
}

pub(super) struct Accumulator {
    top: Buffer<[u64; 3]>,
    bottom: Buffer<[u64; 3]>,
    left: Buffer<[u64; 3]>,
    layout: Layout,
}

impl Accumulator {
    pub(super) fn new(layout: Layout, budget: &Budget) -> Result<Self> {
        Ok(Self {
            top: budget.zeroed(layout.output_width as usize, "boundary sums")?,
            bottom: budget.zeroed(layout.output_width as usize, "boundary sums")?,
            left: budget.zeroed(layout.output_height as usize, "boundary sums")?,
            layout,
        })
    }

    pub(super) fn finish_tile_row(&mut self) {
        std::mem::swap(&mut self.top, &mut self.bottom);
        self.bottom.fill([0; 3]);
        self.left.fill([0; 3]);
    }

    #[cfg(feature = "incremental-experiment")]
    pub(super) fn merge_row(
        &mut self,
        region: Region,
        oy: u32,
        sums: &[[u64; 3]],
        output: &mut [u8],
    ) {
        let layout = self.layout;
        let denominator = u64::from(layout.width) * u64::from(layout.height);
        for ox in region.output_left..region.output_right {
            let mut sum = sums[(ox - region.output_left) as usize];
            if u64::from(ox) * u64::from(layout.width)
                < u64::from(region.left) * u64::from(layout.output_width)
            {
                for (value, previous) in sum.iter_mut().zip(self.left[oy as usize]) {
                    *value += previous;
                }
            }
            if u64::from(ox + 1) * u64::from(layout.width)
                > u64::from(region.right) * u64::from(layout.output_width)
            {
                self.left[oy as usize] = sum;
                continue;
            }
            if u64::from(oy) * u64::from(layout.height)
                < u64::from(region.top) * u64::from(layout.output_height)
            {
                for (value, previous) in sum.iter_mut().zip(self.top[ox as usize]) {
                    *value += previous;
                }
            }
            if u64::from(oy + 1) * u64::from(layout.height)
                <= u64::from(region.bottom) * u64::from(layout.output_height)
            {
                let index = layout.pixel_index(ox, oy);
                for channel in 0..3 {
                    output[index + channel] =
                        ((sum[channel] + denominator / 2) / denominator) as u8;
                }
            } else {
                self.bottom[ox as usize] = sum;
            }
        }
    }

    pub(super) fn merge(&mut self, region: Region, sums: &Contribution, output: &mut [u8]) {
        let layout = self.layout;
        let out_width = (region.output_right - region.output_left) as usize;
        let denominator = u64::from(layout.width) * u64::from(layout.height);
        for oy in region.output_top..region.output_bottom {
            for ox in region.output_left..region.output_right {
                let x = (ox - region.output_left) as usize;
                let y = (oy - region.output_top) as usize;
                let Some(edge) = sums.edge_index(x, y) else {
                    let src = (y * out_width + x) * 3;
                    let dst = layout.pixel_index(ox, oy);
                    output[dst..dst + 3].copy_from_slice(&sums.pixels[src..src + 3]);
                    continue;
                };
                let mut sum = sums.edges[edge];
                if u64::from(ox) * u64::from(layout.width)
                    < u64::from(region.left) * u64::from(layout.output_width)
                {
                    for (value, previous) in sum.iter_mut().zip(self.left[oy as usize]) {
                        *value += previous;
                    }
                }
                let last_column = u64::from(ox + 1) * u64::from(layout.width)
                    <= u64::from(region.right) * u64::from(layout.output_width);
                if !last_column {
                    self.left[oy as usize] = sum;
                    continue;
                }
                if u64::from(oy) * u64::from(layout.height)
                    < u64::from(region.top) * u64::from(layout.output_height)
                {
                    for (value, previous) in sum.iter_mut().zip(self.top[ox as usize]) {
                        *value += previous;
                    }
                }
                let last_row = u64::from(oy + 1) * u64::from(layout.height)
                    <= u64::from(region.bottom) * u64::from(layout.output_height);
                if last_row {
                    let index = layout.pixel_index(ox, oy);
                    for channel in 0..3 {
                        output[index + channel] =
                            ((sum[channel] + denominator / 2) / denominator) as u8;
                    }
                } else {
                    self.bottom[ox as usize] = sum;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiled_area_filter_matches_independent_full_image_filter() {
        for (width, height) in [(17u32, 13u32), (2, 3), (1, 7), (7, 1), (25, 23)] {
            for (tile_width, tile_height) in [(1, 1), (3, 4), (9, 7), (32, 32)] {
                for (output_width, output_height) in [
                    (1, 1),
                    (width, height),
                    (width.div_ceil(3), height.div_ceil(2)),
                ] {
                    for crop in [0, 1] {
                        let layout = Layout {
                            left: crop,
                            top: crop,
                            width,
                            height,
                            output_width,
                            output_height,
                            display_width: output_width,
                            display_height: output_height,
                            original_dimensions: (width, height),
                            matrix: [1, 0, 0, 1],
                        };
                        let source_width = width + crop;
                        let source_height = height + crop;
                        let source: Vec<u8> = (0..source_width * source_height * 3)
                            .map(|i| ((i * 73 + i / 11) % 256) as u8)
                            .collect();
                        let budget = Budget::new(1024 * 1024);
                        let mut accumulator = Accumulator::new(layout, &budget).unwrap();
                        let mut output =
                            vec![0; output_width as usize * output_height as usize * 3];
                        for ty in (0..source_height).step_by(tile_height as usize) {
                            for tx in (0..source_width).step_by(tile_width as usize) {
                                let mut tile = vec![0; (tile_width * tile_height * 3) as usize];
                                for y in 0..tile_height.min(source_height - ty) {
                                    for x in 0..tile_width.min(source_width - tx) {
                                        let src = (((ty + y) * source_width + tx + x) * 3) as usize;
                                        let dst = ((y * tile_width + x) * 3) as usize;
                                        tile[dst..dst + 3].copy_from_slice(&source[src..src + 3]);
                                    }
                                }
                                let region = layout.region(tx, ty, tile_width, tile_height);
                                let sums = contributions(layout, region, &tile, tx, ty, tile_width)
                                    .unwrap();
                                if !sums.is_empty() {
                                    accumulator.merge(region, &sums, &mut output);
                                }
                            }
                            accumulator.finish_tile_row();
                        }
                        for oy in 0..output_height {
                            for ox in 0..output_width {
                                let x0 = f64::from(ox) * f64::from(width) / f64::from(output_width);
                                let x1 =
                                    f64::from(ox + 1) * f64::from(width) / f64::from(output_width);
                                let y0 =
                                    f64::from(oy) * f64::from(height) / f64::from(output_height);
                                let y1 = f64::from(oy + 1) * f64::from(height)
                                    / f64::from(output_height);
                                let mut sums = [0.0; 3];
                                for sy in y0.floor() as u32..y1.ceil() as u32 {
                                    for sx in x0.floor() as u32..x1.ceil() as u32 {
                                        let weight = (x1.min(f64::from(sx + 1))
                                            - x0.max(f64::from(sx)))
                                            * (y1.min(f64::from(sy + 1)) - y0.max(f64::from(sy)));
                                        let index =
                                            (((sy + crop) * source_width + sx + crop) * 3) as usize;
                                        for c in 0..3 {
                                            sums[c] += f64::from(source[index + c]) * weight;
                                        }
                                    }
                                }
                                for c in 0..3 {
                                    let expected =
                                        (sums[c] / ((x1 - x0) * (y1 - y0))).round() as u8;
                                    let actual =
                                        output[((oy * output_width + ox) * 3) as usize + c];
                                    assert!(
                                        actual.abs_diff(expected) <= 1,
                                        "source {width}x{height}, tiles {tile_width}x{tile_height}, output {output_width}x{output_height}, crop {crop}, pixel ({ox},{oy}): {actual} != {expected}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
