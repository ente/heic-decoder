use crate::heic_decoder::error::HevcError;
use alloc::vec::Vec;

type Result<T> = core::result::Result<T, HevcError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NalType {
    TrailN = 0,
    TrailR = 1,
    TsaN = 2,
    TsaR = 3,
    StsaN = 4,
    StsaR = 5,
    RadlN = 6,
    RadlR = 7,
    RaslN = 8,
    RaslR = 9,
    RsvVclN10 = 10,
    RsvVclR11 = 11,
    RsvVclN12 = 12,
    RsvVclR13 = 13,
    RsvVclN14 = 14,
    RsvVclR15 = 15,
    BlaNLp = 16,
    BlaWLp = 17,
    BlaWRadl = 18,
    IdrWRadl = 19,
    IdrNLp = 20,
    CraNut = 21,
    RsvIrap22 = 22,
    RsvIrap23 = 23,
    VpsNut = 32,
    SpsNut = 33,
    PpsNut = 34,
    AudNut = 35,
    EosNut = 36,
    EobNut = 37,
    FdNut = 38,
    PrefixSeiNut = 39,
    SuffixSeiNut = 40,
    Unknown = 255,
}

impl NalType {
    pub fn from_u8(val: u8) -> Self {
        match val {
            0 => Self::TrailN,
            1 => Self::TrailR,
            2 => Self::TsaN,
            3 => Self::TsaR,
            4 => Self::StsaN,
            5 => Self::StsaR,
            6 => Self::RadlN,
            7 => Self::RadlR,
            8 => Self::RaslN,
            9 => Self::RaslR,
            10 => Self::RsvVclN10,
            11 => Self::RsvVclR11,
            12 => Self::RsvVclN12,
            13 => Self::RsvVclR13,
            14 => Self::RsvVclN14,
            15 => Self::RsvVclR15,
            16 => Self::BlaNLp,
            17 => Self::BlaWLp,
            18 => Self::BlaWRadl,
            19 => Self::IdrWRadl,
            20 => Self::IdrNLp,
            21 => Self::CraNut,
            22 => Self::RsvIrap22,
            23 => Self::RsvIrap23,
            32 => Self::VpsNut,
            33 => Self::SpsNut,
            34 => Self::PpsNut,
            35 => Self::AudNut,
            36 => Self::EosNut,
            37 => Self::EobNut,
            38 => Self::FdNut,
            39 => Self::PrefixSeiNut,
            40 => Self::SuffixSeiNut,
            _ => Self::Unknown,
        }
    }

    pub fn is_slice(self) -> bool {
        matches!(
            self,
            Self::TrailN
                | Self::TrailR
                | Self::TsaN
                | Self::TsaR
                | Self::StsaN
                | Self::StsaR
                | Self::RadlN
                | Self::RadlR
                | Self::RaslN
                | Self::RaslR
                | Self::BlaNLp
                | Self::BlaWLp
                | Self::BlaWRadl
                | Self::IdrWRadl
                | Self::IdrNLp
                | Self::CraNut
        )
    }

    pub fn is_idr(self) -> bool {
        matches!(
            self,
            Self::IdrWRadl | Self::IdrNLp | Self::BlaNLp | Self::BlaWLp | Self::BlaWRadl
        )
    }

    #[allow(dead_code)]
    pub fn is_rasl(self) -> bool {
        matches!(self, Self::RaslN | Self::RaslR)
    }

    #[allow(dead_code)]
    pub fn is_radl(self) -> bool {
        matches!(self, Self::RadlN | Self::RadlR)
    }

    pub fn is_irap(self) -> bool {
        matches!(
            self,
            Self::BlaNLp
                | Self::BlaWLp
                | Self::BlaWRadl
                | Self::IdrWRadl
                | Self::IdrNLp
                | Self::CraNut
                | Self::RsvIrap22
                | Self::RsvIrap23
        )
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct NalUnit<'a> {
    pub nal_type: NalType,
    pub nuh_layer_id: u8,
    pub nuh_temporal_id_plus1: u8,
    pub payload: Vec<u8>,
    /// Positions of removed emulation prevention bytes, expressed as byte
    /// offsets into the transmitted payload (i.e. `raw_data[2..]`, before
    /// removal). Needed to convert slice-header entry point offsets, which
    /// count transmitted bytes, into offsets into `payload`.
    pub skipped_byte_positions: Vec<u32>,
    pub raw_data: &'a [u8],
}

pub fn parse_length_prefixed_ext(data: &[u8], length_size: usize) -> Result<Vec<NalUnit<'_>>> {
    let mut nals = Vec::new();
    let mut i = 0;

    while i + length_size <= data.len() {
        let nal_len = match length_size {
            1 => data[i] as usize,
            2 => u16::from_be_bytes([data[i], data[i + 1]]) as usize,
            3 => {
                ((data[i] as usize) << 16) | ((data[i + 1] as usize) << 8) | (data[i + 2] as usize)
            }
            4 => u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize,
            _ => return Err(HevcError::InvalidBitstream("unsupported length size")),
        };

        i += length_size;

        if i + nal_len > data.len() {
            return Err(HevcError::InvalidBitstream("NAL length exceeds data"));
        }

        let raw_data = &data[i..i + nal_len];
        if nal_len >= 2
            && let Ok(nal) = parse_nal_header(raw_data)
        {
            nals.push(nal);
        }

        i += nal_len;
    }

