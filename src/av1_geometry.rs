use crate::DecodeAvifError;
use rav1d::dav1d_parse_sequence_header;
use rav1d::include::dav1d::headers::{DAV1D_ADAPTIVE, Dav1dSequenceHeader};
use std::mem::MaybeUninit;
use std::ptr::NonNull;

type Result<T> = std::result::Result<T, DecodeAvifError>;

fn invalid() -> DecodeAvifError {
    DecodeAvifError::DecoderApi {
        stage: "AV1 geometry preflight",
        code: -22,
    }
}

struct Bits<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl Bits<'_> {
    fn read(&mut self, count: u8) -> Result<u32> {
        let mut value = 0;
        for _ in 0..count {
            let byte = self.bytes.get(self.offset / 8).ok_or_else(invalid)?;
            value = (value << 1) | u32::from((byte >> (7 - self.offset % 8)) & 1);
            self.offset += 1;
        }
        Ok(value)
    }
}

pub(crate) fn coded_dimensions(mut bytes: &[u8]) -> Result<Option<(u32, u32)>> {
    let mut sequence = None;
    while !bytes.is_empty() {
        let header = bytes[0];
        if header & 0x81 != 0 {
            return Err(invalid());
        }
        let kind = (header >> 3) & 15;
        let mut offset = 1;
        let extension = if header & 4 != 0 {
            offset += 1;
            *bytes.get(1).ok_or_else(invalid)?
        } else {
            0
        };
        let size = if header & 2 != 0 {
            let mut size = 0u64;
            let mut complete = false;
            for shift in (0..56).step_by(7) {
                let value = *bytes.get(offset).ok_or_else(invalid)?;
                offset += 1;
                size |= u64::from(value & 127) << shift;
                if value & 128 == 0 {
                    complete = true;
                    break;
                }
            }
            if !complete {
                return Err(invalid());
            }
            usize::try_from(size).map_err(|_| invalid())?
        } else {
            bytes.len() - offset
        };
        let end = offset.checked_add(size).ok_or_else(invalid)?;
        let obu = bytes.get(..end).ok_or_else(invalid)?;
        let payload = &obu[offset..];
        if kind == 1 {
            let mut parsed = MaybeUninit::<Dav1dSequenceHeader>::uninit();
            let result = unsafe {
                dav1d_parse_sequence_header(
                    NonNull::new(parsed.as_mut_ptr()),
                    NonNull::new(obu.as_ptr().cast_mut()),
                    obu.len(),
                )
            };
            if result.0 != 0 {
                return Err(invalid());
            }
            sequence = Some(unsafe { parsed.assume_init() });
        } else if matches!(kind, 3 | 6) {
            let sequence = sequence
                .as_ref()
                .ok_or(DecodeAvifError::MissingSequenceHeader)?;
            return frame_dimensions(payload, sequence, extension);
        }
        bytes = &bytes[end..];
    }
    Ok(None)
}

