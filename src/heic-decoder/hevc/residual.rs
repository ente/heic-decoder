//! Residual (transform coefficient) decoding
//!
//! This module handles parsing of transform coefficients via CABAC
//! and applying inverse transforms to residuals.

use super::cabac::{CabacDecoder, ContextModel, context};
use super::debug;
use super::transform::MAX_COEFF;
use crate::heic_decoder::error::HevcError;

/// Residual trace output, only available with std feature
macro_rules! rc_eprintln {
    ($($arg:tt)*) => {{
        #[cfg(feature = "std")]
        eprintln!($($arg)*);
        #[cfg(not(feature = "std"))]
        let _ = format_args!($($arg)*);
    }};
}

type Result<T> = core::result::Result<T, HevcError>;

/// Scan order types for coefficient scanning
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanOrder {
    /// Diagonal scan (default)
    Diagonal = 0,
    /// Horizontal scan (for horizontal intra modes)
    Horizontal = 1,
    /// Vertical scan (for vertical intra modes)
    Vertical = 2,
}

/// Get scan order based on intra prediction mode and component index.
///
/// Per H.265 7.4.9.11 (scanIdx derivation):
/// - Luma: directional scan at log2_size 2 (4x4) and 3 (8x8)
/// - Chroma: directional scan at log2_size 2, and also at log2_size 3 when
///   ChromaArrayType == 3 (4:4:4)
pub fn get_scan_order(
    log2_size: u8,
    intra_mode: u8,
    c_idx: u8,
    chroma_format_idc: u8,
) -> ScanOrder {
    let use_directional = if c_idx == 0 {
        log2_size == 2 || log2_size == 3
    } else {
        log2_size == 2 || (log2_size == 3 && chroma_format_idc == 3)
    };

    if use_directional {
        if (6..=14).contains(&intra_mode) {
            ScanOrder::Vertical
        } else if (22..=30).contains(&intra_mode) {
            ScanOrder::Horizontal
        } else {
            ScanOrder::Diagonal
        }
    } else {
        ScanOrder::Diagonal
    }
}

/// 4x4 diagonal scan order (indexed by position 0-15)
/// Per H.265 Table 6-7: within each anti-diagonal, (x increases, y decreases)
/// i.e., bottom-left to top-right within each diagonal
pub static SCAN_ORDER_4X4_DIAG: [(u8, u8); 16] = [
    (0, 0),
    (0, 1),
    (1, 0),
    (0, 2),
    (1, 1),
    (2, 0),
    (0, 3),
    (1, 2),
    (2, 1),
    (3, 0),
    (1, 3),
    (2, 2),
    (3, 1),
    (2, 3),
    (3, 2),
    (3, 3),
];

/// 4x4 horizontal scan order
pub static SCAN_ORDER_4X4_HORIZ: [(u8, u8); 16] = [
    (0, 0),
    (1, 0),
    (2, 0),
    (3, 0),
    (0, 1),
    (1, 1),
    (2, 1),
    (3, 1),
    (0, 2),
    (1, 2),
    (2, 2),
    (3, 2),
    (0, 3),
    (1, 3),
    (2, 3),
    (3, 3),
];

/// 4x4 vertical scan order
pub static SCAN_ORDER_4X4_VERT: [(u8, u8); 16] = [
    (0, 0),
    (0, 1),
    (0, 2),
    (0, 3),
    (1, 0),
    (1, 1),
    (1, 2),
    (1, 3),
    (2, 0),
    (2, 1),
    (2, 2),
    (2, 3),
    (3, 0),
    (3, 1),
    (3, 2),
    (3, 3),
];

/// Raster position (`y * 4 + x`) for each position in the three 4x4 scans.
/// Residual significance decoding uses this compact form on every CABAC bin;
/// keep the coordinate-pair tables above for callers that need coordinates.
const SCAN_RASTER_4X4: [[u8; 16]; 3] = [
    [0, 4, 1, 8, 5, 2, 12, 9, 6, 3, 13, 10, 7, 14, 11, 15],
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15],
];

const fn invert_4x4_scans(scans: [[u8; 16]; 3]) -> [[u8; 16]; 3] {
    let mut inverse = [[0u8; 16]; 3];
    let mut scan_idx = 0;
    while scan_idx < scans.len() {
        let mut pos = 0;
        while pos < scans[scan_idx].len() {
            inverse[scan_idx][scans[scan_idx][pos] as usize] = pos as u8;
            pos += 1;
        }
        scan_idx += 1;
    }
    inverse
}

const INVERSE_SCAN_4X4: [[u8; 16]; 3] = invert_4x4_scans(SCAN_RASTER_4X4);

