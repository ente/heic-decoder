use crate::heic_decoder::error::HevcError;

type Result<T> = core::result::Result<T, HevcError>;

static LPS_TABLE: [[u8; 4]; 64] = [
    [128, 176, 208, 240],
    [128, 167, 197, 227],
    [128, 158, 187, 216],
    [123, 150, 178, 205],
    [116, 142, 169, 195],
    [111, 135, 160, 185],
    [105, 128, 152, 175],
    [100, 122, 144, 166],
    [95, 116, 137, 158],
    [90, 110, 130, 150],
    [85, 104, 123, 142],
    [81, 99, 117, 135],
    [77, 94, 111, 128],
    [73, 89, 105, 122],
    [69, 85, 100, 116],
    [66, 80, 95, 110],
    [62, 76, 90, 104],
    [59, 72, 86, 99],
    [56, 69, 81, 94],
    [53, 65, 77, 89],
    [51, 62, 73, 85],
    [48, 59, 69, 80],
    [46, 56, 66, 76],
    [43, 53, 63, 72],
    [41, 50, 59, 69],
    [39, 48, 56, 65],
    [37, 45, 54, 62],
    [35, 43, 51, 59],
    [33, 41, 48, 56],
    [32, 39, 46, 53],
    [30, 37, 43, 50],
    [29, 35, 41, 48],
    [27, 33, 39, 45],
    [26, 31, 37, 43],
    [24, 30, 35, 41],
    [23, 28, 33, 39],
    [22, 27, 32, 37],
    [21, 26, 30, 35],
    [20, 24, 29, 33],
    [19, 23, 27, 31],
    [18, 22, 26, 30],
    [17, 21, 25, 28],
    [16, 20, 23, 27],
    [15, 19, 22, 25],
    [14, 18, 21, 24],
    [14, 17, 20, 23],
    [13, 16, 19, 22],
    [12, 15, 18, 21],
    [12, 14, 17, 20],
    [11, 14, 16, 19],
    [11, 13, 15, 18],
    [10, 12, 15, 17],
    [10, 12, 14, 16],
    [9, 11, 13, 15],
    [9, 11, 12, 14],
    [8, 10, 12, 14],
    [8, 9, 11, 13],
    [7, 9, 11, 12],
    [7, 9, 10, 12],
    [7, 8, 10, 11],
    [6, 8, 9, 11],
    [6, 7, 9, 10],
    [6, 7, 8, 9],
    [2, 2, 2, 2],
];

const STATE_TRANS_MPS: [u8; 64] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50,
    51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 62, 63,
];

const STATE_TRANS_LPS: [u8; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12, 13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21,
    21, 22, 22, 23, 24, 24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33, 33, 33, 34,
    34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

const fn build_packed_mps_transitions() -> [u8; 128] {
    let mut transitions = [0; 128];
    let mut packed = 0;
    while packed < transitions.len() {
        let state = packed >> 1;
        let mps = packed & 1;
        transitions[packed] = (STATE_TRANS_MPS[state] << 1) | mps as u8;
        packed += 1;
    }
    transitions
}

const fn build_packed_lps_transitions() -> [u8; 128] {
    let mut transitions = [0; 128];
    let mut packed = 0;
    while packed < transitions.len() {
        let state = packed >> 1;
        let mut mps = packed & 1;
        if state == 0 {
            mps ^= 1;
        }
        transitions[packed] = (STATE_TRANS_LPS[state] << 1) | mps as u8;
        packed += 1;
    }
    transitions
}

const PACKED_STATE_TRANS_MPS: [u8; 128] = build_packed_mps_transitions();
const PACKED_STATE_TRANS_LPS: [u8; 128] = build_packed_lps_transitions();

#[derive(Clone, Copy)]
pub struct ContextModel {
    state_mps: u8,
}

#[allow(dead_code)]
impl ContextModel {
    pub fn new(init_value: u8) -> Self {
        let slope = (init_value >> 4) as i32 * 5 - 45;
        let offset = ((init_value & 15) << 3) as i32 - 16;
        let qp = 26;

        let init_state = ((slope * (qp - 16)) >> 4) + offset;
        let init_state = init_state.clamp(1, 126);

        let (state, mps) = if init_state >= 64 {
            ((init_state - 64) as u8, 1)
        } else {
            ((63 - init_state) as u8, 0)
        };

        Self {
            state_mps: (state << 1) | mps,
        }
    }

    pub fn get_state(&self) -> (u8, u8) {
        (self.state_mps >> 1, self.state_mps & 1)
    }

    pub fn init(&mut self, init_value: u8, slice_qp: i32) {
        let m = (init_value >> 4) as i32 * 5 - 45;
        let n = ((init_value & 15) << 3) as i32 - 16;

        let init_state = ((m * slice_qp.clamp(0, 51)) >> 4) + n;
        let init_state = init_state.clamp(1, 126);

        if init_state >= 64 {
            self.state_mps = ((init_state - 64) as u8) << 1 | 1;
        } else {
            self.state_mps = ((63 - init_state) as u8) << 1;
        }
    }
}

pub struct CabacDecoder<'a> {
    data: &'a [u8],
    stream: Option<StreamInput<'a>>,
    byte_pos: usize,
    range: u32,
    value: u32,
    bits_needed: i32,
}

