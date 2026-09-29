use super::*;

fn hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn nal_sets() -> [Vec<u8>; 4] {
    [
        hex("40010c01ffff0408000003009fa800000300001eba0240"),
        hex("4201010408000003009fa800000300001ea0884596eaaf2bc05a020000030002000003003210"),
        hex("4401c173c089"),
        hex("2801af78f85df7e170"),
    ]
}

fn box_bytes(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut result = ((data.len() + 8) as u32).to_be_bytes().to_vec();
    result.extend_from_slice(kind);
    result.extend_from_slice(data);
    result
}

fn full_box(kind: &[u8; 4], version: u8, data: &[u8]) -> Vec<u8> {
    box_bytes(kind, &[&[version, 0, 0, 0], data].concat())
}

fn configuration(sps: &[u8]) -> Vec<u8> {
    let nals = nal_sets();
    let mut result = vec![0; 23];
    result[0] = 1;
    result[21] = 3;
    result[22] = 3;
    for nal in [nals[0].as_slice(), sps, nals[2].as_slice()] {
        result.push((nal[0] >> 1) & 63);
        result.extend_from_slice(&1u16.to_be_bytes());
        result.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        result.extend_from_slice(nal);
    }
    result
}

fn fixture(configs: &[Vec<u8>], payload: &[u8], extra_properties: &[Vec<u8>]) -> Vec<u8> {
    fixture_with_exif(configs, payload, extra_properties, None)
}

fn fixture_with_exif(
    configs: &[Vec<u8>],
    payload: &[u8],
    extra_properties: &[Vec<u8>],
    orientation: Option<u8>,
) -> Vec<u8> {
    fixture_with_exif_info(configs, payload, extra_properties, orientation, b"Exif\0")
}

fn fixture_with_exif_info(
    configs: &[Vec<u8>],
    payload: &[u8],
    extra_properties: &[Vec<u8>],
    orientation: Option<u8>,
    exif_info: &[u8],
) -> Vec<u8> {
    let count = configs.len() as u16;
    let primary = count + 1;
    let item_count = primary + u16::from(orientation.is_some());
    let width = u32::from(count) * 16 - 1;
    let descriptor = [
        vec![0, 0, 0, count as u8 - 1],
        (width as u16).to_be_bytes().to_vec(),
        15u16.to_be_bytes().to_vec(),
    ]
    .concat();
    let payload = [&(payload.len() as u32).to_be_bytes()[..], payload].concat();
    let mut info = item_count.to_be_bytes().to_vec();
    let mut locations = vec![0x44, 0];
    locations.extend_from_slice(&item_count.to_be_bytes());
    let mut associations = u32::from(primary).to_be_bytes().to_vec();
    for id in 1..=primary {
        info.extend(full_box(
            b"infe",
            2,
            &[
                &id.to_be_bytes()[..],
                &[0, 0],
                if id == primary { b"grid" } else { b"hvc1" },
                &[0],
            ]
            .concat(),
        ));
        locations.extend_from_slice(&id.to_be_bytes());
        locations.extend_from_slice(&[0, 1, 0, 0, 0, 1]);
        locations.extend_from_slice(
            &(if id == primary { 0 } else { descriptor.len() } as u32).to_be_bytes(),
        );
        locations.extend_from_slice(
            &(if id == primary {
                descriptor.len()
            } else {
                payload.len()
            } as u32)
                .to_be_bytes(),
        );
        associations.extend_from_slice(&id.to_be_bytes());
        if id == primary {
            associations.push(1 + extra_properties.len() as u8);
            associations.push(0x82);
            for i in 0..extra_properties.len() {
                associations.push(0x80 | (3 + configs.len() + i) as u8);
            }
        } else {
            associations.extend_from_slice(&[2, 0x81, 0x80 | (id + 2) as u8]);
        }
    }
    let mut exif = Vec::new();
    if let Some(orientation) = orientation {
        exif = hex("0000000049492a0008000000010012010300010000000100000000000000");
        exif[22] = orientation;
        info.extend(full_box(
            b"infe",
            2,
            &[&item_count.to_be_bytes()[..], &[0, 0], exif_info].concat(),
        ));
        locations.extend_from_slice(&item_count.to_be_bytes());
        locations.extend_from_slice(&[0, 1, 0, 0, 0, 1]);
        locations.extend_from_slice(&((descriptor.len() + payload.len()) as u32).to_be_bytes());
        locations.extend_from_slice(&(exif.len() as u32).to_be_bytes());
    }
    let mut properties = full_box(
        b"ispe",
        0,
        &[16u32.to_be_bytes(), 16u32.to_be_bytes()].concat(),
    );
    properties.extend(full_box(
        b"ispe",
        0,
        &[width.to_be_bytes(), 15u32.to_be_bytes()].concat(),
    ));
    for config in configs {
        properties.extend(box_bytes(b"hvcC", config));
    }
    for property in extra_properties {
        properties.extend(property);
    }
    let mut references = [primary.to_be_bytes(), count.to_be_bytes()].concat();
    for id in 1..=count {
        references.extend_from_slice(&id.to_be_bytes());
    }
    let mut references = box_bytes(b"dimg", &references);
    if orientation.is_some() {
        references.extend(box_bytes(
            b"cdsc",
            &[
                item_count.to_be_bytes(),
                1u16.to_be_bytes(),
                primary.to_be_bytes(),
            ]
            .concat(),
        ));
    }
    let meta = [
        full_box(b"pitm", 0, &primary.to_be_bytes()),
        full_box(b"iinf", 0, &info),
        full_box(b"iloc", 1, &locations),
        box_bytes(
            b"iprp",
            &[
                box_bytes(b"ipco", &properties),
                full_box(b"ipma", 0, &associations),
            ]
            .concat(),
        ),
        full_box(b"iref", 0, &references),
        box_bytes(b"idat", &[descriptor, payload, exif].concat()),
    ]
    .concat();
    [
        box_bytes(b"ftyp", b"heic\0\0\0\0mif1heic"),
        full_box(b"meta", 0, &meta),
    ]
    .concat()
}