fn frame_dimensions(
    payload: &[u8],
    s: &Dav1dSequenceHeader,
    extension: u8,
) -> Result<Option<(u32, u32)>> {
    let mut bits = Bits {
        bytes: payload,
        offset: 0,
    };
    let reduced = s.reduced_still_picture_header != 0;
    if !reduced && bits.read(1)? != 0 {
        return Ok(None);
    }
    let frame_type = if reduced { 0 } else { bits.read(2)? };
    let show_frame = reduced || bits.read(1)? != 0;
    if show_frame {
        if s.decoder_model_info_present != 0 && s.equal_picture_interval == 0 {
            bits.read(s.frame_presentation_delay_length)?;
        }
    } else {
        bits.read(1)?;
    }
    let resilient =
        reduced || (frame_type == 0 && show_frame) || frame_type == 3 || bits.read(1)? != 0;
    bits.read(1)?;
    let screen = if s.screen_content_tools == DAV1D_ADAPTIVE {
        bits.read(1)? != 0
    } else {
        s.screen_content_tools != 0
    };
    if screen && s.force_integer_mv == DAV1D_ADAPTIVE {
        bits.read(1)?;
    }
    if s.frame_id_numbers_present != 0 {
        bits.read(s.frame_id_n_bits)?;
    }
    let override_size = !reduced && (frame_type == 3 || bits.read(1)? != 0);
    if s.order_hint != 0 {
        bits.read(s.order_hint_n_bits)?;
    }
    if !resilient && matches!(frame_type, 1 | 3) {
        bits.read(3)?;
    }
    if s.decoder_model_info_present != 0 && bits.read(1)? != 0 {
        let temporal = extension >> 5;
        let spatial = (extension >> 3) & 3;
        for op in s
            .operating_points
            .iter()
            .take(s.num_operating_points as usize)
        {
            if op.decoder_model_param_present != 0
                && (op.idc == 0
                    || ((op.idc >> temporal) & 1 != 0 && (op.idc >> (spatial + 8)) & 1 != 0))
            {
                bits.read(s.buffer_removal_delay_length)?;
            }
        }
    }
    if !matches!(frame_type, 0 | 2) {
        return Ok(None);
    }
    let refresh = if frame_type == 0 && show_frame {
        255
    } else {
        bits.read(8)?
    };
    if refresh != 255 && resilient && s.order_hint != 0 {
        for _ in 0..8 {
            bits.read(s.order_hint_n_bits)?;
        }
    }
    let (width, height) = if override_size {
        (
            bits.read(s.width_n_bits)? + 1,
            bits.read(s.height_n_bits)? + 1,
        )
    } else {
        (
            u32::try_from(s.max_width).map_err(|_| invalid())?,
            u32::try_from(s.max_height).map_err(|_| invalid())?,
        )
    };
    Ok(Some((width, height)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sequence() -> Dav1dSequenceHeader {
        let bytes = [0x0a, 0x04, 0x18, 0x00, 0x00, 0x15];
        let mut header = MaybeUninit::uninit();
        let result = unsafe {
            dav1d_parse_sequence_header(
                NonNull::new(header.as_mut_ptr()),
                NonNull::new(bytes.as_ptr().cast_mut()),
                bytes.len(),
            )
        };
        assert_eq!(result.0, 0);
        unsafe { header.assume_init() }
    }

    fn frame_bits(values: &[(u32, u8)]) -> Vec<u8> {
        let mut result = Vec::new();
        let mut offset = 0;
        for &(value, count) in values {
            for bit in (0..count).rev() {
                if offset % 8 == 0 {
                    result.push(0);
                }
                result[offset / 8] |= (((value >> bit) & 1) as u8) << (7 - offset % 8);
                offset += 1;
            }
        }
        result
    }

    #[test]
    fn frame_override_can_be_smaller_than_sequence_maximum() {
        let mut s = sequence();
        s.reduced_still_picture_header = 0;
        s.screen_content_tools = 0;
        s.width_n_bits = 12;
        s.height_n_bits = 12;
        s.max_width = 4096;
        s.max_height = 4096;
        let frame = frame_bits(&[(0, 1), (0, 2), (1, 1), (0, 1), (1, 1), (15, 12), (31, 12)]);
        assert_eq!(frame_dimensions(&frame, &s, 0).unwrap(), Some((16, 32)));
    }

    #[test]
    fn reduced_frame_uses_sequence_dimensions() {
        let mut s = sequence();
        s.max_width = 8192;
        s.max_height = 4096;
        assert_eq!(frame_dimensions(&[0], &s, 0).unwrap(), Some((8192, 4096)));
    }

    #[test]
    fn truncated_frame_and_obu_lengths_are_rejected() {
        assert!(frame_dimensions(&[], &sequence(), 0).is_err());
        for bytes in [
            vec![0x0a, 0x7f],
            vec![0x0e],
            vec![0x0a, 0x80],
            vec![0x0a; 1],
        ] {
            assert!(coded_dimensions(&bytes).is_err());
        }
    }
}
