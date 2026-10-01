use std::io::{BufReader, Read};

use crate::heic_decoder::hevc::DecodedFrame;
use crate::heic_decoder::hevc::bounded::{Geometry, Prepared};

use super::container::{Index, Reader, Source};
use super::grid::{Grid, Properties, Tile};
use super::memory::Budget;
use super::resample::{Accumulator, Layout, Region, zeroed};
use super::{
    BoundedDecodeError as Error, BoundedDecodeOptions, BoundedInput, BoundedRgbImage, Result,
    codec, color,
};

struct Slice {
    offset: usize,
    length: usize,
    prefix: Vec<u8>,
}

fn slice(index: &Index<'_>, source: &mut Source<'_>, item: usize, config: &[u8]) -> Result<Slice> {
    let (_, length_size) = codec::configuration(config)?;
    let length = index.items[item]
        .location
        .ok_or(Error::Malformed("coded item location"))?
        .length;
    let mut position = 0;
    let mut found = None;
    let mut count = 0;
    while position < length {
        count += 1;
        if count > 1024 {
            return Err(Error::LimitExceeded("NAL count"));
        }
        let mut bytes = [0; 4];
        index.read_range(item, source, position, &mut bytes[..length_size])?;
        let size = bytes[..length_size]
            .iter()
            .fold(0usize, |n, &b| (n << 8) | usize::from(b));
        position += length_size;
        if size < 2 || size > length - position {
            return Err(Error::Malformed("NAL length"));
        }
        index.read_range(item, source, position, &mut bytes[..2])?;
        if bytes[0] & 0x81 != 0 || bytes[1] != 1 {
            return Err(Error::Unsupported("NAL layer or temporal id"));
        }
        match bytes[0] >> 1 & 63 {
            19 | 20 => {
                if found.is_some() {
                    return Err(Error::Unsupported("multiple slices"));
                }
                let mut prefix = zeroed(size.min(65536))?;
                index.read_range(item, source, position, &mut prefix)?;
                found = Some(Slice {
                    offset: position + 2,
                    length: size - 2,
                    prefix,
                });
            }
            35 | 38..=40 => {}
            _ => {
                return Err(Error::Unsupported(
                    "experiment requires configuration parameter sets and one IDR",
                ));
            }
        }
        position += size;
    }
    found.ok_or(Error::Malformed("missing IDR"))
}

fn prepared<'a>(config: &'a [u8], slice: &'a Slice) -> Result<Prepared<'a>> {
    let (nals, _) = codec::configuration(config)?;
    let parameter = |i: usize| nals.parameters[i].ok_or(Error::Malformed("missing parameter set"));
    Ok(Prepared::new(
        parameter(0)?,
        parameter(1)?,
        parameter(2)?,
        &slice.prefix,
    )?)
}

struct ItemRange<'a, 'b, 'c> {
    index: &'a Index<'b>,
    source: &'a mut Source<'c>,
    item: usize,
    position: usize,
    remaining: usize,
}

impl Read for ItemRange<'_, '_, '_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let count = output.len().min(self.remaining);
        self.index
            .read_range(self.item, self.source, self.position, &mut output[..count])
            .map_err(std::io::Error::other)?;
        self.position += count;
        self.remaining -= count;
        Ok(count)
    }
}

struct Rbsp<R> {
    input: R,
    zeroes: u8,
}

impl<R: Read> Read for Rbsp<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let mut count = 0;
        while count < output.len() {
            let mut byte = [0];
            if self.input.read(&mut byte)? == 0 {
                break;
            }
            let byte = byte[0];
            if self.zeroes == 2 && byte == 3 {
                self.zeroes = 0;
                continue;
            }
            self.zeroes = if byte == 0 {
                (self.zeroes + 1).min(2)
            } else {
                0
            };
            output[count] = byte;
            count += 1;
        }
        Ok(count)
    }
}

