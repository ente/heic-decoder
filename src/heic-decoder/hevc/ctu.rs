use alloc::vec;
use alloc::vec::Vec;
#[cfg(feature = "decoder-tracing")]
use core::sync::atomic::{AtomicU32, Ordering};

const DEBUG_TRACE: bool = cfg!(feature = "decoder-tracing");

macro_rules! debug_trace {
    ($($arg:tt)*) => {{
        #[cfg(feature = "std")]
        if DEBUG_TRACE {
            eprintln!($($arg)*);
        }
        #[cfg(not(feature = "std"))]
        let _ = format_args!($($arg)*);
    }};
}

use super::cabac::{CabacDecoder, ContextModel, INIT_VALUES, context};
use super::debug;
use super::intra;
use super::params::{Pps, Sps};
use super::picture::DecodedFrame;
use super::residual::{self, ScanOrder};
use super::sao::SaoMap;
use super::slice::{IntraPredMode, PartMode, PredMode, SliceHeader};
use super::transform;
#[cfg(target_arch = "aarch64")]
use super::transform_simd::add_residual_block_neon;
use super::transform_simd::add_residual_block_scalar;
#[cfg(target_arch = "x86_64")]
use super::transform_simd::add_residual_block_v3;
use crate::heic_decoder::error::HevcError;
use archmage::incant;

type Result<T> = core::result::Result<T, HevcError>;

#[cfg(feature = "decoder-tracing")]
pub static SE_COUNTER: AtomicU32 = AtomicU32::new(0);
#[cfg(feature = "decoder-tracing")]
pub const SE_TRACE_LIMIT: u32 = 0;

#[cfg(feature = "decoder-tracing")]
#[allow(clippy::absurd_extreme_comparisons)]
fn se_trace(name: &str, val: i64, cabac: &CabacDecoder) {
    let num = SE_COUNTER.fetch_add(1, Ordering::Relaxed);
    if num < SE_TRACE_LIMIT {
        #[cfg(feature = "std")]
        {
            let (range, _, _) = cabac.get_state_extended();
            let (byte_pos, _, _) = cabac.get_position();
            eprintln!(
                "SE#{} {} val={} range={} byte={}",
                num, name, val, range, byte_pos
            );
        }
    }
    let _ = (name, val, cabac);
}

#[cfg(not(feature = "decoder-tracing"))]
#[inline(always)]
fn se_trace(_name: &str, _val: i64, _cabac: &CabacDecoder) {}

fn chroma_qp_mapping(qp_i: i32) -> i32 {
    static CHROMA_QP_TABLE: [i32; 58] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26, 27, 28, 29, 29, 30, 31, 32, 33, 33, 34, 34, 35, 35, 36, 36, 37, 37, 38, 39, 40, 41,
        42, 43, 44, 45, 46, 47, 48, 49, 50, 51,
    ];
    CHROMA_QP_TABLE[qp_i.clamp(0, 57) as usize]
}

pub struct SliceContext<'a> {
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub header: &'a SliceHeader,
    pub cabac: CabacDecoder<'a>,
    pub ctx: [ContextModel; context::NUM_CONTEXTS],
    pub ctb_x: u32,
    pub ctb_y: u32,
    pub qp_y: i32,
    pub qp_cb: i32,
    pub qp_cr: i32,
    pub is_cu_qp_delta_coded: bool,
    pub cu_qp_delta: i32,
    pub cu_transquant_bypass_flag: bool,
    debug_ctu: bool,
    #[allow(dead_code)]
    chroma_pred_count: u32,
    ct_depth_map: Vec<u8>,
    ct_depth_map_stride: u32,
    intra_mode_map: Vec<u8>,
    intra_mode_map_stride: u32,
    intra_chroma_mode_map: Vec<u8>,
    cu_base_x: u32,
    cu_base_y: u32,
    cu_log2_size: u8,
    qp_map: Vec<i8>,
    qp_map_stride: u32,
    current_qpy: i32,
    last_qpy_in_prev_qg: i32,
    current_qg_x: i32,
    current_qg_y: i32,
    pub sao_map: SaoMap,
    /// The inverse transform writes every element, so no re-zeroing is needed.
    residual_buf: [i16; 1024],
    /// Reusable sparse coefficient workspace. Only indices recorded in
    /// `touched_coeffs` may be non-zero between residual decodes.
    coeff_buf: [i16; 1024],
    touched_coeffs: [u16; 1024],
    scaling_buf: [u8; 1024],
    map_origin: u32,
}

impl<'a> SliceContext<'a> {
    pub fn new(
        sps: &'a Sps,
        pps: &'a Pps,
        header: &'a SliceHeader,
        slice_data: &'a [u8],
    ) -> Result<Self> {
        debug_trace!(
            "DEBUG: Slice data first 16 bytes: {:02x?}",
            &slice_data[..16.min(slice_data.len())]
        );
        Self::new_with_height(sps, pps, header, slice_data, sps.pic_height_in_luma_samples)
    }

    pub(crate) fn new_with_height(
        sps: &'a Sps,
        pps: &'a Pps,
        header: &'a SliceHeader,
        slice_data: &'a [u8],
        height: u32,
    ) -> Result<Self> {
        Self::with_cabac(sps, pps, header, CabacDecoder::new(slice_data)?, height)
    }

    pub(crate) fn new_stream(
        sps: &'a Sps,
        pps: &'a Pps,
        header: &'a SliceHeader,
        input: &'a mut dyn std::io::Read,
        height: u32,
    ) -> Result<Self> {
        Self::with_cabac(sps, pps, header, CabacDecoder::new_stream(input)?, height)
    }

    fn with_cabac(
        sps: &'a Sps,
        pps: &'a Pps,
        header: &'a SliceHeader,
        cabac: CabacDecoder<'a>,
        height: u32,
    ) -> Result<Self> {
        debug_trace!(
            "DEBUG: SPS: {}x{}, ctb_size={}, min_cb_size={}, scaling_list={}",
            sps.pic_width_in_luma_samples,
            sps.pic_height_in_luma_samples,
            sps.ctb_size(),
            1 << sps.log2_min_cb_size(),
            sps.scaling_list_enabled_flag
        );
        debug_trace!(
            "DEBUG: SPS: max_transform_hierarchy_depth_intra={}",
            sps.max_transform_hierarchy_depth_intra
        );
        debug_trace!(
            "DEBUG: SPS: log2_min_tb={}, log2_max_tb={}",
            sps.log2_min_tb_size(),
            sps.log2_max_tb_size()
        );

        let (range, offset) = cabac.get_state();
        debug_trace!(
            "DEBUG: CABAC init state: range={}, offset={}",
            range,
            offset
        );

        let mut ctx = [ContextModel::new(154); context::NUM_CONTEXTS];
        let slice_qp = header.slice_qp_y;

        for (i, init_val) in INIT_VALUES.iter().enumerate() {
            ctx[i].init(*init_val, slice_qp);
        }

        let qp_i_cb = slice_qp + pps.pps_cb_qp_offset as i32 + header.slice_cb_qp_offset as i32;
        let qp_i_cr = slice_qp + pps.pps_cr_qp_offset as i32 + header.slice_cr_qp_offset as i32;

        let (qp_cb, qp_cr) = if sps.chroma_format_idc == 1 {
            (
                chroma_qp_mapping(qp_i_cb.clamp(0, 57)),
                chroma_qp_mapping(qp_i_cr.clamp(0, 57)),
            )
        } else {
            (qp_i_cb.clamp(0, 51), qp_i_cr.clamp(0, 51))
        };

        debug_trace!(
            "DEBUG: Chroma QP: qp_y={}, qp_cb={}, qp_cr={}",
            slice_qp,
            qp_cb,
            qp_cr
        );
        debug_trace!(
            "DEBUG: sign_data_hiding_enabled_flag={}",
            pps.sign_data_hiding_enabled_flag
        );
        debug_trace!(
            "DEBUG: tiles_enabled={} entropy_coding_sync={}",
            pps.tiles_enabled_flag,
            pps.entropy_coding_sync_enabled_flag
        );
        let min_cb_size = 1u32 << sps.log2_min_cb_size();
        let ct_depth_map_stride = sps.pic_width_in_luma_samples.div_ceil(min_cb_size);
        let ct_depth_map_height = height.div_ceil(min_cb_size);
        let ct_map_size = (ct_depth_map_stride * ct_depth_map_height) as usize;
        let ct_depth_map = super::allocation::filled(0xFF, ct_map_size)?;

        let min_pu_size = (min_cb_size / 2).max(1);
        let intra_mode_map_stride = sps.pic_width_in_luma_samples.div_ceil(min_pu_size);
        let intra_mode_map_height = height.div_ceil(min_pu_size);
        let pu_map_size = (intra_mode_map_stride * intra_mode_map_height) as usize;
        let intra_mode_map = super::allocation::filled(IntraPredMode::Dc.as_u8(), pu_map_size)?;
        let intra_chroma_mode_map =
            super::allocation::filled(IntraPredMode::Dc.as_u8(), pu_map_size)?;

        let min_tb_size = 1u32 << sps.log2_min_tb_size();
        let qp_map_stride = sps.pic_width_in_luma_samples.div_ceil(min_tb_size);
        let qp_map_height = height.div_ceil(min_tb_size);
        let qp_map =
            super::allocation::filled(slice_qp as i8, (qp_map_stride * qp_map_height) as usize)?;

        Ok(Self {
            sps,
            pps,
            header,
            cabac,
            ctx,
            ctb_x: 0,
            ctb_y: 0,
            qp_y: slice_qp,
            qp_cb,
            qp_cr,
            is_cu_qp_delta_coded: false,
            cu_qp_delta: 0,
            cu_transquant_bypass_flag: false,
            debug_ctu: false,
            chroma_pred_count: 0,
            ct_depth_map,
            ct_depth_map_stride,
            intra_mode_map,
            intra_mode_map_stride,
            intra_chroma_mode_map,
            cu_base_x: 0,
            cu_base_y: 0,
            cu_log2_size: 0,
            qp_map,
            qp_map_stride,
            current_qpy: slice_qp,
            last_qpy_in_prev_qg: slice_qp,
            current_qg_x: -1,
            current_qg_y: -1,
            sao_map: SaoMap::new(
                sps.pic_width_in_ctbs(),
                if height < sps.pic_height_in_luma_samples {
                    3
                } else {
                    sps.pic_height_in_ctbs()
                },
            )?,
            residual_buf: [0i16; 1024],
            coeff_buf: [0i16; 1024],
            touched_coeffs: [0u16; 1024],
            scaling_buf: [16u8; 1024],
            map_origin: 0,
        })
    }

