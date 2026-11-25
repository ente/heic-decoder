mod chroma;
mod codec;
mod color;
mod container;
mod grid;
#[cfg(feature = "incremental-experiment")]
pub(crate) mod incremental;
mod memory;
mod resample;
#[cfg(test)]
mod tests;

use std::fmt::{Display, Formatter};
use std::path::Path;

use crate::DecodedRgbImage;
use crate::heic_decoder::hevc::DecodedFrame;
use resample::Rectangle;

#[derive(Clone, Copy, Debug)]
pub enum BoundedInput<'a> {
    Path(&'a Path),
    Bytes(&'a [u8]),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundedDecodeOptions {
    pub max_side: u32,
    pub max_memory_bytes: usize,
}

impl Default for BoundedDecodeOptions {
    fn default() -> Self {
        Self {
            max_side: 6000,
            max_memory_bytes: 128 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct BoundedRgbImage {
    pub image: DecodedRgbImage,
    pub original_dimensions: (u32, u32),
}

#[derive(Debug)]
pub enum BoundedDecodeError {
    InvalidOptions(&'static str),
    Malformed(&'static str),
    Unsupported(&'static str),
    LimitExceeded(&'static str),
    MemoryBudgetExceeded {
        stage: &'static str,
        required: usize,
        limit: usize,
    },
    AllocationFailed,
    Io(std::io::Error),
    Decode(String),
}

impl Display for BoundedDecodeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidOptions(s) => write!(f, "invalid bounded decode options: {s}"),
            Self::Malformed(s) => write!(f, "malformed bounded decode input: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported bounded decode input: {s}"),
            Self::LimitExceeded(s) => write!(f, "bounded decode resource limit: {s}"),
            Self::MemoryBudgetExceeded {
                stage,
                required,
                limit,
            } => write!(
                f,
                "bounded decode {stage} requires {required} bytes, budget is {limit}"
            ),
            Self::AllocationFailed => f.write_str("bounded decode allocation failed"),
            Self::Io(e) => Display::fmt(e, f),
            Self::Decode(e) => Display::fmt(e, f),
        }
    }
}

impl std::error::Error for BoundedDecodeError {}

impl From<std::io::Error> for BoundedDecodeError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<crate::heic_decoder::HevcError> for BoundedDecodeError {
    fn from(value: crate::heic_decoder::HevcError) -> Self {
        match value {
            crate::heic_decoder::HevcError::Unsupported(feature) => Self::Unsupported(feature),
            crate::heic_decoder::HevcError::AllocationFailed
            | crate::heic_decoder::HevcError::DecodingError("allocation failed") => {
                Self::AllocationFailed
            }
            other => Self::Decode(other.to_string()),
        }
    }
}

type Result<T> = std::result::Result<T, BoundedDecodeError>;

#[cfg(feature = "parallel-grid")]
const WORKER_STACK_BYTES: usize = 1024 * 1024;

pub fn decode_bounded(
    input: BoundedInput<'_>,
    options: BoundedDecodeOptions,
) -> Result<BoundedRgbImage> {
    if options.max_side == 0 || options.max_side > 6000 {
        return Err(BoundedDecodeError::InvalidOptions(
            "max_side must be in 1..=6000",
        ));
    }
    if options.max_memory_bytes < 64 * 1024 {
        return Err(BoundedDecodeError::InvalidOptions(
            "memory budget must be at least 64 KiB",
        ));
    }
    if cfg!(feature = "decoder-tracing") {
        return Err(BoundedDecodeError::Unsupported(
            "bounded decoding with decoder-tracing enabled",
        ));
    }
    let budget = memory::Budget::new(options.max_memory_bytes);
    let _runtime = budget.reserve(64 * 1024, "runtime")?;
    let mut source = container::Source::new(input)?;
    let metadata = source.metadata(&budget)?;
    let index = container::Index::parse(&metadata, source.len(), &budget)?;
    let grid = grid::Grid::preflight(&index, &mut source, &budget)?;
    let layout = resample::Layout::new(&grid, options.max_side)?;
    let first = &grid.tiles[0];
    let mut color = first.color.clone();
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
    let yuv444 = chroma::Yuv444::new(&grid, &color)?;
    let transform = color::ColorTransform::new(
        &color,
        first.geometry.primaries,
        first.geometry.transfer,
        &budget,
    )?;
    let mut accumulator = resample::Accumulator::new(layout, &budget)?;
    let mut edges = if yuv444.is_some() && grid.tiles.len() > 1 {
        Some(chroma::Edges::new(&grid, &budget)?)
    } else {
        None
    };
    let output_len = layout.display_width as usize * layout.display_height as usize * 3;
    let plan = Plan {
        grid: &grid,
        layout,
        converts: yuv444.is_some(),
    };
    let max_job = (0..grid.tiles.len())
        .map(|i| plan.job_bytes(i))
        .max()
        .unwrap();
    {
        let _admission = budget.reserve(
            output_len
                .checked_add(max_job)
                .ok_or(BoundedDecodeError::LimitExceeded(
                    "output and codec workspace",
                ))?,
            "output and tile reconstruction",
        )?;
    }
    let mut output = budget.zeroed(output_len, "RGB output")?;
    let workers = Workers::new(grid.tiles.len(), max_job, &budget)?;
    let mut next = 0;
    while next < grid.tiles.len() {
        let mut jobs = Vec::with_capacity(8);
        while next < grid.tiles.len() && jobs.len() < workers.count() {
            let tile = &grid.tiles[next];
            let bytes = plan.job_bytes(next);
            if bytes > budget.available() && !jobs.is_empty() {
                break;
            }
            let reservation = budget.reserve(bytes, "tile workspace")?;
            let mut payload = resample::zeroed(tile.payload_len)?;
            index.read_item(tile.item, &mut source, &mut payload)?;
            let (unit, region) = plan.unit(next);
            let tile_area = plan.tile(next);
            jobs.push(Job {
                index: next,
                tile,
                origin: [tile_area.left, tile_area.top],
                unit,
                region,
                decode: plan.decodes(next),
                payload,
                reservation,
            });
            next += 1;
        }
        let render = |job| render_job(job, layout, &color, &transform, yuv444.as_ref());
        let results = if let Some(edges) = &mut edges {
            let mut decoded = workers
                .map(jobs, decode_job)
                .into_iter()
                .collect::<Result<Vec<_>>>()?;
            for job in &mut decoded {
                job.halo = edges.advance(job.index, job.frame.as_ref(), job.tile.geometry)?;
            }
            workers.map(decoded, render)
        } else {
            workers.map(jobs, |job| decode_job(job).and_then(render))
        };
        for result in results {
            let result = result?;
            match &result.data {
                TileData::Pixels(pixels) => {
                    for y in result.region.top..result.region.bottom {
                        for x in result.region.left..result.region.right {
                            let src = ((y + layout.top - result.unit.top) as usize
                                * result.unit.width as usize
                                + (x + layout.left - result.unit.left) as usize)
                                * 3;
                            let dst = layout.pixel_index(x, y);
                            output[dst..dst + 3].copy_from_slice(&pixels[src..src + 3]);
                        }
                    }
                }
                TileData::Sums(sums) if !sums.is_empty() => {
                    accumulator.merge(result.region, sums, &mut output)
                }
                TileData::Sums(_) => {}
            }
            if (result.index + 1) % grid.columns as usize == 0 {
                accumulator.finish_tile_row();
            }
        }
    }
    Ok(BoundedRgbImage {
        image: DecodedRgbImage {
            width: layout.display_width,
            height: layout.display_height,
            source_bit_depth: 8,
            pixels: output.into_vec(),
            icc_profile: None,
        },
        original_dimensions: layout.original_dimensions,
    })
}

struct Plan<'a, 'b> {
    grid: &'b grid::Grid<'a>,
    layout: resample::Layout,
    converts: bool,
}

impl Plan<'_, '_> {
    fn tile(&self, index: usize) -> Rectangle {
        let first = self.grid.tiles[0].geometry;
        let tile = self.grid.tiles[index].geometry;
        Rectangle {
            left: (index as u32 % self.grid.columns) * first.width,
            top: (index as u32 / self.grid.columns) * first.height,
            width: tile.width,
            height: tile.height,
        }
    }

    fn unit(&self, index: usize) -> (Rectangle, resample::Region) {
        let tile = self.tile(index);
        let unit = if self.converts {
            let column = index as u32 % self.grid.columns;
            let row = index as u32 / self.grid.columns;
            let rows = self.grid.tiles.len() as u32 / self.grid.columns;
            Rectangle::from_edges(
                tile.left.saturating_sub(1),
                tile.top.saturating_sub(1),
                tile.right() - u32::from(column + 1 < self.grid.columns),
                tile.bottom() - u32::from(row + 1 < rows),
            )
            .intersection(self.layout.source())
        } else {
            tile
        };
        let region = self
            .layout
            .region(unit.left, unit.top, unit.width, unit.height);
        (unit, region)
    }

    fn decodes(&self, index: usize) -> bool {
        if self.converts {
            !self
                .tile(index)
                .expanded(2)
                .intersection(self.layout.source())
                .is_empty()
        } else {
            self.unit(index).1.len() != 0
        }
    }

    fn job_bytes(&self, index: usize) -> usize {
        let tile = &self.grid.tiles[index];
        let (unit, region) = self.unit(index);
        let contributions = if self.layout.is_unscaled() {
            0
        } else {
            resample::Contribution::workspace_bytes(region)
        };
        let conversion = if self.converts {
            3 * (tile.geometry.width as usize + tile.geometry.height as usize + 1)
                + chroma::Yuv444::workspace_bytes(unit.width)
                + chroma::Halo::bytes(tile.geometry)
        } else {
            0
        };
        tile.workspace_bytes
            .saturating_add(tile.payload_len)
            .saturating_add(contributions)
            .saturating_add(conversion)
    }
}

struct Workers {
    #[cfg(feature = "parallel-grid")]
    pool: Option<(rayon::ThreadPool, memory::Reservation)>,
}

impl Workers {
    #[cfg(feature = "parallel-grid")]
    fn new(tiles: usize, max_job: usize, budget: &memory::Budget) -> Result<Self> {
        let threads = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(8)
            .min(tiles);
        let mut workers = threads;
        while workers > 1
            && max_job
                .saturating_mul(workers)
                .saturating_add(pool_bytes(workers))
                > budget.available()
        {
            workers -= 1;
        }
        if workers <= 1 {
            return Ok(Self { pool: None });
        }
        let reservation = budget.reserve(pool_bytes(workers), "worker pool")?;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .stack_size(WORKER_STACK_BYTES)
            .build()
            .map_err(|_| BoundedDecodeError::AllocationFailed)?;
        Ok(Self {
            pool: Some((pool, reservation)),
        })
    }

    #[cfg(not(feature = "parallel-grid"))]
    fn new(_tiles: usize, _max_job: usize, _budget: &memory::Budget) -> Result<Self> {
        Ok(Self {})
    }

    fn count(&self) -> usize {
        #[cfg(feature = "parallel-grid")]
        if let Some((pool, _)) = &self.pool {
            return pool.current_num_threads();
        }
        1
    }

    fn map<T: Send, U: Send>(&self, items: Vec<T>, f: impl Fn(T) -> U + Sync + Send) -> Vec<U> {
        #[cfg(feature = "parallel-grid")]
        if let Some((pool, _)) = &self.pool {
            use rayon::prelude::*;
            return pool.install(|| items.into_par_iter().map(f).collect());
        }
        items.into_iter().map(f).collect()
    }
}

fn decode_tile(tile: &grid::Tile<'_>, payload: &[u8]) -> Result<DecodedFrame> {
    let prepared = codec::prepare(tile.config, payload)?;
    if prepared.reconstruction_bytes() + payload.len() as u64 * 3 + 1024 * 1024
        > tile.workspace_bytes as u64
    {
        return Err(BoundedDecodeError::Malformed(
            "codec workspace changed after preflight",
        ));
    }
    if prepared.geometry != tile.geometry {
        return Err(BoundedDecodeError::Malformed(
            "coded headers changed after preflight",
        ));
    }
    Ok(prepared.decode()?)
}

fn render_tile(
    tile: &grid::Tile<'_>,
    frame: DecodedFrame,
    color: &grid::Color<'_>,
    transform: &color::ColorTransform,
) -> Result<Vec<u8>> {
    let mut decoded = crate::DecodedHeicImage {
        width: frame.width,
        height: frame.height,
        bit_depth_luma: 8,
        bit_depth_chroma: 8,
        layout: crate::HeicPixelLayout::Yuv420,
        ycbcr_range: if frame.full_range {
            crate::YCbCrRange::Full
        } else {
            crate::YCbCrRange::Limited
        },
        ycbcr_matrix: crate::YCbCrMatrixCoefficients {
            matrix_coefficients: u16::from(frame.matrix_coeffs),
            colour_primaries: u16::from(frame.colour_primaries),
        },
        y_plane: crate::HeicPlane {
            width: frame.width,
            height: frame.height,
            samples: frame.y_plane,
        },
        u_plane: Some(crate::HeicPlane {
            width: frame.width.div_ceil(2),
            height: frame.height.div_ceil(2),
            samples: frame.cb_plane,
        }),
        v_plane: Some(crate::HeicPlane {
            width: frame.width.div_ceil(2),
            height: frame.height.div_ceil(2),
            samples: frame.cr_plane,
        }),
    };
    let colr = crate::isobmff::PrimaryItemColorProperties {
        nclx: color.nclx.clone(),
        icc: None,
    };
    if let Some(range) = crate::ycbcr_range_override_from_primary_colr(&colr) {
        decoded.ycbcr_range = range;
    }
    if let Some(matrix) = crate::ycbcr_matrix_override_from_primary_colr(&colr) {
        decoded.ycbcr_matrix = matrix;
    }
    let mut rgb =
        resample::zeroed(tile.geometry.width as usize * tile.geometry.height as usize * 3)?;
    crate::convert_heic_to_interleaved_rgb8_region_slice::<3>(
        &decoded,
        tile.geometry.crop[0] as usize,
        tile.geometry.crop[2] as usize,
        tile.geometry.width as usize,
        tile.geometry.height as usize,
        &mut rgb,
        crate::scale_heic_sample_to_image_rgb8,
        "bounded RGB",
    )
    .map_err(|_| BoundedDecodeError::Unsupported("tile YCbCr conversion"))?;
    drop(decoded);
    apply_color(rgb, transform)
}

fn render_yuv444(
    job: &Decoded<'_, '_>,
    frame: DecodedFrame,
    yuv444: &chroma::Yuv444,
    transform: &color::ColorTransform,
) -> Result<Vec<u8>> {
    let mut rgb = resample::zeroed(job.unit.width as usize * job.unit.height as usize * 3)?;
    yuv444.convert(
        &chroma::TileSamples {
            frame: &frame,
            geometry: job.tile.geometry,
            origin: job.origin,
            halo: &job.halo,
        },
        job.unit,
        &mut rgb,
    )?;
    drop(frame);
    apply_color(rgb, transform)
}

fn apply_color(rgb: Vec<u8>, transform: &color::ColorTransform) -> Result<Vec<u8>> {
    if transform.is_identity() {
        return Ok(rgb);
    }
    let mut converted = resample::zeroed(rgb.len())?;
    transform.apply(&rgb, &mut converted)?;
    Ok(converted)
}

#[cfg(feature = "parallel-grid")]
fn pool_bytes(workers: usize) -> usize {
    1024 * 1024 + workers * WORKER_STACK_BYTES
}

struct Job<'a, 'b> {
    index: usize,
    tile: &'b grid::Tile<'a>,
    origin: [u32; 2],
    unit: Rectangle,
    region: resample::Region,
    decode: bool,
    payload: Vec<u8>,
    reservation: memory::Reservation,
}

struct Decoded<'a, 'b> {
    index: usize,
    tile: &'b grid::Tile<'a>,
    origin: [u32; 2],
    unit: Rectangle,
    region: resample::Region,
    frame: Option<DecodedFrame>,
    halo: chroma::Halo,
    reservation: memory::Reservation,
}

enum TileData {
    Pixels(Vec<u8>),
    Sums(resample::Contribution),
}

struct TileResult {
    index: usize,
    unit: Rectangle,
    region: resample::Region,
    data: TileData,
    _reservation: memory::Reservation,
}

fn decode_job<'a, 'b>(job: Job<'a, 'b>) -> Result<Decoded<'a, 'b>> {
    let frame = if job.decode {
        Some(decode_tile(job.tile, &job.payload)?)
    } else {
        None
    };
    Ok(Decoded {
        index: job.index,
        tile: job.tile,
        origin: job.origin,
        unit: job.unit,
        region: job.region,
        frame,
        halo: chroma::Halo::default(),
        reservation: job.reservation,
    })
}

fn render_job(
    mut job: Decoded<'_, '_>,
    layout: resample::Layout,
    color: &grid::Color<'_>,
    transform: &color::ColorTransform,
    yuv444: Option<&chroma::Yuv444>,
) -> Result<TileResult> {
    let (index, unit, region) = (job.index, job.unit, job.region);
    let data = match job.frame.take() {
        Some(frame) if region.len() != 0 => {
            let tile = job.tile;
            let pixels = match yuv444 {
                Some(yuv444) => render_yuv444(&job, frame, yuv444, transform)?,
                None => render_tile(tile, frame, color, transform)?,
            };
            if layout.is_unscaled() {
                TileData::Pixels(pixels)
            } else {
                TileData::Sums(resample::contributions(
                    layout, region, &pixels, unit.left, unit.top, unit.width,
                )?)
            }
        }
        _ => TileData::Sums(resample::Contribution::default()),
    };
    Ok(TileResult {
        index,
        unit,
        region,
        data,
        _reservation: job.reservation,
    })
}
