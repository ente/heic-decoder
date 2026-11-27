use crate::chroma_upsampling::{HeicRegion, upsample_heic_region_to_yuv444};
use crate::heic_decoder::hevc::DecodedFrame;
use crate::heic_decoder::hevc::bounded::Geometry;
use crate::isobmff::{PrimaryItemColorProperties, PrimaryItemTransformProperty as Transform};
use crate::{
    DecodeHeicError, DecodedHeicImage, HeicColrOverride, HeicPixelLayout, HeicPlane,
    YCbCrMatrixCoefficients, YCbCrRange,
};

use super::color::ToneMap;
use super::grid::{Color, Grid};
use super::memory::{Budget, Buffer};
use super::resample::{Rectangle, transformed_source, zeroed};
use super::{BoundedDecodeError as Error, Result};

const BAND_PIXELS: usize = 1 << 14;

pub(super) trait Samples {
    fn luma(&self, x: u32, y: u32, out: &mut [u16]);
    fn chroma(&self, plane: usize, x: u32, y: u32, out: &mut [u16]);
}

pub(super) struct Yuv444 {
    window: Rectangle,
    bit_depth: u8,
    range: YCbCrRange,
    matrix: YCbCrMatrixCoefficients,
    colr: HeicColrOverride,
}

impl Yuv444 {
    pub(super) fn new(grid: &Grid<'_>, color: &Color<'_>) -> Result<Option<Self>> {
        let transforms: Vec<Transform> = grid
            .properties
            .transforms
            .iter()
            .flatten()
            .copied()
            .collect();
        for (index, transform) in transforms.iter().enumerate() {
            let (plan, window) = transformed_source(grid.width, grid.height, &transforms[..index])?;
            if crate::yuv_transforms::libheif_converts_to_yuv444_before(
                HeicPixelLayout::Yuv420,
                plan.destination_width,
                plan.destination_height,
                transform,
            ) {
                let geometry = grid.tiles[0].geometry;
                return Ok(Some(Self {
                    window,
                    bit_depth: geometry.bit_depth,
                    range: if geometry.full_range {
                        YCbCrRange::Full
                    } else {
                        YCbCrRange::Limited
                    },
                    matrix: YCbCrMatrixCoefficients {
                        matrix_coefficients: u16::from(geometry.matrix),
                        colour_primaries: u16::from(geometry.primaries),
                    },
                    colr: HeicColrOverride::from_colr(&PrimaryItemColorProperties {
                        nclx: color.nclx.clone(),
                        icc: None,
                    }),
                }));
            }
        }
        Ok(None)
    }

    pub(super) fn workspace_bytes(width: u32) -> usize {
        let width = width as usize;
        (width + 5) * (band_rows(width) + 5) * 10
    }

    pub(super) fn convert(
        &self,
        samples: &impl Samples,
        area: Rectangle,
        tone_map: Option<&ToneMap>,
        out: &mut [u8],
    ) -> Result<()> {
        if area.is_empty() {
            return Ok(());
        }
        let window = self.window;
        if !window.contains(area) {
            return Err(Error::Malformed("4:4:4 area outside the converted image"));
        }
        let width = area.width as usize;
        let first_column = (area.left.saturating_sub(1) / 2).max(window.left / 2);
        let last_column = (area.right() / 2).min((window.right() - 1) / 2);
        let chroma_width = (last_column - first_column + 1) as usize;
        let luma_left = area.left - 2 * first_column;
        let rows = band_rows(width) as u32;
        let mut top = area.top;
        while top < area.bottom() {
            let bottom = area.bottom().min(top.saturating_add(rows));
            let first_row = (top.saturating_sub(1) / 2).max(window.top / 2);
            let last_row = (bottom / 2).min((window.bottom() - 1) / 2);
            let chroma_height = (last_row - first_row + 1) as usize;
            let mut luma = zeroed::<u16>(4 * chroma_width * chroma_height)?;
            for y in top..bottom {
                let start = (y - 2 * first_row) as usize * 2 * chroma_width + luma_left as usize;
                samples.luma(area.left, y, &mut luma[start..start + width]);
            }
            let mut planes = [
                zeroed::<u16>(chroma_width * chroma_height)?,
                zeroed::<u16>(chroma_width * chroma_height)?,
            ];
            for (plane, data) in planes.iter_mut().enumerate() {
                for (row, y) in data.chunks_exact_mut(chroma_width).zip(first_row..) {
                    samples.chroma(plane, first_column, y, row);
                }
            }
            let [u, v] = planes;
            let chroma_plane = |samples| HeicPlane {
                width: chroma_width as u32,
                height: chroma_height as u32,
                samples,
            };
            let source = DecodedHeicImage {
                width: 2 * chroma_width as u32,
                height: 2 * chroma_height as u32,
                bit_depth_luma: self.bit_depth,
                bit_depth_chroma: self.bit_depth,
                layout: HeicPixelLayout::Yuv420,
                ycbcr_range: self.range,
                ycbcr_matrix: self.matrix,
                y_plane: HeicPlane {
                    width: 2 * chroma_width as u32,
                    height: 2 * chroma_height as u32,
                    samples: luma,
                },
                u_plane: Some(chroma_plane(u)),
                v_plane: Some(chroma_plane(v)),
            };
            let mut band = upsample_heic_region_to_yuv444(
                &source,
                HeicRegion {
                    left: luma_left,
                    top: top - 2 * first_row,
                    width: area.width,
                    height: bottom - top,
                },
            )
            .map_err(conversion_error)?;
            drop(source);
            self.colr.apply(&mut band);
            let start = (top - area.top) as usize * width * 3;
            let end = (bottom - area.top) as usize * width * 3;
            let output = &mut out[start..end];
            match tone_map {
                Some(tone_map) => {
                    let mut samples = zeroed::<u16>(width * 3)?;
                    for (row, output) in output.chunks_exact_mut(width * 3).enumerate() {
                        crate::convert_heic_to_rgb_samples_region_slice(
                            &band,
                            0,
                            row,
                            width,
                            1,
                            &mut samples,
                        )
                        .map_err(conversion_error)?;
                        tone_map.apply(&samples, output);
                    }
                }
                None => crate::convert_heic_to_interleaved_rgb8_region_slice::<3>(
                    &band,
                    0,
                    0,
                    width,
                    (bottom - top) as usize,
                    output,
                    crate::scale_heic_sample_to_image_rgb8,
                    "bounded RGB",
                )
                .map_err(conversion_error)?,
            }
            top = bottom;
        }
        Ok(())
    }
}