    pub fn decode_slice(&mut self, frame: &mut DecodedFrame) -> Result<()> {
        #[cfg(feature = "decoder-tracing")]
        debug::init_tracker();

        let ctb_size = self.sps.ctb_size();
        let pic_width_in_ctbs = self.sps.pic_width_in_ctbs();
        let pic_height_in_ctbs = self.sps.pic_height_in_ctbs();
        let wpp = self.pps.entropy_coding_sync_enabled_flag;

        let start_addr = self.header.slice_segment_address;
        let start_ctb_y = start_addr / pic_width_in_ctbs;
        self.ctb_y = start_ctb_y;
        self.ctb_x = start_addr % pic_width_in_ctbs;

        let mut ctu_count = 0u32;
        let total_ctus = pic_width_in_ctbs * pic_height_in_ctbs;

        let mut wpp_saved_ctx: Option<[super::cabac::ContextModel; context::NUM_CONTEXTS]> = None;

        loop {
            if wpp
                && self.ctb_x == 0
                && self.ctb_y > 0
                && pic_width_in_ctbs > 1
                && let Some(saved) = wpp_saved_ctx
            {
                self.ctx = saved;
            }

            let x_ctb = self.ctb_x * ctb_size;
            let y_ctb = self.ctb_y * ctb_size;

            #[cfg(feature = "decoder-tracing")]
            {
                let (byte_pos, _, _) = self.cabac.get_position();
                debug::track_ctu_start(ctu_count, byte_pos);

                if ctu_count.is_multiple_of(50) || ctu_count <= 3 {
                    let (range, offset) = self.cabac.get_state();
                    debug_trace!(
                        "DEBUG: CTU {} byte={} cabac=({},{}) x={} y={}",
                        ctu_count,
                        byte_pos,
                        range,
                        offset,
                        self.ctb_x,
                        self.ctb_y
                    );
                }
            }
            self.debug_ctu = ctu_count == 1;

            self.decode_ctu(x_ctb, y_ctb, frame)?;
            ctu_count += 1;

            if wpp && self.ctb_x == 1 && self.ctb_y < pic_height_in_ctbs - 1 {
                wpp_saved_ctx = Some(self.ctx);
            }

            let end_of_slice = self.cabac.decode_terminate();
            se_trace("end_of_slice", end_of_slice as i64, &self.cabac);
            if end_of_slice != 0 {
                debug_trace!(
                    "DEBUG: end_of_slice after CTU {}, decoded {}/{} CTUs",
                    ctu_count,
                    ctu_count,
                    total_ctus
                );
                break;
            }

            let prev_ctb_y = self.ctb_y;

            self.ctb_x += 1;
            if self.ctb_x >= pic_width_in_ctbs {
                self.ctb_x = 0;
                self.ctb_y += 1;
            }

            if wpp && self.ctb_y != prev_ctb_y {
                let _eoss = self.cabac.decode_terminate();
                let entry_point_index = self.ctb_y.saturating_sub(start_ctb_y + 1) as usize;
                if let Some(&offset) = self.header.entry_point_offsets.get(entry_point_index) {
                    self.cabac.seek_to(offset as usize)?;
                } else {
                    self.cabac.reinit();
                }
                // H.265 9.3.1: when the picture is one CTB wide, the sync CTB
                // (top-right) is unavailable, so context variables are fully
                // re-initialized at each row start (no snapshot to restore).
                if pic_width_in_ctbs == 1 {
                    self.reinit_context_models();
                }
            }

            if self.ctb_y >= pic_height_in_ctbs {
                break;
            }
        }

        #[cfg(feature = "decoder-tracing")]
        if DEBUG_TRACE {
            debug::print_tracker_summary();
        }
        if ctu_count < total_ctus {
            return Err(HevcError::InvalidBitstream(
                "slice ended before the last CTU",
            ));
        }
        Ok(())
    }

    pub(crate) fn decode_row(&mut self, row: u32, frame: &mut DecodedFrame) -> Result<()> {
        let size = self.sps.ctb_size();
        let origin = row.saturating_sub(1) * size;
        let delta = origin - self.map_origin;
        let cb = 1 << self.sps.log2_min_cb_size();
        let pu = cb / 2;
        let tb = 1 << self.sps.log2_min_tb_size();
        super::picture::shift_rows(
            &mut self.ct_depth_map,
            (delta / cb * self.ct_depth_map_stride) as usize,
            0xFF,
        );
        super::picture::shift_rows(
            &mut self.intra_mode_map,
            (delta / pu * self.intra_mode_map_stride) as usize,
            IntraPredMode::Dc.as_u8(),
        );
        super::picture::shift_rows(
            &mut self.intra_chroma_mode_map,
            (delta / pu * self.intra_mode_map_stride) as usize,
            IntraPredMode::Dc.as_u8(),
        );
        super::picture::shift_rows(
            &mut self.qp_map,
            (delta / tb * self.qp_map_stride) as usize,
            self.header.slice_qp_y as i8,
        );
        self.map_origin = origin;
        frame.advance_rows(origin);
        self.ctb_y = row;
        for x in 0..self.sps.pic_width_in_ctbs() {
            self.ctb_x = x;
            *self.sao_map.get_mut(x, row) = super::sao::SaoInfo::default();
            self.decode_ctu(x * size, row * size, frame)?;
            let end = self.cabac.decode_terminate() != 0;
            self.cabac.check_input()?;
            let last =
                row + 1 == self.sps.pic_height_in_ctbs() && x + 1 == self.sps.pic_width_in_ctbs();
            if end != last {
                return Err(HevcError::InvalidBitstream("incremental slice termination"));
            }
        }
        Ok(())
    }

    fn decode_ctu(&mut self, x_ctb: u32, y_ctb: u32, frame: &mut DecodedFrame) -> Result<()> {
        let log2_ctb_size = self.sps.log2_ctb_size();

        if self.pps.cu_qp_delta_enabled_flag {
            self.is_cu_qp_delta_coded = false;
            self.cu_qp_delta = 0;
        }

        if self.header.slice_sao_luma_flag || self.header.slice_sao_chroma_flag {
            self.decode_sao(x_ctb, y_ctb)?;
        }

        self.decode_coding_quadtree(x_ctb, y_ctb, log2_ctb_size, 0, frame)
    }