/// Get scan order table for 4x4 sub-blocks
pub fn get_scan_4x4(order: ScanOrder) -> &'static [(u8, u8); 16] {
    match order {
        ScanOrder::Diagonal => &SCAN_ORDER_4X4_DIAG,
        ScanOrder::Horizontal => &SCAN_ORDER_4X4_HORIZ,
        ScanOrder::Vertical => &SCAN_ORDER_4X4_VERT,
    }
}

/// Decode residual coefficients for a transform unit
/// Debug counter to identify specific TU calls
#[cfg(feature = "decoder-tracing")]
pub static DEBUG_RESIDUAL_COUNTER: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

#[allow(clippy::too_many_arguments)]
pub fn decode_residual(
    cabac: &mut CabacDecoder<'_>,
    ctx: &mut [ContextModel],
    log2_size: u8,
    c_idx: u8, // 0=Y, 1=Cb, 2=Cr
    scan_order: ScanOrder,
    sign_data_hiding_enabled: bool,
    cu_transquant_bypass: bool,
    transform_skip_enabled: bool,
    _x0: u32,
    _y0: u32,
    coeffs: &mut [i16; MAX_COEFF],
    touched_coeffs: &mut [u16; MAX_COEFF],
) -> Result<(usize, bool)> {
    #[cfg(feature = "decoder-tracing")]
    DEBUG_RESIDUAL_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

    // SE trace for differential testing against libde265
    #[cfg(feature = "decoder-tracing")]
    {
        let se_num = crate::heic_decoder::hevc::ctu::SE_COUNTER
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        #[allow(clippy::absurd_extreme_comparisons)]
        if se_num < crate::heic_decoder::hevc::ctu::SE_TRACE_LIMIT {
            let (range, _, _) = cabac.get_state_extended();
            let (byte_pos, _, _) = cabac.get_position();
            eprintln!(
                "SE#{} residual_coding c_idx={} log2={} range={} byte={}",
                se_num, c_idx, log2_size, range, byte_pos
            );
        }
    }

    // Surgical trace for diverging residual (set to u32::MAX to disable)
    #[cfg(feature = "std")]
    let rc_trace = false;
    #[cfg(not(feature = "std"))]
    let rc_trace = false;
    let rcp = "RCX";

    let size = 1u32 << log2_size;
    let mut num_nonzero = 0usize;

    // Decode transform_skip_flag (H.265 7.3.8.11)
    // Per spec: if transform_skip_enabled_flag && !cu_transquant_bypass_flag
    //           && log2TrafoSize <= Log2MaxTransformSkipSize
    // Log2MaxTransformSkipSize defaults to 2 (4x4 blocks only)
    let transform_skip = if transform_skip_enabled && !cu_transquant_bypass && log2_size <= 2 {
        let ctx_idx = context::TRANSFORM_SKIP_FLAG + if c_idx > 0 { 1 } else { 0 };
        let flag = cabac.decode_bin(&mut ctx[ctx_idx]) != 0;
        if rc_trace {
            let (range, _, _) = cabac.get_state_extended();
            let (byte_pos, _, _) = cabac.get_position();
            rc_eprintln!(
                "{rcp}_TSKIP c_idx={} val={} range={} byte={}",
                c_idx,
                flag as u8,
                range,
                byte_pos
            );
        }
        flag
    } else {
        false
    };

    // Decode last significant coefficient position
    let (last_x, last_y) = decode_last_sig_coeff_pos(cabac, ctx, log2_size, c_idx)?;
    if rc_trace {
        let (range, _, _) = cabac.get_state_extended();
        let (byte_pos, _, _) = cabac.get_position();
        rc_eprintln!(
            "{rcp}_LAST lastX={} lastY={} range={} byte={}",
            last_x,
            last_y,
            range,
            byte_pos
        );
    }

    // Swap coordinates for vertical scan
    let (last_x, last_y) = if scan_order == ScanOrder::Vertical {
        (last_y, last_x)
    } else {
        (last_x, last_y)
    };

    if last_x >= size || last_y >= size {
        return Err(HevcError::InvalidBitstream("invalid last coeff position"));
    }

    // Get scan tables
    let scan_sub = get_scan_sub_block(log2_size, scan_order);
    // Convert ScanOrder to scan_idx for context derivation
    let scan_idx = match scan_order {
        ScanOrder::Diagonal => 0,
        ScanOrder::Horizontal => 1,
        ScanOrder::Vertical => 2,
    };
    let scan_raster = &SCAN_RASTER_4X4[scan_idx];
    let inverse_scan = &INVERSE_SCAN_4X4[scan_idx];

    // Find last sub-block
    let sb_width = (size / 4) as usize;
    let last_sb_x = last_x / 4;
    let last_sb_y = last_y / 4;
    let last_sb_idx = sub_block_scan_pos(
        last_sb_x,
        last_sb_y,
        sb_width as u32,
        scan_order == ScanOrder::Horizontal,
    );

    // Find last position within sub-block
    let local_x = (last_x % 4) as u8;
    let local_y = (last_y % 4) as u8;
    let last_pos_in_sb = inverse_scan[(local_y * 4 + local_x) as usize];

    if rc_trace {
        rc_eprintln!(
            "{rcp}_SCAN scanIdx={} lastSB={} lastPos={} sb=({},{}) local=({},{})",
            scan_idx,
            last_sb_idx,
            last_pos_in_sb,
            last_sb_x,
            last_sb_y,
            local_x,
            local_y
        );
    }

    // Track coded_sub_block_flag for prevCsbf calculation
    // Max sub-block grid is 8x8 for 32x32 TU
    let mut coded_sb_flags = 0u64;

    // Track whether the previously-processed subblock had any greater1_flag == 1
    // This is used for ctx_set derivation per H.265 section 9.3.4.2.6
    // "Previous" means the one already processed (higher scan index since we process high to low)
    let mut prev_subblock_had_gt1 = false;

    // Decode coefficients
    for sb_idx in (0..=last_sb_idx).rev() {
        let (sb_x, sb_y) = scan_sub[sb_idx as usize];

        // Calculate neighbor flags BEFORE decoding coded_sub_block_flag
        // These are used for both coded_sub_block_flag and sig_coeff_flag contexts
        // Per H.265: bit 0 = right neighbor coded, bit 1 = below neighbor coded
        let sb_bit = sb_y as usize * 8 + sb_x as usize;
        let csbf_neighbors = {
            let right_coded = if (sb_x as usize + 1) < sb_width {
                coded_sb_flags & (1 << (sb_bit + 1)) != 0
            } else {
                false
            };
            let below_coded = if (sb_y as usize + 1) < sb_width {
                coded_sb_flags & (1 << (sb_bit + 8)) != 0
            } else {
                false
            };
            (if right_coded { 1 } else { 0 }) | (if below_coded { 2 } else { 0 })
        };

        // Check if sub-block is coded
        // Middle sub-blocks need coded_sub_block_flag decoded
        // First (i=0) and last sub-blocks are always considered coded
        let (sb_coded, infer_sb_dc_sig) = if sb_idx > 0 && sb_idx < last_sb_idx {
            // Context offset: 0-1 for luma, 2-3 for chroma. Hoist this
            // directly into the sub-block loop so the hot CABAC call does not
            // carry an infallible Result/helper layer.
            let csbf_ctx = usize::from(csbf_neighbors != 0);
            let ctx_idx = context::CODED_SUB_BLOCK_FLAG + csbf_ctx + if c_idx > 0 { 2 } else { 0 };
            let coded = cabac.decode_bin(&mut ctx[ctx_idx]) != 0;
            // If sub-block is coded, we may need to infer DC later
            (coded, coded)
        } else {
            (true, false) // First and last sub-blocks don't use DC inference
        };

        // Track coded sub-block flag
        if sb_coded {
            coded_sb_flags |= 1 << sb_bit;
        }

        // prevCsbf for sig_coeff_flag context
        // Per libde265: bit 0 = right neighbor, bit 1 = below neighbor
        // csbf_neighbors already has the correct bit ordering
        let prev_csbf = csbf_neighbors;

        if !sb_coded {
            continue;
        }

        // Decode coefficients in this sub-block
        let start_pos = if sb_idx == last_sb_idx {
            last_pos_in_sb
        } else {
            15
        };

        // Significance is decoded in reverse scan order. Append raster indices
        // directly to the persistent sparse list in that same order; it then
        // doubles as the dense level/sign worklist for this sub-block.
        let subblock_start = num_nonzero;
        let size = size as usize;
        let sb_offset = sb_y as usize * 4 * size + sb_x as usize * 4;
        let mut first_sig_pos = 0u8;
        let mut last_sig_pos = 0u8;
        let mut can_infer_dc = infer_sb_dc_sig;

        // For all transforms larger than 4x4, only the compact local-position
        // contribution varies per significance bin. The component, transform,
        // scan, and sub-block contributions are invariant here.
        let sig_ctx_base = if log2_size == 2 {
            context::SIG_COEFF_FLAG + if c_idx > 0 { 27 } else { 0 }
        } else {
            let mut base = context::SIG_COEFF_FLAG + if c_idx > 0 { 27 } else { 0 };
            if c_idx == 0 {
                if sb_x + sb_y > 0 {
                    base += 3;
                }
                base += if log2_size == 3 {
                    if scan_idx == 0 { 9 } else { 15 }
                } else {
                    21
                };
            } else {
                base += if log2_size == 3 { 9 } else { 12 };
            }
            base
        };
        let sig_ctx_local = if log2_size == 2 {
            &CTX_IDX_MAP_4X4
        } else {
            &SIG_CTX_LOCAL[prev_csbf as usize]
        };

        // Determine the last position to check
        // For last sub-block: start from last_pos_in_sb (the known last significant coeff)
        // For other sub-blocks: start from position 15
        let last_coeff = if sb_idx == last_sb_idx {
            // Set the known last significant coefficient (no need to decode sig_coeff_flag)
            let raster_pos = scan_raster[start_pos as usize] as usize;
            let dst_idx = sb_offset + (raster_pos >> 2) * size + (raster_pos & 3);
            coeffs[dst_idx] = 1;
            touched_coeffs[num_nonzero] = dst_idx as u16;
            num_nonzero += 1;
            first_sig_pos = start_pos;
            last_sig_pos = start_pos;
            can_infer_dc = false; // Can't infer DC if we have other coeffs
            // Then check positions from start_pos-1 down to 1
            start_pos.saturating_sub(1)
        } else {
            // For non-last sub-blocks, check all positions from 15 down to 1
            15
        };

        // Decode significant_coeff_flags for positions last_coeff down to 1
        // (DC at position 0 is handled separately for inference)
        for n in (1..=last_coeff).rev() {
            let raster_pos = scan_raster[n as usize] as usize;
            let ctx_idx = sig_ctx_base + sig_ctx_local[raster_pos] as usize;
            let sig = cabac.decode_bin(&mut ctx[ctx_idx]) != 0;
            if rc_trace {
                let (range, _, _) = cabac.get_state_extended();
                let (byte_pos, _, _) = cabac.get_position();
                let x_in_sb = (raster_pos & 3) as u8;
                let y_in_sb = (raster_pos >> 2) as u8;
                let xc = sb_x * 4 + x_in_sb;
                let yc = sb_y * 4 + y_in_sb;
                rc_eprintln!(
                    "{rcp}_SIG n={} pos=({},{}) ctx={} val={} range={} byte={}",
                    n,
                    xc,
                    yc,
                    ctx_idx,
                    if sig { 1 } else { 0 },
                    range,
                    byte_pos
                );
            }
            if sig {
                let dst_idx = sb_offset + (raster_pos >> 2) * size + (raster_pos & 3);
                coeffs[dst_idx] = 1;
                touched_coeffs[num_nonzero] = dst_idx as u16;
                num_nonzero += 1;
                if num_nonzero == subblock_start + 1 {
                    last_sig_pos = n;
                }
                first_sig_pos = n;
                can_infer_dc = false; // Found a coefficient, can't infer DC
            }
        }

        // Handle DC coefficient (position 0)
        // Per H.265: if sub-block is coded but no other coefficients found,
        // DC is inferred to be significant (otherwise the sub-block would be all zeros)
        if start_pos > 0 {
            if can_infer_dc {
                let dst_idx = sb_offset;
                coeffs[dst_idx] = 1;
                touched_coeffs[num_nonzero] = dst_idx as u16;
                num_nonzero += 1;
                if num_nonzero == subblock_start + 1 {
                    last_sig_pos = 0;
                }
                first_sig_pos = 0;
                if rc_trace {
                    rc_eprintln!("{rcp}_SIG n=0 DC_INFERRED");
                }
            } else {
                let ctx_idx = if log2_size == 2 {
                    sig_ctx_base + CTX_IDX_MAP_4X4[0] as usize
                } else if sb_x == 0 && sb_y == 0 {
                    context::SIG_COEFF_FLAG + if c_idx > 0 { 27 } else { 0 }
                } else {
                    sig_ctx_base + SIG_CTX_LOCAL[prev_csbf as usize][0] as usize
                };
                let sig = cabac.decode_bin(&mut ctx[ctx_idx]) != 0;
                if rc_trace {
                    let (range, _, _) = cabac.get_state_extended();
                    let (byte_pos, _, _) = cabac.get_position();
                    rc_eprintln!(
                        "{rcp}_SIG n=0 pos=({},{}) ctx={} val={} range={} byte={}",
                        sb_x * 4,
                        sb_y * 4,
                        ctx_idx,
                        if sig { 1 } else { 0 },
                        range,
                        byte_pos
                    );
                }
                if sig {
                    let dst_idx = sb_offset;
                    coeffs[dst_idx] = 1;
                    touched_coeffs[num_nonzero] = dst_idx as u16;
                    num_nonzero += 1;
                    if num_nonzero == subblock_start + 1 {
                        last_sig_pos = 0;
                    }
                    first_sig_pos = 0;
                }
            }
        }

        let num_coeffs = num_nonzero - subblock_start;
        if num_coeffs == 0 {
            continue;
        }

        // Calculate ctx_set per H.265 section 9.3.4.2.6
        // base = 0 for DC subblock (sb_idx==0) or chroma (c_idx!=0)
        // base = 2 for luma non-DC subblock
        // ctx_set = base + (prev_gt1 ? 1 : 0)
        let base = if sb_idx == 0 || c_idx > 0 { 0 } else { 2 };
        let ctx_set = base + if prev_subblock_had_gt1 { 1 } else { 0 };

        if rc_trace {
            rc_eprintln!(
                "{rcp}_COEFF sb_idx={} num_coeffs={} ctx_set={}",
                sb_idx,
                num_coeffs,
                ctx_set
            );
        }

        // Track if this subblock has any g1=1 (for next subblock's ctx_set)
        let mut this_subblock_had_gt1 = false;

        // greater1Ctx: context index component, reset to 1 each subblock
        // Updated BEFORE decoding each coefficient (except first) based on PREVIOUS flag
        let mut greater1_ctx = 1u8;
        let mut last_greater1_flag = false;

        // Decode greater-1 flags (up to 8)
        let mut first_g1_idx: Option<usize> = None;
        let max_g1 = num_coeffs.min(8);

        // Track dense coefficient indices that need remaining level decoding.
        let mut needs_remaining = 0u16;

        let g1_ctx_base = context::COEFF_ABS_LEVEL_GREATER1_FLAG
            + if c_idx > 0 { 16 } else { 0 }
            + (ctx_set as usize) * 4;
        for coeff_idx in 0..num_coeffs {
            if coeff_idx >= max_g1 {
                // Beyond first 8: base=1, always needs remaining for values > 1
                needs_remaining |= 1u16 << coeff_idx;
                continue;
            }

            // Update greater1Ctx BEFORE decoding, using PREVIOUS flag
            // (skip for first coefficient in subblock)
            // Per libde265: greater1Ctx increments without clamping (clamped in ctx calc)
            if coeff_idx > 0 && greater1_ctx > 0 {
                if last_greater1_flag {
                    greater1_ctx = 0;
                } else {
                    greater1_ctx += 1;
                }
            }

            // Use ctx_set (captured at subblock start) for ALL greater1_flags
            let ctx_idx = g1_ctx_base + (greater1_ctx as usize).min(3);
            let g1 = cabac.decode_bin(&mut ctx[ctx_idx]) != 0;
            if rc_trace {
                let (range, _, _) = cabac.get_state_extended();
                let (byte_pos, _, _) = cabac.get_position();
                let full_ctx = context::COEFF_ABS_LEVEL_GREATER1_FLAG
                    + if c_idx > 0 { 16 } else { 0 }
                    + (ctx_set as usize) * 4
                    + (greater1_ctx as usize).min(3);
                rc_eprintln!(
                    "{rcp}_G1 c={} ctxSet={} g1ctx={} fullCtx={} val={} range={} byte={}",
                    coeff_idx,
                    ctx_set,
                    greater1_ctx,
                    full_ctx,
                    if g1 { 1 } else { 0 },
                    range,
                    byte_pos
                );
            }
            last_greater1_flag = g1;

            if g1 {
                let dst_idx = touched_coeffs[subblock_start + coeff_idx] as usize;
                coeffs[dst_idx] = 2;
                this_subblock_had_gt1 = true; // Track for next subblock's ctx_set
                if first_g1_idx.is_none() {
                    first_g1_idx = Some(coeff_idx);
                } else {
                    // Non-first g1=1: base=2, needs remaining for values > 2
                    needs_remaining |= 1u16 << coeff_idx;
                }
            }
        }

        // Decode greater-2 flag (only for first coefficient with greater-1)
        // Uses same ctx_set as greater1_flags (captured at subblock start)
        if let Some(g1_idx) = first_g1_idx {
            let ctx_idx = context::COEFF_ABS_LEVEL_GREATER2_FLAG
                + if c_idx > 0 { 4 } else { 0 }
                + ctx_set as usize;
            let g2 = cabac.decode_bin(&mut ctx[ctx_idx]) != 0;
            if rc_trace {
                let (range, _, _) = cabac.get_state_extended();
                let (byte_pos, _, _) = cabac.get_position();
                rc_eprintln!(
                    "{rcp}_G2 ctxSet={} val={} range={} byte={}",
                    ctx_set,
                    if g2 { 1 } else { 0 },
                    range,
                    byte_pos
                );
            }
            if g2 {
                let dst_idx = touched_coeffs[subblock_start + g1_idx] as usize;
                coeffs[dst_idx] = 3;
                needs_remaining |= 1u16 << g1_idx;
            }
        }

        // Determine if sign is hidden for this sub-block
        // Per H.265 9.3.4.3: sign is hidden if:
        // - sign_data_hiding_enabled_flag is true
        // - cu_transquant_bypass_flag is false
        // - lastScanPos - firstScanPos > 3
        // Note: also disabled for RDPCM modes (not implemented here)
        let sign_hidden =
            sign_data_hiding_enabled && !cu_transquant_bypass && (last_sig_pos - first_sig_pos) > 3;

        // Decode signs (bypass mode)
        // Following libde265's approach: decode signs in coefficient order (high scan pos to low)
        // The LAST coefficient (at first_sig_pos) has its sign hidden when sign_hidden=true
        //
        // Decode signs following libde265 order:
        // - Decode signs for coefficients 0 to n_sig-2 (high scan pos to low)
        // - For coefficient at n_sig-1 (lowest scan pos, first in scan order):
        //   - If sign_hidden: skip decoding, will infer from parity later
        //   - Otherwise: decode the sign
        //
        // This matches the H.265 spec and libde265: the FIRST coefficient in
        // scanning order (lowest scan position) has its sign hidden.
        let num_signs = num_coeffs - usize::from(sign_hidden);
        let coeff_signs = cabac.decode_bypass_bits(num_signs as u8);
        if rc_trace {
            let (range, _, _) = cabac.get_state_extended();
            let (byte_pos, _, _) = cabac.get_position();
            let mut signs = [0u8; 16];
            for (coeff_idx, sign) in signs[..num_signs].iter_mut().enumerate() {
                *sign = ((coeff_signs >> (num_signs - 1 - coeff_idx)) & 1) as u8;
            }
            rc_eprintln!(
                "{rcp}_SIGNS n_sig={} hidden={} signs={:?} range={} byte={}",
                num_coeffs,
                sign_hidden,
                &signs[..num_coeffs],
                range,
                byte_pos
            );
        }

        // Decode remaining levels for all coefficients that need it
        // Rice parameter starts at 0 and is updated adaptively
        let mut rice_param = 0u8;

        // Decode remaining levels for coefficients that need it
        let mut remaining_flags = needs_remaining;
        while remaining_flags != 0 {
            let coeff_idx = remaining_flags.trailing_zeros() as usize;
            remaining_flags &= remaining_flags - 1;
            let dst_idx = touched_coeffs[subblock_start + coeff_idx] as usize;
            let base = coeffs[dst_idx];
            let (remaining, new_rice) = decode_coeff_abs_level_remaining(cabac, rice_param, base)?;
            if rc_trace {
                let (range, _, _) = cabac.get_state_extended();
                let (byte_pos, _, _) = cabac.get_position();
                let raster_x = dst_idx % size;
                let raster_y = dst_idx / size;
                let local_raster =
                    (raster_y - sb_y as usize * 4) * 4 + raster_x - sb_x as usize * 4;
                let n = inverse_scan[local_raster];
                rc_eprintln!(
                    "{rcp}_REM n={} base={} rice={} rem={} final={} range={} byte={}",
                    n,
                    base,
                    rice_param,
                    remaining,
                    base + remaining,
                    range,
                    byte_pos
                );
            }
            rice_param = new_rice;
            coeffs[dst_idx] = base + remaining;
        }

        // Apply signs and compute sum for parity inference
        // At this point all workspace values are positive absolute levels.
        let mut sum_abs_level = 0i32;

        for coeff_idx in 0..num_coeffs {
            let dst_idx = touched_coeffs[subblock_start + coeff_idx] as usize;
            let coeff_value = &mut coeffs[dst_idx];
            if coeff_idx < num_signs && coeff_signs & (1 << (num_signs - 1 - coeff_idx)) != 0 {
                *coeff_value = -*coeff_value;
            }
            sum_abs_level += *coeff_value as i32;

            // Infer the hidden sign at the first coefficient in scan order.
            if coeff_idx + 1 == num_coeffs && sign_hidden && (sum_abs_level & 1) != 0 {
                *coeff_value = -*coeff_value;
            }
        }

        #[cfg(feature = "decoder-tracing")]
        for &dst_idx in &touched_coeffs[subblock_start..num_nonzero] {
            // Track large coefficients (indicates CABAC desync)
            if coeffs[dst_idx as usize].abs() > 500 {
                let (byte_pos, _, _) = cabac.get_position();
                debug::track_large_coeff(byte_pos);
            }
        }
        // Update prev_subblock_had_gt1 for the next subblock (lower scan index)
        prev_subblock_had_gt1 = this_subblock_had_gt1;
    }

    if rc_trace {
        let (range, _, _) = cabac.get_state_extended();
        let (byte_pos, _, _) = cabac.get_position();
        rc_eprintln!("{rcp}_END range={} byte={}", range, byte_pos);
    }
    Ok((num_nonzero, transform_skip))
}