    Ok(nals)
}

pub fn parse_single_nal(data: &[u8]) -> Result<NalUnit<'_>> {
    parse_nal_header(data)
}

pub(crate) fn parse_single_nal_bounded(data: &[u8]) -> Result<NalUnit<'_>> {
    let mut nal = parse_nal_header(
        data.get(..2)
            .ok_or(HevcError::InvalidNalUnit("too short"))?,
    )?;
    let data_payload = &data[2..];
    let count = data_payload
        .windows(3)
        .filter(|window| *window == [0, 0, 3])
        .count();
    nal.payload
        .try_reserve_exact(data_payload.len())
        .map_err(|_| HevcError::DecodingError("allocation failed"))?;
    nal.skipped_byte_positions
        .try_reserve_exact(count)
        .map_err(|_| HevcError::DecodingError("allocation failed"))?;
    for (i, &byte) in data_payload.iter().enumerate() {
        if i >= 2 && byte == 3 && data_payload[i - 1] == 0 && data_payload[i - 2] == 0 {
            nal.skipped_byte_positions.push(i as u32);
        } else {
            nal.payload.push(byte);
        }
    }
    nal.raw_data = data;
    Ok(nal)
}

fn parse_nal_header(raw_data: &[u8]) -> Result<NalUnit<'_>> {
    if raw_data.len() < 2 {
        return Err(HevcError::InvalidNalUnit("too short"));
    }

    if (raw_data[0] & 0x80) != 0 {
        return Err(HevcError::InvalidNalUnit("forbidden_zero_bit is set"));
    }

    let nal_type = NalType::from_u8((raw_data[0] >> 1) & 0x3F);
    let nuh_layer_id = ((raw_data[0] & 0x01) << 5) | ((raw_data[1] >> 3) & 0x1F);
    let nuh_temporal_id_plus1 = raw_data[1] & 0x07;

    if nuh_temporal_id_plus1 == 0 {
        return Err(HevcError::InvalidNalUnit("temporal_id_plus1 is zero"));
    }

    let (payload, skipped_byte_positions) = remove_emulation_prevention(&raw_data[2..]);

    Ok(NalUnit {
        nal_type,
        nuh_layer_id,
        nuh_temporal_id_plus1,
        payload,
        skipped_byte_positions,
        raw_data,
    })
}

fn remove_emulation_prevention(data: &[u8]) -> (Vec<u8>, Vec<u32>) {
    let mut result = Vec::with_capacity(data.len());
    let mut skipped = Vec::new();
    let mut i = 0;

    while i < data.len() {
        if i + 2 < data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 3 {
            result.push(0);
            result.push(0);
            skipped.push((i + 2) as u32);
            i += 3;
        } else {
            result.push(data[i]);
            i += 1;
        }
    }

    (result, skipped)
}

#[allow(dead_code)]
pub struct BitstreamReader<'a> {
    data: &'a [u8],
    byte_offset: usize,
    bit_offset: u8,
}

impl<'a> BitstreamReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_offset: 0,
            bit_offset: 0,
        }
    }

    #[allow(dead_code)]
    pub fn is_byte_aligned(&self) -> bool {
        self.bit_offset == 0
    }

    pub fn byte_align(&mut self) {
        if self.bit_offset != 0 {
            self.bit_offset = 0;
            self.byte_offset += 1;
        }
    }

    pub fn read_bit(&mut self) -> Result<u8> {
        if self.byte_offset >= self.data.len() {
            return Err(HevcError::InvalidBitstream("unexpected end of data"));
        }

        let bit = (self.data[self.byte_offset] >> (7 - self.bit_offset)) & 1;
        self.bit_offset += 1;
        if self.bit_offset == 8 {
            self.bit_offset = 0;
            self.byte_offset += 1;
        }

        Ok(bit)
    }

    pub fn read_bits(&mut self, n: u8) -> Result<u32> {
        if n > 32 {
            return Err(HevcError::InvalidBitstream("too many bits requested"));
        }

        let mut value = 0u32;
        for _ in 0..n {
            value = (value << 1) | self.read_bit()? as u32;
        }
        Ok(value)
    }

    pub fn read_ue(&mut self) -> Result<u32> {
        let mut leading_zeros = 0u32;
        while self.read_bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return Err(HevcError::InvalidBitstream("exp-golomb overflow"));
            }
        }

        if leading_zeros == 0 {
            return Ok(0);
        }

        let suffix = self.read_bits(leading_zeros as u8)?;
        Ok((1 << leading_zeros) - 1 + suffix)
    }

    pub fn read_se(&mut self) -> Result<i32> {
        let ue = self.read_ue()?;
        let value = ue.div_ceil(2) as i32;
        if ue & 1 == 0 { Ok(-value) } else { Ok(value) }
    }

    #[allow(dead_code)]
    pub fn more_rbsp_data(&self) -> bool {
        if self.byte_offset >= self.data.len() {
            return false;
        }

        let remaining_bits = (self.data.len() - self.byte_offset) * 8 - self.bit_offset as usize;
        if remaining_bits == 0 {
            return false;
        }

        remaining_bits > 8
    }

    #[allow(dead_code)]
    pub fn remaining(&self) -> usize {
        if self.byte_offset >= self.data.len() {
            0
        } else {
            self.data.len() - self.byte_offset
        }
    }

    pub fn byte_position(&self) -> usize {
        self.byte_offset
    }
}
