use crate::heic_decoder::hevc::bounded::Geometry;
use crate::isobmff::{
    ImageCleanApertureProperty, ImageMirrorDirection, ImageMirrorProperty, ImageRotationProperty,
    NclxColorProfile, PrimaryItemTransformProperty as Transform,
};

use super::container::{Index, MAX_ITEM_BYTES, Reader, Source};
use super::memory::{Budget, Buffer};
use super::{BoundedDecodeError as Error, Result, codec};

#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Color<'a> {
    pub(super) nclx: Option<NclxColorProfile>,
    pub(super) icc: Option<&'a [u8]>,
}

pub(super) struct Properties<'a> {
    pub(super) config: Option<&'a [u8]>,
    pub(super) dimensions: Option<(u32, u32)>,
    pub(super) color: Color<'a>,
    pub(super) transforms: [Option<Transform>; 16],
    pub(super) transform_count: usize,
}

impl<'a> Properties<'a> {
    pub(super) fn read(index: &Index<'a>, item: usize, tile: bool) -> Result<Self> {
        let mut result = Self {
            config: None,
            dimensions: None,
            color: Color::default(),
            transforms: [None; 16],
            transform_count: 0,
        };
        for (essential, b) in index.properties(item) {
            let mut r = Reader::new(b.data);
            let mut transform = None;
            match &b.kind {
                b"ispe" => {
                    if r.full_box()? != (0, 0) {
                        return Err(Error::Unsupported("ispe version or flags"));
                    }
                    let dimensions = (r.uint(4)? as u32, r.uint(4)? as u32);
                    if result.dimensions.replace(dimensions).is_some() {
                        return Err(Error::Malformed("duplicate ispe"));
                    }
                    r.finish()?;
                }
                b"hvcC" => {
                    if b.data.len() > 1024 * 1024 {
                        return Err(Error::LimitExceeded("hvcC bytes"));
                    }
                    if result.config.replace(b.data).is_some() {
                        return Err(Error::Malformed("duplicate hvcC"));
                    }
                }
                b"colr" => match r.take(4)? {
                    b"nclx" => {
                        let nclx = NclxColorProfile {
                            colour_primaries: r.uint(2)? as u16,
                            transfer_characteristics: r.uint(2)? as u16,
                            matrix_coefficients: r.uint(2)? as u16,
                            full_range_flag: r.uint(1)? & 128 != 0,
                        };
                        r.finish()?;
                        if result.color.nclx.replace(nclx).is_some() {
                            return Err(Error::Malformed("duplicate nclx"));
                        }
                    }
                    b"prof" | b"rICC" => {
                        if result.color.icc.replace(&b.data[4..]).is_some() {
                            return Err(Error::Malformed("duplicate ICC"));
                        }
                    }
                    _ => return Err(Error::Unsupported("color property")),
                },
                b"pixi" => {
                    if r.full_box()? != (0, 0) {
                        return Err(Error::Unsupported("pixi version or flags"));
                    }
                    let count = r.uint(1)? as usize;
                    if count != 3 || r.take(count)?.iter().any(|&n| n != 8) {
                        return Err(Error::Unsupported("8-bit RGB channels required"));
                    }
                    r.finish()?;
                }
                b"irot" => {
                    let value = r.uint(1)?;
                    if value > 3 {
                        return Err(Error::Malformed("rotation"));
                    }
                    r.finish()?;
                    if tile && value != 0 {
                        return Err(Error::Unsupported("tile rotation"));
                    }
                    transform = Some(Transform::Rotation(ImageRotationProperty {
                        rotation_ccw_degrees: value as u16 * 90,
                    }));
                }
                b"imir" => {
                    let value = r.uint(1)?;
                    if value > 1 {
                        return Err(Error::Malformed("mirror"));
                    }
                    r.finish()?;
                    if tile {
                        return Err(Error::Unsupported("tile mirror"));
                    }
                    transform = Some(Transform::Mirror(ImageMirrorProperty {
                        direction: if value == 1 {
                            ImageMirrorDirection::Horizontal
                        } else {
                            ImageMirrorDirection::Vertical
                        },
                    }));
                }
                b"clap" => {
                    if tile {
                        return Err(Error::Unsupported("tile clean aperture"));
                    }
                    let crop = ImageCleanApertureProperty {
                        clean_aperture_width_num: r.uint(4)? as u32,
                        clean_aperture_width_den: r.uint(4)? as u32,
                        clean_aperture_height_num: r.uint(4)? as u32,
                        clean_aperture_height_den: r.uint(4)? as u32,
                        horizontal_offset_num: r.uint(4)? as i32,
                        horizontal_offset_den: r.uint(4)? as u32,
                        vertical_offset_num: r.uint(4)? as i32,
                        vertical_offset_den: r.uint(4)? as u32,
                    };
                    r.finish()?;
                    if [
                        crop.clean_aperture_width_den,
                        crop.clean_aperture_height_den,
                        crop.horizontal_offset_den,
                        crop.vertical_offset_den,
                    ]
                    .contains(&0)
                    {
                        return Err(Error::Malformed("clean aperture denominator"));
                    }
                    transform = Some(Transform::CleanAperture(crop));
                }
                b"auxC" => return Err(Error::Unsupported("auxiliary primary or tile")),
                _ if essential => return Err(Error::Unsupported("essential item property")),
                _ => {}
            }
            if let Some(transform) = transform {
                if result.transform_count == result.transforms.len() {
                    return Err(Error::LimitExceeded("transform count"));
                }
                result.transforms[result.transform_count] = Some(transform);
                result.transform_count += 1;
            }
        }
        Ok(result)
    }
}

