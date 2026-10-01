use super::ctu::SliceContext;
use super::params::{Pps, Sps};
use super::picture::DecodedFrame;
use super::sao::{SaoMap, write_filtered_rows};
use super::slice::SliceHeader;
use super::{HevcError, Result, deblock};

#[derive(Debug, Default)]
pub(crate) struct Statistics {
    pub(crate) rows: u32,
    pub(crate) sao_edge_components: usize,
    pub(crate) sao_band_components: usize,
}

pub(crate) fn decode(
    sps: &Sps,
    pps: &Pps,
    header: &SliceHeader,
    data: &mut dyn std::io::Read,
    mut emit: impl FnMut(u32, &mut DecodedFrame) -> Result<()>,
) -> Result<Statistics> {
    if pps.entropy_coding_sync_enabled_flag
        || pps.tiles_enabled_flag
        || !header.first_slice_segment_in_pic_flag
    {
        return Err(HevcError::Unsupported("incremental coding structure"));
    }
    let size = sps.ctb_size();
    let width = sps.pic_width_in_luma_samples;
    let height = sps.pic_height_in_luma_samples;
    let mut raw = DecodedFrame::try_with_params(width, 2 * size, 8, 1)?;
    let mut filtered = DecodedFrame::try_with_params(width, 3 * size, 8, 1)?;
    let mut output = DecodedFrame::try_with_params(width, size, 8, 1)?;
    raw.height = height;
    filtered.height = height;
    output.full_range = sps.video_full_range_flag;
    output.matrix_coeffs = sps.matrix_coeffs;
    output.colour_primaries = sps.colour_primaries;
    let mut context = SliceContext::new_stream(sps, pps, header, data, 2 * size)?;
    let mut statistics = Statistics::default();
    for row in 0..sps.pic_height_in_ctbs() {
        let start = row * size;
        let rows = size.min(height - start);
        context.decode_row(row, &mut raw)?;
        filtered.advance_rows(row.saturating_sub(2) * size);
        filtered.copy_rows_from(&raw, start, rows);
        if !header.slice_deblocking_filter_disabled_flag {
            deblock::apply_deblocking_rows(
                &mut filtered,
                i32::from(header.slice_beta_offset_div2) * 2,
                i32::from(header.slice_tc_offset_div2) * 2,
                i32::from(pps.pps_cb_qp_offset),
                i32::from(pps.pps_cr_qp_offset),
                start,
                start + rows,
            );
        }
        for x in 0..sps.pic_width_in_ctbs() {
            let info = context.sao_map.get(x, row);
            statistics.sao_edge_components += info.sao_type_idx.iter().filter(|&&n| n == 2).count();
            statistics.sao_band_components += info.sao_type_idx.iter().filter(|&&n| n == 1).count();
        }
        if row > 0 {
            emit_rows(
                &filtered,
                &context.sao_map,
                size,
                start - size,
                size,
                &mut output,
                &mut emit,
            )?;
            statistics.rows += size;
        }
        if start + rows == height {
            emit_rows(
                &filtered,
                &context.sao_map,
                size,
                start,
                rows,
                &mut output,
                &mut emit,
            )?;
            statistics.rows += rows;
        }
    }
    Ok(statistics)
}

fn emit_rows(
    frame: &DecodedFrame,
    map: &SaoMap,
    size: u32,
    start: u32,
    rows: u32,
    output: &mut DecodedFrame,
    emit: &mut impl FnMut(u32, &mut DecodedFrame) -> Result<()>,
) -> Result<()> {
    output.height = rows;
    write_filtered_rows(frame, map, size, start, rows, output);
    emit(start, output)
}
