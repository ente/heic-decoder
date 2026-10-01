use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use super::memory::{Budget, Buffer};
use super::{BoundedDecodeError as Error, BoundedInput, Result};

const MAX_INPUT: u64 = 512 * 1024 * 1024;
const MAX_METADATA: usize = 8 * 1024 * 1024;
const MAX_ITEMS: usize = 16384;
const MAX_PROPERTIES: usize = 4096;
const MAX_REFERENCES: usize = 65536;
pub(super) const MAX_ITEM_BYTES: usize = 16 * 1024 * 1024;

pub(super) enum Source<'a> {
    Bytes(&'a [u8]),
    File(File, u64),
}

impl<'a> Source<'a> {
    pub(super) fn new(input: BoundedInput<'a>) -> Result<Self> {
        let result = match input {
            BoundedInput::Bytes(bytes) => Self::Bytes(bytes),
            BoundedInput::Path(path) => {
                if path.as_os_str().len() > 32 * 1024 {
                    return Err(Error::LimitExceeded("input path bytes"));
                }
                let file = File::open(path)?;
                let len = file.metadata()?.len();
                Self::File(file, len)
            }
        };
        if result.len() > MAX_INPUT {
            return Err(Error::LimitExceeded("input bytes"));
        }
        Ok(result)
    }

    pub(super) fn len(&self) -> u64 {
        match self {
            Self::Bytes(b) => b.len() as u64,
            Self::File(_, len) => *len,
        }
    }

    pub(super) fn read(&mut self, offset: u64, output: &mut [u8]) -> Result<()> {
        if offset
            .checked_add(output.len() as u64)
            .is_none_or(|end| end > self.len())
        {
            return Err(Error::Malformed("item extent outside input"));
        }
        match self {
            Self::Bytes(bytes) => {
                output.copy_from_slice(&bytes[offset as usize..offset as usize + output.len()])
            }
            Self::File(file, _) => {
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(output)?;
            }
        }
        Ok(())
    }

    pub(super) fn metadata(&mut self, budget: &Budget) -> Result<Buffer<u8>> {
        let mut offset = 0;
        let mut metadata = None;
        let mut ftyp = false;
        let mut count = 0;
        while offset < self.len() {
            count += 1;
            if count > MAX_REFERENCES {
                return Err(Error::LimitExceeded("top-level boxes"));
            }
            let mut header = [0; 16];
            self.read(offset, &mut header[..8])?;
            let mut size = u32::from_be_bytes(header[..4].try_into().unwrap()) as u64;
            let header_size = if size == 1 {
                self.read(offset + 8, &mut header[8..])?;
                size = u64::from_be_bytes(header[8..].try_into().unwrap());
                16
            } else {
                8
            };
            if size == 0 {
                size = self.len() - offset;
            }
            if size < header_size || size > self.len() - offset {
                return Err(Error::Malformed("box size"));
            }
            match &header[4..8] {
                b"meta" => {
                    if metadata.is_some() {
                        return Err(Error::Malformed("duplicate meta"));
                    }
                    let len = (size - header_size) as usize;
                    if len > MAX_METADATA {
                        return Err(Error::LimitExceeded("metadata bytes"));
                    }
                    let mut bytes = budget.zeroed(len, "metadata")?;
                    self.read(offset + header_size, &mut bytes)?;
                    metadata = Some(bytes);
                }
                b"ftyp" => {
                    if ftyp || size - header_size < 8 || size - header_size > 4096 {
                        return Err(Error::Malformed("ftyp"));
                    }
                    let mut brands = [0; 4096];
                    let len = (size - header_size) as usize;
                    self.read(offset + header_size, &mut brands[..len])?;
                    ftyp = brands[..len]
                        .chunks_exact(4)
                        .enumerate()
                        .any(|(i, b)| i != 1 && matches!(b, b"heic" | b"heix" | b"mif1"));
                    if !ftyp {
                        return Err(Error::Unsupported("HEIF still-image brand"));
                    }
                }
                _ => {}
            }
            offset += size;
        }
        if !ftyp {
            return Err(Error::Malformed("missing ftyp"));
        }
        metadata.ok_or(Error::Malformed("missing meta"))
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct BoxRef<'a> {
    pub(super) kind: [u8; 4],
    pub(super) data: &'a [u8],
}

pub(super) struct Boxes<'a>(&'a [u8]);

