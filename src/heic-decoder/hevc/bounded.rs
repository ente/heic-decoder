use super::bitstream::{BitstreamReader, NalUnit, parse_single_nal_bounded};
use super::params::{Pps, Sps, parse_pps_bounded, parse_sps_bounded, parse_vps};
use super::slice::SliceHeader;
use super::{DecodedFrame, HevcError, Result, decode_slice};

pub(crate) struct Prepared<'a> {
    sps: Sps,
    pps: Pps,
    slice: NalUnit<'a>,
    pub(crate) geometry: Geometry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Geometry {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) coded_width: u32,
    pub(crate) coded_height: u32,
    pub(crate) crop: [u32; 4],
    pub(crate) full_range: bool,
    pub(crate) matrix: u8,
    pub(crate) primaries: u8,
    pub(crate) transfer: u8,
}

impl<'a> Prepared<'a> {
    pub(crate) fn reconstruction_bytes(&self) -> u64 {
        let w = u64::from(self.geometry.coded_width);
        let h = u64::from(self.geometry.coded_height);
        let blocks = |side: u64| w.div_ceil(side) * h.div_ceil(side);
        let planes = 2 * (w * h + 2 * blocks(2));
        let maps = blocks(1 << self.sps.log2_min_cb_size())
            + 2 * blocks(1 << (self.sps.log2_min_cb_size() - 1))
            + blocks(1 << self.sps.log2_min_tb_size())
            + blocks(u64::from(self.sps.ctb_size())) * size_of::<super::sao::SaoInfo>() as u64;
        let reconstruction = 2 * planes + 3 * blocks(4) + maps;
        let pixels = u64::from(self.geometry.width) * u64::from(self.geometry.height);
        reconstruction
            .max(planes + 3 * pixels + 2 * blocks(4))
            .max(6 * pixels + 2 * blocks(4))
    }

    pub(crate) fn new(vps: &[u8], sps: &[u8], pps: &[u8], slice: &'a [u8]) -> Result<Self> {
        let vps = parse_single_nal_bounded(vps)?;
        let vps = parse_vps(&vps.payload)?;
        if vps.max_layers_minus1 != 0 || vps.max_sub_layers_minus1 != 0 {
            return Err(HevcError::Unsupported("bounded multilayer HEVC"));
        }
        let sps = parse_single_nal_bounded(sps)?;
        let sps = parse_sps_bounded(&sps.payload)?;
        validate_sps(&sps)?;
        if sps.vps_id != vps.vps_id {
            return Err(HevcError::InvalidBitstream("SPS VPS mismatch"));
        }
        let pps = parse_single_nal_bounded(pps)?;
        let pps = parse_pps_bounded(&pps.payload)?;
        if pps.sps_id != sps.sps_id || pps.pps_id > 63 || pps.dependent_slice_segments_enabled_flag
        {
            return Err(HevcError::Unsupported("bounded PPS references"));
        }
        if pps.diff_cu_qp_delta_depth > sps.log2_diff_max_min_luma_coding_block_size
            || !(-26..=25).contains(&pps.init_qp_minus26)
            || !(-12..=12).contains(&pps.pps_cb_qp_offset)
            || !(-12..=12).contains(&pps.pps_cr_qp_offset)
            || !(-6..=6).contains(&pps.pps_beta_offset_div2)
            || !(-6..=6).contains(&pps.pps_tc_offset_div2)
        {
            return Err(HevcError::InvalidBitstream("bounded PPS parameters"));
        }
        let slice = parse_single_nal_bounded(slice)?;
        let mut reader = BitstreamReader::new(&slice.payload);
        if reader.read_bit()? != 1 {
            return Err(HevcError::Unsupported("bounded multiple slices"));
        }
        reader.read_bit()?;
        if reader.read_ue()? != u32::from(pps.pps_id) {
            return Err(HevcError::InvalidBitstream("slice PPS mismatch"));
        }
        for _ in 0..pps.num_extra_slice_header_bits {
            reader.read_bit()?;
        }
        if reader.read_ue()? != 2 {
            return Err(HevcError::Unsupported("bounded non-intra slice"));
        }
        let parsed = SliceHeader::parse_bounded(&slice, &sps, &pps)?;
        if !(0..=51).contains(&parsed.header.slice_qp_y)
            || !(-12..=12).contains(&parsed.header.slice_cb_qp_offset)
            || !(-12..=12).contains(&parsed.header.slice_cr_qp_offset)
            || !(-6..=6).contains(&parsed.header.slice_beta_offset_div2)
            || !(-6..=6).contains(&parsed.header.slice_tc_offset_div2)
            || !parsed.header.pic_output_flag
        {
            return Err(HevcError::InvalidBitstream("bounded slice parameters"));
        }
        if parsed.data_offset >= slice.payload.len() {
            return Err(HevcError::InvalidBitstream("empty slice data"));
        }
        let crop = [
            sps.conf_win_offset.0 * 2,
            sps.conf_win_offset.1 * 2,
            sps.conf_win_offset.2 * 2,
            sps.conf_win_offset.3 * 2,
        ];
        let geometry = Geometry {
            width: sps.pic_width_in_luma_samples - crop[0] - crop[1],
            height: sps.pic_height_in_luma_samples - crop[2] - crop[3],
            coded_width: sps.pic_width_in_luma_samples,
            coded_height: sps.pic_height_in_luma_samples,
            crop,
            full_range: sps.video_full_range_flag,
            matrix: sps.matrix_coeffs,
            primaries: sps.colour_primaries,
            transfer: sps.transfer_characteristics,
        };
        Ok(Self {
            sps,
            pps,
            slice,
            geometry,
        })
    }