fn band_rows(width: usize) -> usize {
    (BAND_PIXELS / width.max(1)).max(1)
}

fn conversion_error(error: DecodeHeicError) -> Error {
    match error {
        DecodeHeicError::AllocationFailed { .. } => Error::AllocationFailed,
        _ => Error::Unsupported("4:4:4 chroma conversion"),
    }
}

#[derive(Default)]
pub(super) struct Halo {
    left: u32,
    top_luma: Vec<u16>,
    top_chroma: [Vec<u16>; 2],
    left_luma: Vec<u16>,
    left_chroma: [Vec<u16>; 2],
}

impl Halo {
    pub(super) fn bytes(geometry: Geometry) -> usize {
        8 * (geometry.width as usize + geometry.height as usize + 8)
    }
}

pub(super) struct TileSamples<'a> {
    pub(super) frame: &'a DecodedFrame,
    pub(super) geometry: Geometry,
    pub(super) origin: [u32; 2],
    pub(super) halo: &'a Halo,
}

impl TileSamples<'_> {
    fn plane(&self, plane: usize) -> &[u16] {
        if plane == 0 {
            &self.frame.cb_plane
        } else {
            &self.frame.cr_plane
        }
    }
}

impl Samples for TileSamples<'_> {
    fn luma(&self, x: u32, y: u32, out: &mut [u16]) {
        let [origin_x, origin_y] = self.origin;
        if y + 1 == origin_y {
            let start = (x - self.halo.left) as usize;
            out.copy_from_slice(&self.halo.top_luma[start..start + out.len()]);
            return;
        }
        let (out, x) = if x + 1 == origin_x {
            out[0] = self.halo.left_luma[(y - origin_y) as usize];
            (&mut out[1..], x + 1)
        } else {
            (out, x)
        };
        let start = (self.geometry.crop[2] + y - origin_y) as usize * self.frame.y_stride()
            + (self.geometry.crop[0] + x - origin_x) as usize;
        out.copy_from_slice(&self.frame.y_plane[start..start + out.len()]);
    }

    fn chroma(&self, plane: usize, x: u32, y: u32, out: &mut [u16]) {
        let [origin_x, origin_y] = self.origin.map(|n| n / 2);
        if y + 1 == origin_y {
            let start = (x - self.halo.left / 2) as usize;
            out.copy_from_slice(&self.halo.top_chroma[plane][start..start + out.len()]);
            return;
        }
        let (out, x) = if x + 1 == origin_x {
            out[0] = self.halo.left_chroma[plane][(y - origin_y) as usize];
            (&mut out[1..], x + 1)
        } else {
            (out, x)
        };
        let start = (self.geometry.crop[2] / 2 + y - origin_y) as usize * self.frame.c_stride()
            + (self.geometry.crop[0] / 2 + x - origin_x) as usize;
        out.copy_from_slice(&self.plane(plane)[start..start + out.len()]);
    }
}

pub(super) struct Edges {
    width: u32,
    columns: u32,
    rows: u32,
    tile_width: u32,
    tile_height: u32,
    luma: Buffer<u16>,
    chroma: [Buffer<u16>; 2],
    left_luma: Buffer<u16>,
    left_chroma: [Buffer<u16>; 2],
    left_decoded: bool,
}