/// Get sub-block scan order
/// For TU of size 2^log2_size, sub-blocks are arranged in a grid of size 2^(log2_size-2)
/// This function returns the diagonal scan order for sub-blocks
fn get_scan_sub_block(log2_size: u8, order: ScanOrder) -> &'static [(u8, u8)] {
    // Sub-block scan tables
    // Note: The order here must match how coefficients are accessed in the decoder
    static SCAN_1X1: [(u8, u8); 1] = [(0, 0)];
    static SCAN_2X2_DIAG: [(u8, u8); 4] = [(0, 0), (0, 1), (1, 0), (1, 1)];
    // Per libde265 init_scan_h: (0,0),(1,0),(0,1),(1,1) — row by row
    static SCAN_2X2_HORIZ: [(u8, u8); 4] = [(0, 0), (1, 0), (0, 1), (1, 1)];
    // Per libde265 init_scan_v: (0,0),(0,1),(1,0),(1,1) — column by column
    // Note: 2x2 diagonal and vertical are identical
    #[allow(dead_code)]
    static SCAN_2X2_VERT: [(u8, u8); 4] = [(0, 0), (0, 1), (1, 0), (1, 1)];
    // Per libde265 init_scan_d(blkSize=4): verified against C++ trace
    static SCAN_4X4_DIAG: [(u8, u8); 16] = [
        (0, 0),
        (0, 1),
        (1, 0),
        (0, 2),
        (1, 1),
        (2, 0),
        (0, 3),
        (1, 2),
        (2, 1),
        (3, 0),
        (1, 3),
        (2, 2),
        (3, 1),
        (2, 3),
        (3, 2),
        (3, 3),
    ];
    // Per libde265 init_scan_d(blkSize=8): verified against C++ trace
    static SCAN_8X8_DIAG: [(u8, u8); 64] = [
        (0, 0),
        (0, 1),
        (1, 0),
        (0, 2),
        (1, 1),
        (2, 0),
        (0, 3),
        (1, 2),
        (2, 1),
        (3, 0),
        (0, 4),
        (1, 3),
        (2, 2),
        (3, 1),
        (4, 0),
        (0, 5),
        (1, 4),
        (2, 3),
        (3, 2),
        (4, 1),
        (5, 0),
        (0, 6),
        (1, 5),
        (2, 4),
        (3, 3),
        (4, 2),
        (5, 1),
        (6, 0),
        (0, 7),
        (1, 6),
        (2, 5),
        (3, 4),
        (4, 3),
        (5, 2),
        (6, 1),
        (7, 0),
        (1, 7),
        (2, 6),
        (3, 5),
        (4, 4),
        (5, 3),
        (6, 2),
        (7, 1),
        (2, 7),
        (3, 6),
        (4, 5),
        (5, 4),
        (6, 3),
        (7, 2),
        (3, 7),
        (4, 6),
        (5, 5),
        (6, 4),
        (7, 3),
        (4, 7),
        (5, 6),
        (6, 5),
        (7, 4),
        (5, 7),
        (6, 6),
        (7, 5),
        (6, 7),
        (7, 6),
        (7, 7),
    ];

    match log2_size {
        2 => &SCAN_1X1,
        3 => match order {
            ScanOrder::Horizontal => &SCAN_2X2_HORIZ,
            _ => &SCAN_2X2_DIAG, // Diagonal and Vertical are identical for 2x2
        },
        // For 16x16 and larger, scan is always diagonal per H.265
        4 => &SCAN_4X4_DIAG,
        5 => &SCAN_8X8_DIAG,
        _ => &SCAN_1X1,
    }
}