    fn decode_sao(&mut self, x_ctb_pixels: u32, y_ctb_pixels: u32) -> Result<()> {
        let ctb_size = self.sps.ctb_size();
        let x_ctb = x_ctb_pixels / ctb_size;
        let y_ctb = y_ctb_pixels / ctb_size;

        let mut sao_merge_left_flag = false;
        let mut sao_merge_up_flag = false;

        if x_ctb > 0 {
            let pic_width_ctbs = self.sps.pic_width_in_ctbs();
            let ctb_addr_rs = y_ctb * pic_width_ctbs + x_ctb;
            let slice_addr_rs = 0u32;
            let left_in_slice = ctb_addr_rs > slice_addr_rs;
            if left_in_slice {
                let ctx_idx = context::SAO_MERGE_FLAG;
                sao_merge_left_flag = self.cabac.decode_bin(&mut self.ctx[ctx_idx]) != 0;
                se_trace("sao_merge_left", sao_merge_left_flag as i64, &self.cabac);
            }
        }

        if y_ctb > 0 && !sao_merge_left_flag {
            let pic_width_ctbs = self.sps.pic_width_in_ctbs();
            let ctb_addr_rs = y_ctb * pic_width_ctbs + x_ctb;
            let slice_addr_rs = 0u32;
            let up_in_slice = ctb_addr_rs >= pic_width_ctbs + slice_addr_rs;
            if up_in_slice {
                let ctx_idx = context::SAO_MERGE_FLAG;
                sao_merge_up_flag = self.cabac.decode_bin(&mut self.ctx[ctx_idx]) != 0;
                se_trace("sao_merge_up", sao_merge_up_flag as i64, &self.cabac);
            }
        }

        let sao_info = if sao_merge_left_flag {
            *self.sao_map.get(x_ctb - 1, y_ctb)
        } else if sao_merge_up_flag {
            *self.sao_map.get(x_ctb, y_ctb - 1)
        } else {
            let mut info = super::sao::SaoInfo::default();
            let is_mono = self.sps.chroma_format_idc == 0;
            let n_chroma = if is_mono { 1 } else { 3 };

            #[allow(unused_assignments)]
            let mut sao_type_idx_luma = 0u8;
            let mut sao_type_idx_chroma = 0u8;
            let mut eo_class_chroma = 0u8;

            for c_idx in 0..n_chroma {
                let should_decode = (self.header.slice_sao_luma_flag && c_idx == 0)
                    || (self.header.slice_sao_chroma_flag && c_idx > 0);

                if !should_decode {
                    continue;
                }

                let sao_type_idx = if c_idx == 0 {
                    sao_type_idx_luma = self.decode_sao_type_idx()?;
                    se_trace("sao_type_idx_luma", sao_type_idx_luma as i64, &self.cabac);
                    sao_type_idx_luma
                } else if c_idx == 1 {
                    sao_type_idx_chroma = self.decode_sao_type_idx()?;
                    se_trace(
                        "sao_type_idx_chroma",
                        sao_type_idx_chroma as i64,
                        &self.cabac,
                    );
                    sao_type_idx_chroma
                } else {
                    sao_type_idx_chroma
                };

                info.sao_type_idx[c_idx] = sao_type_idx;

                if sao_type_idx != 0 {
                    let bit_depth = if c_idx == 0 {
                        self.sps.bit_depth_y() as u32
                    } else {
                        self.sps.bit_depth_c() as u32
                    };
                    let c_max = (1u32 << (bit_depth.min(10) - 5)) - 1;
                    let log2_offset_scale = if c_idx == 0 {
                        self.pps.log2_sao_offset_scale_luma
                    } else {
                        self.pps.log2_sao_offset_scale_chroma
                    };
                    let offset_scale = 1i32 << log2_offset_scale;

                    let mut offsets_abs = [0u32; 4];
                    for elem in &mut offsets_abs {
                        *elem = self.decode_cabac_tu_bypass(c_max)?;
                        se_trace("sao_offset_abs", *elem as i64, &self.cabac);
                    }

                    if sao_type_idx == 1 {
                        let mut signed_offsets = [0i8; 4];
                        for i in 0..4 {
                            if offsets_abs[i] != 0 {
                                let sign = self.cabac.decode_bypass();
                                se_trace("sao_offset_sign", sign as i64, &self.cabac);
                                let val = (offsets_abs[i] as i32 * offset_scale) as i8;
                                signed_offsets[i] = if sign != 0 { -val } else { val };
                            }
                        }
                        info.sao_offset_val[c_idx] = signed_offsets;

                        let band_pos = self.cabac.decode_bypass_bits(5);
                        se_trace("sao_band_position", band_pos as i64, &self.cabac);
                        info.sao_band_position[c_idx] = band_pos as u8;
                    } else {
                        for (i, &offset) in offsets_abs.iter().enumerate() {
                            info.sao_offset_val[c_idx][i] = (offset as i32 * offset_scale) as i8;
                        }

                        if c_idx <= 1 {
                            let eo_class = self.cabac.decode_bypass_bits(2);
                            se_trace("sao_eo_class", eo_class as i64, &self.cabac);
                            if c_idx == 0 {
                                info.sao_eo_class[0] = eo_class as u8;
                            } else {
                                eo_class_chroma = eo_class as u8;
                                info.sao_eo_class[1] = eo_class_chroma;
                            }
                        } else {
                            info.sao_eo_class[2] = eo_class_chroma;
                        }
                    }
                }
            }
            info
        };

        *self.sao_map.get_mut(x_ctb, y_ctb) = sao_info;
        Ok(())
    }

    fn decode_sao_type_idx(&mut self) -> Result<u8> {
        let ctx_idx = context::SAO_TYPE_IDX;
        let bit0 = self.cabac.decode_bin(&mut self.ctx[ctx_idx]);
        if bit0 == 0 {
            Ok(0)
        } else {
            let bit1 = self.cabac.decode_bypass();
            if bit1 == 0 { Ok(1) } else { Ok(2) }
        }
    }

    fn decode_cabac_tu_bypass(&mut self, c_max: u32) -> Result<u32> {
        for i in 0..c_max {
            let bit = self.cabac.decode_bypass();
            if bit == 0 {
                return Ok(i);
            }
        }
        Ok(c_max)
    }

    fn decode_coding_quadtree(
        &mut self,
        x0: u32,
        y0: u32,
        log2_cb_size: u8,
        ct_depth: u8,
        frame: &mut DecodedFrame,
    ) -> Result<()> {
        let cb_size = 1u32 << log2_cb_size;
        let pic_width = self.sps.pic_width_in_luma_samples;
        let pic_height = self.sps.pic_height_in_luma_samples;
        let log2_min_cb_size = self.sps.log2_min_cb_size();

        let split_flag = if x0 + cb_size <= pic_width
            && y0 + cb_size <= pic_height
            && log2_cb_size > log2_min_cb_size
        {
            let flag = self.decode_split_cu_flag(x0, y0, ct_depth)?;
            if self.debug_ctu {
                let (r, o) = self.cabac.get_state();
                debug_trace!(
                    "  CTU37: split_cu_flag at ({},{}) depth={} log2={} → {} (r={},o={})",
                    x0,
                    y0,
                    ct_depth,
                    log2_cb_size,
                    flag,
                    r,
                    o
                );
            }
            flag
        } else if log2_cb_size > log2_min_cb_size {
            if self.debug_ctu {
                debug_trace!(
                    "  CTU37: forced split at ({},{}) depth={} - outside picture",
                    x0,
                    y0,
                    ct_depth
                );
            }
            true
        } else {
            if self.debug_ctu {
                debug_trace!(
                    "  CTU37: no split at ({},{}) depth={} - min size",
                    x0,
                    y0,
                    ct_depth
                );
            }
            false
        };

        if self.pps.cu_qp_delta_enabled_flag
            && log2_cb_size >= self.sps.log2_ctb_size() - self.pps.diff_cu_qp_delta_depth
        {
            self.is_cu_qp_delta_coded = false;
            self.cu_qp_delta = 0;
        }

        if split_flag {
            let half = cb_size / 2;
            let x1 = x0 + half;
            let y1 = y0 + half;

            self.decode_coding_quadtree(x0, y0, log2_cb_size - 1, ct_depth + 1, frame)?;

            if x1 < pic_width {
                self.decode_coding_quadtree(x1, y0, log2_cb_size - 1, ct_depth + 1, frame)?;
            }

            if y1 < pic_height {
                self.decode_coding_quadtree(x0, y1, log2_cb_size - 1, ct_depth + 1, frame)?;
            }

            if x1 < pic_width && y1 < pic_height {
                self.decode_coding_quadtree(x1, y1, log2_cb_size - 1, ct_depth + 1, frame)?;
            }
        } else {
            self.decode_coding_unit(x0, y0, log2_cb_size, ct_depth, frame)?;
        }

        Ok(())
    }

