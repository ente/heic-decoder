mod codec;
mod color;
mod container;
mod grid;
mod memory;
mod resample;
#[cfg(test)]
mod tests;

use std::fmt::{Display, Formatter};
use std::path::Path;

use crate::DecodedRgbImage;

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
            crate::heic_decoder::HevcError::DecodingError("allocation failed") => {
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
    let transform = color::ColorTransform::new(
        &color,
        first.geometry.primaries,
        first.geometry.transfer,
        &budget,
    )?;
    let mut accumulator = resample::Accumulator::new(layout, &budget)?;
    let output_len = layout.display_width as usize * layout.display_height as usize * 3;
    let max_job = grid
        .tiles
        .iter()
        .enumerate()
        .map(|(i, tile)| {
            let region = layout.region(
                (i as u32 % grid.columns) * first.geometry.width,
                (i as u32 / grid.columns) * first.geometry.height,
                tile.geometry.width,
                tile.geometry.height,
            );
            job_bytes(tile, region, layout)
        })
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
    #[cfg(feature = "parallel-grid")]
    let (pool, _pool_reservation) = {
        let threads = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(8);
        let mut workers = threads;
        while workers > 1
            && max_job
                .saturating_mul(workers)
                .saturating_add(pool_bytes(workers))
                > budget.available()
        {
            workers -= 1;
        }
        if workers > 1 {
            let reservation = budget.reserve(pool_bytes(workers), "worker pool")?;
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .stack_size(WORKER_STACK_BYTES)
                .build()
                .map_err(|_| BoundedDecodeError::AllocationFailed)?;
            (Some(pool), Some(reservation))
        } else {
            (None, None)
        }
    };
    let mut next = 0;
    while next < grid.tiles.len() {
        let mut jobs = Vec::with_capacity(8);
        #[cfg(feature = "parallel-grid")]
        let workers = pool
            .as_ref()
            .map_or(1, rayon::ThreadPool::current_num_threads);
        #[cfg(not(feature = "parallel-grid"))]
        let workers = 1;
        while next < grid.tiles.len() && jobs.len() < workers {
            let tile = &grid.tiles[next];
            let tile_x = (next as u32 % grid.columns) * first.geometry.width;
            let tile_y = (next as u32 / grid.columns) * first.geometry.height;
            let region = layout.region(tile_x, tile_y, tile.geometry.width, tile.geometry.height);
            let bytes = job_bytes(tile, region, layout);
            if bytes > budget.available() && !jobs.is_empty() {
                break;
            }
            let reservation = budget.reserve(bytes, "tile workspace")?;
            let mut payload = resample::zeroed(tile.payload_len)?;
            index.read_item(tile.item, &mut source, &mut payload)?;
            jobs.push(Job {
                index: next,
                tile,
                tile_x,
                tile_y,
                region,
                payload,
                reservation,
            });
            next += 1;
        }
        let run = |job| run_job(job, layout, &color, &transform);
        #[cfg(feature = "parallel-grid")]
        let results: Vec<_> = if let Some(pool) = &pool {
            use rayon::prelude::*;
            pool.install(|| jobs.into_par_iter().map(run).collect())
        } else {
            jobs.into_iter().map(run).collect()
        };
        #[cfg(not(feature = "parallel-grid"))]
        let results: Vec<_> = jobs.into_iter().map(run).collect();
        for result in results {
            let result = result?;
            match &result.data {
                TileData::Pixels(pixels) => {
                    for y in result.region.top..result.region.bottom {
                        for x in result.region.left..result.region.right {
                            let src = ((y + layout.top - result.tile_y) as usize
                                * first.geometry.width as usize
                                + (x + layout.left - result.tile_x) as usize)
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

fn job_bytes(tile: &grid::Tile<'_>, region: resample::Region, layout: resample::Layout) -> usize {
    tile.workspace_bytes
        + tile.payload_len
        + if layout.is_unscaled() {
            0
        } else {
            resample::Contribution::workspace_bytes(region)
        }
}

fn render_tile(
    tile: &grid::Tile<'_>,
    payload: &[u8],
    color: &grid::Color<'_>,
    transform: &color::ColorTransform,
) -> Result<Vec<u8>> {
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
    let frame = prepared.decode()?;
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
    tile_x: u32,
    tile_y: u32,
    region: resample::Region,
    payload: Vec<u8>,
    reservation: memory::Reservation,
}

enum TileData {
    Pixels(Vec<u8>),
    Sums(resample::Contribution),
}

struct TileResult {
    index: usize,
    tile_x: u32,
    tile_y: u32,
    region: resample::Region,
    data: TileData,
    _reservation: memory::Reservation,
}

fn run_job(
    job: Job<'_, '_>,
    layout: resample::Layout,
    color: &grid::Color<'_>,
    transform: &color::ColorTransform,
) -> Result<TileResult> {
    let data = if job.region.len() == 0 {
        TileData::Sums(resample::Contribution::default())
    } else {
        let pixels = render_tile(job.tile, &job.payload, color, transform)?;
        if layout.is_unscaled() {
            TileData::Pixels(pixels)
        } else {
            TileData::Sums(resample::contributions(
                layout,
                job.region,
                &pixels,
                job.tile_x,
                job.tile_y,
                job.tile.geometry.width,
            )?)
        }
    };
    Ok(TileResult {
        index: job.index,
        tile_x: job.tile_x,
        tile_y: job.tile_y,
        region: job.region,
        data,
        _reservation: job.reservation,
    })
}