/// Inverse of the sub-block scan tables. Only 8x8 luma can use the horizontal
/// 2x2 order; all larger sub-block grids use the diagonal order.
#[inline]
fn sub_block_scan_pos(x: u32, y: u32, width: u32, horizontal: bool) -> u32 {
    if horizontal && width == 2 {
        return y * width + x;
    }

    let diagonal = x + y;
    if diagonal < width {
        diagonal * (diagonal + 1) / 2 + x
    } else {
        let tail = 2 * width - 1 - diagonal;
        width * width - tail * (tail + 1) / 2 + x - (diagonal + 1 - width)
    }
}

/// Decode last significant coefficient position
fn decode_last_sig_coeff_pos(
    cabac: &mut CabacDecoder<'_>,
    ctx: &mut [ContextModel],
    log2_size: u8,
    c_idx: u8,
) -> Result<(u32, u32)> {
    // Decode prefix
    let x_prefix = decode_last_sig_coeff_prefix(cabac, ctx, log2_size, c_idx, true)?;
    let y_prefix = decode_last_sig_coeff_prefix(cabac, ctx, log2_size, c_idx, false)?;

    // Decode suffix if needed
    let x = if x_prefix > 3 {
        let n_bits = (x_prefix >> 1) - 1;
        let suffix = cabac.decode_bypass_bits(n_bits as u8);
        ((2 + (x_prefix & 1)) << n_bits) + suffix
    } else {
        x_prefix
    };

    let y = if y_prefix > 3 {
        let n_bits = (y_prefix >> 1) - 1;
        let suffix = cabac.decode_bypass_bits(n_bits as u8);
        ((2 + (y_prefix & 1)) << n_bits) + suffix
    } else {
        y_prefix
    };

    Ok((x, y))
}