    fn get_ct_depth(&self, x: u32, y: u32) -> u8 {
        let min_cb_size = 1u32 << self.sps.log2_min_cb_size();
        let map_x = x / min_cb_size;
        let Some(y) = y.checked_sub(self.map_origin) else {
            return 0xFF;
        };
        let map_y = y / min_cb_size;

        if map_x >= self.ct_depth_map_stride
            || map_y * self.ct_depth_map_stride + map_x >= self.ct_depth_map.len() as u32
        {
            return 0xFF;
        }

        self.ct_depth_map[(map_y * self.ct_depth_map_stride + map_x) as usize]
    }

    fn set_ct_depth(&mut self, x0: u32, y0: u32, log2_cb_size: u8, ct_depth: u8) {
        let min_cb_size = 1u32 << self.sps.log2_min_cb_size();
        let cb_size = 1u32 << log2_cb_size;

        let start_x = x0 / min_cb_size;
        let start_y = (y0 - self.map_origin) / min_cb_size;
        let num_blocks = cb_size / min_cb_size;

        for dy in 0..num_blocks {
            for dx in 0..num_blocks {
                let map_x = start_x + dx;
                let map_y = start_y + dy;
                if map_x < self.ct_depth_map_stride {
                    let idx = (map_y * self.ct_depth_map_stride + map_x) as usize;
                    if idx < self.ct_depth_map.len() {
                        self.ct_depth_map[idx] = ct_depth;
                    }
                }
            }
        }
    }

    fn is_neighbor_available(&self, x: i32, y: i32) -> bool {
        x >= 0
            && y >= 0
            && (x as u32) < self.sps.pic_width_in_luma_samples
            && (y as u32) < self.sps.pic_height_in_luma_samples
    }

    fn decode_split_cu_flag(&mut self, x0: u32, y0: u32, ct_depth: u8) -> Result<bool> {
        let available_l = self.is_neighbor_available(x0 as i32 - 1, y0 as i32);
        let available_a = self.is_neighbor_available(x0 as i32, y0 as i32 - 1);

        let mut cond_l = 0;
        let mut cond_a = 0;

        if available_l {
            let depth_l = self.get_ct_depth(x0 - 1, y0);
            if depth_l != 0xFF && depth_l > ct_depth {
                cond_l = 1;
            }
        }

        if available_a {
            let depth_a = self.get_ct_depth(x0, y0 - 1);
            if depth_a != 0xFF && depth_a > ct_depth {
                cond_a = 1;
            }
        }

        let ctx_idx = context::SPLIT_CU_FLAG + cond_l + cond_a;
        let bin = self.cabac.decode_bin(&mut self.ctx[ctx_idx]);
        se_trace("split_cu_flag", bin as i64, &self.cabac);

        Ok(bin != 0)
    }