fn plan<'a>(index: &Index<'a>, source: &mut Source<'_>, budget: &Budget) -> Result<Grid<'a>> {
    let primary = index.primary;
    let properties = Properties::read_incremental(index, primary, false)?;
    let (width, height) = properties
        .dimensions
        .ok_or(Error::Malformed("primary dimensions"))?;
    let is_grid = index.items[primary].kind == *b"grid";
    let (rows, columns) = if is_grid {
        let len = index.items[primary]
            .location
            .ok_or(Error::Malformed("grid location"))?
            .length;
        if !matches!(len, 8 | 12) {
            return Err(Error::Malformed("grid length"));
        }
        let mut bytes = [0; 12];
        index.read_item(primary, source, &mut bytes[..len])?;
        let mut r = Reader::new(&bytes[..len]);
        if r.uint(1)? != 0 {
            return Err(Error::Unsupported("grid version"));
        }
        let flags = r.uint(1)?;
        if flags > 1 {
            return Err(Error::Unsupported("grid flags"));
        }
        let rows = r.uint(1)? as u32 + 1;
        let columns = r.uint(1)? as u32 + 1;
        let size = if flags == 0 { 2 } else { 4 };
        if r.uint(size)? != u64::from(width) || r.uint(size)? != u64::from(height) {
            return Err(Error::Malformed("grid dimensions"));
        }
        r.finish()?;
        (rows, columns)
    } else {
        (1, 1)
    };
    if width == 0
        || height == 0
        || u64::from(width) * u64::from(height) > 256_000_000
        || rows * columns > 4096
    {
        return Err(Error::LimitExceeded("experiment image dimensions"));
    }
    let tile_count = (rows * columns) as usize;
    let reference = if is_grid {
        let mut references = index
            .references
            .iter()
            .filter(|r| r.from == index.items[primary].id && r.kind == *b"dimg");
        let reference = references
            .next()
            .ok_or(Error::Malformed("grid references"))?;
        if references.next().is_some() {
            return Err(Error::Malformed("duplicate grid references"));
        }
        if reference.targets().count() != tile_count {
            return Err(Error::Malformed("tile count"));
        }
        Some(reference)
    } else {
        None
    };
    let tile_ids = reference
        .into_iter()
        .flat_map(|reference| reference.targets())
        .chain((!is_grid).then_some(index.items[primary].id));
    let mut tiles = budget.buffer::<Tile<'a>>(tile_count, "tile index")?;
    for id in tile_ids {
        let item = index.item(id)?;
        if !matches!(&index.items[item].kind, b"hvc1" | b"hev1") {
            return Err(Error::Unsupported("direct HEVC item required"));
        }
        let props = Properties::read_incremental(index, item, is_grid)?;
        let config = props.config.ok_or(Error::Malformed("missing hvcC"))?;
        let _headers = budget.reserve(2 * 1024 * 1024, "header parsing")?;
        let slice = slice(index, source, item, config)?;
        let prepared = prepared(config, &slice)?;
        let geometry = prepared.geometry;
        let workspace_bytes = prepared.incremental_workspace()?;
        if props.dimensions != Some((geometry.width, geometry.height)) {
            return Err(Error::Malformed("ispe differs from SPS"));
        }
        if tiles
            .first()
            .is_some_and(|first| first.geometry != geometry || first.color != props.color)
        {
            return Err(Error::Unsupported("nonuniform grid"));
        }
        tiles.push(Tile {
            item,
            config,
            geometry,
            color: props.color,
            payload_len: slice.length,
            workspace_bytes,
        })?;
    }
    let g = tiles[0].geometry;
    if g.width * columns < width
        || g.height * rows < height
        || g.width * (columns - 1) >= width
        || g.height * (rows - 1) >= height
    {
        return Err(Error::Malformed("tile coverage"));
    }
    let exif = super::grid::validate_references(index, source, &tiles, budget)?;
    Ok(Grid {
        width,
        height,
        columns,
        tiles,
        properties,
        exif,
    })
}