fn read_ue(bits: &[bool], position: &mut usize) -> u32 {
    let mut zeros = 0;
    while !bits[*position] {
        zeros += 1;
        *position += 1;
    }
    *position += 1;
    let mut value = 1;
    for _ in 0..zeros {
        value = (value << 1) | u32::from(bits[*position]);
        *position += 1;
    }
    value - 1
}

fn write_ue(bits: &mut Vec<bool>, value: u32) {
    let value = value + 1;
    let width = 32 - value.leading_zeros();
    bits.extend(std::iter::repeat_n(false, (width - 1) as usize));
    for i in (0..width).rev() {
        bits.push(value & (1 << i) != 0);
    }
}

fn huge_cropped_sps() -> Vec<u8> {
    let nals = nal_sets();
    let sps = crate::heic_decoder::hevc::bitstream::parse_single_nal(&nals[1]).unwrap();
    let mut bits: Vec<bool> = sps
        .payload
        .iter()
        .flat_map(|byte| (0..8).rev().map(move |i| byte & (1 << i) != 0))
        .collect();
    let mut cursor = 104;
    read_ue(&bits, &mut cursor);
    assert_eq!(read_ue(&bits, &mut cursor), 1);
    let start = cursor;
    assert_eq!(read_ue(&bits, &mut cursor), 16);
    assert_eq!(read_ue(&bits, &mut cursor), 16);
    assert!(!bits[cursor]);
    cursor += 1;
    let mut replacement = Vec::new();
    write_ue(&mut replacement, 16384);
    write_ue(&mut replacement, 16384);
    replacement.push(true);
    for crop in [0, 8184, 0, 8184] {
        write_ue(&mut replacement, crop);
    }
    bits.splice(start..cursor, replacement);
    while !bits.len().is_multiple_of(8) {
        bits.push(false);
    }
    let raw: Vec<u8> = bits
        .chunks_exact(8)
        .map(|b| b.iter().fold(0, |v, &bit| (v << 1) | u8::from(bit)))
        .collect();
    let mut result = vec![0x42, 1];
    let mut zeros = 0;
    for byte in raw {
        if zeros >= 2 && byte <= 3 {
            result.push(3);
            zeros = 0;
        }
        result.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    result
}

#[test]
fn rejects_oversized_first_and_late_coded_frames_before_reconstruction() {
    let nals = nal_sets();
    let small = configuration(&nals[1]);
    let huge = configuration(&huge_cropped_sps());
    for configs in [[huge.clone(), small.clone()], [small.clone(), huge.clone()]] {
        let bytes = fixture(&configs, &nals[3], &[]);
        let budget = memory::Budget::new(128 * 1024 * 1024);
        let mut source = container::Source::new(BoundedInput::Bytes(&bytes)).unwrap();
        let metadata = source.metadata(&budget).unwrap();
        let index = container::Index::parse(&metadata, source.len(), &budget).unwrap();
        assert!(matches!(
            grid::Grid::preflight(&index, &mut source, &budget),
            Err(BoundedDecodeError::MemoryBudgetExceeded {
                stage: "tile reconstruction",
                ..
            })
        ));
    }
}

#[test]
fn rejects_payload_parameter_set_replacement() {
    let nals = nal_sets();
    let config = configuration(&nals[1]);
    let huge = huge_cropped_sps();
    let payload = [
        &(huge.len() as u32).to_be_bytes()[..],
        &huge,
        &(nals[3].len() as u32).to_be_bytes(),
        &nals[3],
    ]
    .concat();
    assert!(matches!(
        codec::prepare(&config, &payload),
        Err(BoundedDecodeError::Unsupported(
            "replacement parameter sets"
        ))
    ));
}

#[test]
fn budget_reservations_survive_until_their_buffers_are_dropped() {
    let budget = memory::Budget::new(1024);
    let buffer = budget.zeroed::<u8>(1024, "test").unwrap();
    assert!(matches!(
        budget.zeroed::<u8>(1, "test"),
        Err(BoundedDecodeError::MemoryBudgetExceeded { .. })
    ));
    drop(buffer);
    assert_eq!(budget.available(), 1024);
}

#[test]
fn rejects_untrusted_container_counts_without_growing_indexes() {
    let nals = nal_sets();
    let original = fixture(&[configuration(&nals[1])], &nals[3], &[]);
    for (kind, relative, value) in [
        (b"iinf", 4, vec![255; 2]),
        (b"iloc", 6, vec![255; 2]),
        (b"ipma", 4, vec![255; 4]),
    ] {
        let mut bytes = original.clone();
        let offset = bytes.windows(4).position(|b| b == kind).unwrap() + 4 + relative;
        bytes[offset..offset + value.len()].copy_from_slice(&value);
        let budget = memory::Budget::new(64 * 1024);
        let mut source = container::Source::new(BoundedInput::Bytes(&bytes)).unwrap();
        let metadata = source.metadata(&budget).unwrap();
        assert!(matches!(
            container::Index::parse(&metadata, source.len(), &budget),
            Err(BoundedDecodeError::Malformed(_) | BoundedDecodeError::LimitExceeded(_))
        ));
        assert!(budget.available() > 60 * 1024);
    }
}

#[test]
fn rejects_required_unknown_properties_and_auxiliary_primaries() {
    let nals = nal_sets();
    for property in [
        box_bytes(b"junk", &[]),
        full_box(b"auxC", 0, b"urn:mpeg:hevc:2015:auxid:1\0"),
    ] {
        let bytes = fixture(&[configuration(&nals[1])], &nals[3], &[property]);
        let budget = memory::Budget::new(128 * 1024 * 1024);
        let mut source = container::Source::new(BoundedInput::Bytes(&bytes)).unwrap();
        let metadata = source.metadata(&budget).unwrap();
        let index = container::Index::parse(&metadata, source.len(), &budget).unwrap();
        assert!(matches!(
            grid::Grid::preflight(&index, &mut source, &budget),
            Err(BoundedDecodeError::Unsupported(_))
        ));
    }
}

#[test]
fn display_mapping_matches_existing_transform_plan() {
    let nals = nal_sets();
    let config = configuration(&nals[1]);
    for rotation in 0..4 {
        for mirror in 0..2 {
            let crop = [13u32, 1, 11, 1, 0, 1, 0, 1]
                .into_iter()
                .flat_map(u32::to_be_bytes)
                .collect::<Vec<_>>();
            let extras = [
                box_bytes(b"irot", &[rotation]),
                box_bytes(b"imir", &[mirror]),
                box_bytes(b"clap", &crop),
            ];
            let bytes = fixture(std::slice::from_ref(&config), &nals[3], &extras);
            let budget = memory::Budget::new(128 * 1024 * 1024);
            let mut source = container::Source::new(BoundedInput::Bytes(&bytes)).unwrap();
            let metadata = source.metadata(&budget).unwrap();
            let index = container::Index::parse(&metadata, source.len(), &budget).unwrap();
            let grid = grid::Grid::preflight(&index, &mut source, &budget).unwrap();
            let layout = resample::Layout::new(&grid, 6000).unwrap();
            let transforms = crate::isobmff::parse_primary_item_transform_properties(&bytes)
                .unwrap()
                .transforms;
            let reference =
                crate::RgbaTransformPlan::from_primary_transforms(15, 15, &transforms).unwrap();
            for y in 0..layout.height {
                for x in 0..layout.width {
                    let expected = reference
                        .map_source_pixel((x + layout.left) as usize, (y + layout.top) as usize)
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        layout.pixel_index(x, y),
                        (expected.1 * layout.display_width as usize + expected.0) * 3
                    );
                }
            }
        }
    }
}