impl<'a> Boxes<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }
}

impl<'a> Iterator for Boxes<'a> {
    type Item = Result<BoxRef<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        let result = (|| {
            let mut reader = Reader::new(self.0);
            let mut size = reader.uint(4)?;
            let kind = reader.take(4)?.try_into().unwrap();
            if size == 1 {
                size = reader.uint(8)?;
            }
            if size == 0 {
                size = self.0.len() as u64;
            }
            if size < reader.pos as u64 || size > self.0.len() as u64 {
                return Err(Error::Malformed("nested box size"));
            }
            let data = &self.0[reader.pos..size as usize];
            self.0 = &self.0[size as usize..];
            Ok(BoxRef { kind, data })
        })();
        if result.is_err() {
            self.0 = &[];
        }
        Some(result)
    }
}

pub(super) struct Reader<'a> {
    pub(super) data: &'a [u8],
    pub(super) pos: usize,
}

impl<'a> Reader<'a> {
    pub(super) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    pub(super) fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(Error::Malformed("length overflow"))?;
        let result = self
            .data
            .get(self.pos..end)
            .ok_or(Error::Malformed("truncated metadata"))?;
        self.pos = end;
        Ok(result)
    }
    pub(super) fn uint(&mut self, len: usize) -> Result<u64> {
        if len > 8 {
            return Err(Error::Malformed("integer width"));
        }
        Ok(self
            .take(len)?
            .iter()
            .fold(0, |value, &byte| (value << 8) | u64::from(byte)))
    }
    fn null_terminated(&mut self) -> Result<&'a [u8]> {
        let len = self.data[self.pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or(Error::Malformed("unterminated item string"))?;
        let value = self.take(len)?;
        self.take(1)?;
        Ok(value)
    }
    pub(super) fn full_box(&mut self) -> Result<(u8, u32)> {
        Ok((self.uint(1)? as u8, self.uint(3)? as u32))
    }
    pub(super) fn finish(&self) -> Result<()> {
        if self.pos == self.data.len() {
            Ok(())
        } else {
            Err(Error::Malformed("trailing metadata"))
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct Location<'a> {
    extents: &'a [u8],
    base: u64,
    offset_size: usize,
    length_size: usize,
    index_size: usize,
    method: u16,
    pub(super) length: usize,
}

pub(super) struct Item<'a> {
    pub(super) id: u32,
    pub(super) kind: [u8; 4],
    pub(super) is_exif: bool,
    pub(super) location: Option<Location<'a>>,
    associations: Option<&'a [u8]>,
}

pub(super) struct Reference<'a> {
    pub(super) kind: [u8; 4],
    pub(super) from: u32,
    targets: &'a [u8],
    width: usize,
}

impl Reference<'_> {
    pub(super) fn targets(&self) -> impl Iterator<Item = u32> + '_ {
        self.targets
            .chunks_exact(self.width)
            .map(|b| b.iter().fold(0u32, |v, &n| (v << 8) | u32::from(n)))
    }
}

pub(super) struct Index<'a> {
    pub(super) primary: usize,
    pub(super) items: Buffer<Item<'a>>,
    properties: Buffer<BoxRef<'a>>,
    pub(super) references: Buffer<Reference<'a>>,
    wide_associations: bool,
    idat: &'a [u8],
}

fn unique<'a>(slot: &mut Option<&'a [u8]>, data: &'a [u8]) -> Result<()> {
    if slot.replace(data).is_some() {
        return Err(Error::Malformed("duplicate metadata box"));
    }
    Ok(())
}

impl<'a> Index<'a> {
    pub(super) fn parse(metadata: &'a [u8], source_len: u64, budget: &Budget) -> Result<Self> {
        Self::parse_primary(metadata, source_len, budget, false)
    }