#[allow(dead_code)]
impl<'a> CabacDecoder<'a> {
    pub(crate) fn new_stream(reader: &'a mut dyn std::io::Read) -> Result<Self> {
        let mut decoder = Self::new(&[0, 0])?;
        decoder.stream = Some(StreamInput {
            reader,
            buffer: super::allocation::filled(0, 16384)?,
            position: 0,
            length: 0,
            failed: false,
        });
        decoder.byte_pos = 0;
        decoder.reinit();
        decoder.check_input()?;
        if decoder.byte_pos < 2 {
            return Err(HevcError::CabacError("data too short"));
        }
        Ok(decoder)
    }

    pub(crate) fn check_input(&self) -> Result<()> {
        if self.stream.as_ref().is_some_and(|input| input.failed) {
            Err(HevcError::DecodingError("stream input failed"))
        } else {
            Ok(())
        }
    }

    #[inline]
    fn next_byte(&mut self) -> Option<u8> {
        let byte = if let Some(input) = &mut self.stream {
            if input.position == input.length {
                match input.reader.read(&mut input.buffer) {
                    Ok(length) => {
                        input.position = 0;
                        input.length = length;
                    }
                    Err(_) => {
                        input.failed = true;
                        return None;
                    }
                }
            }
            if input.position == input.length {
                return None;
            }
            let byte = input.buffer[input.position];
            input.position += 1;
            byte
        } else {
            *self.data.get(self.byte_pos)?
        };
        self.byte_pos += 1;
        Some(byte)
    }

    pub fn get_state(&self) -> (u16, u16) {
        (self.range as u16, (self.value >> 7) as u16)
    }

    pub fn get_state_extended(&self) -> (u32, u32, i32) {
        (self.range, self.value, self.bits_needed)
    }

    pub fn get_position(&self) -> (usize, usize, u32) {
        (self.byte_pos, self.data.len(), self.byte_pos as u32 * 8)
    }

    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.len() < 2 {
            return Err(HevcError::CabacError("data too short"));
        }

        let mut decoder = Self {
            data,
            stream: None,
            byte_pos: 0,
            range: 510,
            value: 0,
            bits_needed: 8,
        };

        decoder.bits_needed = -8;
        if let Some(byte) = decoder.next_byte() {
            decoder.value = u32::from(byte);
        }
        decoder.value <<= 8;
        decoder.bits_needed = 0;
        if let Some(byte) = decoder.next_byte() {
            decoder.value |= u32::from(byte);
            decoder.bits_needed = -8;
        }