    pub(crate) fn decode(self) -> Result<DecodedFrame> {
        let g = self.geometry;
        let mut frame = DecodedFrame::try_with_params(g.coded_width, g.coded_height, 8, 1)?;
        frame.set_crop(g.crop[0], g.crop[1], g.crop[2], g.crop[3]);
        frame.full_range = g.full_range;
        frame.matrix_coeffs = g.matrix;
        frame.colour_primaries = g.primaries;
        decode_slice(&self.slice, &self.sps, &self.pps, &mut frame)?;
        Ok(frame)
    }
}

fn validate_sps(sps: &Sps) -> Result<()> {
    let w = sps.pic_width_in_luma_samples;
    let h = sps.pic_height_in_luma_samples;
    if w == 0 || h == 0 || w > 16384 || h > 16384 {
        return Err(HevcError::InvalidBitstream("bounded SPS dimensions"));
    }
    if sps.sps_id > 15
        || sps.max_sub_layers_minus1 != 0
        || sps.chroma_format_idc != 1
        || sps.bit_depth_luma_minus8 != 0
        || sps.bit_depth_chroma_minus8 != 0
        || sps.separate_colour_plane_flag
        || sps.pcm_enabled_flag
        || sps.unsupported_rext_tool.is_some()
    {
        return Err(HevcError::Unsupported("bounded HEVC profile"));
    }
    if sps.log2_min_luma_coding_block_size_minus3 > 3
        || sps.log2_diff_max_min_luma_coding_block_size > 3
        || sps.log2_min_luma_transform_block_size_minus2 > 3
        || sps.log2_diff_max_min_luma_transform_block_size > 3
    {
        return Err(HevcError::InvalidBitstream("bounded block sizes"));
    }
    if !(4..=6).contains(&sps.log2_ctb_size())
        || sps.log2_max_tb_size() > 5
        || sps.log2_max_tb_size() > sps.log2_ctb_size()
        || sps.log2_min_tb_size() > sps.log2_min_cb_size()
        || sps.max_transform_hierarchy_depth_intra > sps.log2_ctb_size() - sps.log2_min_tb_size()
        || sps.max_transform_hierarchy_depth_inter > sps.log2_ctb_size() - sps.log2_min_tb_size()
    {
        return Err(HevcError::InvalidBitstream("bounded block hierarchy"));
    }
    let minimum_block = 1 << sps.log2_min_cb_size();
    if !w.is_multiple_of(minimum_block) || !h.is_multiple_of(minimum_block) {
        return Err(HevcError::InvalidBitstream(
            "bounded coding block alignment",
        ));
    }
    let (left, right, top, bottom) = sps.conf_win_offset;
    if u64::from(left) * 2 + u64::from(right) * 2 >= u64::from(w)
        || u64::from(top) * 2 + u64::from(bottom) * 2 >= u64::from(h)
    {
        return Err(HevcError::InvalidBitstream("bounded conformance window"));
    }
    Ok(())
}