    pub(super) fn parse_primary(
        metadata: &'a [u8],
        source_len: u64,
        budget: &Budget,
        direct: bool,
    ) -> Result<Self> {
        let mut reader = Reader::new(metadata);

        if reader.full_box()? != (0, 0) {
            return Err(Error::Unsupported("meta version or flags"));
        }
        let (mut pitm, mut iinf, mut iloc, mut iprp, mut iref, mut idat) =
            (None, None, None, None, None, None);
        for b in Boxes::new(&metadata[4..]) {
            let b = b?;
            match &b.kind {
                b"pitm" => unique(&mut pitm, b.data)?,
                b"iinf" => unique(&mut iinf, b.data)?,
                b"iloc" => unique(&mut iloc, b.data)?,
                b"iprp" => unique(&mut iprp, b.data)?,
                b"iref" => unique(&mut iref, b.data)?,
                b"idat" => unique(&mut idat, b.data)?,
                b"ipro" => return Err(Error::Unsupported("item protection")),
                _ => {}
            }
        }
        let mut reader = Reader::new(iinf.ok_or(Error::Malformed("missing iinf"))?);
        let (version, flags) = reader.full_box()?;
        if version > 1 || flags != 0 {
            return Err(Error::Unsupported("iinf version or flags"));
        }
        let count = reader.uint(if version == 0 { 2 } else { 4 })? as usize;
        if count == 0 || count > MAX_ITEMS {
            return Err(Error::LimitExceeded("item count"));
        }
        let mut items = budget.buffer(count, "item index")?;
        for b in Boxes::new(&reader.data[reader.pos..]) {
            let b = b?;
            if b.kind != *b"infe" {
                return Err(Error::Malformed("iinf child"));
            }
            let mut r = Reader::new(b.data);
            let (version, flags) = r.full_box()?;
            if !(2..=3).contains(&version) || flags & !1 != 0 {
                return Err(Error::Unsupported("infe version or flags"));
            }
            let id = r.uint(if version == 2 { 2 } else { 4 })? as u32;
            if r.uint(2)? != 0 {
                return Err(Error::Unsupported("protected item"));
            }
            let kind = r.take(4)?.try_into().unwrap();
            let name = r.null_terminated()?;
            let is_exif = if kind == *b"mime" {
                let content_type = r.null_terminated()?;
                name.eq_ignore_ascii_case(b"Exif")
                    || content_type.eq_ignore_ascii_case(b"application/exif")
                    || content_type.eq_ignore_ascii_case(b"image/tiff")
            } else {
                kind == *b"Exif"
            };
            items.push(Item {
                id,
                kind,
                is_exif,
                location: None,
                associations: None,
            })?;
        }
        if items.len() != count {
            return Err(Error::Malformed("item count mismatch"));
        }
        items.sort_unstable_by_key(|item| item.id);
        if items.windows(2).any(|p| p[0].id == p[1].id) {
            return Err(Error::Malformed("duplicate item id"));
        }
        let mut reader = Reader::new(pitm.ok_or(Error::Malformed("missing pitm"))?);
        let (version, flags) = reader.full_box()?;
        if version > 1 || flags != 0 {
            return Err(Error::Unsupported("pitm version or flags"));
        }
        let primary_id = reader.uint(if version == 0 { 2 } else { 4 })? as u32;
        reader.finish()?;
        let primary = items
            .binary_search_by_key(&primary_id, |item| item.id)
            .map_err(|_| Error::Malformed("primary item missing"))?;
        if items[primary].kind != *b"grid"
            && !(direct && matches!(&items[primary].kind, b"hvc1" | b"hev1"))
        {
            return Err(Error::Unsupported("primary item must be a HEIC grid"));
        }
        let idat = idat.unwrap_or_default();
        Self::locations(
            &mut items,
            iloc.ok_or(Error::Malformed("missing iloc"))?,
            source_len,
            idat.len(),
        )?;
        let (mut ipco, mut ipma) = (None, None);
        for b in Boxes::new(iprp.ok_or(Error::Malformed("missing iprp"))?) {
            let b = b?;
            match &b.kind {
                b"ipco" => unique(&mut ipco, b.data)?,
                b"ipma" => unique(&mut ipma, b.data)?,
                _ => return Err(Error::Unsupported("item property container")),
            }
        }
        let ipco = ipco.ok_or(Error::Malformed("missing ipco"))?;
        let mut property_count = 0;
        for b in Boxes::new(ipco) {
            b?;
            property_count += 1;
        }
        if property_count > MAX_PROPERTIES {
            return Err(Error::LimitExceeded("property count"));
        }
        let mut properties = budget.buffer(property_count, "property index")?;
        for b in Boxes::new(ipco) {
            properties.push(b?)?;
        }
        let wide_associations = Self::associations(
            &mut items,
            ipma.ok_or(Error::Malformed("missing ipma"))?,
            property_count,
        )?;
        let mut reference_count = 0;
        let mut total_targets = 0;
        let (reference_data, reference_width) = if let Some(data) = iref {
            let mut r = Reader::new(data);
            let (version, flags) = r.full_box()?;
            if version > 1 || flags != 0 {
                return Err(Error::Unsupported("iref version or flags"));
            }
            for b in Boxes::new(&data[4..]) {
                b?;
                reference_count += 1;
            }
            (&data[4..], if version == 0 { 2 } else { 4 })
        } else {
            (&[][..], 2)
        };
        if reference_count > MAX_ITEMS {
            return Err(Error::LimitExceeded("reference count"));
        }
        let mut references = budget.buffer(reference_count, "reference index")?;
        for b in Boxes::new(reference_data) {
            let b = b?;
            let mut r = Reader::new(b.data);
            let from = r.uint(reference_width)? as u32;
            let count = r.uint(2)? as usize;
            total_targets += count;
            if total_targets > MAX_REFERENCES {
                return Err(Error::LimitExceeded("reference targets"));
            }
            let targets = r.take(count * reference_width)?;
            r.finish()?;
            references.push(Reference {
                kind: b.kind,
                from,
                targets,
                width: reference_width,
            })?;
        }
        Ok(Self {
            primary,
            items,
            properties,
            references,
            wide_associations,
            idat,
        })
    }