pub(super) struct Tile<'a> {
    pub(super) item: usize,
    pub(super) config: &'a [u8],
    pub(super) geometry: Geometry,
    pub(super) color: Color<'a>,
    pub(super) payload_len: usize,
    pub(super) workspace_bytes: usize,
}

pub(super) struct Grid<'a> {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) columns: u32,
    pub(super) tiles: Buffer<Tile<'a>>,
    pub(super) properties: Properties<'a>,
    pub(super) exif: Option<u8>,
}

impl<'a> Grid<'a> {
    pub(super) fn preflight(
        index: &Index<'a>,
        source: &mut Source<'_>,
        budget: &Budget,
    ) -> Result<Self> {
        let primary = index.primary;
        let primary_id = index.items[primary].id;
        let location = index.items[primary]
            .location
            .ok_or(Error::Malformed("grid location"))?;
        if !matches!(location.length, 8 | 12) {
            return Err(Error::Malformed("grid descriptor length"));
        }
        let mut descriptor = [0; 12];
        index.read_item(primary, source, &mut descriptor[..location.length])?;
        let mut r = Reader::new(&descriptor[..location.length]);
        if r.uint(1)? != 0 {
            return Err(Error::Unsupported("grid version"));
        }
        let flags = r.uint(1)?;
        if flags > 1 {
            return Err(Error::Unsupported("grid flags"));
        }
        let rows = r.uint(1)? as u32 + 1;
        let columns = r.uint(1)? as u32 + 1;
        let width = r.uint(if flags == 0 { 2 } else { 4 })? as u32;
        let height = r.uint(if flags == 0 { 2 } else { 4 })? as u32;
        r.finish()?;
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 256_000_000 {
            return Err(Error::LimitExceeded("grid dimensions"));
        }
        let count = (rows * columns) as usize;
        if count > 4096 {
            return Err(Error::LimitExceeded("grid tile count"));
        }
        let properties = Properties::read(index, primary, false)?;
        if properties.dimensions != Some((width, height)) {
            return Err(Error::Malformed("grid ispe mismatch"));
        }
        if properties.config.is_some() {
            return Err(Error::Unsupported("coded grid configuration"));
        }
        let mut references = index
            .references
            .iter()
            .filter(|reference| reference.from == primary_id && reference.kind == *b"dimg");
        let reference = references
            .next()
            .ok_or(Error::Malformed("grid references missing"))?;
        if references.next().is_some() || reference.targets().count() != count {
            return Err(Error::Malformed("grid reference count"));
        }
        let mut tiles = budget.buffer::<Tile<'a>>(count, "tile index")?;
        for id in reference.targets() {
            let item = index.item(id)?;
            if !matches!(&index.items[item].kind, b"hvc1" | b"hev1") {
                return Err(Error::Unsupported("grid requires direct HEVC tiles"));
            }
            let props = Properties::read(index, item, true)?;
            let config = props.config.ok_or(Error::Malformed("missing tile hvcC"))?;
            let payload_len = index.items[item]
                .location
                .ok_or(Error::Malformed("missing tile location"))?
                .length;
            if payload_len == 0 || payload_len > MAX_ITEM_BYTES {
                return Err(Error::LimitExceeded("tile payload bytes"));
            }
            let mut payload = budget.zeroed(payload_len, "preflight payload")?;
            index.read_item(item, source, &mut payload)?;
            let (geometry, reconstruction_bytes) = {
                let _headers =
                    budget.reserve(payload_len * 5 + 1024 * 1024, "HEVC header parsing")?;
                let prepared = codec::prepare(config, &payload)?;
                (prepared.geometry, prepared.reconstruction_bytes())
            };
            if props.dimensions != Some((geometry.width, geometry.height)) {
                return Err(Error::Malformed("tile ispe differs from SPS"));
            }
            if tiles.first().is_some_and(|first| {
                (
                    geometry.width,
                    geometry.height,
                    geometry.full_range,
                    geometry.matrix,
                    geometry.primaries,
                    geometry.transfer,
                ) != (
                    first.geometry.width,
                    first.geometry.height,
                    first.geometry.full_range,
                    first.geometry.matrix,
                    first.geometry.primaries,
                    first.geometry.transfer,
                ) || props.color != first.color
            }) {
                return Err(Error::Unsupported("nonuniform grid tiles"));
            }
            let workspace_bytes = (reconstruction_bytes + payload_len as u64 * 3 + 1024 * 1024)
                .try_into()
                .map_err(|_| Error::LimitExceeded("codec workspace"))?;
            let _admission = budget.reserve(workspace_bytes, "tile reconstruction")?;
            tiles.push(Tile {
                item,
                config,
                geometry,
                color: props.color,
                payload_len,
                workspace_bytes,
            })?;
        }
        let tile = &tiles[0];
        if u64::from(tile.geometry.width) * u64::from(columns) < u64::from(width)
            || u64::from(tile.geometry.height) * u64::from(rows) < u64::from(height)
            || u64::from(tile.geometry.width) * u64::from(columns - 1) >= u64::from(width)
            || u64::from(tile.geometry.height) * u64::from(rows - 1) >= u64::from(height)
        {
            return Err(Error::Malformed("grid coverage"));
        }
        let mut exif = None;
        for reference in index.references.iter() {
            let required_source = reference.from == primary_id
                || tiles
                    .iter()
                    .any(|tile| index.items[tile.item].id == reference.from);
            let touches = required_source
                || reference.targets().any(|id| {
                    id == primary_id || tiles.iter().any(|tile| index.items[tile.item].id == id)
                });
            if !touches {
                continue;
            }
            match &reference.kind {
                b"dimg" if reference.from == primary_id => {}
                b"dimg" if !required_source => {}
                b"auxl" => {
                    let item = index.item(reference.from)?;
                    let mut found = false;
                    for (essential, property) in index.properties(item) {
                        if property.kind != *b"auxC" {
                            continue;
                        }
                        let mut r = Reader::new(property.data);
                        if r.full_box()? != (0, 0) {
                            return Err(Error::Unsupported("auxC version or flags"));
                        }
                        let value = &r.data[r.pos..];
                        let end = value
                            .iter()
                            .position(|&b| b == 0)
                            .ok_or(Error::Malformed("auxiliary type"))?;
                        let value = &value[..end];
                        if crate::ALPHA_AUX_TYPES.contains(&value) {
                            return Err(Error::Unsupported("alpha auxiliary"));
                        }
                        let known = [
                            b"urn:com:apple:photo:2020:aux:hdrgainmap".as_slice(),
                            b"urn:com:apple:photo:2020:aux:semanticskymatte",
                            b"urn:com:apple:photo:2018:aux:portraiteffectsmatte",
                            b"urn:com:apple:photo:2019:aux:semanticskinmatte",
                            b"tag:apple.com,2023:photo:aux:styledeltamap",
                            b"tag:apple.com,2023:photo:aux:linearthumbnail",
                            b"urn:mpeg:hevc:2015:auxid:2",
                            b"urn:iso:std:iso:ts:21496:-1",
                        ]
                        .contains(&value);
                        if essential && !known {
                            return Err(Error::Unsupported("required auxiliary type"));
                        }
                        found = true;
                    }
                    if !found {
                        return Err(Error::Malformed("auxiliary type missing"));
                    }
                }
                b"cdsc" => {
                    let item = index.item(reference.from)?;
                    if index.items[item].is_exif && reference.targets().any(|id| id == primary_id) {
                        let len = index.items[item]
                            .location
                            .ok_or(Error::Malformed("Exif location"))?
                            .length;
                        if len > 1024 * 1024 {
                            return Err(Error::LimitExceeded("Exif bytes"));
                        }
                        let mut bytes = budget.zeroed(len, "Exif")?;
                        index.read_item(item, source, &mut bytes)?;
                        let orientation = crate::parse_exif_orientation_from_item_payload(&bytes)
                            .and_then(|n| u8::try_from(n).ok())
                            .filter(|n| (1..=8).contains(n));
                        if exif.is_some() && exif != orientation {
                            return Err(Error::Malformed("conflicting Exif orientation"));
                        }
                        exif = orientation;
                    }
                }
                b"thmb" => {}
                _ => return Err(Error::Unsupported("required item dependency")),
            }
        }
        Ok(Self {
            width,
            height,
            columns,
            tiles,
            properties,
            exif,
        })
    }
}