impl Edges {
    pub(super) fn new(grid: &Grid<'_>, budget: &Budget) -> Result<Self> {
        let tile = grid.tiles[0].geometry;
        let chroma_width = grid.width.div_ceil(2) as usize;
        let stage = "4:4:4 tile edges";
        Ok(Self {
            width: grid.width,
            columns: grid.columns,
            rows: grid.tiles.len() as u32 / grid.columns,
            tile_width: tile.width,
            tile_height: tile.height,
            luma: budget.zeroed(grid.width as usize, stage)?,
            chroma: [
                budget.zeroed(chroma_width, stage)?,
                budget.zeroed(chroma_width, stage)?,
            ],
            left_luma: budget.zeroed(tile.height as usize, stage)?,
            left_chroma: [
                budget.zeroed(tile.height as usize / 2, stage)?,
                budget.zeroed(tile.height as usize / 2, stage)?,
            ],
            left_decoded: false,
        })
    }

    pub(super) fn advance(
        &mut self,
        index: usize,
        frame: Option<&DecodedFrame>,
        geometry: Geometry,
    ) -> Result<Halo> {
        let column = index as u32 % self.columns;
        let row = index as u32 / self.columns;
        let (tile_width, tile_height) = (self.tile_width, self.tile_height);
        let x = column * tile_width;
        let last_column = column + 1 == self.columns;
        let chroma_width = self.width.div_ceil(2);
        let mut halo = Halo::default();
        if frame.is_some() {
            if row > 0 {
                let start = x.saturating_sub(1);
                halo.left = start;
                halo.top_luma =
                    copy(&self.luma[start as usize..(x + tile_width).min(self.width) as usize])?;
                let chroma_end = ((x + tile_width) / 2 + 1).min(chroma_width) as usize;
                for (halo, edge) in halo.top_chroma.iter_mut().zip(&self.chroma) {
                    *halo = copy(&edge[start as usize / 2..chroma_end])?;
                }
            }
            if column > 0 {
                halo.left_luma = copy(&self.left_luma)?;
                for (halo, edge) in halo.left_chroma.iter_mut().zip(&self.left_chroma) {
                    *halo = copy(edge)?;
                }
            }
        }
        if row + 1 < self.rows {
            if column > 0 && self.left_decoded {
                self.luma[x as usize - 1] = self.left_luma[tile_height as usize - 1];
                for (edge, left) in self.chroma.iter_mut().zip(&self.left_chroma) {
                    edge[x as usize / 2 - 1] = left[tile_height as usize / 2 - 1];
                }
            }
            if let Some(frame) = frame {
                let end = if last_column {
                    (x + tile_width).min(self.width)
                } else {
                    x + tile_width - 1
                };
                let start = (geometry.crop[2] + tile_height - 1) as usize * frame.y_stride()
                    + geometry.crop[0] as usize;
                self.luma[x as usize..end as usize]
                    .copy_from_slice(&frame.y_plane[start..start + (end - x) as usize]);
                let chroma_end = if last_column {
                    ((x + tile_width) / 2).min(chroma_width)
                } else {
                    (x + tile_width) / 2 - 1
                };
                let start = (geometry.crop[2] / 2 + tile_height / 2 - 1) as usize
                    * frame.c_stride()
                    + geometry.crop[0] as usize / 2;
                let count = (chroma_end - x / 2) as usize;
                for (edge, plane) in self
                    .chroma
                    .iter_mut()
                    .zip([&frame.cb_plane, &frame.cr_plane])
                {
                    edge[x as usize / 2..chroma_end as usize]
                        .copy_from_slice(&plane[start..start + count]);
                }
            }
        }
        if !last_column {
            self.left_decoded = frame.is_some();
            if let Some(frame) = frame {
                let column = (geometry.crop[0] + tile_width - 1) as usize;
                for (y, sample) in self.left_luma.iter_mut().enumerate() {
                    *sample =
                        frame.y_plane[(geometry.crop[2] as usize + y) * frame.y_stride() + column];
                }
                let column = (geometry.crop[0] / 2 + tile_width / 2 - 1) as usize;
                for (edge, plane) in self
                    .left_chroma
                    .iter_mut()
                    .zip([&frame.cb_plane, &frame.cr_plane])
                {
                    for (y, sample) in edge.iter_mut().enumerate() {
                        *sample =
                            plane[(geometry.crop[2] as usize / 2 + y) * frame.c_stride() + column];
                    }
                }
            }
        }
        Ok(halo)
    }
}

fn copy(samples: &[u16]) -> Result<Vec<u16>> {
    let mut copied = Vec::new();
    copied
        .try_reserve_exact(samples.len())
        .map_err(|_| Error::AllocationFailed)?;
    copied.extend_from_slice(samples);
    Ok(copied)
}