    fn locations(
        items: &mut [Item<'a>],
        data: &'a [u8],
        source_len: u64,
        idat_len: usize,
    ) -> Result<()> {
        let mut r = Reader::new(data);
        let (version, flags) = r.full_box()?;
        if version > 2 || flags != 0 {
            return Err(Error::Unsupported("iloc version or flags"));
        }
        let sizes = r.uint(1)?;
        let offset_size = (sizes >> 4) as usize;
        let length_size = (sizes & 15) as usize;
        let sizes = r.uint(1)?;
        let base_size = (sizes >> 4) as usize;
        let index_size = if version == 0 {
            0
        } else {
            (sizes & 15) as usize
        };
        if [offset_size, length_size, base_size, index_size]
            .iter()
            .any(|&n| !matches!(n, 0 | 4 | 8))
        {
            return Err(Error::Unsupported("iloc field width"));
        }
        let count = r.uint(if version == 2 { 4 } else { 2 })? as usize;
        if count > items.len() {
            return Err(Error::Malformed("iloc item count"));
        }
        let mut total_extents = 0;
        for _ in 0..count {
            let id = r.uint(if version == 2 { 4 } else { 2 })? as u32;
            let index = items
                .binary_search_by_key(&id, |item| item.id)
                .map_err(|_| Error::Malformed("iloc item missing"))?;
            if items[index].location.is_some() {
                return Err(Error::Malformed("duplicate item location"));
            }
            let method = if version == 0 { 0 } else { r.uint(2)? as u16 };
            if method > 1 || r.uint(2)? != 0 {
                return Err(Error::Unsupported("external or derived item location"));
            }
            let base = r.uint(base_size)?;
            let count = r.uint(2)? as usize;
            total_extents += count;
            if total_extents > MAX_REFERENCES {
                return Err(Error::LimitExceeded("extent count"));
            }
            let start = r.pos;
            let mut length = 0u64;
            for _ in 0..count {
                if r.uint(index_size)? != 0 {
                    return Err(Error::Unsupported("extent index"));
                }
                let offset = r
                    .uint(offset_size)?
                    .checked_add(base)
                    .ok_or(Error::Malformed("extent offset overflow"))?;
                let len = r.uint(length_size)?;
                let end = offset
                    .checked_add(len)
                    .ok_or(Error::Malformed("extent end overflow"))?;
                if end
                    > if method == 0 {
                        source_len
                    } else {
                        idat_len as u64
                    }
                {
                    return Err(Error::Malformed("extent out of bounds"));
                }
                length = length
                    .checked_add(len)
                    .ok_or(Error::Malformed("item length overflow"))?;
            }
            if length > MAX_INPUT {
                return Err(Error::LimitExceeded("item bytes"));
            }
            items[index].location = Some(Location {
                extents: &data[start..r.pos],
                base,
                offset_size,
                length_size,
                index_size,
                method,
                length: length as usize,
            });
        }
        r.finish()
    }

    fn associations(items: &mut [Item<'a>], data: &'a [u8], property_count: usize) -> Result<bool> {
        let mut r = Reader::new(data);
        let (version, flags) = r.full_box()?;
        if version > 1 || flags > 1 {
            return Err(Error::Unsupported("ipma version or flags"));
        }
        let wide = flags == 1;
        let count = r.uint(4)? as usize;
        if count > items.len() {
            return Err(Error::Malformed("ipma entry count"));
        }
        let mut total = 0;
        for _ in 0..count {
            let id = r.uint(if version == 0 { 2 } else { 4 })? as u32;
            let index = items
                .binary_search_by_key(&id, |item| item.id)
                .map_err(|_| Error::Malformed("ipma item missing"))?;
            let count = r.uint(1)? as usize;
            total += count;
            if total > MAX_REFERENCES {
                return Err(Error::LimitExceeded("property associations"));
            }
            let start = r.pos;
            for _ in 0..count {
                let value = r.uint(if wide { 2 } else { 1 })? as usize;
                if value & if wide { 0x7fff } else { 0x7f } > property_count {
                    return Err(Error::Malformed("property index"));
                }
            }
            if items[index]
                .associations
                .replace(&data[start..r.pos])
                .is_some()
            {
                return Err(Error::Malformed("duplicate associations"));
            }
        }
        r.finish()?;
        Ok(wide)
    }

    pub(super) fn item(&self, id: u32) -> Result<usize> {
        self.items
            .binary_search_by_key(&id, |item| item.id)
            .map_err(|_| Error::Malformed("referenced item missing"))
    }

    pub(super) fn properties(&self, item: usize) -> impl Iterator<Item = (bool, BoxRef<'a>)> + '_ {
        let wide = self.wide_associations;
        self.items[item]
            .associations
            .unwrap_or_default()
            .chunks_exact(if wide { 2 } else { 1 })
            .filter_map(move |b| {
                let value = if wide {
                    u16::from_be_bytes([b[0], b[1]])
                } else {
                    u16::from(b[0])
                };
                let mask = if wide { 0x8000 } else { 0x80 };
                let index = value & !mask;
                index
                    .checked_sub(1)
                    .map(|i| (value & mask != 0, self.properties[i as usize]))
            })
    }

    #[cfg(feature = "incremental-experiment")]
    pub(super) fn read_range(
        &self,
        item: usize,
        source: &mut Source<'_>,
        mut position: usize,
        mut output: &mut [u8],
    ) -> Result<()> {
        let location = self.items[item]
            .location
            .ok_or(Error::Malformed("missing item location"))?;
        if position
            .checked_add(output.len())
            .is_none_or(|end| end > location.length)
        {
            return Err(Error::Malformed("item range"));
        }
        let mut r = Reader::new(location.extents);
        while !output.is_empty() && r.pos < r.data.len() {
            r.uint(location.index_size)?;
            let offset = location.base + r.uint(location.offset_size)?;
            let length = r.uint(location.length_size)? as usize;
            if position >= length {
                position -= length;
                continue;
            }
            let count = output.len().min(length - position);
            let offset = offset + position as u64;
            if location.method == 0 {
                source.read(offset, &mut output[..count])?;
            } else {
                output[..count]
                    .copy_from_slice(&self.idat[offset as usize..offset as usize + count]);
            }
            output = &mut output[count..];
            position = 0;
        }
        if output.is_empty() {
            Ok(())
        } else {
            Err(Error::Malformed("item extent coverage"))
        }
    }

    pub(super) fn read_item(
        &self,
        item: usize,
        source: &mut Source<'_>,
        output: &mut [u8],
    ) -> Result<()> {
        let location = self.items[item]
            .location
            .ok_or(Error::Malformed("missing item location"))?;
        if location.length != output.len() {
            return Err(Error::Malformed("item length"));
        }
        let mut r = Reader::new(location.extents);
        let mut cursor = 0;
        while r.pos < r.data.len() {
            r.uint(location.index_size)?;
            let offset = location.base + r.uint(location.offset_size)?;
            let len = r.uint(location.length_size)? as usize;
            if location.method == 0 {
                source.read(offset, &mut output[cursor..cursor + len])?;
            } else {
                output[cursor..cursor + len]
                    .copy_from_slice(&self.idat[offset as usize..offset as usize + len]);
            }
            cursor += len;
        }
        Ok(())
    }
}