        Ok(decoder)
    }

    pub fn reinit(&mut self) {
        self.range = 510;
        // With fewer than two bytes left, the legacy per-bit refill consumed
        // the available high byte and then normalized bits_needed to -8 on
        // the first decoded bit. Represent that pending first-bit transition
        // directly as -9 so batched renormalization remains exactly
        // equivalent without a special case in its hot path.
        self.bits_needed = -9;
        self.value = 0;

        if let Some(byte) = self.next_byte() {
            self.value = u32::from(byte) << 8;
        }
        if let Some(byte) = self.next_byte() {
            self.value |= u32::from(byte);
            self.bits_needed = -8;
        }
    }

    pub fn seek_to(&mut self, byte_pos: usize) -> Result<()> {
        if self.stream.is_some() {
            return Err(HevcError::Unsupported("streaming CABAC seek"));
        }
        if byte_pos > self.data.len() {
            return Err(HevcError::CabacError(
                "entry point offset beyond slice data",
            ));
        }
        self.byte_pos = byte_pos;
        self.reinit();
        Ok(())
    }

    #[inline]
    pub fn decode_bin(&mut self, ctx: &mut ContextModel) -> u8 {
        let packed_state = ctx.state_mps as usize;
        let state = packed_state >> 1;
        let mps = (packed_state & 1) as u8;
        let q_range_idx = (self.range >> 6) & 3;
        let lps_range = LPS_TABLE[state][q_range_idx as usize] as u32;

        self.range -= lps_range;

        let scaled_range = self.range << 7;

        let bin_val;
        if self.value < scaled_range {
            bin_val = mps;
            ctx.state_mps = PACKED_STATE_TRANS_MPS[packed_state];
        } else {
            bin_val = mps ^ 1;
            self.value -= scaled_range;
            self.range = lps_range;
            ctx.state_mps = PACKED_STATE_TRANS_LPS[packed_state];
        }

        self.renormalize();

        bin_val
    }

    #[inline]
    pub fn decode_bypass(&mut self) -> u8 {
        self.value <<= 1;
        self.bits_needed += 1;

        if self.bits_needed >= 0 {
            if let Some(byte) = self.next_byte() {
                self.bits_needed = -8;
                self.value |= u32::from(byte);
            } else {
                self.bits_needed = -8;
            }
        }

        let scaled_range = self.range << 7;
        if self.value >= scaled_range {
            self.value -= scaled_range;
            1
        } else {
            0
        }
    }

    #[inline]
    pub fn decode_bypass_bits(&mut self, n: u8) -> u32 {
        let mut result = 0u32;
        for _ in 0..n {
            result = (result << 1) | self.decode_bypass() as u32;
        }
        result
    }

    pub fn decode_egk_bypass(&mut self, k: u8) -> Result<u32> {
        let mut base = 0u32;
        let mut n = k;
        loop {
            let bit = self.decode_bypass();
            if bit == 0 {
                break;
            }
            base += 1 << n;
            n += 1;
            if n >= k + 32 {
                return Err(HevcError::InvalidBitstream("EGk prefix too long"));
            }
        }
        let suffix = self.decode_bypass_bits(n);
        Ok(base + suffix)
    }

    pub fn decode_terminate(&mut self) -> u8 {
        self.range -= 2;

        let scaled_range = self.range << 7;
        if self.value >= scaled_range {
            1
        } else {
            self.renormalize();
            0
        }
    }

    #[inline]
    fn renormalize(&mut self) {
        if self.range < 256 {
            // `range` is non-zero and at most seven shifts are needed. Since
            // `bits_needed` starts in -9..=-1, the batch crosses at most one
            // byte boundary. (`-9` is the canonical short-reinit state and
            // cannot cross within a seven-bit batch.) When a normal state
            // crosses, place the new byte exactly where the remaining
            // single-bit shifts would have moved it.
            let shift = self.range.leading_zeros() - 23;
            self.range <<= shift;
            self.value <<= shift;
            self.bits_needed += shift as i32;
            if self.bits_needed >= 0 {
                if let Some(byte) = self.next_byte() {
                    self.value |= u32::from(byte) << self.bits_needed;
                }
                self.bits_needed -= 8;
            }
        }
        debug_assert!(self.range >= 256, "range {} < 256 after renorm", self.range);
    }

    pub fn decode_eg(&mut self, k: u8) -> Result<u32> {
        let mut n = 0;
        while self.decode_bypass() != 0 {
            n += 1;
            if n > 31 {
                return Err(HevcError::CabacError("exp-golomb overflow"));
            }
        }

        let mut value = 0u32;
        for _ in 0..(n + k) {
            value = (value << 1) | self.decode_bypass() as u32;
        }

        Ok((1 << n) - 1 + value)
    }
}

#[allow(dead_code)]
pub mod context {
    pub const SPLIT_CU_FLAG: usize = 0;
    pub const CU_TRANSQUANT_BYPASS_FLAG: usize = 3;
    pub const CU_SKIP_FLAG: usize = 4;
    pub const PALETTE_MODE_FLAG: usize = 7;
    pub const PRED_MODE_FLAG: usize = 8;
    pub const PART_MODE: usize = 9;
    pub const PREV_INTRA_LUMA_PRED_FLAG: usize = 13;
    pub const INTRA_CHROMA_PRED_MODE: usize = 14;
    pub const INTER_PRED_IDC: usize = 15;
    pub const MERGE_FLAG: usize = 20;
    pub const MERGE_IDX: usize = 21;
    pub const MVP_LX_FLAG: usize = 22;
    pub const REF_IDX: usize = 23;
    pub const ABS_MVD_GREATER0_FLAG: usize = 25;
    pub const ABS_MVD_GREATER1_FLAG: usize = 27;
    pub const SPLIT_TRANSFORM_FLAG: usize = 28;
    pub const CBF_LUMA: usize = 31;
    pub const CBF_CBCR: usize = 33;
    pub const TRANSFORM_SKIP_FLAG: usize = 38;
    pub const LAST_SIG_COEFF_X_PREFIX: usize = 40;
    pub const LAST_SIG_COEFF_Y_PREFIX: usize = 58;
    pub const CODED_SUB_BLOCK_FLAG: usize = 76;
    pub const SIG_COEFF_FLAG: usize = 80;
    pub const COEFF_ABS_LEVEL_GREATER1_FLAG: usize = 124;
    pub const COEFF_ABS_LEVEL_GREATER2_FLAG: usize = 148;
    pub const SAO_MERGE_FLAG: usize = 154;
    pub const SAO_TYPE_IDX: usize = 155;
    pub const CU_QP_DELTA_ABS: usize = 156;
    pub const CU_CHROMA_QP_OFFSET_FLAG: usize = 158;
    pub const CU_CHROMA_QP_OFFSET_IDX: usize = 159;
    pub const LOG2_RES_SCALE_ABS_PLUS1: usize = 160;
    pub const RES_SCALE_SIGN_FLAG: usize = 168;
    pub const NUM_CONTEXTS: usize = 170;
}