pub fn decode(
    input: BoundedInput<'_>,
    options: BoundedDecodeOptions,
) -> Result<(BoundedRgbImage, [u64; 3])> {
    if options.max_side == 0 || options.max_side > 6000 || options.max_memory_bytes < 65536 {
        return Err(Error::InvalidOptions("experiment dimensions or budget"));
    }
    if cfg!(feature = "decoder-tracing") {
        return Err(Error::Unsupported("decoder tracing"));
    }
    let budget = Budget::new(options.max_memory_bytes);
    let _runtime = budget.reserve(65536, "runtime")?;
    let mut source = Source::new(input)?;
    let metadata = source.metadata(&budget)?;
    let index = Index::parse_primary(&metadata, source.len(), &budget, true)?;
    let grid = plan(&index, &mut source, &budget)?;
    let layout = Layout::new(&grid, options.max_side)?;
    for (i, transform) in grid.properties.transforms.iter().flatten().enumerate() {
        if i > 0
            && matches!(
                transform,
                crate::isobmff::PrimaryItemTransformProperty::CleanAperture(_)
            )
        {
            return Err(Error::Unsupported(
                "repeated crop or crop after orientation",
            ));
        }
    }
    let bilinear = layout.left % 2 != 0 || layout.top % 2 != 0;
    if bilinear && grid.tiles.len() != 1 {
        return Err(Error::Unsupported("odd grid crop chroma interpolation"));
    }
    let _chroma_workspace = if bilinear {
        Some(budget.reserve(
            grid.tiles[0].geometry.coded_width as usize * 4 * 65,
            "chroma interpolation band",
        )?)
    } else {
        None
    };
    let mut color = grid.tiles[0].color.clone();
    if grid
        .properties
        .color
        .nclx
        .as_ref()
        .is_some_and(|n| !n.is_undefined())
    {
        color.nclx = grid.properties.color.nclx.clone();
    }
    if grid.properties.color.icc.is_some() {
        color.icc = grid.properties.color.icc;
    }
    let transform = color::ColorTransform::new(
        &color,
        grid.tiles[0].geometry.primaries,
        grid.tiles[0].geometry.transfer,
        &budget,
    )?;
    let mut accumulator = Accumulator::new(layout, &budget)?;
    let output_len = layout.display_width as usize * layout.display_height as usize * 3;
    let workspace = grid.tiles.iter().map(|t| t.workspace_bytes).max().unwrap();
    let _workspace = budget.reserve(workspace, "rolling reconstruction and conversion")?;
    let mut output = budget.zeroed(output_len, "RGB output")?;
    let mut statistics = [0; 3];
    for (i, tile) in grid.tiles.iter().enumerate() {
        let tx = i as u32 % grid.columns * tile.geometry.width;
        let ty = i as u32 / grid.columns * tile.geometry.height;
        let region = layout.region(tx, ty, tile.geometry.width, tile.geometry.height);
        let mut rows = Rows::new(layout, region)?;
        let slice = slice(&index, &mut source, tile.item, tile.config)?;
        let prepared = prepared(tile.config, &slice)?;
        if prepared.geometry != tile.geometry {
            return Err(Error::Malformed("changed coded header"));
        }
        let range = ItemRange {
            index: &index,
            source: &mut source,
            item: tile.item,
            position: slice.offset,
            remaining: slice.length,
        };
        let mut rbsp = Rbsp {
            input: BufReader::with_capacity(16384, range),
            zeroes: 0,
        };
        let mut conversion = Conversion::default();
        let mut bands = ChromaBands::new(if bilinear {
            Some(prepared.output_band()?)
        } else {
            None
        })?;
        let mut conversion_geometry = tile.geometry;
        if grid.tiles.len() == 1 {
            conversion_geometry.width = conversion_geometry.width.min(grid.width);
            conversion_geometry.height = conversion_geometry.height.min(grid.height);
        }
        let mut process =
            |start: u32, frame: &DecodedFrame, halo: Option<ChromaHalo<'_>>| -> Result<()> {
                let g = conversion_geometry;
                let first = start.max(g.crop[2]);
                let last = (start + frame.height).min(g.crop[2] + g.height);
                if first >= last {
                    return Ok(());
                }
                let pixels = conversion.convert(
                    frame,
                    g,
                    ConversionRegion {
                        start,
                        y: first - start,
                        height: last - first,
                        halo,
                    },
                    &color,
                    &transform,
                )?;
                for row in 0..last - first {
                    let y = ty + first - g.crop[2] + row;
                    if y < layout.top + region.top || y >= layout.top + region.bottom {
                        continue;
                    }
                    let offset = row as usize * g.width as usize * 3;
                    rows.add(
                        y - layout.top,
                        tx,
                        &pixels[offset..offset + g.width as usize * 3],
                        &mut accumulator,
                        &mut output,
                    );
                }
                Ok(())
            };
        let stats = if u64::from(tile.geometry.width) * u64::from(tile.geometry.height)
            >= 1024 * 1024
        {
            let spare = prepared.output_band()?;
            std::thread::scope(|scope| -> Result<_> {
                let (work_tx, work_rx) = std::sync::mpsc::sync_channel::<(u32, DecodedFrame)>(0);
                let (free_tx, free_rx) = std::sync::mpsc::sync_channel(1);
                free_tx
                    .send(spare)
                    .map_err(|_| Error::Unsupported("band queue initialization"))?;
                let worker = std::thread::Builder::new()
                    .stack_size(512 * 1024)
                    .spawn_scoped(scope, move || -> Result<()> {
                        while let Ok((start, mut frame)) = work_rx.recv() {
                            bands.push(start, &mut frame, &mut process)?;
                            free_tx
                                .send(frame)
                                .map_err(|_| Error::Unsupported("band return queue"))?;
                        }
                        bands.finish(&mut process)
                    })
                    .map_err(|_| Error::AllocationFailed)?;
                let decoded = prepared.decode_stream(&mut rbsp, |start, frame| {
                    let mut spare = free_rx.recv().map_err(|_| {
                        crate::heic_decoder::HevcError::DecodingError("band worker stopped")
                    })?;
                    std::mem::swap(frame, &mut spare);
                    work_tx.send((start, spare)).map_err(|_| {
                        crate::heic_decoder::HevcError::DecodingError("band worker stopped")
                    })
                });
                drop(work_tx);
                worker
                    .join()
                    .map_err(|_| Error::Unsupported("band worker panicked"))??;
                Ok(decoded?)
            })?
        } else {
            let mut callback_error = None;
            let decoded = prepared.decode_stream(&mut rbsp, |start, frame| {
                if let Err(error) = bands.push(start, frame, &mut process) {
                    callback_error = Some(error);
                    return Err(crate::heic_decoder::HevcError::DecodingError(
                        "row sink failed",
                    ));
                }
                Ok(())
            });
            if let Some(error) = callback_error {
                return Err(error);
            }
            let stats = decoded?;
            bands.finish(&mut process)?;
            stats
        };
        statistics[0] += u64::from(stats.rows);
        statistics[1] += stats.sao_edge_components as u64;
        statistics[2] += stats.sao_band_components as u64;
        rows.finish(&mut accumulator, &mut output);
        if (i as u32 + 1).is_multiple_of(grid.columns) {
            accumulator.finish_tile_row();
        }
    }
    Ok((
        BoundedRgbImage {
            image: crate::DecodedRgbImage {
                width: layout.display_width,
                height: layout.display_height,
                pixels: output.into_vec(),
                source_bit_depth: 8,
                icc_profile: None,
            },
            original_dimensions: layout.original_dimensions,
        },
        statistics,
    ))
}