#[test]
fn exif_orientation_applies_only_without_container_orientation() {
    let nals = nal_sets();
    for orientation in 1..=8 {
        for rotation in [None, Some(0), Some(1)] {
            let extras = rotation
                .map(|r| vec![box_bytes(b"irot", &[r])])
                .unwrap_or_default();
            let config = configuration(&nals[1]);
            let bytes = fixture_with_exif(
                &[config.clone(), config],
                &nals[3],
                &extras,
                Some(orientation),
            );
            let budget = memory::Budget::new(128 * 1024 * 1024);
            let mut source = container::Source::new(BoundedInput::Bytes(&bytes)).unwrap();
            let metadata = source.metadata(&budget).unwrap();
            let index = container::Index::parse(&metadata, source.len(), &budget).unwrap();
            let grid = grid::Grid::preflight(&index, &mut source, &budget).unwrap();
            let layout = resample::Layout::new(&grid, 6000).unwrap();
            let mut transforms = crate::isobmff::parse_primary_item_transform_properties(&bytes)
                .unwrap()
                .transforms;
            if let Some(orientation) = crate::exif_orientation_hint(&bytes).orientation_to_apply() {
                transforms.extend(
                    crate::exif_orientation_to_primary_item_transforms(u16::from(orientation))
                        .unwrap_or_default(),
                );
            }
            let reference =
                crate::RgbaTransformPlan::from_primary_transforms(31, 15, &transforms).unwrap();
            assert_eq!(
                layout.original_dimensions,
                (reference.destination_width, reference.destination_height)
            );
            for y in 0..15 {
                for x in 0..31 {
                    let (dx, dy) = reference.map_source_pixel(x, y).unwrap().unwrap();
                    assert_eq!(
                        layout.pixel_index(x as u32, y as u32),
                        (dy * layout.display_width as usize + dx) * 3
                    );
                }
            }
        }
    }
}