    fn decode_coding_unit(
        &mut self,
        x0: u32,
        y0: u32,
        log2_cb_size: u8,
        ct_depth: u8,
        frame: &mut DecodedFrame,
    ) -> Result<()> {
        let cb_size = 1u32 << log2_cb_size;
        let _ = cb_size;

        self.cu_base_x = x0;
        self.cu_base_y = y0;
        self.cu_log2_size = log2_cb_size;

        self.decode_quantization_parameters(x0, y0, x0, y0);
        self.store_qpy(x0, y0, log2_cb_size, self.current_qpy);

        self.set_ct_depth(x0, y0, log2_cb_size, ct_depth);

        let pred_mode = PredMode::Intra;

        self.cu_transquant_bypass_flag = if self.pps.transquant_bypass_enabled_flag {
            let ctx_idx = context::CU_TRANSQUANT_BYPASS_FLAG;
            self.cabac.decode_bin(&mut self.ctx[ctx_idx]) != 0
        } else {
            false
        };

        let part_mode = if log2_cb_size == self.sps.log2_min_cb_size() {
            let pm = self.decode_part_mode(pred_mode, log2_cb_size)?;
            #[cfg(feature = "decoder-tracing")]
            if pm == PartMode::PartNxN {
                static NXN_COUNT: core::sync::atomic::AtomicU32 =
                    core::sync::atomic::AtomicU32::new(0);
                let count = NXN_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                if count == 0 || x0 < 64 && y0 < 64 {
                    let (r, o) = self.cabac.get_state();
                    debug_trace!(
                        "DEBUG: part_mode at ({},{}) log2={}: {:?} cabac=({},{})",
                        x0,
                        y0,
                        log2_cb_size,
                        pm,
                        r,
                        o
                    );
                }
            }
            if self.debug_ctu {
                let (r, o) = self.cabac.get_state();
                debug_trace!(
                    "  CTU37: CU at ({},{}) log2={} part_mode={:?} (r={},o={})",
                    x0,
                    y0,
                    log2_cb_size,
                    pm,
                    r,
                    o
                );
            }
            pm
        } else {
            if self.debug_ctu {
                debug_trace!(
                    "  CTU37: CU at ({},{}) log2={} part_mode=2Nx2N (implicit)",
                    x0,
                    y0,
                    log2_cb_size
                );
            }
            PartMode::Part2Nx2N
        };

        let (intra_luma_mode, intra_chroma_mode) = match part_mode {
            PartMode::Part2Nx2N => {
                let modes = self.decode_intra_prediction(x0, y0, log2_cb_size, true, frame)?;
                if self.debug_ctu {
                    let (r, o) = self.cabac.get_state();
                    debug_trace!(
                        "  CTU37: After intra_prediction at (1144,120): mode={:?} (r={},o={}) bits={}",
                        modes,
                        r,
                        o,
                        self.cabac.get_position().2
                    );
                }
                modes
            }
            PartMode::PartNxN => {
                let half = cb_size / 2;
                let log2_pu_size = log2_cb_size - 1;

                // Per H.265 spec 7.3.8.5:
                // FIRST pass: decode ALL prev_intra_luma_pred_flag (context-coded bins)
                let prev_flags = [
                    self.decode_prev_intra_luma_pred_flag()?,
                    self.decode_prev_intra_luma_pred_flag()?,
                    self.decode_prev_intra_luma_pred_flag()?,
                    self.decode_prev_intra_luma_pred_flag()?,
                ];

                // SECOND pass: decode mpm_idx/rem (bypass bins) and store IMMEDIATELY
                // per libde265: each PU mode stored before next PU's neighbor lookup
                let luma_mode_0 = self.derive_intra_luma_mode(x0, y0, prev_flags[0])?;
                self.store_intra_mode(x0, y0, log2_pu_size, luma_mode_0);

                let luma_mode_1 = self.derive_intra_luma_mode(x0 + half, y0, prev_flags[1])?;
                self.store_intra_mode(x0 + half, y0, log2_pu_size, luma_mode_1);

                let luma_mode_2 = self.derive_intra_luma_mode(x0, y0 + half, prev_flags[2])?;
                self.store_intra_mode(x0, y0 + half, log2_pu_size, luma_mode_2);

                let luma_mode_3 =
                    self.derive_intra_luma_mode(x0 + half, y0 + half, prev_flags[3])?;
                self.store_intra_mode(x0 + half, y0 + half, log2_pu_size, luma_mode_3);

                let chroma_mode =
                    if self.sps.chroma_format_idc == 3 && !self.sps.separate_colour_plane_flag {
                        let c0 = self.decode_intra_chroma_mode(luma_mode_0)?;
                        self.store_intra_chroma_mode(x0, y0, log2_pu_size, c0);
                        let c1 = self.decode_intra_chroma_mode(luma_mode_1)?;
                        self.store_intra_chroma_mode(x0 + half, y0, log2_pu_size, c1);
                        let c2 = self.decode_intra_chroma_mode(luma_mode_2)?;
                        self.store_intra_chroma_mode(x0, y0 + half, log2_pu_size, c2);
                        let c3 = self.decode_intra_chroma_mode(luma_mode_3)?;
                        self.store_intra_chroma_mode(x0 + half, y0 + half, log2_pu_size, c3);
                        c0
                    } else {
                        let chroma_mode = self.decode_intra_chroma_mode(luma_mode_0)?;
                        self.store_intra_chroma_mode(x0, y0, log2_cb_size, chroma_mode);
                        chroma_mode
                    };

                (luma_mode_0, chroma_mode)
            }
            _ => {
                return Err(HevcError::InvalidBitstream("invalid intra partition mode"));
            }
        };

        if self.cu_transquant_bypass_flag {
            frame.store_block_bypass(x0, y0, cb_size);
        }
        let intra_split_flag = part_mode == PartMode::PartNxN;
        self.decode_transform_tree(
            x0,
            y0,
            log2_cb_size,
            0,
            intra_luma_mode,
            intra_chroma_mode,
            intra_split_flag,
            frame,
        )?;

        if self.debug_ctu {
            let (r, o) = self.cabac.get_state();
            debug_trace!(
                "  CTU37: After transform_tree at ({},{}) log2={} (r={},o={})",
                x0,
                y0,
                log2_cb_size,
                r,
                o
            );
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_transform_tree(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        trafo_depth: u8,
        intra_luma_mode: IntraPredMode,
        intra_chroma_mode: IntraPredMode,
        intra_split_flag: bool,
        frame: &mut DecodedFrame,
    ) -> Result<()> {
        self.decode_transform_tree_inner(
            x0,
            y0,
            log2_size,
            trafo_depth,
            intra_luma_mode,
            intra_chroma_mode,
            intra_split_flag,
            1,
            1,
            frame,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_transform_tree_inner(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        trafo_depth: u8,
        intra_luma_mode: IntraPredMode,
        intra_chroma_mode: IntraPredMode,
        intra_split_flag: bool,
        cbf_cb_parent: u8,
        cbf_cr_parent: u8,
        frame: &mut DecodedFrame,
    ) -> Result<()> {
        let max_trafo_depth =
            self.sps.max_transform_hierarchy_depth_intra + if intra_split_flag { 1 } else { 0 };
        let log2_min_trafo_size = self.sps.log2_min_tb_size();
        let log2_max_trafo_size = self.sps.log2_max_tb_size();

        let debug_tt = self.debug_ctu;

        let split_transform = if log2_size <= log2_max_trafo_size
            && log2_size > log2_min_trafo_size
            && trafo_depth < max_trafo_depth
            && !(intra_split_flag && trafo_depth == 0)
        {
            let ctx_idx = context::SPLIT_TRANSFORM_FLAG + (5 - log2_size as usize).min(2);
            let flag = self.cabac.decode_bin(&mut self.ctx[ctx_idx]) != 0;
            se_trace("split_transform", flag as i64, &self.cabac);
            flag
        } else if log2_size > log2_max_trafo_size || (intra_split_flag && trafo_depth == 0) {
            true
        } else {
            if debug_tt {
                debug_trace!(
                    "    TT(1144,120): no split (log2={} min={} max={} depth={} maxdepth={})",
                    log2_size,
                    log2_min_trafo_size,
                    log2_max_trafo_size,
                    trafo_depth,
                    max_trafo_depth
                );
            }
            false
        };

        let chroma_format = frame.chroma_format;
        let has_chroma = chroma_format != 0;
        let decode_chroma_cbf = (log2_size > 2 && has_chroma) || chroma_format == 3;
        let (cbf_cb, cbf_cr) = if decode_chroma_cbf {
            let decode_extra_422_cbf = chroma_format == 2 && (!split_transform || log2_size == 3);

            let cb = if cbf_cb_parent != 0 {
                let ctx_idx = context::CBF_CBCR + trafo_depth as usize;
                let mut val = self.cabac.decode_bin(&mut self.ctx[ctx_idx]);
                se_trace("cbf_cb", val as i64, &self.cabac);
                if decode_extra_422_cbf {
                    let extra = self.cabac.decode_bin(&mut self.ctx[ctx_idx]);
                    se_trace("cbf_cb", extra as i64, &self.cabac);
                    val |= extra << 1;
                }
                val
            } else {
                0
            };
            let cr = if cbf_cr_parent != 0 {
                let ctx_idx = context::CBF_CBCR + trafo_depth as usize;
                let mut val = self.cabac.decode_bin(&mut self.ctx[ctx_idx]);
                se_trace("cbf_cr", val as i64, &self.cabac);
                if decode_extra_422_cbf {
                    let extra = self.cabac.decode_bin(&mut self.ctx[ctx_idx]);
                    se_trace("cbf_cr", extra as i64, &self.cabac);
                    val |= extra << 1;
                }
                val
            } else {
                0
            };
            (cb, cr)
        } else if trafo_depth > 0 && log2_size == 2 {
            (cbf_cb_parent, cbf_cr_parent)
        } else {
            (0, 0)
        };

        if split_transform {
            let half = 1u32 << (log2_size - 1);
            let new_depth = trafo_depth + 1;
            let new_log2_size = log2_size - 1;

            self.decode_transform_tree_inner(
                x0,
                y0,
                new_log2_size,
                new_depth,
                intra_luma_mode,
                intra_chroma_mode,
                intra_split_flag,
                cbf_cb,
                cbf_cr,
                frame,
            )?;
            self.decode_transform_tree_inner(
                x0 + half,
                y0,
                new_log2_size,
                new_depth,
                intra_luma_mode,
                intra_chroma_mode,
                intra_split_flag,
                cbf_cb,
                cbf_cr,
                frame,
            )?;
            self.decode_transform_tree_inner(
                x0,
                y0 + half,
                new_log2_size,
                new_depth,
                intra_luma_mode,
                intra_chroma_mode,
                intra_split_flag,
                cbf_cb,
                cbf_cr,
                frame,
            )?;
            self.decode_transform_tree_inner(
                x0 + half,
                y0 + half,
                new_log2_size,
                new_depth,
                intra_luma_mode,
                intra_chroma_mode,
                intra_split_flag,
                cbf_cb,
                cbf_cr,
                frame,
            )?;

            if log2_size == 3 && matches!(frame.chroma_format, 1 | 2) {
                self.decode_split_subsampled_chroma(
                    x0,
                    y0,
                    intra_chroma_mode,
                    cbf_cb,
                    cbf_cr,
                    frame,
                )?;
            }
        } else {
            self.decode_transform_unit_leaf(
                x0,
                y0,
                log2_size,
                trafo_depth,
                intra_luma_mode,
                intra_chroma_mode,
                cbf_cb,
                cbf_cr,
                frame,
            )?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_transform_unit_leaf(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        trafo_depth: u8,
        _intra_luma_mode: IntraPredMode,
        intra_chroma_mode: IntraPredMode,
        cbf_cb: u8,
        cbf_cr: u8,
        frame: &mut DecodedFrame,
    ) -> Result<()> {
        let debug_tt = self.debug_ctu;

        let ctx_offset = if trafo_depth == 0 { 1 } else { 0 };
        let ctx_idx = context::CBF_LUMA + ctx_offset;
        let cbf_luma = self.cabac.decode_bin(&mut self.ctx[ctx_idx]) != 0;
        se_trace("cbf_luma", cbf_luma as i64, &self.cabac);

        if (cbf_luma || cbf_cb != 0 || cbf_cr != 0)
            && self.pps.cu_qp_delta_enabled_flag
            && !self.is_cu_qp_delta_coded
        {
            let cu_qp_delta_abs = self.decode_cu_qp_delta_abs()?;
            let cu_qp_delta_sign = if cu_qp_delta_abs != 0 {
                self.cabac.decode_bypass()
            } else {
                0
            };
            self.is_cu_qp_delta_coded = true;
            self.cu_qp_delta = cu_qp_delta_abs as i32 * (1 - 2 * cu_qp_delta_sign as i32);
            se_trace("cu_qp_delta", self.cu_qp_delta as i64, &self.cabac);

            let cu_x = self.cu_base_x;
            let cu_y = self.cu_base_y;
            let cu_log2 = self.cu_log2_size;
            self.decode_quantization_parameters(x0, y0, cu_x, cu_y);
            self.store_qpy(cu_x, cu_y, cu_log2, self.current_qpy);
            // Mirror libde265 behavior: once cu_qp_delta is decoded, QPY applies
            // to the whole CU. Re-stamp the deblock QP map so TU leaves decoded
            // earlier in the same CU do not retain stale pre-delta QP values.
            frame.store_block_qp(cu_x, cu_y, 1u32 << cu_log2, self.current_qpy as i8);
        }

        let tu_size = 1u32 << log2_size;
        frame.mark_tu_boundary(x0, y0, tu_size);
        frame.store_block_qp(x0, y0, tu_size, self.current_qpy as i8);

        let actual_luma_mode = self.get_intra_mode_at(x0, y0);
        let sis = self.sps.strong_intra_smoothing_enabled_flag;

        intra::predict_intra(frame, x0, y0, log2_size, actual_luma_mode, 0, sis);

        let scan_order = residual::get_scan_order(
            log2_size,
            actual_luma_mode.as_u8(),
            0,
            self.sps.chroma_format_idc,
        );

        if cbf_luma {
            if debug_tt {
                let (r, o) = self.cabac.get_state();
                debug_trace!(
                    "    TT: decoding luma residual at ({},{}) log2={} (r={},o={})",
                    x0,
                    y0,
                    log2_size,
                    r,
                    o
                );
            }
            self.decode_and_apply_residual(x0, y0, log2_size, 0, scan_order, frame)?;
        }

        self.decode_leaf_chroma(x0, y0, log2_size, intra_chroma_mode, cbf_cb, cbf_cr, frame)?;

        Ok(())
    }

    fn decode_split_subsampled_chroma(
        &mut self,
        x0: u32,
        y0: u32,
        intra_chroma_mode: IntraPredMode,
        cbf_cb: u8,
        cbf_cr: u8,
        frame: &mut DecodedFrame,
    ) -> Result<()> {
        match frame.chroma_format {
            1 => {
                let cx = x0 / 2;
                let cy = y0 / 2;
                self.decode_chroma_block(cx, cy, 2, 1, cbf_cb & 1 != 0, intra_chroma_mode, frame)?;
                self.decode_chroma_block(cx, cy, 2, 2, cbf_cr & 1 != 0, intra_chroma_mode, frame)?;
            }
            2 => {
                let cx = x0 / 2;
                let cy = y0;
                self.decode_chroma_block(cx, cy, 2, 1, cbf_cb & 1 != 0, intra_chroma_mode, frame)?;
                self.decode_chroma_block(
                    cx,
                    cy + 4,
                    2,
                    1,
                    cbf_cb & 2 != 0,
                    intra_chroma_mode,
                    frame,
                )?;
                self.decode_chroma_block(cx, cy, 2, 2, cbf_cr & 1 != 0, intra_chroma_mode, frame)?;
                self.decode_chroma_block(
                    cx,
                    cy + 4,
                    2,
                    2,
                    cbf_cr & 2 != 0,
                    intra_chroma_mode,
                    frame,
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_leaf_chroma(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        intra_chroma_mode: IntraPredMode,
        cbf_cb: u8,
        cbf_cr: u8,
        frame: &mut DecodedFrame,
    ) -> Result<()> {
        match frame.chroma_format {
            1 if log2_size >= 3 => {
                let chroma_log2_size = log2_size - 1;
                let cx = x0 / 2;
                let cy = y0 / 2;
                self.decode_chroma_block(
                    cx,
                    cy,
                    chroma_log2_size,
                    1,
                    cbf_cb & 1 != 0,
                    intra_chroma_mode,
                    frame,
                )?;
                self.decode_chroma_block(
                    cx,
                    cy,
                    chroma_log2_size,
                    2,
                    cbf_cr & 1 != 0,
                    intra_chroma_mode,
                    frame,
                )?;
            }
            2 if log2_size >= 3 => {
                let chroma_log2_size = (log2_size - 1).max(2);
                let chroma_size = 1u32 << chroma_log2_size;
                let cx = x0 / 2;
                let cy = y0;
                self.decode_chroma_block(
                    cx,
                    cy,
                    chroma_log2_size,
                    1,
                    cbf_cb & 1 != 0,
                    intra_chroma_mode,
                    frame,
                )?;
                self.decode_chroma_block(
                    cx,
                    cy + chroma_size,
                    chroma_log2_size,
                    1,
                    cbf_cb & 2 != 0,
                    intra_chroma_mode,
                    frame,
                )?;
                self.decode_chroma_block(
                    cx,
                    cy,
                    chroma_log2_size,
                    2,
                    cbf_cr & 1 != 0,
                    intra_chroma_mode,
                    frame,
                )?;
                self.decode_chroma_block(
                    cx,
                    cy + chroma_size,
                    chroma_log2_size,
                    2,
                    cbf_cr & 2 != 0,
                    intra_chroma_mode,
                    frame,
                )?;
            }
            3 => {
                let mode = self.get_intra_chroma_mode_at(x0, y0);
                self.decode_chroma_block(x0, y0, log2_size, 1, cbf_cb & 1 != 0, mode, frame)?;
                self.decode_chroma_block(x0, y0, log2_size, 2, cbf_cr & 1 != 0, mode, frame)?;
            }
            _ => {}
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_chroma_block(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        c_idx: u8,
        cbf: bool,
        intra_chroma_mode: IntraPredMode,
        frame: &mut DecodedFrame,
    ) -> Result<()> {
        let sis = self.sps.strong_intra_smoothing_enabled_flag;
        let scan_order = residual::get_scan_order(
            log2_size,
            intra_chroma_mode.as_u8(),
            c_idx,
            self.sps.chroma_format_idc,
        );
        intra::predict_intra(frame, x0, y0, log2_size, intra_chroma_mode, c_idx, sis);
        if cbf {
            self.decode_and_apply_residual(x0, y0, log2_size, c_idx, scan_order, frame)?;
        }
        Ok(())
    }

    fn decode_cu_qp_delta_abs(&mut self) -> Result<u32> {
        let first_bin = self
            .cabac
            .decode_bin(&mut self.ctx[context::CU_QP_DELTA_ABS]);
        if first_bin == 0 {
            return Ok(0);
        }
        let mut prefix = 1u32;
        for _ in 0..4 {
            let bin = self
                .cabac
                .decode_bin(&mut self.ctx[context::CU_QP_DELTA_ABS + 1]);
            if bin == 0 {
                break;
            }
            prefix += 1;
        }
        if prefix == 5 {
            let suffix = self.cabac.decode_egk_bypass(0)?;
            Ok(suffix + 5)
        } else {
            Ok(prefix)
        }
    }

    fn decode_and_apply_residual(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        c_idx: u8,
        scan_order: ScanOrder,
        frame: &mut DecodedFrame,
    ) -> Result<()> {
        let (num_nonzero, transform_skip) = residual::decode_residual(
            &mut self.cabac,
            &mut self.ctx,
            log2_size,
            c_idx,
            scan_order,
            self.pps.sign_data_hiding_enabled_flag,
            self.cu_transquant_bypass_flag,
            self.pps.transform_skip_enabled_flag,
            x0,
            y0,
            &mut self.coeff_buf,
            &mut self.touched_coeffs,
        )?;

        if num_nonzero == 0 {
            return Ok(());
        }

        let size = 1usize << log2_size;
        let num_coeffs = size * size;

        let (qp, bit_depth) = match c_idx {
            0 => (self.qp_y, self.sps.bit_depth_y()),
            1 => (self.qp_cb, self.sps.bit_depth_c()),
            2 => (self.qp_cr, self.sps.bit_depth_c()),
            _ => (self.qp_y, self.sps.bit_depth_y()),
        };

        if self.cu_transquant_bypass_flag {
            let residual = &mut self.residual_buf;
            residual[..num_coeffs].copy_from_slice(&self.coeff_buf[..num_coeffs]);
            for &idx in &self.touched_coeffs[..num_nonzero] {
                self.coeff_buf[idx as usize] = 0;
            }
            self.add_residual_to_plane(x0, y0, log2_size, c_idx, bit_depth, frame);
            return Ok(());
        }

        let coeffs = &mut self.coeff_buf;

        let dequant_params = transform::DequantParams {
            qp,
            bit_depth,
            log2_tr_size: log2_size,
        };

        // m[x][y] is flat only when scaling lists are off or when
        // transform_skip is used with nTbS > 4; 4x4 transform-skip blocks
        // still use the scaling factors.
        let scaling_list =
            if self.sps.scaling_list_enabled_flag && !(transform_skip && log2_size > 2) {
                self.pps
                    .pps_scaling_list
                    .as_ref()
                    .or(self.sps.scaling_list.as_ref())
            } else {
                None
            };

        if let Some(sl) = scaling_list {
            let matrix_id = c_idx;
            let scaling_matrix = &mut self.scaling_buf;
            for py in 0..size {
                for px in 0..size {
                    scaling_matrix[py * size + px] =
                        sl.get_scaling_factor(log2_size, matrix_id, px as u32, py as u32);
                }
            }
            transform::dequantize_scaled(
                &mut coeffs[..num_coeffs],
                dequant_params,
                &scaling_matrix[..num_coeffs],
            );
        } else {
            transform::dequantize(&mut coeffs[..num_coeffs], dequant_params);
        }

        let residual = &mut self.residual_buf;
        if transform_skip {
            let ts_shift = 5 + log2_size as i32;
            let bd_shift = (20 - bit_depth as i32).max(0);
            let rnd = if bd_shift > 0 {
                1i32 << (bd_shift - 1)
            } else {
                0
            };
            for i in 0..num_coeffs {
                let c = (coeffs[i] as i32) << ts_shift;
                residual[i] = ((c + rnd) >> bd_shift) as i16;
            }
        } else {
            let is_intra_4x4_luma = log2_size == 2 && c_idx == 0;
            transform::inverse_transform(coeffs, residual, size, bit_depth, is_intra_4x4_luma);
        }

        // Dequantization only changes non-zero inputs, so the dense list from
        // residual parsing is sufficient to restore the workspace invariant.
        for &idx in &self.touched_coeffs[..num_nonzero] {
            coeffs[idx as usize] = 0;
        }

        self.add_residual_to_plane(x0, y0, log2_size, c_idx, bit_depth, frame);

        Ok(())
    }

    fn add_residual_to_plane(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        c_idx: u8,
        bit_depth: u8,
        frame: &mut DecodedFrame,
    ) {
        let size = 1usize << log2_size;
        let residual = &self.residual_buf;
        let max_val = (1i32 << bit_depth) - 1;
        let y0 = y0 - frame.plane_origin(c_idx);
        let (plane, stride) = frame.plane_mut(c_idx);
        let last_row_end = (y0 as usize + size - 1) * stride + x0 as usize + size;
        if last_row_end <= plane.len() {
            incant!(
                add_residual_block(
                    plane,
                    stride,
                    x0 as usize,
                    y0 as usize,
                    residual,
                    size,
                    max_val
                ),
                [v3, neon, scalar]
            );
        } else {
            for py in 0..size {
                let row_start = (y0 as usize + py) * stride + x0 as usize;
                for px in 0..size {
                    let idx = row_start + px;
                    if idx < plane.len() {
                        let pred = plane[idx] as i32;
                        let r = residual[py * size + px] as i32;
                        plane[idx] = (pred + r).clamp(0, max_val) as u16;
                    }
                }
            }
        }
    }

    fn decode_part_mode(&mut self, pred_mode: PredMode, log2_cb_size: u8) -> Result<PartMode> {
        if pred_mode == PredMode::Intra {
            let ctx_idx = context::PART_MODE;
            let bin = self.cabac.decode_bin(&mut self.ctx[ctx_idx]);
            se_trace("part_mode", bin as i64, &self.cabac);

            if bin != 0 {
                Ok(PartMode::Part2Nx2N)
            } else if log2_cb_size == self.sps.log2_min_cb_size() {
                Ok(PartMode::PartNxN)
            } else {
                Err(HevcError::InvalidBitstream("NxN not allowed at this size"))
            }
        } else {
            Err(HevcError::Unsupported("inter partition modes"))
        }
    }

    fn decode_intra_prediction(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        _apply_chroma: bool,
        frame: &mut DecodedFrame,
    ) -> Result<(IntraPredMode, IntraPredMode)> {
        let (intra_luma_mode, intra_chroma_mode) =
            self.decode_intra_prediction_modes(x0, y0, log2_size, frame)?;

        self.store_intra_mode(x0, y0, log2_size, intra_luma_mode);
        self.store_intra_chroma_mode(x0, y0, log2_size, intra_chroma_mode);

        // NOTE: Prediction is NOT applied here. It happens in decode_transform_unit_leaf
        // and the 8x8→4x4 chroma split handler, so each TU is predicted →
        // reconstructed before the next TU reads its neighbors.

        Ok((intra_luma_mode, intra_chroma_mode))
    }

    fn reinit_context_models(&mut self) {
        let slice_qp = self.header.slice_qp_y;
        for (i, init_val) in INIT_VALUES.iter().enumerate() {
            self.ctx[i].init(*init_val, slice_qp);
        }
    }

    fn decode_prev_intra_luma_pred_flag(&mut self) -> Result<bool> {
        let ctx_idx = context::PREV_INTRA_LUMA_PRED_FLAG;
        let val = self.cabac.decode_bin(&mut self.ctx[ctx_idx]) != 0;
        se_trace("prev_intra_luma_pred", val as i64, &self.cabac);
        Ok(val)
    }

    fn derive_intra_luma_mode(
        &mut self,
        x0: u32,
        y0: u32,
        prev_flag: bool,
    ) -> Result<IntraPredMode> {
        let cand_a = self.get_neighbor_intra_mode_left(x0, y0);
        let cand_b = self.get_neighbor_intra_mode_above(x0, y0);
        let mpm = intra::fill_mpm_candidates(cand_a, cand_b);

        if prev_flag {
            let mpm_idx = self.decode_mpm_idx()?;
            Ok(mpm[mpm_idx as usize])
        } else {
            let rem = self.decode_rem_intra_luma_pred_mode()?;
            Ok(self.map_rem_mode_to_intra(rem, &mpm))
        }
    }

    fn decode_intra_luma_mode(&mut self, x0: u32, y0: u32) -> Result<IntraPredMode> {
        let prev_flag = self.decode_prev_intra_luma_pred_flag()?;
        self.derive_intra_luma_mode(x0, y0, prev_flag)
    }

    fn decode_intra_chroma_mode(&mut self, luma_mode: IntraPredMode) -> Result<IntraPredMode> {
        if self.sps.chroma_array_type() == 0 {
            return Ok(luma_mode);
        }
        let ctx_idx = context::INTRA_CHROMA_PRED_MODE;
        let first_bin = self.cabac.decode_bin(&mut self.ctx[ctx_idx]);
        let intra_chroma_mode = if first_bin == 0 {
            se_trace("intra_chroma_mode", 4, &self.cabac);
            luma_mode
        } else {
            let mode_idx = self.cabac.decode_bypass_bits(2) as u8;
            se_trace("intra_chroma_mode", mode_idx as i64, &self.cabac);

            let candidate = match mode_idx {
                0 => IntraPredMode::Planar,
                1 => IntraPredMode::Angular26,
                2 => IntraPredMode::Angular10,
                _ => IntraPredMode::Dc,
            };

            if candidate == luma_mode {
                IntraPredMode::Angular34
            } else {
                candidate
            }
        };

        Ok(self.map_chroma_mode_for_format(intra_chroma_mode))
    }

    fn map_chroma_mode_for_format(&self, mode: IntraPredMode) -> IntraPredMode {
        if self.sps.chroma_format_idc != 2 {
            return mode;
        }

        const MAP_CHROMA_422: [u8; 35] = [
            0, 1, 2, 2, 2, 2, 3, 5, 7, 8, 10, 12, 13, 15, 17, 18, 19, 20, 21, 22, 23, 23, 24, 24,
            25, 25, 26, 27, 27, 28, 28, 29, 29, 30, 31,
        ];
        let mapped = MAP_CHROMA_422[mode.as_u8() as usize];
        IntraPredMode::from_u8(mapped).unwrap_or(mode)
    }

    fn decode_intra_prediction_modes(
        &mut self,
        x0: u32,
        y0: u32,
        log2_size: u8,
        _frame: &DecodedFrame,
    ) -> Result<(IntraPredMode, IntraPredMode)> {
        let intra_luma_mode = self.decode_intra_luma_mode(x0, y0)?;

        if x0 < 16 && y0 < 16 {
            debug_trace!(
                "DEBUG: intra_mode at ({},{}) size={}: mode={:?}",
                x0,
                y0,
                1u32 << log2_size,
                intra_luma_mode
            );
        }

        let intra_chroma_mode = self.decode_intra_chroma_mode(intra_luma_mode)?;

        Ok((intra_luma_mode, intra_chroma_mode))
    }

    fn min_pu_size(&self) -> u32 {
        ((1u32 << self.sps.log2_min_cb_size()) / 2).max(1)
    }

    fn store_intra_mode(&mut self, x0: u32, y0: u32, log2_size: u8, mode: IntraPredMode) {
        let min_pu = self.min_pu_size();
        let stride = self.intra_mode_map_stride;
        let count = ((1u32 << log2_size) / min_pu).max(1);
        let start_x = x0 / min_pu;
        let start_y = (y0 - self.map_origin) / min_pu;
        for dy in 0..count {
            for dx in 0..count {
                let idx = ((start_y + dy) * stride + (start_x + dx)) as usize;
                if idx < self.intra_mode_map.len() {
                    self.intra_mode_map[idx] = mode.as_u8();
                }
            }
        }
    }

    fn store_intra_chroma_mode(&mut self, x0: u32, y0: u32, log2_size: u8, mode: IntraPredMode) {
        let min_pu = self.min_pu_size();
        let stride = self.intra_mode_map_stride;
        let count = ((1u32 << log2_size) / min_pu).max(1);
        let start_x = x0 / min_pu;
        let start_y = (y0 - self.map_origin) / min_pu;
        for dy in 0..count {
            for dx in 0..count {
                let idx = ((start_y + dy) * stride + (start_x + dx)) as usize;
                if idx < self.intra_chroma_mode_map.len() {
                    self.intra_chroma_mode_map[idx] = mode.as_u8();
                }
            }
        }
    }

    fn get_intra_mode_at(&self, x: u32, y: u32) -> IntraPredMode {
        let min_pu = self.min_pu_size();
        let stride = self.intra_mode_map_stride;
        let Some(y) = y.checked_sub(self.map_origin) else {
            return IntraPredMode::Dc;
        };
        let idx = ((y / min_pu) * stride + (x / min_pu)) as usize;
        if idx < self.intra_mode_map.len() {
            IntraPredMode::from_u8(self.intra_mode_map[idx]).unwrap_or(IntraPredMode::Dc)
        } else {
            IntraPredMode::Dc
        }
    }

    #[allow(dead_code)]
    fn get_intra_chroma_mode_at(&self, x: u32, y: u32) -> IntraPredMode {
        let min_pu = self.min_pu_size();
        let stride = self.intra_mode_map_stride;
        let Some(y) = y.checked_sub(self.map_origin) else {
            return IntraPredMode::Dc;
        };
        let idx = ((y / min_pu) * stride + (x / min_pu)) as usize;
        if idx < self.intra_chroma_mode_map.len() {
            IntraPredMode::from_u8(self.intra_chroma_mode_map[idx]).unwrap_or(IntraPredMode::Dc)
        } else {
            IntraPredMode::Dc
        }
    }

    fn get_neighbor_intra_mode_left(&self, x0: u32, y0: u32) -> IntraPredMode {
        if x0 == 0 {
            return IntraPredMode::Dc;
        }
        self.get_intra_mode_at(x0 - 1, y0)
    }

    fn get_neighbor_intra_mode_above(&self, x0: u32, y0: u32) -> IntraPredMode {
        if y0 == 0 {
            return IntraPredMode::Dc;
        }
        let ctb_size = self.sps.ctb_size();
        let ctb_y_start = (y0 / ctb_size) * ctb_size;
        if y0 - 1 < ctb_y_start {
            return IntraPredMode::Dc;
        }
        self.get_intra_mode_at(x0, y0 - 1)
    }

    fn map_rem_mode_to_intra(&self, rem: u32, mpm: &[IntraPredMode; 3]) -> IntraPredMode {
        let mut mpm_vals = [mpm[0].as_u8(), mpm[1].as_u8(), mpm[2].as_u8()];
        mpm_vals.sort_unstable();

        let mut mode = rem as u8;
        for &mpm_val in &mpm_vals {
            if mode >= mpm_val {
                mode += 1;
            }
        }

        IntraPredMode::from_u8(mode).unwrap_or(IntraPredMode::Dc)
    }

    fn decode_mpm_idx(&mut self) -> Result<u8> {
        let val = if self.cabac.decode_bypass() == 0 {
            0
        } else if self.cabac.decode_bypass() == 0 {
            1
        } else {
            2
        };
        se_trace("mpm_idx", val as i64, &self.cabac);
        Ok(val)
    }

    fn decode_rem_intra_luma_pred_mode(&mut self) -> Result<u32> {
        let mut val = 0u32;
        for _ in 0..5 {
            val = (val << 1) | self.cabac.decode_bypass() as u32;
        }
        se_trace("rem_intra_luma", val as i64, &self.cabac);
        Ok(val)
    }

    fn chroma_qp_from_luma(qpi: i32) -> i32 {
        static TAB8_22: [i32; 13] = [29, 30, 31, 32, 33, 33, 34, 34, 35, 35, 36, 36, 37];
        if qpi < 30 {
            qpi
        } else if qpi >= 43 {
            qpi - 6
        } else {
            TAB8_22[(qpi - 30) as usize]
        }
    }

    fn get_qpy_at(&self, x: u32, y: u32) -> i32 {
        let min_tb = 1u32 << self.sps.log2_min_tb_size();
        let Some(y) = y.checked_sub(self.map_origin) else {
            return self.header.slice_qp_y;
        };
        let idx = ((y / min_tb) * self.qp_map_stride + (x / min_tb)) as usize;
        if idx < self.qp_map.len() {
            self.qp_map[idx] as i32
        } else {
            self.header.slice_qp_y
        }
    }

    fn store_qpy(&mut self, x0: u32, y0: u32, log2_cb_size: u8, qpy: i32) {
        let min_tb = 1u32 << self.sps.log2_min_tb_size();
        let count = ((1u32 << log2_cb_size) / min_tb).max(1);
        let start_x = x0 / min_tb;
        let start_y = (y0 - self.map_origin) / min_tb;
        for dy in 0..count {
            for dx in 0..count {
                let idx = ((start_y + dy) * self.qp_map_stride + (start_x + dx)) as usize;
                if idx < self.qp_map.len() {
                    self.qp_map[idx] = qpy as i8;
                }
            }
        }
    }

    fn decode_quantization_parameters(
        &mut self,
        x0: u32,
        _y0: u32,
        x_cu_base: u32,
        y_cu_base: u32,
    ) {
        let log2_min_cu_qp_delta_size = self.sps.log2_ctb_size() - self.pps.diff_cu_qp_delta_depth;
        let qg_mask = (1u32 << log2_min_cu_qp_delta_size) - 1;

        let x_qg = (x_cu_base & !qg_mask) as i32;
        let y_qg = (y_cu_base & !qg_mask) as i32;

        if x_qg != self.current_qg_x || y_qg != self.current_qg_y {
            self.last_qpy_in_prev_qg = self.current_qpy;
            self.current_qg_x = x_qg;
            self.current_qg_y = y_qg;
        }

        let ctb_mask = ((1u32 << self.sps.log2_ctb_size()) - 1) as i32;
        let first_in_ctb_row = x_qg == 0 && (y_qg & ctb_mask) == 0;

        let first_ctb_in_slice = self.header.slice_segment_address;
        let slice_start_x = (first_ctb_in_slice % self.sps.pic_width_in_ctbs()) as i32
            * (1 << self.sps.log2_ctb_size());
        let slice_start_y = (first_ctb_in_slice / self.sps.pic_width_in_ctbs()) as i32
            * (1 << self.sps.log2_ctb_size());
        let first_qg_in_slice = slice_start_x == x_qg && slice_start_y == y_qg;

        let qp_y_pred = if first_qg_in_slice
            || (first_in_ctb_row && self.pps.entropy_coding_sync_enabled_flag)
        {
            self.header.slice_qp_y
        } else {
            self.last_qpy_in_prev_qg
        };

        let qp_y_a = if x_qg > 0 {
            let left_x = (x_qg - 1) as u32;
            let left_y = y_qg as u32;
            let ctb_size = self.sps.ctb_size();
            let our_ctb_x = x0 / ctb_size;
            let left_ctb_x = left_x / ctb_size;
            if our_ctb_x == left_ctb_x || (x_qg as u32) < ctb_size {
                if left_ctb_x == self.ctb_x {
                    self.get_qpy_at(left_x, left_y)
                } else {
                    qp_y_pred
                }
            } else {
                qp_y_pred
            }
        } else {
            qp_y_pred
        };

        let qp_y_b = if y_qg > 0 {
            let above_x = x_qg as u32;
            let above_y = (y_qg - 1) as u32;
            let ctb_size = self.sps.ctb_size();
            let above_ctb_y = above_y / ctb_size;
            if above_ctb_y == self.ctb_y {
                self.get_qpy_at(above_x, above_y)
            } else {
                qp_y_pred
            }
        } else {
            qp_y_pred
        };

        let qp_y_pred = (qp_y_a + qp_y_b + 1) >> 1;

        let qp_bd_offset_y = 6 * (self.sps.bit_depth_y() as i32 - 8);
        let qpy = ((qp_y_pred + self.cu_qp_delta + 52 + 2 * qp_bd_offset_y)
            % (52 + qp_bd_offset_y))
            - qp_bd_offset_y;

        self.qp_y = qpy + qp_bd_offset_y;
        if self.qp_y < 0 {
            self.qp_y = 0;
        }

        let qp_bd_offset_c = 6 * (self.sps.bit_depth_c() as i32 - 8);
        let qpi_cb =
            (qpy + self.pps.pps_cb_qp_offset as i32 + self.header.slice_cb_qp_offset as i32)
                .clamp(-qp_bd_offset_c, 57);
        let qpi_cr =
            (qpy + self.pps.pps_cr_qp_offset as i32 + self.header.slice_cr_qp_offset as i32)
                .clamp(-qp_bd_offset_c, 57);

        let (qp_cb_mapped, qp_cr_mapped) = if self.sps.chroma_format_idc == 1 {
            (
                Self::chroma_qp_from_luma(qpi_cb),
                Self::chroma_qp_from_luma(qpi_cr),
            )
        } else {
            (qpi_cb.min(51), qpi_cr.min(51))
        };
        self.qp_cb = qp_cb_mapped + qp_bd_offset_c;
        self.qp_cr = qp_cr_mapped + qp_bd_offset_c;

        self.current_qpy = qpy;
    }
}