struct Rows {
    layout: Layout,
    region: Region,
    y: u32,
    sums: Vec<[u64; 3]>,
    horizontal: Vec<[u32; 3]>,
    spans: Vec<Span>,
}

#[derive(Default, Clone)]
struct Span {
    start: u32,
    end: u32,
    first: u32,
    last: u32,
}

impl Rows {
    fn new(layout: Layout, region: Region) -> Result<Self> {
        let width = if layout.is_unscaled() || region.len() == 0 {
            0
        } else {
            (region.output_right - region.output_left) as usize
        };
        let mut spans: Vec<Span> = zeroed(width)?;
        for (i, span) in spans.iter_mut().enumerate() {
            let x = u64::from(region.output_left) + i as u64;
            let x0 = x * u64::from(layout.width);
            let x1 = (x + 1) * u64::from(layout.width);
            let start = (x0 / u64::from(layout.output_width)).max(u64::from(region.left));
            let end = x1
                .div_ceil(u64::from(layout.output_width))
                .min(u64::from(region.right));
            let weight = |sx: u64| {
                (x1.min((sx + 1) * u64::from(layout.output_width))
                    - x0.max(sx * u64::from(layout.output_width))) as u32
            };
            *span = Span {
                start: start as u32,
                end: end as u32,
                first: weight(start),
                last: weight(end - 1),
            };
        }
        Ok(Self {
            layout,
            region,
            y: region.output_top,
            sums: zeroed(width)?,
            horizontal: zeroed(width)?,
            spans,
        })
    }