#[cfg(not(feature = "decoder-tracing"))]
#[test]
fn decodes_borrowed_grid_with_clipped_edges_and_scaling() {
    let nals = nal_sets();
    let config = configuration(&nals[1]);
    let bytes = fixture(&[config.clone(), config], &nals[3], &[]);
    let normal = crate::decode_bytes_to_rgb8(&bytes).unwrap();
    for max_side in [1, 7, 6000] {
        let decoded = decode_bounded(
            BoundedInput::Bytes(&bytes),
            BoundedDecodeOptions {
                max_side,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(decoded.original_dimensions, (31, 15));
        assert!(decoded.image.width.max(decoded.image.height) <= max_side);
        assert!(decoded.image.icc_profile.is_none());
        assert!(
            decoded
                .image
                .pixels
                .chunks_exact(3)
                .all(|pixel| pixel == &normal.pixels[..3])
        );
    }
}

#[cfg(not(feature = "decoder-tracing"))]
#[test]
fn truncated_and_mutated_small_grids_do_not_panic() {
    let nals = nal_sets();
    let crop: Vec<_> = [13u32, 1, 11, 1, 0, 1, 0, 1]
        .into_iter()
        .flat_map(u32::to_be_bytes)
        .collect();
    let bytes = fixture(
        &[configuration(&nals[1])],
        &nals[3],
        &[
            box_bytes(b"irot", &[1]),
            box_bytes(b"imir", &[0]),
            box_bytes(b"clap", &crop),
        ],
    );
    let options = BoundedDecodeOptions {
        max_side: 7,
        max_memory_bytes: 4 * 1024 * 1024,
    };
    for end in 0..bytes.len() {
        assert!(decode_bounded(BoundedInput::Bytes(&bytes[..end]), options).is_err());
    }
    for position in 0..bytes.len() {
        for mask in [0x80, 0xff] {
            let mut mutated = bytes.clone();
            mutated[position] ^= mask;
            assert!(
                std::panic::catch_unwind(|| decode_bounded(BoundedInput::Bytes(&mutated), options))
                    .is_ok(),
                "mutation at {position} with mask {mask}"
            );
        }
    }
}

#[cfg(not(feature = "decoder-tracing"))]
fn asymmetric_payload() -> Vec<u8> {
    hex(
        "2801af789d164be6c7c7d2a6f22d8319f0cf3e91b68f45f4fb739b6dc6ebf3645a7d962b1c0c064ddedfe6eac278cdb3cccd7d0156c51a326123fe7d0c5f129cb11eeaa86d984b1b8ce871e5681fe7396fde3baae7800bfdd51f79196d2f71e786c59b843792325b8f87dfb423affd1e30757e70ea414bb2abe61e72db48578e061d61d2ba91c5cc67eac035476f87aa9ee7edb58c0cb631a16608c56a4805e32f30291683a3de",
    )
}

#[cfg(not(feature = "decoder-tracing"))]
#[test]
fn mirrored_asymmetric_pixels_match_normal_decode() {
    let config = configuration(&nal_sets()[1]);
    for rotation in 0..4 {
        for mirror in 0..2 {
            let bytes = fixture(
                &[config.clone(), config.clone()],
                &asymmetric_payload(),
                &[
                    box_bytes(b"irot", &[rotation]),
                    box_bytes(b"imir", &[mirror]),
                ],
            );
            let normal = crate::decode_bytes_to_rgb8(&bytes).unwrap();
            assert!(
                normal
                    .pixels
                    .chunks_exact(3)
                    .any(|p| p != &normal.pixels[..3])
            );
            let bounded = decode_bounded(BoundedInput::Bytes(&bytes), Default::default()).unwrap();
            assert_eq!(
                (bounded.image.width, bounded.image.height),
                (normal.width, normal.height)
            );
            assert_eq!(bounded.image.pixels, normal.pixels);
        }
    }
}

#[cfg(not(feature = "decoder-tracing"))]
#[test]
fn public_decode_applies_associated_exif_after_identity_rotation() {
    let config = configuration(&nal_sets()[1]);
    for exif_info in [
        b"Exif\0".as_slice(),
        b"mimeeXiF\0application/octet-stream\0",
        b"mime\0APPLICATION/EXIF\0",
        b"mime\0IMAGE/TIFF\0",
    ] {
        for orientation in 2..=8 {
            let bytes = fixture_with_exif_info(
                &[config.clone(), config.clone()],
                &asymmetric_payload(),
                &[box_bytes(b"irot", &[0])],
                Some(orientation),
                exif_info,
            );
            let hint = crate::exif_orientation_hint(&bytes);
            assert_eq!(hint.orientation_to_apply(), Some(orientation));
            let normal = crate::decode_bytes_to_rgb8(&bytes).unwrap();
            let transforms =
                crate::exif_orientation_to_primary_item_transforms(u16::from(orientation)).unwrap();
            let plan = crate::RgbaTransformPlan::from_primary_transforms(
                normal.width,
                normal.height,
                &transforms,
            )
            .unwrap();
            let bounded = decode_bounded(BoundedInput::Bytes(&bytes), Default::default()).unwrap();
            assert_eq!(
                bounded.original_dimensions,
                (plan.destination_width, plan.destination_height)
            );
            for y in 0..bounded.image.height as usize {
                for x in 0..bounded.image.width as usize {
                    let (sx, sy) = plan.map_destination_pixel(x, y).unwrap();
                    let src = (sy * normal.width as usize + sx) * 3;
                    let dst = (y * bounded.image.width as usize + x) * 3;
                    assert_eq!(
                        &bounded.image.pixels[dst..dst + 3],
                        &normal.pixels[src..src + 3]
                    );
                }
            }
        }
    }
}

#[cfg(not(feature = "decoder-tracing"))]
#[test]
fn path_and_borrowed_bytes_produce_identical_output() {
    use std::io::Write;

    let config = configuration(&nal_sets()[1]);
    let bytes = fixture(&[config.clone(), config], &asymmetric_payload(), &[]);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("heic-bounded-{}-{nonce}.heic", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    file.write_all(&bytes).unwrap();
    drop(file);
    let actual = decode_bounded(BoundedInput::Path(&path), Default::default());
    std::fs::remove_file(&path).unwrap();
    let actual = actual.unwrap();
    let expected = decode_bounded(BoundedInput::Bytes(&bytes), Default::default()).unwrap();
    assert_eq!(actual.original_dimensions, expected.original_dimensions);
    assert_eq!(actual.image.width, expected.image.width);
    assert_eq!(actual.image.height, expected.image.height);
    assert_eq!(actual.image.pixels, expected.image.pixels);
}

#[test]
fn render_rejects_workspace_and_geometry_changes_after_preflight() {
    let nals = nal_sets();
    let bytes = fixture(&[configuration(&nals[1])], &nals[3], &[]);
    let budget = memory::Budget::new(128 * 1024 * 1024);
    let mut source = container::Source::new(BoundedInput::Bytes(&bytes)).unwrap();
    let metadata = source.metadata(&budget).unwrap();
    let index = container::Index::parse(&metadata, source.len(), &budget).unwrap();
    let mut grid = grid::Grid::preflight(&index, &mut source, &budget).unwrap();
    let tile = &mut grid.tiles[0];
    let mut payload = vec![0; tile.payload_len];
    index
        .read_item(tile.item, &mut source, &mut payload)
        .unwrap();
    let color = tile.color.clone();
    let transform = color::ColorTransform::new(
        &color,
        tile.geometry.primaries,
        tile.geometry.transfer,
        &budget,
    )
    .unwrap();
    assert_eq!(
        render_tile(tile, &payload, &color, &transform)
            .unwrap()
            .len(),
        16 * 16 * 3
    );
    let geometry = tile.geometry;
    tile.geometry.width += 1;
    assert!(matches!(
        render_tile(tile, &payload, &color, &transform),
        Err(BoundedDecodeError::Malformed(
            "coded headers changed after preflight"
        ))
    ));
    tile.geometry = geometry;
    tile.workspace_bytes = 0;
    assert!(matches!(
        render_tile(tile, &payload, &color, &transform),
        Err(BoundedDecodeError::Malformed(
            "codec workspace changed after preflight"
        ))
    ));
}

#[cfg(not(feature = "decoder-tracing"))]
#[test]
fn rejects_zero_clean_aperture_denominators_without_panicking() {
    let nals = nal_sets();
    for index in [1, 3, 5, 7] {
        let mut crop = [13u32, 1, 11, 1, 0, 1, 0, 1];
        crop[index] = 0;
        let crop: Vec<_> = crop.into_iter().flat_map(u32::to_be_bytes).collect();
        let bytes = fixture(
            &[configuration(&nals[1])],
            &nals[3],
            &[box_bytes(b"clap", &crop)],
        );
        assert!(matches!(
            decode_bounded(BoundedInput::Bytes(&bytes), Default::default()),
            Err(BoundedDecodeError::Malformed("clean aperture denominator"))
        ));
    }
}

#[cfg(not(feature = "decoder-tracing"))]
#[test]
fn rejects_unprofiled_wide_gamut_grid_before_output() {
    let sps =
        hex("4201010408000003009fa800000300001ea0884596eaaf2bc05a848684820000030002000003003210");
    let payload = hex("2801af78f70403fe65f7f877430938");
    let bytes = fixture(&[configuration(&sps)], &payload, &[]);
    assert!(matches!(
        decode_bounded(BoundedInput::Bytes(&bytes), Default::default()),
        Err(BoundedDecodeError::Unsupported("unprofiled HEVC color"))
    ));
    let bytes = fixture(
        &[configuration(&sps)],
        &payload,
        &[box_bytes(b"colr", b"prof")],
    );
    let normal = crate::decode_bytes_to_rgb8(&bytes).unwrap();
    let bounded = decode_bounded(BoundedInput::Bytes(&bytes), Default::default()).unwrap();
    assert_eq!(normal.icc_profile, Some(Vec::new()));
    assert_eq!(bounded.image.pixels, normal.pixels);
}