/// Decode last_significant_coeff prefix
fn decode_last_sig_coeff_prefix(
    cabac: &mut CabacDecoder<'_>,
    ctx: &mut [ContextModel],
    log2_size: u8,
    c_idx: u8,
    is_x: bool,
) -> Result<u32> {
    let ctx_base = if is_x {
        context::LAST_SIG_COEFF_X_PREFIX
    } else {
        context::LAST_SIG_COEFF_Y_PREFIX
    };

    // Context offset and shift based on component and size
    // Per H.265 spec (matches libde265):
    // Luma: ctxOffset = 3*(log2Size-2) + ((log2Size-1)>>2), ctxShift = (log2Size+1)>>2
    // Chroma: ctxOffset = 15, ctxShift = log2Size-2
    let (ctx_offset, ctx_shift) = if c_idx == 0 {
        // Luma
        let offset = 3 * (log2_size as usize - 2) + ((log2_size as usize - 1) >> 2);
        let shift = (log2_size + 1) >> 2;
        (offset, shift)
    } else {
        // Chroma
        (15, log2_size - 2)
    };

    let max_prefix = (log2_size << 1) - 1;

    let mut prefix = 0u32;
    while prefix < max_prefix as u32 {
        let ctx_idx = ctx_base + ctx_offset + (prefix as usize >> ctx_shift as usize);
        let bin = cabac.decode_bin(&mut ctx[ctx_idx]);
        if bin == 0 {
            break;
        }
        prefix += 1;
    }

    Ok(prefix)
}

