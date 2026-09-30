use crate::heic_decoder::hevc::bounded::Prepared;

use super::container::Reader;
use super::{BoundedDecodeError as Error, Result};

pub(super) fn prepare<'a>(config: &'a [u8], payload: &'a [u8]) -> Result<Prepared<'a>> {
    let mut r = Reader::new(config);
    let header = r.take(23)?;
    if header[0] != 1 {
        return Err(Error::Unsupported("hvcC version"));
    }
    let length_size = usize::from((header[21] & 3) + 1);
    let mut nals = Nals::default();
    for _ in 0..header[22] {
        let array_type = r.uint(1)? as u8 & 63;
        let count = r.uint(2)?;
        for _ in 0..count {
            let len = r.uint(2)? as usize;
            let nal = r.take(len)?;
            if nal
                .first()
                .is_none_or(|byte| (byte >> 1) & 63 != array_type)
            {
                return Err(Error::Malformed("hvcC NAL type"));
            }
            nals.add(nal, false)?;
        }
    }
    r.finish()?;
    let mut r = Reader::new(payload);
    while r.pos < payload.len() {
        let len = r.uint(length_size)? as usize;
        nals.add(r.take(len)?, true)?;
    }
    Ok(Prepared::new(
        nals.parameters[0].ok_or(Error::Malformed("missing VPS"))?,
        nals.parameters[1].ok_or(Error::Malformed("missing SPS"))?,
        nals.parameters[2].ok_or(Error::Malformed("missing PPS"))?,
        nals.slice.ok_or(Error::Malformed("missing IDR slice"))?,
    )?)
}

#[derive(Default)]
struct Nals<'a> {
    parameters: [Option<&'a [u8]>; 3],
    slice: Option<&'a [u8]>,
    count: usize,
}

impl<'a> Nals<'a> {
    fn add(&mut self, nal: &'a [u8], in_payload: bool) -> Result<()> {
        self.count += 1;
        if self.count > 1024 {
            return Err(Error::LimitExceeded("NAL count"));
        }
        if nal.len() < 2 || nal[0] & 0x81 != 0 || nal[1] != 1 {
            return Err(Error::Unsupported("NAL layer or temporal id"));
        }
        let kind = (nal[0] >> 1) & 63;
        match kind {
            32..=34 => {
                if nal.len() > 65536 {
                    return Err(Error::LimitExceeded("parameter NAL bytes"));
                }
                let slot = &mut self.parameters[usize::from(kind - 32)];
                if slot.is_some_and(|previous| previous != nal) {
                    return Err(Error::Unsupported("replacement parameter sets"));
                }
                *slot = Some(nal);
            }
            19 | 20 if in_payload => {
                if self.slice.replace(nal).is_some() {
                    return Err(Error::Unsupported("multiple coded pictures or slices"));
                }
            }
            35 | 38..=40 => {}
            _ => return Err(Error::Unsupported("HEVC NAL type")),
        }
        Ok(())
    }
}