pub static INIT_VALUES: [u8; context::NUM_CONTEXTS] = [
    139, 141, 157, 154, 197, 185, 201, 154, 149, 184, 154, 139, 154, 184, 63, 95, 79, 63, 31, 31,
    110, 122, 168, 153, 153, 140, 198, 140, 153, 138, 138, 111, 141, 94, 138, 182, 154, 154, 139,
    139, 110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63,
    110, 110, 124, 125, 140, 153, 125, 127, 140, 109, 111, 143, 127, 111, 79, 108, 123, 63, 91,
    171, 134, 141, 111, 111, 125, 110, 110, 94, 124, 108, 124, 107, 125, 141, 179, 153, 125, 107,
    125, 141, 179, 153, 125, 107, 125, 141, 179, 153, 125, 140, 139, 182, 182, 152, 136, 152, 136,
    153, 136, 139, 111, 136, 139, 111, 155, 154, 140, 92, 137, 138, 140, 152, 138, 139, 153, 74,
    149, 92, 139, 107, 122, 152, 140, 179, 166, 182, 140, 227, 122, 197, 138, 153, 136, 167, 152,
    152, 153, 200, 154, 154, 154, 154, 154, 154, 154, 154, 154, 154, 154, 154, 154, 154,
];

struct StreamInput<'a> {
    reader: &'a mut dyn std::io::Read,
    buffer: alloc::vec::Vec<u8>,
    position: usize,
    length: usize,
    failed: bool,
}

#[cfg(test)]
mod tests {
    use super::CabacDecoder;

    fn legacy_reinit(decoder: &mut CabacDecoder<'_>, byte_pos: usize) {
        decoder.byte_pos = byte_pos;
        decoder.range = 510;
        decoder.bits_needed = 8;
        decoder.value = 0;

        let remaining = decoder.data.len() - decoder.byte_pos;
        if remaining > 0 {
            decoder.value = (decoder.data[decoder.byte_pos] as u32) << 8;
            decoder.byte_pos += 1;
            decoder.bits_needed -= 8;
        }
        if remaining > 1 {
            decoder.value |= decoder.data[decoder.byte_pos] as u32;
            decoder.byte_pos += 1;
            decoder.bits_needed -= 8;
        }
    }

    fn legacy_renormalize(decoder: &mut CabacDecoder<'_>) {
        while decoder.range < 256 {
            decoder.range <<= 1;
            decoder.value <<= 1;
            decoder.bits_needed += 1;
            if decoder.bits_needed >= 0 {
                decoder.bits_needed = -8;
                if decoder.byte_pos < decoder.data.len() {
                    decoder.value |= decoder.data[decoder.byte_pos] as u32;
                    decoder.byte_pos += 1;
                }
            }
        }
    }

    #[test]
    fn batched_renormalization_matches_legacy_after_short_reinit() {
        let data = [0x12, 0x34, 0x56, 0x78];

        for remaining in 0..=2 {
            let byte_pos = data.len() - remaining;
            for range in [128, 64, 32, 16, 8, 4, 2] {
                let mut batched = CabacDecoder::new(&data).expect("CABAC test data");
                batched.seek_to(byte_pos).expect("valid byte offset");
                batched.range = range;

                let mut legacy = CabacDecoder::new(&data).expect("CABAC test data");
                legacy_reinit(&mut legacy, byte_pos);
                legacy.range = range;

                batched.renormalize();
                legacy_renormalize(&mut legacy);

                assert_eq!(
                    (
                        batched.range,
                        batched.value,
                        batched.bits_needed,
                        batched.byte_pos,
                    ),
                    (
                        legacy.range,
                        legacy.value,
                        legacy.bits_needed,
                        legacy.byte_pos,
                    ),
                    "remaining={remaining}, range={range}"
                );
            }
        }
    }
}