/// Context index map for 4x4 TU sig_coeff_flag (H.265 Table 9-41)
/// Maps position (y*4 + x) to context index
static CTX_IDX_MAP_4X4: [u8; 16] = [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 8];

/// Local sig_coeff_flag context contribution for each `prevCsbf` value and
/// raster position. The component, transform-size, scan, and sub-block terms
/// are invariant across a sub-block and are added once in `decode_residual`.
static SIG_CTX_LOCAL: [[u8; 16]; 4] = [
    [2, 1, 1, 0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 2, 2, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 1, 0, 0, 2, 1, 0, 0, 2, 1, 0, 0, 2, 1, 0, 0],
    [2; 16],
];

/// Decode coeff_abs_level_remaining (Golomb-Rice with adaptive rice parameter)
/// Returns (value, updated_rice_param)
fn decode_coeff_abs_level_remaining(
    cabac: &mut CabacDecoder<'_>,
    rice_param: u8,
    base_level: i16,
) -> Result<(i16, u8)> {
    // Decode prefix (unary part)
    let mut prefix = 0u32;
    while cabac.decode_bypass() != 0 && prefix < 32 {
        prefix += 1;
    }

    let value = if prefix <= 3 {
        // TR part only: value = (prefix << rice_param) + suffix
        let suffix = if rice_param > 0 {
            cabac.decode_bypass_bits(rice_param)
        } else {
            0
        };
        ((prefix << rice_param) + suffix) as i16
    } else {
        // Conformant streams keep values within CoeffMax (32767), bounding
        // prefix-3+rice at 14 bits. Larger prefixes only arise from corrupt
        // or desynced data; error out instead of wrapping silently.
        if prefix - 3 + rice_param as u32 > 14 {
            return Err(HevcError::InvalidBitstream(
                "coeff_abs_level_remaining out of range",
            ));
        }
        // EGk part: suffix bits = prefix - 3 + rice_param
        let suffix_bits = (prefix - 3 + rice_param as u32) as u8;
        let suffix = cabac.decode_bypass_bits(suffix_bits);
        // value = (((1 << (prefix-3)) + 3 - 1) << rice_param) + suffix
        let base = ((1u32 << (prefix - 3)) + 2) << rice_param;
        let v = base + suffix;
        if v > 32767 {
            return Err(HevcError::InvalidBitstream(
                "coeff_abs_level_remaining out of range",
            ));
        }
        v as i16
    };

    // Update rice parameter: if baseLevel + value > 3 * (1 << rice_param), increase
    let threshold = 3 * (1 << rice_param);
    let new_rice_param = if (base_level.unsigned_abs() as u32 + value as u32) > threshold {
        (rice_param + 1).min(4)
    } else {
        rice_param
    };

    Ok((value, new_rice_param))
}
