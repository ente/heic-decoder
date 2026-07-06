//! HEVC/H.265 decoder
//!
//! This module implements the HEVC (High Efficiency Video Coding) decoder
//! for decoding HEIC still images.

pub(crate) mod bitstream;
mod cabac;
pub(crate) mod color_convert;
mod ctu;
mod deblock;
pub(crate) mod debug;
mod intra;
pub(crate) mod params;
mod picture;
mod residual;
mod sao;
mod slice;
mod transform;
mod transform_simd;
mod transforms;

pub use picture::DecodedFrame;

use crate::heic_decoder::error::HevcError;

type Result<T> = core::result::Result<T, HevcError>;

/// Decode HEVC bitstream to pixels (Annex B or raw format)
pub fn decode(data: &[u8]) -> Result<DecodedFrame> {
    // Parse NAL units
    let nal_units = bitstream::parse_nal_units(data)?;
    decode_nal_units(&nal_units)
}

/// Internal: decode from parsed NAL units
fn decode_nal_units(nal_units: &[bitstream::NalUnit<'_>]) -> Result<DecodedFrame> {
    // Find and parse parameter sets
    let mut _vps = None;
    let mut sps = None;
    let mut pps = None;

    for nal in nal_units {
        match nal.nal_type {
            bitstream::NalType::VpsNut => {
                _vps = Some(params::parse_vps(&nal.payload)?);
            }
            bitstream::NalType::SpsNut => {
                sps = Some(params::parse_sps(&nal.payload)?);
            }
            bitstream::NalType::PpsNut => {
                pps = Some(params::parse_pps(&nal.payload)?);
            }
            _ => {}
        }
    }

    let sps = sps.ok_or(HevcError::MissingParameterSet("SPS"))?;
    let pps = pps.ok_or(HevcError::MissingParameterSet("PPS"))?;

    // Capability checks live here rather than in the SPS parser so that
    // metadata-only callers can still read dimensions/bit depths of valid
    // streams this decoder cannot decode.
    if sps.pcm_enabled_flag {
        // pcm_flag is not decoded in the CU layer; with PCM enabled every
        // eligible CU codes that bin, so continuing would desync CABAC and
        // produce silent garbage.
        return Err(HevcError::Unsupported("IPCM (pcm_enabled_flag)"));
    }
    if sps.chroma_format_idc != 0 && sps.bit_depth_chroma_minus8 != sps.bit_depth_luma_minus8 {
        // The pipeline (intra clamps, deblock/SAO scaling, output conversion)
        // assumes one bit depth for all planes.
        return Err(HevcError::Unsupported(
            "different luma and chroma bit depths",
        ));
    }
    if let Some(tool) = sps.unsupported_rext_tool {
        return Err(HevcError::Unsupported(tool));
    }

    // Sanity-check dimensions before allocating (prevent OOM from malicious SPS)
    let w = sps.pic_width_in_luma_samples;
    let h = sps.pic_height_in_luma_samples;
    if w == 0 || h == 0 || w > 16384 || h > 16384 {
        return Err(HevcError::InvalidParameterSet {
            kind: "SPS",
            msg: alloc::format!("invalid dimensions {}x{}", w, h),
        });
    }
    if w.checked_mul(h).is_none() {
        return Err(HevcError::InvalidParameterSet {
            kind: "SPS",
            msg: alloc::format!("dimensions {}x{} overflow u32", w, h),
        });
    }

    // Create frame buffer with actual bit depth and chroma format from SPS
    let mut frame = DecodedFrame::with_params(
        sps.pic_width_in_luma_samples,
        sps.pic_height_in_luma_samples,
        sps.bit_depth_y(),
        sps.chroma_format_idc,
    );
    frame.full_range = sps.video_full_range_flag;
    frame.matrix_coeffs = sps.matrix_coeffs;
    frame.colour_primaries = sps.colour_primaries;

    // Set conformance window cropping from SPS
    // Offsets are in units of SubWidthC/SubHeightC, need to convert to luma samples
    if sps.conformance_window_flag {
        let (sub_width_c, sub_height_c) = match sps.chroma_format_idc {
            0 => (1, 1), // Monochrome
            1 => (2, 2), // 4:2:0
            2 => (2, 1), // 4:2:2
            3 => (1, 1), // 4:4:4
            _ => (2, 2), // Default to 4:2:0
        };
        let crop_left = sps.conf_win_offset.0.saturating_mul(sub_width_c);
        let crop_right = sps.conf_win_offset.1.saturating_mul(sub_width_c);
        let crop_top = sps.conf_win_offset.2.saturating_mul(sub_height_c);
        let crop_bottom = sps.conf_win_offset.3.saturating_mul(sub_height_c);
        // Guard against crops that consume the whole picture (u32 underflow
        // in cropped_width/height on malicious SPS).
        if crop_left.saturating_add(crop_right) >= w || crop_top.saturating_add(crop_bottom) >= h {
            return Err(HevcError::InvalidParameterSet {
                kind: "SPS",
                msg: alloc::format!(
                    "conformance window ({crop_left},{crop_right},{crop_top},{crop_bottom}) exceeds picture {w}x{h}"
                ),
            });
        }
        frame.set_crop(crop_left, crop_right, crop_top, crop_bottom);
    }

    // Decode slice data (base layer only — skip enhancement layer NALs in L-HEVC streams)
    for nal in nal_units {
        if nal.nal_type.is_slice() && nal.nuh_layer_id == 0 {
            decode_slice(nal, &sps, &pps, &mut frame)?;
        }
    }

    Ok(frame)
}

fn decode_slice(
    nal: &bitstream::NalUnit<'_>,
    sps: &params::Sps,
    pps: &params::Pps,
    frame: &mut DecodedFrame,
) -> Result<()> {
    // 1. Parse slice header and get data offset
    let parse_result = slice::SliceHeader::parse(nal, sps, pps)?;
    let mut slice_header = parse_result.header;
    let data_offset = parse_result.data_offset;

    // Entry point offsets count transmitted bytes (including emulation
    // prevention bytes), but the CABAC decoder reads the stripped payload.
    // Subtract the number of emulation prevention bytes removed from the slice
    // data before each entry point (mirrors libde265
    // NAL_unit::num_skipped_bytes_before + decctx entry point adjustment).
    if !slice_header.entry_point_offsets.is_empty() && !nal.skipped_byte_positions.is_empty() {
        let header_len = data_offset as u32;
        for offset in slice_header.entry_point_offsets.iter_mut() {
            let skipped = nal
                .skipped_byte_positions
                .iter()
                .filter(|&&pos| pos >= header_len && pos - header_len <= *offset)
                .count() as u32;
            if skipped > *offset {
                return Err(HevcError::InvalidBitstream(
                    "entry point offset smaller than removed emulation prevention bytes",
                ));
            }
            *offset -= skipped;
        }
    }

    // Verify this is an I-slice (required for HEIC still images)
    if !slice_header.slice_type.is_intra() {
        return Err(HevcError::Unsupported(
            "only I-slices supported for still images",
        ));
    }

    // 2. Get slice data (after header)
    // Use the offset from slice header parsing to skip the header bytes
    let slice_data = &nal.payload[data_offset..];

    // 3. Create slice context and decode CTUs
    let mut ctx = ctu::SliceContext::new(sps, pps, &slice_header, slice_data)?;

    // 4. Decode all CTUs in the slice
    ctx.decode_slice(frame)?;

    // 5. Apply deblocking filter
    if !slice_header.slice_deblocking_filter_disabled_flag {
        let beta_offset = slice_header.slice_beta_offset_div2 as i32 * 2;
        let tc_offset = slice_header.slice_tc_offset_div2 as i32 * 2;
        let cb_qp_offset = pps.pps_cb_qp_offset as i32;
        let cr_qp_offset = pps.pps_cr_qp_offset as i32;
        deblock::apply_deblocking_filter(frame, beta_offset, tc_offset, cb_qp_offset, cr_qp_offset);
    }

    // 6. Apply SAO (Sample Adaptive Offset)
    if slice_header.slice_sao_luma_flag || slice_header.slice_sao_chroma_flag {
        sao::apply_sao(frame, &ctx.sao_map, sps.ctb_size());
    }

    Ok(())
}
