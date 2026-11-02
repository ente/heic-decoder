use super::bitstream::{BitstreamReader, NalUnit};
use super::params::{Pps, Sps};
use crate::heic_decoder::error::HevcError;
use alloc::vec::Vec;

type Result<T> = core::result::Result<T, HevcError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SliceType {
    B = 0,
    P = 1,
    I = 2,
}

impl SliceType {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::B),
            1 => Some(Self::P),
            2 => Some(Self::I),
            _ => None,
        }
    }

    pub fn is_intra(self) -> bool {
        self == Self::I
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PartMode {
    Part2Nx2N = 0,
    Part2NxN = 1,
    PartNx2N = 2,
    PartNxN = 3,
    Part2NxnU = 4,
    Part2NxnD = 5,
    PartnLx2N = 6,
    PartnRx2N = 7,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredMode {
    Intra,
    Inter,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IntraPredMode {
    Planar = 0,
    Dc = 1,
    Angular2 = 2,
    Angular3 = 3,
    Angular4 = 4,
    Angular5 = 5,
    Angular6 = 6,
    Angular7 = 7,
    Angular8 = 8,
    Angular9 = 9,
    Angular10 = 10,
    Angular11 = 11,
    Angular12 = 12,
    Angular13 = 13,
    Angular14 = 14,
    Angular15 = 15,
    Angular16 = 16,
    Angular17 = 17,
    Angular18 = 18,
    Angular19 = 19,
    Angular20 = 20,
    Angular21 = 21,
    Angular22 = 22,
    Angular23 = 23,
    Angular24 = 24,
    Angular25 = 25,
    Angular26 = 26,
    Angular27 = 27,
    Angular28 = 28,
    Angular29 = 29,
    Angular30 = 30,
    Angular31 = 31,
    Angular32 = 32,
    Angular33 = 33,
    Angular34 = 34,
}

impl IntraPredMode {
    #[inline]
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::Planar),
            1 => Some(Self::Dc),
            2 => Some(Self::Angular2),
            3 => Some(Self::Angular3),
            4 => Some(Self::Angular4),
            5 => Some(Self::Angular5),
            6 => Some(Self::Angular6),
            7 => Some(Self::Angular7),
            8 => Some(Self::Angular8),
            9 => Some(Self::Angular9),
            10 => Some(Self::Angular10),
            11 => Some(Self::Angular11),
            12 => Some(Self::Angular12),
            13 => Some(Self::Angular13),
            14 => Some(Self::Angular14),
            15 => Some(Self::Angular15),
            16 => Some(Self::Angular16),
            17 => Some(Self::Angular17),
            18 => Some(Self::Angular18),
            19 => Some(Self::Angular19),
            20 => Some(Self::Angular20),
            21 => Some(Self::Angular21),
            22 => Some(Self::Angular22),
            23 => Some(Self::Angular23),
            24 => Some(Self::Angular24),
            25 => Some(Self::Angular25),
            26 => Some(Self::Angular26),
            27 => Some(Self::Angular27),
            28 => Some(Self::Angular28),
            29 => Some(Self::Angular29),
            30 => Some(Self::Angular30),
            31 => Some(Self::Angular31),
            32 => Some(Self::Angular32),
            33 => Some(Self::Angular33),
            34 => Some(Self::Angular34),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SliceHeader {
    pub first_slice_segment_in_pic_flag: bool,
    pub no_output_of_prior_pics_flag: bool,
    pub pps_id: u8,
    pub dependent_slice_segment_flag: bool,
    pub slice_segment_address: u32,

    pub slice_type: SliceType,
    pub pic_output_flag: bool,
    pub colour_plane_id: u8,
    pub slice_pic_order_cnt_lsb: u32,

    pub slice_sao_luma_flag: bool,
    pub slice_sao_chroma_flag: bool,

    pub slice_qp_delta: i8,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,

    pub cu_chroma_qp_offset_enabled_flag: bool,

    pub deblocking_filter_override_flag: bool,
    pub slice_deblocking_filter_disabled_flag: bool,
    pub slice_beta_offset_div2: i8,
    pub slice_tc_offset_div2: i8,

    pub slice_loop_filter_across_slices_enabled_flag: bool,

    pub num_entry_point_offsets: u32,
    pub entry_point_offsets: Vec<u32>,

    pub slice_qp_y: i32,
}

pub struct SliceParseResult {
    pub header: SliceHeader,
    pub data_offset: usize,
}

impl SliceHeader {
    pub fn parse(nal: &NalUnit<'_>, sps: &Sps, pps: &Pps) -> Result<SliceParseResult> {
        Self::parse_impl(nal, sps, pps, false)
    }

    pub(crate) fn parse_bounded(
        nal: &NalUnit<'_>,
        sps: &Sps,
        pps: &Pps,
    ) -> Result<SliceParseResult> {
        Self::parse_impl(nal, sps, pps, true)
    }

    fn parse_impl(
        nal: &NalUnit<'_>,
        sps: &Sps,
        pps: &Pps,
        bounded: bool,
    ) -> Result<SliceParseResult> {
        let mut reader = BitstreamReader::new(&nal.payload);

        let first_slice_segment_in_pic_flag = reader.read_bit()? != 0;

        let no_output_of_prior_pics_flag = if nal.nal_type.is_irap() {
            reader.read_bit()? != 0
        } else {
            false
        };

        let pps_id = super::params::bounded_ue_u8(&mut reader, bounded)?;
        if pps_id != pps.pps_id {
            return Err(HevcError::InvalidBitstream("PPS ID mismatch"));
        }

        let dependent_slice_segment_flag;
        let slice_segment_address;

        if !first_slice_segment_in_pic_flag {
            dependent_slice_segment_flag = if pps.dependent_slice_segments_enabled_flag {
                reader.read_bit()? != 0
            } else {
                false
            };

            let pic_size_in_ctbs = sps.pic_width_in_ctbs() * sps.pic_height_in_ctbs();
            let address_bits = ceil_log2(pic_size_in_ctbs);
            slice_segment_address = reader.read_bits(address_bits)?;
        } else {
            dependent_slice_segment_flag = false;
            slice_segment_address = 0;
        }

        if dependent_slice_segment_flag {
            return Err(HevcError::Unsupported("dependent slice segments"));
        }

        // Multi-slice pictures are rejected loudly: SAO merge availability,
        // qPY prediction and the per-picture loop filters all assume a single
        // slice covering the frame, so decoding further slices would produce
        // silently wrong pixels.
        if !first_slice_segment_in_pic_flag || slice_segment_address != 0 {
            return Err(HevcError::Unsupported("multi-slice pictures"));
        }

        for _ in 0..pps.num_extra_slice_header_bits {
            reader.read_bit()?;
        }

        let slice_type_val = super::params::bounded_ue_u8(&mut reader, bounded)?;
        let slice_type = SliceType::from_u8(slice_type_val)
            .ok_or(HevcError::InvalidBitstream("invalid slice type"))?;

        let pic_output_flag = if pps.output_flag_present_flag {
            reader.read_bit()? != 0
        } else {
            true
        };

        let colour_plane_id = if sps.separate_colour_plane_flag {
            reader.read_bits(2)? as u8
        } else {
            0
        };

        let slice_pic_order_cnt_lsb = if !nal.nal_type.is_idr() {
            let poc_bits = sps.log2_max_pic_order_cnt_lsb_minus4 + 4;
            reader.read_bits(poc_bits)?
        } else {
            0
        };

        if !nal.nal_type.is_idr() {
            skip_ref_pic_set(&mut reader, sps)?;
        }

        let (slice_sao_luma_flag, slice_sao_chroma_flag) =
            if sps.sample_adaptive_offset_enabled_flag {
                let luma = reader.read_bit()? != 0;
                let chroma = if sps.chroma_array_type() != 0 {
                    reader.read_bit()? != 0
                } else {
                    false
                };
                (luma, chroma)
            } else {
                (false, false)
            };

        if slice_type != SliceType::I {
            return Err(HevcError::Unsupported("P/B slices not yet implemented"));
        }

        let slice_qp_delta = super::params::bounded_se_i8(&mut reader, bounded)?;

        let (slice_cb_qp_offset, slice_cr_qp_offset) =
            if pps.pps_slice_chroma_qp_offsets_present_flag {
                let cb = super::params::bounded_se_i8(&mut reader, bounded)?;
                let cr = super::params::bounded_se_i8(&mut reader, bounded)?;
                (cb, cr)
            } else {
                (0, 0)
            };

        let cu_chroma_qp_offset_enabled_flag = false;

        let deblocking_filter_override_flag = if pps.deblocking_filter_override_enabled_flag {
            reader.read_bit()? != 0
        } else {
            false
        };

        let (slice_deblocking_filter_disabled_flag, slice_beta_offset_div2, slice_tc_offset_div2) =
            if deblocking_filter_override_flag {
                let disabled = reader.read_bit()? != 0;
                if !disabled {
                    let beta = super::params::bounded_se_i8(&mut reader, bounded)?;
                    let tc = super::params::bounded_se_i8(&mut reader, bounded)?;
                    (disabled, beta, tc)
                } else {
                    (disabled, 0, 0)
                }
            } else {
                (
                    pps.pps_deblocking_filter_disabled_flag,
                    pps.pps_beta_offset_div2,
                    pps.pps_tc_offset_div2,
                )
            };

        let slice_loop_filter_across_slices_enabled_flag = if pps
            .pps_loop_filter_across_slices_enabled_flag
            && (slice_sao_luma_flag
                || slice_sao_chroma_flag
                || !slice_deblocking_filter_disabled_flag)
        {
            reader.read_bit()? != 0
        } else {
            pps.pps_loop_filter_across_slices_enabled_flag
        };

        let (num_entry_point_offsets, entry_point_offsets) =
            if pps.tiles_enabled_flag || pps.entropy_coding_sync_enabled_flag {
                let n = reader.read_ue()?;
                // Bound: with tiles limited to 1x1, entry points only come
                // from WPP, at most one per CTB row after the first.
                if n >= sps.pic_height_in_ctbs() {
                    return Err(HevcError::InvalidBitstream(
                        "num_entry_point_offsets out of range",
                    ));
                }
                let mut offsets = Vec::new();
                offsets
                    .try_reserve_exact(n as usize)
                    .map_err(|_| HevcError::DecodingError("allocation failed"))?;
                if n > 0 {
                    let offset_len_minus1 = reader.read_ue()?;
                    if offset_len_minus1 > 31 {
                        return Err(HevcError::InvalidBitstream(
                            "offset_len_minus1 out of range",
                        ));
                    }
                    let offset_len = offset_len_minus1 as u8 + 1;
                    let mut cumulative = 0u32;
                    for _ in 0..n {
                        let offset = reader
                            .read_bits(offset_len)?
                            .checked_add(1)
                            .ok_or(HevcError::InvalidBitstream("entry point offset overflow"))?;
                        cumulative = cumulative
                            .checked_add(offset)
                            .ok_or(HevcError::InvalidBitstream("entry point offset overflow"))?;
                        offsets.push(cumulative);
                    }
                } else {
                    offsets.clear();
                }
                (n, offsets)
            } else {
                (0, Vec::new())
            };

        if pps.slice_segment_header_extension_present_flag {
            let ext_len = reader.read_ue()?;
            for _ in 0..ext_len {
                reader.read_bits(8)?;
            }
        }

        let _alignment_bit = reader.read_bit()?;
        reader.byte_align();

        let data_offset = reader.byte_position();

        let slice_qp_y = 26 + pps.init_qp_minus26 as i32 + slice_qp_delta as i32;

        Ok(SliceParseResult {
            header: SliceHeader {
                first_slice_segment_in_pic_flag,
                no_output_of_prior_pics_flag,
                pps_id,
                dependent_slice_segment_flag,
                slice_segment_address,
                slice_type,
                pic_output_flag,
                colour_plane_id,
                slice_pic_order_cnt_lsb,
                slice_sao_luma_flag,
                slice_sao_chroma_flag,
                slice_qp_delta,
                slice_cb_qp_offset,
                slice_cr_qp_offset,
                cu_chroma_qp_offset_enabled_flag,
                deblocking_filter_override_flag,
                slice_deblocking_filter_disabled_flag,
                slice_beta_offset_div2,
                slice_tc_offset_div2,
                slice_loop_filter_across_slices_enabled_flag,
                num_entry_point_offsets,
                entry_point_offsets,
                slice_qp_y,
            },
            data_offset,
        })
    }
}

fn skip_ref_pic_set(reader: &mut BitstreamReader<'_>, sps: &Sps) -> Result<()> {
    let short_term_ref_pic_set_sps_flag = reader.read_bit()? != 0;

    if !short_term_ref_pic_set_sps_flag {
        skip_short_term_ref_pic_set(reader, sps.num_short_term_ref_pic_sets)?;
    } else if sps.num_short_term_ref_pic_sets > 1 {
        let bits = ceil_log2(sps.num_short_term_ref_pic_sets as u32);
        reader.read_bits(bits)?;
    }

    if sps.long_term_ref_pics_present_flag {
        let num_long_term_sps = reader.read_ue()?;
        let num_long_term_pics = reader.read_ue()?;

        let poc_bits = sps.log2_max_pic_order_cnt_lsb_minus4 + 4;

        for i in 0..(num_long_term_sps + num_long_term_pics) {
            if i < num_long_term_sps {
                let bits = ceil_log2(sps.num_long_term_ref_pics_sps);
                if bits > 0 {
                    reader.read_bits(bits)?;
                }
            } else {
                reader.read_bits(poc_bits)?;
                reader.read_bit()?;
            }
            let delta_poc_msb_present = reader.read_bit()? != 0;
            if delta_poc_msb_present {
                reader.read_ue()?;
            }
        }
    }

    if sps.sps_temporal_mvp_enabled_flag {
        reader.read_bit()?;
    }

    Ok(())
}

fn skip_short_term_ref_pic_set(reader: &mut BitstreamReader<'_>, idx: u8) -> Result<()> {
    let inter_ref_pic_set_prediction_flag = if idx != 0 {
        reader.read_bit()? != 0
    } else {
        false
    };

    if inter_ref_pic_set_prediction_flag {
        reader.read_bit()?;
        reader.read_ue()?;
        return Err(HevcError::Unsupported("inter-predicted ref pic set"));
    }

    let num_negative_pics = reader.read_ue()?;
    let num_positive_pics = reader.read_ue()?;

    for _ in 0..num_negative_pics {
        reader.read_ue()?;
        reader.read_bit()?;
    }

    for _ in 0..num_positive_pics {
        reader.read_ue()?;
        reader.read_bit()?;
    }

    Ok(())
}

fn ceil_log2(x: u32) -> u8 {
    if x <= 1 {
        0
    } else {
        32 - (x - 1).leading_zeros() as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ceil_log2() {
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(4), 2);
        assert_eq!(ceil_log2(5), 3);
        assert_eq!(ceil_log2(8), 3);
        assert_eq!(ceil_log2(9), 4);
    }

    #[test]
    fn test_intra_pred_mode() {
        assert_eq!(IntraPredMode::from_u8(0), Some(IntraPredMode::Planar));
        assert_eq!(IntraPredMode::from_u8(1), Some(IntraPredMode::Dc));
        assert_eq!(IntraPredMode::from_u8(26), Some(IntraPredMode::Angular26));
        assert_eq!(IntraPredMode::from_u8(34), Some(IntraPredMode::Angular34));
        assert_eq!(IntraPredMode::from_u8(35), None);
    }
}