    fn add(
        &mut self,
        sy: u32,
        tx: u32,
        pixels: &[u8],
        accumulator: &mut Accumulator,
        output: &mut [u8],
    ) {
        let l = self.layout;
        if self.region.len() == 0 {
            return;
        }
        if l.is_unscaled() {
            let start = (self.region.left + l.left - tx) as usize * 3;
            let end = (self.region.right + l.left - tx) as usize * 3;
            l.write_row(self.region.left, sy, &pixels[start..end], output);
            return;
        }
        for (sum, span) in self.horizontal.iter_mut().zip(&self.spans) {
            let start = (span.start + l.left - tx) as usize * 3;
            let end = (span.end + l.left - tx) as usize * 3;
            let input = &pixels[start..end];
            for c in 0..3 {
                sum[c] = u32::from(input[c]) * span.first;
            }
            if input.len() > 3 {
                for pixel in input[3..input.len() - 3].chunks_exact(3) {
                    for c in 0..3 {
                        sum[c] += u32::from(pixel[c]) * l.output_width;
                    }
                }
                for c in 0..3 {
                    sum[c] += u32::from(input[input.len() - 3 + c]) * span.last;
                }
            }
        }
        let oy_start = u64::from(sy) * u64::from(l.output_height) / u64::from(l.height);
        let oy_end = (u64::from(sy + 1) * u64::from(l.output_height)).div_ceil(u64::from(l.height));
        for oy in oy_start as u32..oy_end as u32 {
            if oy != self.y {
                accumulator.merge_row(self.region, self.y, &self.sums, output);
                self.sums.fill([0; 3]);
                self.y = oy;
            }
            let wy = (u64::from(sy + 1) * u64::from(l.output_height))
                .min(u64::from(oy + 1) * u64::from(l.height))
                - (u64::from(sy) * u64::from(l.output_height))
                    .max(u64::from(oy) * u64::from(l.height));
            for (sum, horizontal) in self.sums.iter_mut().zip(&self.horizontal) {
                for c in 0..3 {
                    sum[c] += u64::from(horizontal[c]) * wy;
                }
            }
        }
    }

    fn finish(self, accumulator: &mut Accumulator, output: &mut [u8]) {
        if self.region.len() != 0 && !self.layout.is_unscaled() {
            accumulator.merge_row(self.region, self.y, &self.sums, output);
        }
    }
}

#[derive(Default)]
struct Conversion {
    rgb: Vec<u8>,
    converted: Vec<u8>,
}

#[derive(Clone, Copy)]
struct ChromaHalo<'a> {
    before: [&'a [u16]; 2],
    after: [&'a [u16]; 2],
}

struct ConversionRegion<'a> {
    start: u32,
    y: u32,
    height: u32,
    halo: Option<ChromaHalo<'a>>,
}

