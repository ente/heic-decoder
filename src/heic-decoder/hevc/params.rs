use alloc::string::ToString;
use alloc::vec::Vec;

use super::bitstream::BitstreamReader;
use crate::heic_decoder::error::HevcError;

type Result<T> = core::result::Result<T, HevcError>;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Vps {
    pub vps_id: u8,
    pub base_layer_internal_flag: bool,
    pub base_layer_available_flag: bool,
    pub max_layers_minus1: u8,
    pub max_sub_layers_minus1: u8,
    pub temporal_id_nesting_flag: bool,
    pub ptl: ProfileTierLevel,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Sps {
    pub sps_id: u8,
    pub vps_id: u8,
    pub max_sub_layers_minus1: u8,
    pub temporal_id_nesting_flag: bool,
    pub ptl: ProfileTierLevel,
    pub chroma_format_idc: u8,
    pub separate_colour_plane_flag: bool,
    pub pic_width_in_luma_samples: u32,
    pub pic_height_in_luma_samples: u32,
    pub conformance_window_flag: bool,
    pub conf_win_offset: (u32, u32, u32, u32),
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub sub_layer_ordering_info_present_flag: bool,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_luma_transform_block_size_minus2: u8,
    pub log2_diff_max_min_luma_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub scaling_list_enabled_flag: bool,
    pub scaling_list: Option<ScalingListData>,
    pub amp_enabled_flag: bool,
    pub sample_adaptive_offset_enabled_flag: bool,
    pub pcm_enabled_flag: bool,
    pub pcm_params: Option<PcmParams>,
    pub num_short_term_ref_pic_sets: u8,
    pub long_term_ref_pics_present_flag: bool,
    pub num_long_term_ref_pics_sps: u32,
    pub sps_temporal_mvp_enabled_flag: bool,
    pub strong_intra_smoothing_enabled_flag: bool,
    pub vui_parameters_present_flag: bool,
    pub video_full_range_flag: bool,
    pub matrix_coeffs: u8,
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub unsupported_rext_tool: Option<&'static str>,
}

impl Sps {
    pub fn chroma_array_type(&self) -> u8 {
        if self.separate_colour_plane_flag {
            0
        } else {
            self.chroma_format_idc
        }
    }

    pub fn bit_depth_y(&self) -> u8 {
        8 + self.bit_depth_luma_minus8
    }

    pub fn bit_depth_c(&self) -> u8 {
        8 + self.bit_depth_chroma_minus8
    }

    pub fn log2_min_cb_size(&self) -> u8 {
        self.log2_min_luma_coding_block_size_minus3 + 3
    }

    pub fn log2_ctb_size(&self) -> u8 {
        self.log2_min_cb_size() + self.log2_diff_max_min_luma_coding_block_size
    }

    pub fn ctb_size(&self) -> u32 {
        1 << self.log2_ctb_size()
    }

    pub fn pic_width_in_ctbs(&self) -> u32 {
        self.pic_width_in_luma_samples.div_ceil(self.ctb_size())
    }

    pub fn pic_height_in_ctbs(&self) -> u32 {
        self.pic_height_in_luma_samples.div_ceil(self.ctb_size())
    }

    pub fn log2_min_tb_size(&self) -> u8 {
        self.log2_min_luma_transform_block_size_minus2 + 2
    }

    pub fn log2_max_tb_size(&self) -> u8 {
        self.log2_min_tb_size() + self.log2_diff_max_min_luma_transform_block_size
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PcmParams {
    pub pcm_sample_bit_depth_luma_minus1: u8,
    pub pcm_sample_bit_depth_chroma_minus1: u8,
    pub log2_min_pcm_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_pcm_luma_coding_block_size: u8,
    pub pcm_loop_filter_disabled_flag: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ScalingListData {
    pub lists: [[[u8; 64]; 6]; 4],
    pub dc_coef: [[u8; 6]; 2],
}

impl Default for ScalingListData {
    fn default() -> Self {
        Self::new_default()
    }
}

impl ScalingListData {
    pub fn new_default() -> Self {
        let mut data = Self {
            lists: [[[16; 64]; 6]; 4],
            dc_coef: [[16; 6]; 2],
        };

        #[rustfmt::skip]
        const DEFAULT_INTRA_8X8: [u8; 64] = [
            16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18,
            17, 18, 18, 17, 18, 21, 19, 20, 21, 20, 19, 21, 24, 22, 22, 24,
            24, 22, 22, 24, 25, 25, 27, 30, 27, 25, 25, 29, 31, 35, 35, 31,
            29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115,
        ];
        #[rustfmt::skip]
        const DEFAULT_INTER_8X8: [u8; 64] = [
            16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18,
            18, 18, 18, 18, 18, 20, 20, 20, 20, 20, 20, 20, 24, 24, 24, 24,
            24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 28, 28, 28, 28, 28,
            28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91,
        ];

        for size_id in 1..4 {
            for matrix_id in 0..3 {
                data.lists[size_id][matrix_id] = DEFAULT_INTRA_8X8;
            }
            for matrix_id in 3..6 {
                data.lists[size_id][matrix_id] = DEFAULT_INTER_8X8;
            }
        }

        data
    }

    #[allow(dead_code)]
    pub fn new_flat() -> Self {
        Self {
            lists: [[[16; 64]; 6]; 4],
            dc_coef: [[16; 6]; 2],
        }
    }

    #[inline]
    pub fn get_scaling_factor(&self, log2_size: u8, matrix_id: u8, x: u32, y: u32) -> u8 {
        let size_id = (log2_size - 2) as usize;
        let mid = matrix_id as usize;

        match size_id {
            0 => {
                let idx = DIAG_SCAN_4X4_INV[x as usize][y as usize];
                self.lists[0][mid][idx]
            }
            1 => {
                let idx = DIAG_SCAN_8X8_INV[x as usize][y as usize];
                self.lists[1][mid][idx]
            }
            2 => {
                if x == 0 && y == 0 {
                    return self.dc_coef[0][mid];
                }
                let rx = (x / 2) as usize;
                let ry = (y / 2) as usize;
                let idx = DIAG_SCAN_8X8_INV[rx][ry];
                self.lists[2][mid][idx]
            }
            3 => {
                if x == 0 && y == 0 {
                    return self.dc_coef[1][mid];
                }
                let rx = (x / 4) as usize;
                let ry = (y / 4) as usize;
                let idx = DIAG_SCAN_8X8_INV[rx][ry];
                self.lists[3][mid][idx]
            }
            _ => 16,
        }
    }
}

#[rustfmt::skip]
static DIAG_SCAN_4X4_INV: [[usize; 4]; 4] = [
    [ 0,  1,  3,  6],
    [ 2,  4,  7, 10],
    [ 5,  8, 11, 13],
    [ 9, 12, 14, 15],
];

#[rustfmt::skip]
static DIAG_SCAN_8X8_INV: [[usize; 8]; 8] = [
    [ 0,  1,  3,  6, 10, 15, 21, 28],
    [ 2,  4,  7, 11, 16, 22, 29, 36],
    [ 5,  8, 12, 17, 23, 30, 37, 43],
    [ 9, 13, 18, 24, 31, 38, 44, 49],
    [14, 19, 25, 32, 39, 45, 50, 54],
    [20, 26, 33, 40, 46, 51, 55, 58],
    [27, 34, 41, 47, 52, 56, 59, 61],
    [35, 42, 48, 53, 57, 60, 62, 63],
];

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Pps {
    pub pps_id: u8,
    pub sps_id: u8,
    pub dependent_slice_segments_enabled_flag: bool,
    pub output_flag_present_flag: bool,
    pub num_extra_slice_header_bits: u8,
    pub sign_data_hiding_enabled_flag: bool,
    pub cabac_init_present_flag: bool,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i8,
    pub constrained_intra_pred_flag: bool,
    pub transform_skip_enabled_flag: bool,
    pub cu_qp_delta_enabled_flag: bool,
    pub diff_cu_qp_delta_depth: u8,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub pps_slice_chroma_qp_offsets_present_flag: bool,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_flag: bool,
    pub transquant_bypass_enabled_flag: bool,
    pub tiles_enabled_flag: bool,
    pub entropy_coding_sync_enabled_flag: bool,
    pub tile_info: Option<TileInfo>,
    pub pps_loop_filter_across_slices_enabled_flag: bool,
    pub deblocking_filter_control_present_flag: bool,
    pub deblocking_filter_override_enabled_flag: bool,
    pub pps_deblocking_filter_disabled_flag: bool,
    pub pps_beta_offset_div2: i8,
    pub pps_tc_offset_div2: i8,
    pub pps_scaling_list_data_present_flag: bool,
    pub pps_scaling_list: Option<ScalingListData>,
    pub lists_modification_present_flag: bool,
    pub log2_parallel_merge_level_minus2: u8,
    pub slice_segment_header_extension_present_flag: bool,
    pub log2_sao_offset_scale_luma: u8,
    pub log2_sao_offset_scale_chroma: u8,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TileInfo {
    pub num_tile_columns_minus1: u16,
    pub num_tile_rows_minus1: u16,
    pub uniform_spacing_flag: bool,
    pub column_widths: Vec<u16>,
    pub row_heights: Vec<u16>,
    pub loop_filter_across_tiles_enabled_flag: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct ProfileTierLevel {
    pub general_profile_space: u8,
    pub general_tier_flag: bool,
    pub general_profile_idc: u8,
    pub general_profile_compatibility_flag: [bool; 32],
    pub general_progressive_source_flag: bool,
    pub general_interlaced_source_flag: bool,
    pub general_non_packed_constraint_flag: bool,
    pub general_frame_only_constraint_flag: bool,
    pub general_level_idc: u8,
}

pub fn parse_vps(data: &[u8]) -> Result<Vps> {
    let mut reader = BitstreamReader::new(data);

    let vps_id = reader.read_bits(4)? as u8;
    let base_layer_internal_flag = reader.read_bit()? != 0;
    let base_layer_available_flag = reader.read_bit()? != 0;
    let max_layers_minus1 = reader.read_bits(6)? as u8;
    let max_sub_layers_minus1 = reader.read_bits(3)? as u8;
    let temporal_id_nesting_flag = reader.read_bit()? != 0;

    let reserved = reader.read_bits(16)?;
    if reserved != 0xFFFF {
        return Err(HevcError::InvalidParameterSet {
            kind: "VPS",
            msg: "invalid reserved bits".to_string(),
        });
    }

    let ptl = parse_profile_tier_level(&mut reader, true, max_sub_layers_minus1)?;

    Ok(Vps {
        vps_id,
        base_layer_internal_flag,
        base_layer_available_flag,
        max_layers_minus1,
        max_sub_layers_minus1,
        temporal_id_nesting_flag,
        ptl,
    })
}

pub fn parse_sps(data: &[u8]) -> Result<Sps> {
    parse_sps_impl(data, false)
}

pub(crate) fn parse_sps_bounded(data: &[u8]) -> Result<Sps> {
    parse_sps_impl(data, true)
}

fn parse_sps_impl(data: &[u8], bounded: bool) -> Result<Sps> {
    let mut reader = BitstreamReader::new(data);

    let vps_id = reader.read_bits(4)? as u8;
    let max_sub_layers_minus1 = reader.read_bits(3)? as u8;
    let temporal_id_nesting_flag = reader.read_bit()? != 0;

    let ptl = parse_profile_tier_level(&mut reader, true, max_sub_layers_minus1)?;

    let sps_id = bounded_ue_u8(&mut reader, bounded)?;
    let chroma_format_idc = bounded_ue_u8(&mut reader, bounded)?;

    let separate_colour_plane_flag = if chroma_format_idc == 3 {
        reader.read_bit()? != 0
    } else {
        false
    };

    let pic_width_in_luma_samples = reader.read_ue()?;
    let pic_height_in_luma_samples = reader.read_ue()?;

    let conformance_window_flag = reader.read_bit()? != 0;
    let conf_win_offset = if conformance_window_flag {
        let left = reader.read_ue()?;
        let right = reader.read_ue()?;
        let top = reader.read_ue()?;
        let bottom = reader.read_ue()?;
        (left, right, top, bottom)
    } else {
        (0, 0, 0, 0)
    };

    let bit_depth_luma_minus8 = reader.read_ue()?;
    let bit_depth_chroma_minus8 = reader.read_ue()?;
    if bit_depth_luma_minus8 > 8 || bit_depth_chroma_minus8 > 8 {
        return Err(HevcError::InvalidBitstream(
            "SPS bit_depth_minus8 must be within 0..=8",
        ));
    }
    let bit_depth_luma_minus8 = bit_depth_luma_minus8 as u8;
    let bit_depth_chroma_minus8 = bit_depth_chroma_minus8 as u8;
    let log2_max_pic_order_cnt_lsb_minus4 = bounded_ue_u8(&mut reader, bounded)?;
    if bounded && log2_max_pic_order_cnt_lsb_minus4 > 12 {
        return Err(HevcError::Unsupported("bounded POC width"));
    }

    let sub_layer_ordering_info_present_flag = reader.read_bit()? != 0;

    let start = if sub_layer_ordering_info_present_flag {
        0
    } else {
        max_sub_layers_minus1
    };
    for _ in start..=max_sub_layers_minus1 {
        let _max_dec_pic_buffering_minus1 = reader.read_ue()?;
        let _max_num_reorder_pics = reader.read_ue()?;
        let _max_latency_increase_plus1 = reader.read_ue()?;
    }

    let log2_min_luma_coding_block_size_minus3 = bounded_ue_u8(&mut reader, bounded)?;
    let log2_diff_max_min_luma_coding_block_size = bounded_ue_u8(&mut reader, bounded)?;
    let log2_min_luma_transform_block_size_minus2 = bounded_ue_u8(&mut reader, bounded)?;
    let log2_diff_max_min_luma_transform_block_size = bounded_ue_u8(&mut reader, bounded)?;
    let max_transform_hierarchy_depth_inter = bounded_ue_u8(&mut reader, bounded)?;
    let max_transform_hierarchy_depth_intra = bounded_ue_u8(&mut reader, bounded)?;

    let scaling_list_enabled_flag = reader.read_bit()? != 0;
    let scaling_list = if scaling_list_enabled_flag {
        let scaling_list_data_present = reader.read_bit()? != 0;
        if scaling_list_data_present {
            Some(parse_scaling_list_data(&mut reader, bounded)?)
        } else {
            Some(ScalingListData::new_default())
        }
    } else {
        None
    };

    let amp_enabled_flag = reader.read_bit()? != 0;
    let sample_adaptive_offset_enabled_flag = reader.read_bit()? != 0;

    let pcm_enabled_flag = reader.read_bit()? != 0;
    let pcm_params = if pcm_enabled_flag {
        let pcm_sample_bit_depth_luma_minus1 = reader.read_bits(4)? as u8;
        let pcm_sample_bit_depth_chroma_minus1 = reader.read_bits(4)? as u8;
        let log2_min_pcm_luma_coding_block_size_minus3 = bounded_ue_u8(&mut reader, bounded)?;
        let log2_diff_max_min_pcm_luma_coding_block_size = bounded_ue_u8(&mut reader, bounded)?;
        let pcm_loop_filter_disabled_flag = reader.read_bit()? != 0;
        Some(PcmParams {
            pcm_sample_bit_depth_luma_minus1,
            pcm_sample_bit_depth_chroma_minus1,
            log2_min_pcm_luma_coding_block_size_minus3,
            log2_diff_max_min_pcm_luma_coding_block_size,
            pcm_loop_filter_disabled_flag,
        })
    } else {
        None
    };

    let num_short_term_ref_pic_sets = bounded_ue_u8(&mut reader, bounded)?;
    let mut num_delta_pocs: Vec<u32> = Vec::new();
    num_delta_pocs
        .try_reserve_exact(num_short_term_ref_pic_sets as usize)
        .map_err(|_| HevcError::DecodingError("allocation failed"))?;
    for i in 0..num_short_term_ref_pic_sets {
        let n = skip_short_term_ref_pic_set(&mut reader, i, &num_delta_pocs)?;
        num_delta_pocs.push(n);
    }

    let long_term_ref_pics_present_flag = reader.read_bit()? != 0;
    let mut num_long_term_ref_pics_sps = 0u32;
    if long_term_ref_pics_present_flag {
        num_long_term_ref_pics_sps = reader.read_ue()?;
        if num_long_term_ref_pics_sps > 32 {
            return Err(HevcError::InvalidBitstream(
                "num_long_term_ref_pics_sps out of range",
            ));
        }
        for _ in 0..num_long_term_ref_pics_sps {
            let _lt_ref_pic_poc_lsb_sps =
                reader.read_bits(log2_max_pic_order_cnt_lsb_minus4 + 4)?;
            let _used_by_curr_pic_lt_sps_flag = reader.read_bit()?;
        }
    }

    let sps_temporal_mvp_enabled_flag = reader.read_bit()? != 0;
    let strong_intra_smoothing_enabled_flag = reader.read_bit()? != 0;

    let vui_parameters_present_flag = reader.read_bit()? != 0;

    let mut video_full_range_flag = false;
    let mut matrix_coeffs = 2u8;
    let mut transfer_characteristics = 2u8;
    let mut colour_primaries = 2u8;
    if vui_parameters_present_flag {
        let aspect_ratio_info_present = reader.read_bit()? != 0;
        if aspect_ratio_info_present {
            let aspect_ratio_idc = reader.read_bits(8)?;
            if aspect_ratio_idc == 255 {
                let _sar_width = reader.read_bits(16)?;
                let _sar_height = reader.read_bits(16)?;
            }
        }
        let overscan_info_present = reader.read_bit()? != 0;
        if overscan_info_present {
            let _overscan_appropriate = reader.read_bit()?;
        }
        let video_signal_type_present = reader.read_bit()? != 0;
        if video_signal_type_present {
            let _video_format = reader.read_bits(3)?;
            video_full_range_flag = reader.read_bit()? != 0;
            let colour_description_present = reader.read_bit()? != 0;
            if colour_description_present {
                colour_primaries = reader.read_bits(8)? as u8;
                transfer_characteristics = reader.read_bits(8)? as u8;
                matrix_coeffs = reader.read_bits(8)? as u8;
            }
        }
        skip_vui_remainder(&mut reader, max_sub_layers_minus1)?;
    }

    let mut unsupported_rext_tool: Option<&'static str> = None;
    let sps_extension_present_flag = reader.read_bit()? != 0;
    if bounded && sps_extension_present_flag {
        return Err(HevcError::Unsupported("bounded SPS extensions"));
    }
    if sps_extension_present_flag {
        let sps_range_extension_flag = reader.read_bit()? != 0;
        let _sps_multilayer_extension_flag = reader.read_bit()? != 0;
        let _sps_3d_extension_flag = reader.read_bit()? != 0;
        let _sps_scc_extension_flag = reader.read_bit()? != 0;
        let _sps_extension_4bits = reader.read_bits(4)?;

        if sps_range_extension_flag {
            let names: [&'static str; 9] = [
                "transform_skip_rotation_enabled_flag",
                "transform_skip_context_enabled_flag",
                "implicit_rdpcm_enabled_flag",
                "explicit_rdpcm_enabled_flag",
                "extended_precision_processing_flag",
                "intra_smoothing_disabled_flag",
                "high_precision_offsets_enabled_flag",
                "persistent_rice_adaptation_enabled_flag",
                "cabac_bypass_alignment_enabled_flag",
            ];
            for name in names {
                let flag = reader.read_bit()? != 0;
                // high_precision_offsets only affects inter weighted
                // prediction, which still images never use.
                if flag
                    && name != "high_precision_offsets_enabled_flag"
                    && unsupported_rext_tool.is_none()
                {
                    unsupported_rext_tool = Some(name);
                }
            }
        }
    }

    Ok(Sps {
        sps_id,
        vps_id,
        max_sub_layers_minus1,
        temporal_id_nesting_flag,
        ptl,
        chroma_format_idc,
        separate_colour_plane_flag,
        pic_width_in_luma_samples,
        pic_height_in_luma_samples,
        conformance_window_flag,
        conf_win_offset,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        log2_max_pic_order_cnt_lsb_minus4,
        sub_layer_ordering_info_present_flag,
        log2_min_luma_coding_block_size_minus3,
        log2_diff_max_min_luma_coding_block_size,
        log2_min_luma_transform_block_size_minus2,
        log2_diff_max_min_luma_transform_block_size,
        max_transform_hierarchy_depth_inter,
        max_transform_hierarchy_depth_intra,
        scaling_list_enabled_flag,
        scaling_list,
        amp_enabled_flag,
        sample_adaptive_offset_enabled_flag,
        pcm_enabled_flag,
        pcm_params,
        num_short_term_ref_pic_sets,
        long_term_ref_pics_present_flag,
        num_long_term_ref_pics_sps,
        sps_temporal_mvp_enabled_flag,
        strong_intra_smoothing_enabled_flag,
        vui_parameters_present_flag,
        video_full_range_flag,
        matrix_coeffs,
        colour_primaries,
        transfer_characteristics,
        unsupported_rext_tool,
    })
}

pub fn parse_pps(data: &[u8]) -> Result<Pps> {
    parse_pps_impl(data, false)
}

pub(crate) fn parse_pps_bounded(data: &[u8]) -> Result<Pps> {
    parse_pps_impl(data, true)
}

fn parse_pps_impl(data: &[u8], bounded: bool) -> Result<Pps> {
    let mut reader = BitstreamReader::new(data);

    let pps_id = bounded_ue_u8(&mut reader, bounded)?;
    let sps_id = bounded_ue_u8(&mut reader, bounded)?;
    let dependent_slice_segments_enabled_flag = reader.read_bit()? != 0;
    let output_flag_present_flag = reader.read_bit()? != 0;
    let num_extra_slice_header_bits = reader.read_bits(3)? as u8;
    let sign_data_hiding_enabled_flag = reader.read_bit()? != 0;
    let cabac_init_present_flag = reader.read_bit()? != 0;
    let num_ref_idx_l0_default_active_minus1 = bounded_ue_u8(&mut reader, bounded)?;
    let num_ref_idx_l1_default_active_minus1 = bounded_ue_u8(&mut reader, bounded)?;
    let init_qp_minus26 = bounded_se_i8(&mut reader, bounded)?;
    let constrained_intra_pred_flag = reader.read_bit()? != 0;
    let transform_skip_enabled_flag = reader.read_bit()? != 0;

    let cu_qp_delta_enabled_flag = reader.read_bit()? != 0;
    let diff_cu_qp_delta_depth = if cu_qp_delta_enabled_flag {
        bounded_ue_u8(&mut reader, bounded)?
    } else {
        0
    };

    let pps_cb_qp_offset = bounded_se_i8(&mut reader, bounded)?;
    let pps_cr_qp_offset = bounded_se_i8(&mut reader, bounded)?;
    let pps_slice_chroma_qp_offsets_present_flag = reader.read_bit()? != 0;
    let weighted_pred_flag = reader.read_bit()? != 0;
    let weighted_bipred_flag = reader.read_bit()? != 0;
    let transquant_bypass_enabled_flag = reader.read_bit()? != 0;
    let tiles_enabled_flag = reader.read_bit()? != 0;
    let entropy_coding_sync_enabled_flag = reader.read_bit()? != 0;

    let tile_info = if tiles_enabled_flag {
        let columns = reader.read_ue()?;
        if bounded && columns != 0 {
            return Err(HevcError::Unsupported("bounded HEVC tiles"));
        }
        let num_tile_columns_minus1 = columns as u16;
        let rows = reader.read_ue()?;
        if bounded && rows != 0 {
            return Err(HevcError::Unsupported("bounded HEVC tiles"));
        }
        let num_tile_rows_minus1 = rows as u16;
        let uniform_spacing_flag = reader.read_bit()? != 0;

        let (column_widths, row_heights) = if !uniform_spacing_flag {
            let mut cols = Vec::with_capacity(num_tile_columns_minus1 as usize);
            let mut rows = Vec::with_capacity(num_tile_rows_minus1 as usize);
            for _ in 0..num_tile_columns_minus1 {
                cols.push(reader.read_ue()? as u16);
            }
            for _ in 0..num_tile_rows_minus1 {
                rows.push(reader.read_ue()? as u16);
            }
            (cols, rows)
        } else {
            (Vec::new(), Vec::new())
        };

        let loop_filter_across_tiles_enabled_flag = reader.read_bit()? != 0;

        // The CTB decode loop walks raster order with WPP-only entry point
        // handling; multi-tile pictures would silently desync CABAC. Reject
        // them loudly until tile-scan decoding is implemented.
        if num_tile_columns_minus1 > 0 || num_tile_rows_minus1 > 0 {
            return Err(HevcError::Unsupported("PPS with multiple tiles"));
        }

        Some(TileInfo {
            num_tile_columns_minus1,
            num_tile_rows_minus1,
            uniform_spacing_flag,
            column_widths,
            row_heights,
            loop_filter_across_tiles_enabled_flag,
        })
    } else {
        None
    };

    let pps_loop_filter_across_slices_enabled_flag = reader.read_bit()? != 0;
    let deblocking_filter_control_present_flag = reader.read_bit()? != 0;

    let (
        deblocking_filter_override_enabled_flag,
        pps_deblocking_filter_disabled_flag,
        pps_beta_offset_div2,
        pps_tc_offset_div2,
    ) = if deblocking_filter_control_present_flag {
        let override_enabled = reader.read_bit()? != 0;
        let disabled = reader.read_bit()? != 0;
        let (beta, tc) = if !disabled {
            (
                bounded_se_i8(&mut reader, bounded)?,
                bounded_se_i8(&mut reader, bounded)?,
            )
        } else {
            (0, 0)
        };
        (override_enabled, disabled, beta, tc)
    } else {
        (false, false, 0, 0)
    };

    let pps_scaling_list_data_present_flag = reader.read_bit()? != 0;
    let pps_scaling_list = if pps_scaling_list_data_present_flag {
        Some(parse_scaling_list_data(&mut reader, bounded)?)
    } else {
        None
    };

    let lists_modification_present_flag = reader.read_bit()? != 0;
    let log2_parallel_merge_level_minus2 = bounded_ue_u8(&mut reader, bounded)?;
    let slice_segment_header_extension_present_flag = reader.read_bit()? != 0;

    let mut log2_sao_offset_scale_luma = 0u8;
    let mut log2_sao_offset_scale_chroma = 0u8;
    let pps_extension_present_flag = reader.read_bit().unwrap_or(0) != 0;
    if bounded && pps_extension_present_flag {
        return Err(HevcError::Unsupported("bounded PPS extensions"));
    }
    if pps_extension_present_flag {
        let pps_range_extension_flag = reader.read_bit()? != 0;
        let _pps_multilayer_extension_flag = reader.read_bit()? != 0;
        let _pps_3d_extension_flag = reader.read_bit()? != 0;
        let _pps_scc_extension_flag = reader.read_bit()? != 0;
        let _pps_extension_4bits = reader.read_bits(4)?;

        if pps_range_extension_flag {
            if transform_skip_enabled_flag {
                let log2_max_transform_skip_block_size_minus2 = reader.read_ue()?;
                if log2_max_transform_skip_block_size_minus2 > 0 {
                    return Err(HevcError::Unsupported(
                        "transform skip for blocks larger than 4x4",
                    ));
                }
            }
            let cross_component_prediction_enabled_flag = reader.read_bit()? != 0;
            if cross_component_prediction_enabled_flag {
                return Err(HevcError::Unsupported("cross-component prediction"));
            }
            let chroma_qp_offset_list_enabled_flag = reader.read_bit()? != 0;
            if chroma_qp_offset_list_enabled_flag {
                return Err(HevcError::Unsupported("chroma QP offset lists"));
            }
            log2_sao_offset_scale_luma = bounded_ue_u8(&mut reader, bounded)?;
            log2_sao_offset_scale_chroma = bounded_ue_u8(&mut reader, bounded)?;
            // Spec bound is Max(0, bitDepth-10); the SAO offset storage (i8)
            // holds scaled offsets up to 31<<2, covering 12-bit content.
            if log2_sao_offset_scale_luma > 2 || log2_sao_offset_scale_chroma > 2 {
                return Err(HevcError::InvalidParameterSet {
                    kind: "PPS",
                    msg: alloc::format!(
                        "log2_sao_offset_scale out of range: {log2_sao_offset_scale_luma}/{log2_sao_offset_scale_chroma}"
                    ),
                });
            }
        }
    }

    Ok(Pps {
        pps_id,
        sps_id,
        dependent_slice_segments_enabled_flag,
        output_flag_present_flag,
        num_extra_slice_header_bits,
        sign_data_hiding_enabled_flag,
        cabac_init_present_flag,
        num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1,
        init_qp_minus26,
        constrained_intra_pred_flag,
        transform_skip_enabled_flag,
        cu_qp_delta_enabled_flag,
        diff_cu_qp_delta_depth,
        pps_cb_qp_offset,
        pps_cr_qp_offset,
        pps_slice_chroma_qp_offsets_present_flag,
        weighted_pred_flag,
        weighted_bipred_flag,
        transquant_bypass_enabled_flag,
        tiles_enabled_flag,
        entropy_coding_sync_enabled_flag,
        tile_info,
        pps_loop_filter_across_slices_enabled_flag,
        deblocking_filter_control_present_flag,
        deblocking_filter_override_enabled_flag,
        pps_deblocking_filter_disabled_flag,
        pps_beta_offset_div2,
        pps_tc_offset_div2,
        pps_scaling_list_data_present_flag,
        pps_scaling_list,
        lists_modification_present_flag,
        log2_parallel_merge_level_minus2,
        slice_segment_header_extension_present_flag,
        log2_sao_offset_scale_luma,
        log2_sao_offset_scale_chroma,
    })
}

fn parse_profile_tier_level(
    reader: &mut BitstreamReader<'_>,
    profile_present: bool,
    max_sub_layers_minus1: u8,
) -> Result<ProfileTierLevel> {
    let mut ptl = ProfileTierLevel::default();

    if profile_present {
        ptl.general_profile_space = reader.read_bits(2)? as u8;
        ptl.general_tier_flag = reader.read_bit()? != 0;
        ptl.general_profile_idc = reader.read_bits(5)? as u8;

        for i in 0..32 {
            ptl.general_profile_compatibility_flag[i] = reader.read_bit()? != 0;
        }

        ptl.general_progressive_source_flag = reader.read_bit()? != 0;
        ptl.general_interlaced_source_flag = reader.read_bit()? != 0;
        ptl.general_non_packed_constraint_flag = reader.read_bit()? != 0;
        ptl.general_frame_only_constraint_flag = reader.read_bit()? != 0;

        reader.read_bits(32)?;
        reader.read_bits(12)?;
    }

    ptl.general_level_idc = reader.read_bits(8)? as u8;

    let mut sub_layer_profile_present = [false; 8];
    let mut sub_layer_level_present = [false; 8];

    for i in 0..max_sub_layers_minus1 as usize {
        sub_layer_profile_present[i] = reader.read_bit()? != 0;
        sub_layer_level_present[i] = reader.read_bit()? != 0;
    }

    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            reader.read_bits(2)?;
        }
    }

    for i in 0..max_sub_layers_minus1 as usize {
        if sub_layer_profile_present[i] {
            reader.read_bits(32)?;
            reader.read_bits(32)?;
            reader.read_bits(24)?;
        }
        if sub_layer_level_present[i] {
            reader.read_bits(8)?;
        }
    }

    Ok(ptl)
}

fn parse_scaling_list_data(
    reader: &mut BitstreamReader<'_>,
    bounded: bool,
) -> Result<ScalingListData> {
    let mut data = ScalingListData::new_default();

    for size_id in 0..4usize {
        let matrix_step = if size_id == 3 { 3 } else { 1 };
        let mut matrix_id = 0usize;
        while matrix_id < 6 {
            let pred_mode_flag = reader.read_bit()? != 0;
            if !pred_mode_flag {
                let pred_matrix_id_delta = reader.read_ue()? as usize;
                if bounded && pred_matrix_id_delta > matrix_id / matrix_step {
                    return Err(HevcError::InvalidBitstream("scaling list matrix reference"));
                }
                if pred_matrix_id_delta == 0 {
                } else if let Some(ref_id) =
                    matrix_id.checked_sub(pred_matrix_id_delta * matrix_step)
                    && ref_id < 6
                {
                    data.lists[size_id][matrix_id] = data.lists[size_id][ref_id];
                    if size_id >= 2 {
                        data.dc_coef[size_id - 2][matrix_id] = data.dc_coef[size_id - 2][ref_id];
                    }
                }
            } else {
                let coef_num = core::cmp::min(64, 1usize << (4 + (size_id << 1)));
                let mut next_coef: i32 = 8;
                if size_id > 1 {
                    let dc_coef_minus8 = reader.read_se()?;
                    if bounded && !(-7..=247).contains(&dc_coef_minus8) {
                        return Err(HevcError::InvalidBitstream("scaling list DC coefficient"));
                    }
                    next_coef = dc_coef_minus8 + 8;
                    data.dc_coef[size_id - 2][matrix_id] = ((next_coef + 256) % 256) as u8;
                }
                for i in 0..coef_num {
                    let delta = reader.read_se()?;
                    if bounded && !(-128..=127).contains(&delta) {
                        return Err(HevcError::InvalidBitstream(
                            "scaling list coefficient delta",
                        ));
                    }
                    next_coef = (next_coef + delta + 256) % 256;
                    data.lists[size_id][matrix_id][i] = next_coef as u8;
                }
            }
            matrix_id += matrix_step;
        }
    }
    Ok(data)
}

fn skip_vui_remainder(reader: &mut BitstreamReader<'_>, max_sub_layers_minus1: u8) -> Result<()> {
    let chroma_loc_info_present_flag = reader.read_bit()? != 0;
    if chroma_loc_info_present_flag {
        let _chroma_sample_loc_type_top_field = reader.read_ue()?;
        let _chroma_sample_loc_type_bottom_field = reader.read_ue()?;
    }
    let _neutral_chroma_indication_flag = reader.read_bit()?;
    let _field_seq_flag = reader.read_bit()?;
    let _frame_field_info_present_flag = reader.read_bit()?;
    let default_display_window_flag = reader.read_bit()? != 0;
    if default_display_window_flag {
        let _def_disp_win_left_offset = reader.read_ue()?;
        let _def_disp_win_right_offset = reader.read_ue()?;
        let _def_disp_win_top_offset = reader.read_ue()?;
        let _def_disp_win_bottom_offset = reader.read_ue()?;
    }
    let vui_timing_info_present_flag = reader.read_bit()? != 0;
    if vui_timing_info_present_flag {
        let _vui_num_units_in_tick = reader.read_bits(32)?;
        let _vui_time_scale = reader.read_bits(32)?;
        let vui_poc_proportional_to_timing_flag = reader.read_bit()? != 0;
        if vui_poc_proportional_to_timing_flag {
            let _vui_num_ticks_poc_diff_one_minus1 = reader.read_ue()?;
        }
        let vui_hrd_parameters_present_flag = reader.read_bit()? != 0;
        if vui_hrd_parameters_present_flag {
            skip_hrd_parameters(reader, max_sub_layers_minus1)?;
        }
    }
    let bitstream_restriction_flag = reader.read_bit()? != 0;
    if bitstream_restriction_flag {
        let _tiles_fixed_structure_flag = reader.read_bit()?;
        let _motion_vectors_over_pic_boundaries_flag = reader.read_bit()?;
        let _restricted_ref_pic_lists_flag = reader.read_bit()?;
        let _min_spatial_segmentation_idc = reader.read_ue()?;
        let _max_bytes_per_pic_denom = reader.read_ue()?;
        let _max_bits_per_min_cu_denom = reader.read_ue()?;
        let _log2_max_mv_length_horizontal = reader.read_ue()?;
        let _log2_max_mv_length_vertical = reader.read_ue()?;
    }
    Ok(())
}

fn skip_hrd_parameters(reader: &mut BitstreamReader<'_>, max_sub_layers_minus1: u8) -> Result<()> {
    let nal_hrd_parameters_present_flag = reader.read_bit()? != 0;
    let vcl_hrd_parameters_present_flag = reader.read_bit()? != 0;
    let mut sub_pic_hrd_params_present_flag = false;
    if nal_hrd_parameters_present_flag || vcl_hrd_parameters_present_flag {
        sub_pic_hrd_params_present_flag = reader.read_bit()? != 0;
        if sub_pic_hrd_params_present_flag {
            let _tick_divisor_minus2 = reader.read_bits(8)?;
            let _du_cpb_removal_delay_increment_length_minus1 = reader.read_bits(5)?;
            let _sub_pic_cpb_params_in_pic_timing_sei_flag = reader.read_bit()?;
            let _dpb_output_delay_du_length_minus1 = reader.read_bits(5)?;
        }
        let _bit_rate_scale = reader.read_bits(4)?;
        let _cpb_size_scale = reader.read_bits(4)?;
        if sub_pic_hrd_params_present_flag {
            let _cpb_size_du_scale = reader.read_bits(4)?;
        }
        let _initial_cpb_removal_delay_length_minus1 = reader.read_bits(5)?;
        let _au_cpb_removal_delay_length_minus1 = reader.read_bits(5)?;
        let _dpb_output_delay_length_minus1 = reader.read_bits(5)?;
    }
    for _ in 0..=max_sub_layers_minus1 {
        let fixed_pic_rate_general_flag = reader.read_bit()? != 0;
        let fixed_pic_rate_within_cvs_flag = if !fixed_pic_rate_general_flag {
            reader.read_bit()? != 0
        } else {
            true
        };
        let low_delay_hrd_flag = if fixed_pic_rate_within_cvs_flag {
            let _elemental_duration_in_tc_minus1 = reader.read_ue()?;
            false
        } else {
            reader.read_bit()? != 0
        };
        let cpb_cnt_minus1 = if !low_delay_hrd_flag {
            let n = reader.read_ue()?;
            if n > 31 {
                return Err(HevcError::InvalidBitstream("cpb_cnt_minus1 out of range"));
            }
            n
        } else {
            0
        };
        for hrd_present in [
            nal_hrd_parameters_present_flag,
            vcl_hrd_parameters_present_flag,
        ] {
            if hrd_present {
                for _ in 0..=cpb_cnt_minus1 {
                    let _bit_rate_value_minus1 = reader.read_ue()?;
                    let _cpb_size_value_minus1 = reader.read_ue()?;
                    if sub_pic_hrd_params_present_flag {
                        let _cpb_size_du_value_minus1 = reader.read_ue()?;
                        let _bit_rate_du_value_minus1 = reader.read_ue()?;
                    }
                    let _cbr_flag = reader.read_bit()?;
                }
            }
        }
    }
    Ok(())
}

fn skip_short_term_ref_pic_set(
    reader: &mut BitstreamReader<'_>,
    idx: u8,
    prev_num_delta_pocs: &[u32],
) -> Result<u32> {
    let inter_ref_pic_set_prediction_flag = if idx != 0 {
        reader.read_bit()? != 0
    } else {
        false
    };

    if inter_ref_pic_set_prediction_flag {
        // Inside the SPS loop delta_idx_minus1 is not present (inferred 0),
        // so the reference set is always the immediately preceding one.
        let ref_num_delta_pocs = *prev_num_delta_pocs
            .last()
            .ok_or(HevcError::InvalidBitstream(
                "inter-predicted st_ref_pic_set without a previous set",
            ))?;
        let _delta_rps_sign = reader.read_bit()?;
        let _abs_delta_rps_minus1 = reader.read_ue()?;
        let mut num_delta_pocs = 0u32;
        for _ in 0..=ref_num_delta_pocs {
            let used_by_curr_pic_flag = reader.read_bit()? != 0;
            let use_delta_flag = if !used_by_curr_pic_flag {
                reader.read_bit()? != 0
            } else {
                true
            };
            if used_by_curr_pic_flag || use_delta_flag {
                num_delta_pocs += 1;
            }
        }
        Ok(num_delta_pocs)
    } else {
        let num_negative_pics = reader.read_ue()?;
        let num_positive_pics = reader.read_ue()?;
        // Bound the loop to keep corrupt streams from spinning on huge ue(v)
        // values; conformant streams keep these within DPB limits (<= 16).
        if num_negative_pics > 4096 || num_positive_pics > 4096 {
            return Err(HevcError::InvalidBitstream(
                "st_ref_pic_set pic count out of range",
            ));
        }
        for _ in 0..num_negative_pics {
            let _delta_poc_s0_minus1 = reader.read_ue()?;
            let _used_by_curr_pic_s0_flag = reader.read_bit()?;
        }
        for _ in 0..num_positive_pics {
            let _delta_poc_s1_minus1 = reader.read_ue()?;
            let _used_by_curr_pic_s1_flag = reader.read_bit()?;
        }
        Ok(num_negative_pics + num_positive_pics)
    }
}

pub(super) fn bounded_ue_u8(reader: &mut BitstreamReader<'_>, bounded: bool) -> Result<u8> {
    let value = reader.read_ue()?;
    if bounded && value > u8::MAX as u32 {
        return Err(HevcError::InvalidBitstream("parameter narrowing overflow"));
    }
    Ok(value as u8)
}

pub(super) fn bounded_se_i8(reader: &mut BitstreamReader<'_>, bounded: bool) -> Result<i8> {
    let value = reader.read_se()?;
    if bounded && i8::try_from(value).is_err() {
        return Err(HevcError::InvalidBitstream("parameter narrowing overflow"));
    }
    Ok(value as i8)
}