struct ChromaBands {
    pending: Option<DecodedFrame>,
    start: Option<u32>,
    before: [Vec<u16>; 2],
}

impl ChromaBands {
    fn new(pending: Option<DecodedFrame>) -> Result<Self> {
        let width = pending.as_ref().map_or(0, DecodedFrame::c_stride);
        Ok(Self {
            pending,
            start: None,
            before: [zeroed(width)?, zeroed(width)?],
        })
    }

    fn push(
        &mut self,
        start: u32,
        frame: &mut DecodedFrame,
        emit: &mut impl FnMut(u32, &DecodedFrame, Option<ChromaHalo<'_>>) -> Result<()>,
    ) -> Result<()> {
        let Some(pending) = &mut self.pending else {
            return emit(start, frame, None);
        };
        let width = frame.c_stride();
        if let Some(previous_start) = self.start {
            emit(
                previous_start,
                pending,
                Some(ChromaHalo {
                    before: [&self.before[0], &self.before[1]],
                    after: [&frame.cb_plane[..width], &frame.cr_plane[..width]],
                }),
            )?;
            let last = (pending.height as usize / 2 - 1) * width;
            self.before[0].copy_from_slice(&pending.cb_plane[last..last + width]);
            self.before[1].copy_from_slice(&pending.cr_plane[last..last + width]);
        } else {
            self.before[0].copy_from_slice(&frame.cb_plane[..width]);
            self.before[1].copy_from_slice(&frame.cr_plane[..width]);
        }
        std::mem::swap(pending, frame);
        self.start = Some(start);
        Ok(())
    }

    fn finish(
        &self,
        emit: &mut impl FnMut(u32, &DecodedFrame, Option<ChromaHalo<'_>>) -> Result<()>,
    ) -> Result<()> {
        if let (Some(pending), Some(start)) = (&self.pending, self.start) {
            let width = pending.c_stride();
            let last = (pending.height as usize / 2 - 1) * width;
            emit(
                start,
                pending,
                Some(ChromaHalo {
                    before: [&self.before[0], &self.before[1]],
                    after: [
                        &pending.cb_plane[last..last + width],
                        &pending.cr_plane[last..last + width],
                    ],
                }),
            )?;
        }
        Ok(())
    }
}

impl Conversion {
    fn convert(
        &mut self,
        frame: &DecodedFrame,
        geometry: Geometry,
        region: ConversionRegion<'_>,
        color: &super::grid::Color<'_>,
        transform: &color::ColorTransform,
    ) -> Result<&[u8]> {
        let ConversionRegion {
            start,
            y,
            height,
            halo,
        } = region;
        let colr = crate::isobmff::PrimaryItemColorProperties {
            nclx: color.nclx.clone(),
            icc: None,
        };
        let range =
            crate::ycbcr_range_override_from_primary_colr(&colr).unwrap_or(if frame.full_range {
                crate::YCbCrRange::Full
            } else {
                crate::YCbCrRange::Limited
            });
        let matrix = crate::ycbcr_matrix_override_from_primary_colr(&colr).unwrap_or(
            crate::YCbCrMatrixCoefficients {
                matrix_coefficients: u16::from(frame.matrix_coeffs),
                colour_primaries: u16::from(frame.colour_primaries),
            },
        );
        let matrix = crate::ycbcr_transform_from_matrix(matrix)
            .map_err(|_| Error::Unsupported("row YCbCr matrix"))?;
        let converter = crate::PreparedYcbcrToRgb::new(8, range, matrix, true);
        let width = geometry.width as usize;
        let height = height as usize;
        let x = geometry.crop[0] as usize;
        let y = y as usize;
        let len = width * height * 3;
        self.rgb
            .try_reserve_exact(len.saturating_sub(self.rgb.len()))
            .map_err(|_| Error::AllocationFailed)?;
        self.rgb.resize(len, 0);
        if let Some(halo) = halo {
            for row in 0..height {
                for column in 0..width {
                    let cy = (y + row) / 2;
                    let sy = start as usize + y + row;
                    let neighbor_y = if sy.is_multiple_of(2) {
                        (sy / 2)
                            .saturating_sub(1)
                            .max(geometry.crop[2] as usize / 2)
                    } else {
                        (sy / 2 + 1)
                            .min(((geometry.crop[2] + geometry.height) as usize).div_ceil(2) - 1)
                    };
                    let cx = (x + column) / 2;
                    let neighbor_x = if column % 2 == 0 {
                        cx.saturating_sub(1).max(x / 2)
                    } else {
                        (cx + 1).min((x + width).div_ceil(2) - 1)
                    };
                    let chroma = |plane: &[u16], component: usize| {
                        let stride = frame.c_stride();
                        let current = &plane[cy * stride..(cy + 1) * stride];
                        let neighbor = if neighbor_y < start as usize / 2 {
                            halo.before[component]
                        } else if neighbor_y >= (start as usize + frame.height as usize) / 2 {
                            halo.after[component]
                        } else {
                            let local = neighbor_y - start as usize / 2;
                            &plane[local * stride..(local + 1) * stride]
                        };
                        (9 * i32::from(current[cx])
                            + 3 * i32::from(current[neighbor_x])
                            + 3 * i32::from(neighbor[cx])
                            + i32::from(neighbor[neighbor_x])
                            + 8)
                            / 16
                    };
                    let (r, g, b) = converter.convert(
                        i32::from(frame.y_plane[(y + row) * frame.y_stride() + x + column]),
                        chroma(&frame.cb_plane, 0),
                        chroma(&frame.cr_plane, 1),
                    );
                    self.rgb[(row * width + column) * 3..(row * width + column) * 3 + 3]
                        .copy_from_slice(&[r as u8, g as u8, b as u8]);
                }
            }
        } else if let crate::PreparedYcbcrTransform::MatrixFull { coeffs, .. } = converter.transform
        {
            crate::heic_decoder::hevc::color_convert::convert_420_8bit_region_to_interleaved(
                &frame.y_plane,
                &frame.cb_plane,
                &frame.cr_plane,
                frame.y_stride(),
                frame.c_stride(),
                x,
                y,
                width,
                height,
                coeffs.r_cr_fp8,
                coeffs.g_cb_fp8,
                coeffs.g_cr_fp8,
                coeffs.b_cb_fp8,
                3,
                &mut self.rgb,
            );
        } else if let Some(params) = crate::prepared_float_matrix_params(converter.transform) {
            crate::heic_decoder::hevc::color_convert::convert_float_matrix_8bit_region_to_interleaved(
                &frame.y_plane, &frame.cb_plane, &frame.cr_plane,
                frame.y_stride(), frame.c_stride(), 2, 2, x, y, width, height,
                params, 3, &mut self.rgb,
            );
        } else {
            for row in 0..height {
                for column in 0..width {
                    let yi = (y + row) * frame.y_stride() + x + column;
                    let ci = (y + row) / 2 * frame.c_stride() + (x + column) / 2;
                    let (r, g, b) = converter.convert(
                        i32::from(frame.y_plane[yi]),
                        i32::from(frame.cb_plane[ci]),
                        i32::from(frame.cr_plane[ci]),
                    );
                    let out = (row * width + column) * 3;
                    self.rgb[out..out + 3].copy_from_slice(&[r as u8, g as u8, b as u8]);
                }
            }
        }
        if transform.is_identity() {
            return Ok(&self.rgb);
        }
        self.converted
            .try_reserve_exact(len.saturating_sub(self.converted.len()))
            .map_err(|_| Error::AllocationFailed)?;
        self.converted.resize(len, 0);
        transform.apply(&self.rgb, &mut self.converted)?;
        Ok(&self.converted)
    }
}
