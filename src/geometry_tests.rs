use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static LARGEST_ALLOCATION: Cell<Option<usize>> = const { Cell::new(None) };
    static FAIL_ALLOCATION_AT: Cell<Option<usize>> = const { Cell::new(None) };
}

struct TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if FAIL_ALLOCATION_AT.with(|limit| limit.get().is_some_and(|limit| layout.size() >= limit))
        {
            return std::ptr::null_mut();
        }
        LARGEST_ALLOCATION.with(|largest| {
            if let Some(previous) = largest.get() {
                largest.set(Some(previous.max(layout.size())));
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[test]
fn hevc_frame_allocation_failure_retains_bounded_error_variant() {
    FAIL_ALLOCATION_AT.with(|limit| limit.set(Some(512)));
    let result = HeicFrame::try_with_params(16, 16, 8, 1);
    FAIL_ALLOCATION_AT.with(|limit| limit.set(None));
    assert!(matches!(
        BoundedDecodeError::from(result.unwrap_err()),
        BoundedDecodeError::AllocationFailed
    ));
}

#[test]
fn hevc_frame_dimension_overflow_returns_an_error() {
    assert!(HeicFrame::try_with_params(u32::MAX, 2, 8, 1).is_err());
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

fn hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn tracked<T>(run: impl FnOnce() -> T) -> (T, usize) {
    LARGEST_ALLOCATION.with(|largest| largest.set(Some(0)));
    let result = run();
    let largest = LARGEST_ALLOCATION.with(|largest| largest.replace(None).unwrap());
    (result, largest)
}

fn boxed(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    [&((data.len() + 8) as u32).to_be_bytes()[..], kind, data].concat()
}

fn full_box(kind: &[u8; 4], version: u8, data: &[u8]) -> Vec<u8> {
    boxed(kind, &[&[version, 0, 0, 0], data].concat())
}

fn ispe(width: u32, height: u32) -> Vec<u8> {
    full_box(
        b"ispe",
        0,
        &[width.to_be_bytes(), height.to_be_bytes()].concat(),
    )
}

type FixtureItem = ((u32, u32), Vec<u8>, Vec<u8>);

fn image_fixture(
    kind: &[u8; 4],
    dimensions: (u32, u32),
    config: Vec<u8>,
    payload: Vec<u8>,
    auxiliary: Option<FixtureItem>,
) -> Vec<u8> {
    let count: u16 = if auxiliary.is_some() { 2 } else { 1 };
    let mut info = count.to_be_bytes().to_vec();
    let mut locations = [vec![0x44, 0], count.to_be_bytes().to_vec()].concat();
    let mut associations = u32::from(count).to_be_bytes().to_vec();
    let mut data = Vec::new();
    let mut properties = Vec::new();
    let mut items = vec![(dimensions, config, payload)];
    if let Some(auxiliary) = auxiliary {
        items.push(auxiliary);
    }
    for (index, (dimensions, config, payload)) in items.into_iter().enumerate() {
        let id = index as u16 + 1;
        info.extend(full_box(
            b"infe",
            2,
            &[&id.to_be_bytes()[..], &[0, 0], kind, &[0]].concat(),
        ));
        locations.extend_from_slice(&id.to_be_bytes());
        locations.extend_from_slice(&[0, 1, 0, 0, 0, 1]);
        locations.extend_from_slice(&(data.len() as u32).to_be_bytes());
        locations.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        data.extend(payload);
        associations.extend_from_slice(&id.to_be_bytes());
        associations.extend_from_slice(if id == 1 {
            &[2, 0x81, 0x82]
        } else {
            &[3, 0x83, 0x84, 0x85]
        });
        properties.extend(ispe(dimensions.0, dimensions.1));
        properties.extend(config);
    }
    if count == 2 {
        properties.extend(full_box(
            b"auxC",
            0,
            b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha\0",
        ));
    }
    let mut meta = [
        full_box(b"pitm", 0, &1u16.to_be_bytes()),
        full_box(b"iinf", 0, &info),
        full_box(b"iloc", 1, &locations),
        boxed(
            b"iprp",
            &[
                boxed(b"ipco", &properties),
                full_box(b"ipma", 0, &associations),
            ]
            .concat(),
        ),
        boxed(b"idat", &data),
    ]
    .concat();
    if count == 2 {
        meta.extend(full_box(
            b"iref",
            0,
            &boxed(
                b"auxl",
                &[2u16.to_be_bytes(), 1u16.to_be_bytes(), 1u16.to_be_bytes()].concat(),
            ),
        ));
    }
    [
        boxed(
            b"ftyp",
            &[
                if kind == b"av01" {
                    &b"avif"[..]
                } else {
                    &b"heic"[..]
                },
                &[0, 0, 0, 0],
                b"mif1",
            ]
            .concat(),
        ),
        full_box(b"meta", 0, &meta),
    ]
    .concat()
}

fn hevc_configuration(sps: Vec<u8>) -> Vec<u8> {
    let mut config = vec![0; 23];
    config[0] = 1;
    config[1] = 1;
    config[16] = 1;
    config[17] = 1;
    config[18] = 0xf8;
    config[19] = 0xf8;
    config[21] = 3;
    config[22] = 3;
    for nal in [
        hex("40010c01ffff0408000003009fa800000300001eba0240"),
        sps,
        hex("4401c173c089"),
    ] {
        config.push((nal[0] >> 1) & 63);
        config.extend_from_slice(&1u16.to_be_bytes());
        config.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        config.extend(nal);
    }
    boxed(b"hvcC", &config)
}

fn small_hevc_sps() -> Vec<u8> {
    hex("4201010408000003009fa800000300001ea0884596eaaf2bc05a020000030002000003003210")
}

fn large_hevc_sps() -> Vec<u8> {
    hex("42010103700000030090000003000003001ea010202075596566924cae0100000303e800000303e808")
}

fn hevc_payload() -> Vec<u8> {
    let slice = hex("2801af78f85df7e170");
    [&(slice.len() as u32).to_be_bytes()[..], &slice].concat()
}

#[test]
fn heic_alpha_rejects_coded_geometry_before_plane_allocation() {
    let input = image_fixture(
        b"hvc1",
        (16, 16),
        hevc_configuration(small_hevc_sps()),
        hevc_payload(),
        Some((
            (16, 16),
            hevc_configuration(large_hevc_sps()),
            hevc_payload(),
        )),
    );
    let (alpha, largest) = tracked(|| {
        decode_primary_heic_auxiliary_alpha_plane_internal(
            &input,
            &mut None,
            16,
            16,
            &DecodeGuardrails {
                max_pixels: Some(256),
                ..Default::default()
            },
        )
    });
    assert!(alpha.unwrap().is_none());
    assert!(
        largest < 128 * 128 * 2,
        "allocated a frame plane: {largest}"
    );
}

fn grid_fixture() -> isobmff::HeicGridPrimaryItemData {
    let configuration = hevc_configuration(large_hevc_sps());
    let hvcc = isobmff::parse_boxes(&configuration).unwrap()[0]
        .parse_hvcc()
        .unwrap();
    isobmff::HeicGridPrimaryItemData {
        item_id: 2,
        construction_method: 1,
        descriptor: isobmff::HeicGridDescriptor {
            version: 0,
            rows: 1,
            columns: 1,
            output_width: 16,
            output_height: 16,
        },
        colr: Default::default(),
        tile_item_ids: vec![1],
        tiles: vec![isobmff::HeicGridTileItemData {
            item_id: 1,
            construction_method: 1,
            hvcc,
            colr: Default::default(),
            transforms: vec![],
            payload: hevc_payload(),
        }],
    }
}

#[test]
fn heic_grid_rejects_oversized_first_tile_before_plane_allocation() {
    let grid = grid_fixture();
    let (result, largest) = tracked(|| decode_primary_heic_grid_to_image(&grid));
    assert!(result.is_err());
    assert!(
        largest < 128 * 128 * 2,
        "allocated a frame plane: {largest}"
    );
}

fn av1_payload() -> Vec<u8> {
    hex(
        "0a0a191ebffff8640808181032a6051230034d186081fe024bbd7cdc902b8d7b2198a3c4d5957f8508779db412f630d58e2f586aee73c279733d70bbf79a6c3f7cd8dc5bc4c62982cecea9341bfdf6063be969c9d9f69d44f40479d0c313bf993c5cc6c29f0ed3abd114e8533d14a3794ee5bf9fb04e2bc144eb17a66ff12e3c5032112ae0e9f1cfec19876265cd63b6311d94a598b40b4d6e88ef403777276cc936fea2ca79f20aeecf9c07f34ce7e99a4d60bb344c5f7622fe11824045f5a7e99d6dc9a0a52ee9b60849ca2610949e602f8f905b0a474aed603aa4f6fdf14dc25499ed41a095bde98727b494c94f90c75a6d42dc39bcadc1f0dea9229010ef7ff9121d2caac0b25f0d7b748b3261a94f03f8fe32a7cb35e99a8875ccc36620b42e8d348b0509ca325cac66a78b9928aafb6156a490527f0a4f1f65606d05d61c563cd4ab259d33a5985cb04f997e5afeb67ac0dab782b02ee244feb8ca1c02338ec5779c35b9296ef437da4d7b215f1750ecda87494c2f9060ced8e6ce48e4a27ca77b17be83d652b8ed151d377fe1146a33c70559948e805da49ee8458fd6e2aa0dfc91b41f81fc066f926d71513c4763121826e9379837ce290c57c50cca91c69bd279270f6c2894d2a674625975cedff2be70809bde67a4fc29dfe1936a97d8e2ac8524c647f556b9115d74d4f385a42161ab5fc914958c04239b8a5453339e195c08d25acce7f0f7db23299fab27349c78973b1872b2eb25fd121806a7aaff2a95c2bbd1e85ac8c29346e76e10921c1d0cc0b90da9a8e96c1e3278fec1319044ffff1b0c9245e800fc2c332ac1f815d963d671f859a2985ad56bef7961cb6716e5f95695d2c060d4438c354afc1d5544a4ba3216029091c7e59dd254e461714c48f7a9248c97af44fca436dc8119ad130815c9ab878ec2f700434928e0f8a424768164f0d9368621bd480e58373a2e57ef53",
    )
}

#[test]
fn avif_primary_rejects_coded_geometry_before_plane_allocation() {
    let input = image_fixture(
        b"av01",
        (16, 16),
        boxed(b"av1C", &[0x81, 0x04, 0x0c, 0]),
        av1_payload(),
        None,
    );
    let (result, largest) = tracked(|| {
        decode_bytes_to_rgba_with_guardrails(
            &input,
            DecodeGuardrails {
                max_pixels: Some(256),
                ..Default::default()
            },
        )
    });
    assert!(result.is_err());
    assert!(largest < 256 * 2048, "allocated a frame plane: {largest}");
}

#[test]
fn avif_alpha_rejects_coded_geometry_before_plane_allocation() {
    let input = image_fixture(
        b"av01",
        (16, 16),
        boxed(b"av1C", &[0x81, 0x04, 0x0c, 0]),
        av1_payload(),
        Some((
            (16, 16),
            boxed(b"av1C", &[0x81, 0x04, 0x0c, 0]),
            av1_payload(),
        )),
    );
    let (meta, resolved) = isobmff::resolve_primary_avif_item_graph(&input).unwrap();
    let (alpha, largest) = tracked(|| {
        decode_primary_avif_auxiliary_alpha_plane(
            &input,
            &mut None,
            &meta,
            &resolved,
            16,
            16,
            Some(256),
        )
    });
    assert!(alpha.is_none());
    assert!(largest < 256 * 2048, "allocated a frame plane: {largest}");
}

#[test]
fn heic_matching_alpha_is_preserved() {
    let input = image_fixture(
        b"hvc1",
        (16, 16),
        hevc_configuration(small_hevc_sps()),
        hevc_payload(),
        Some((
            (16, 16),
            hevc_configuration(small_hevc_sps()),
            hevc_payload(),
        )),
    );
    let alpha = decode_primary_heic_auxiliary_alpha_plane_internal(
        &input,
        &mut None,
        16,
        16,
        &DecodeGuardrails {
            max_pixels: Some(256),
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        (alpha.width, alpha.height, alpha.samples.len()),
        (16, 16, 256)
    );
}

#[test]
fn avif_matching_alpha_is_preserved() {
    let input = image_fixture(
        b"av01",
        (256, 2048),
        boxed(b"av1C", &[0x81, 0x04, 0x0c, 0]),
        av1_payload(),
        Some((
            (256, 2048),
            boxed(b"av1C", &[0x81, 0x04, 0x0c, 0]),
            av1_payload(),
        )),
    );
    let (meta, resolved) = isobmff::resolve_primary_avif_item_graph(&input).unwrap();
    let alpha = decode_primary_avif_auxiliary_alpha_plane(
        &input,
        &mut None,
        &meta,
        &resolved,
        256,
        2048,
        Some(256 * 2048),
    )
    .unwrap();
    assert_eq!((alpha.width, alpha.height), (256, 2048));
    assert!(
        matches!(alpha.samples, AvifPlaneSamples::U8(ref samples) if samples.len() == 256 * 2048)
    );
}

#[test]
fn avif_zero_pixel_limit_cannot_disable_backend_limit() {
    let input = image_fixture(
        b"av01",
        (16, 16),
        boxed(b"av1C", &[0x81, 0x04, 0x0c, 0]),
        av1_payload(),
        None,
    );
    let (result, largest) = tracked(|| {
        decode_bytes_to_rgba_with_guardrails(
            &input,
            DecodeGuardrails {
                max_pixels: Some(0),
                ..Default::default()
            },
        )
    });
    assert!(matches!(
        result,
        Err(DecodeError::Guardrail(
            DecodeGuardrailError::PixelCountExceeded { max_pixels: 0, .. }
        ))
    ));
    assert!(largest < 256 * 2048);
}

#[test]
fn avif_auxiliary_container_geometry_is_checked_before_backend() {
    let input = image_fixture(
        b"av01",
        (16, 16),
        boxed(b"av1C", &[0x81, 0x04, 0x0c, 0]),
        av1_payload(),
        Some((
            (256, 2048),
            boxed(b"av1C", &[0x81, 0x04, 0x0c, 0]),
            av1_payload(),
        )),
    );
    let (meta, resolved) = isobmff::resolve_primary_avif_item_graph(&input).unwrap();
    let (alpha, largest) = tracked(|| {
        decode_primary_avif_auxiliary_alpha_plane(
            &input,
            &mut None,
            &meta,
            &resolved,
            16,
            16,
            Some(256),
        )
    });
    assert!(alpha.is_none());
    assert!(
        largest < 4096,
        "entered backend before container check: {largest}"
    );
}

#[test]
fn heic_grid_tile_crop_cannot_hide_oversized_coded_frame() {
    let mut grid = grid_fixture();
    grid.tiles[0]
        .transforms
        .push(isobmff::PrimaryItemTransformProperty::CleanAperture(
            isobmff::ImageCleanApertureProperty {
                clean_aperture_width_num: 16,
                clean_aperture_width_den: 1,
                clean_aperture_height_num: 16,
                clean_aperture_height_den: 1,
                horizontal_offset_num: 0,
                horizontal_offset_den: 1,
                vertical_offset_num: 0,
                vertical_offset_den: 1,
            },
        ));
    let (result, largest) = tracked(|| decode_primary_heic_grid_to_image(&grid));
    assert!(result.is_err());
    assert!(
        largest < 128 * 128 * 2,
        "tile crop hid coded frame allocation: {largest}"
    );
}

#[test]
fn heic_grid_checks_later_tiles_before_decoding_first_tile() {
    let mut grid = grid_fixture();
    grid.descriptor.columns = 2;
    grid.descriptor.output_width = 256;
    grid.descriptor.output_height = 126;
    let big = hex(
        "420101037000000300900000030000030078a007d200539f596ea4929ae6e021a0c08000000c800000030084",
    );
    let config = hevc_configuration(big);
    let mut second = grid.tiles[0].clone();
    second.item_id = 3;
    second.hvcc = isobmff::parse_boxes(&config).unwrap()[0]
        .parse_hvcc()
        .unwrap();
    grid.tile_item_ids.push(3);
    grid.tiles.push(second);
    let (result, largest) = tracked(|| decode_primary_heic_grid_to_image(&grid));
    assert!(result.is_err());
    assert!(
        largest < 128 * 128 * 2,
        "first tile decoded before later preflight: {largest}"
    );
}

#[test]
fn heic_grid_checks_replacement_sps_in_payload() {
    let mut grid = grid_fixture();
    let config = hevc_configuration(small_hevc_sps());
    grid.tiles[0].hvcc = isobmff::parse_boxes(&config).unwrap()[0]
        .parse_hvcc()
        .unwrap();
    let replacement = large_hevc_sps();
    grid.tiles[0].payload = [
        &(replacement.len() as u32).to_be_bytes()[..],
        &replacement,
        &hevc_payload(),
    ]
    .concat();
    let (result, largest) = tracked(|| decode_primary_heic_grid_to_image(&grid));
    assert!(result.is_err());
    assert!(largest < 128 * 128 * 2);
}

#[test]
fn heic_grid_preflight_alignment_does_not_overflow() {
    let mut grid = grid_fixture();
    grid.descriptor.output_width = u32::MAX;
    grid.descriptor.output_height = u32::MAX;
    assert!(geometry::preflight_grid(&grid, &DecodeGuardrails::default()).is_ok());
}

#[test]
fn heic_auxiliary_allocation_failure_is_propagated() {
    let input = image_fixture(
        b"hvc1",
        (126, 126),
        hevc_configuration(large_hevc_sps()),
        hevc_payload(),
        Some((
            (126, 126),
            hevc_configuration(large_hevc_sps()),
            hevc_payload(),
        )),
    );
    FAIL_ALLOCATION_AT.with(|limit| limit.set(Some(128 * 128 * 2)));
    let result = decode_primary_heic_auxiliary_alpha_plane_internal(
        &input,
        &mut None,
        126,
        126,
        &DecodeGuardrails::default(),
    );
    FAIL_ALLOCATION_AT.with(|limit| limit.set(None));
    assert!(matches!(
        result,
        Err(DecodeHeicError::AllocationFailed { .. })
    ));
}

#[test]
fn heic_copied_plane_allocation_failure_is_a_resource_error() {
    let source = vec![0u16; 256];
    FAIL_ALLOCATION_AT.with(|limit| limit.set(Some(512)));
    let result = extract_cropped_heic_plane(&source, 16, 0, 0, 16, 16, "alpha");
    FAIL_ALLOCATION_AT.with(|limit| limit.set(None));
    assert!(matches!(
        result,
        Err(DecodeHeicError::AllocationFailed { .. })
    ));
}

#[test]
fn heic_alpha_checks_decoded_bytes_before_allocating_planes() {
    let config = hevc_configuration(small_hevc_sps());
    let hvcc = isobmff::parse_boxes(&config).unwrap()[0]
        .parse_hvcc()
        .unwrap();
    let oversized_nal = vec![0x7fu8; 40 * 1024 * 1024];
    let payload = [
        &(oversized_nal.len() as u32).to_be_bytes()[..],
        &oversized_nal,
    ]
    .concat();
    let result = geometry::preflight_hevc(
        &hvcc,
        &payload,
        Some((16, 16)),
        &DecodeGuardrails {
            max_pixels: Some(256),
            ..Default::default()
        },
    );
    assert!(matches!(
        result,
        Err(DecodeError::Guardrail(
            DecodeGuardrailError::DecodedByteCountExceeded { .. }
        ))
    ));
}

fn grid_with_filler(tile_count: u16, filler_bytes: usize) -> isobmff::HeicGridPrimaryItemData {
    let mut grid = grid_fixture();
    let config = hevc_configuration(small_hevc_sps());
    grid.tiles[0].hvcc = isobmff::parse_boxes(&config).unwrap()[0]
        .parse_hvcc()
        .unwrap();
    let mut filler = vec![0xffu8; filler_bytes];
    filler[..2].copy_from_slice(&[0x4c, 0x01]);
    *filler.last_mut().unwrap() = 0x80;
    grid.tiles[0].payload = [
        &(filler.len() as u32).to_be_bytes()[..],
        &filler,
        &hevc_payload(),
    ]
    .concat();
    grid.descriptor.columns = tile_count;
    grid.descriptor.output_width = 16 * u32::from(tile_count);
    for index in 1..tile_count {
        let mut tile = grid.tiles[0].clone();
        tile.item_id = u32::from(index) + 3;
        grid.tile_item_ids.push(tile.item_id);
        grid.tiles.push(tile);
    }
    grid
}

#[test]
fn heic_grid_first_tile_uses_decoded_byte_budget() {
    let grid = grid_with_filler(1, 24 * 1024 * 1024);
    let result = geometry::preflight_grid(&grid, &DecodeGuardrails::default());
    assert!(matches!(
        result,
        Err(DecodeError::Guardrail(
            DecodeGuardrailError::DecodedByteCountExceeded {
                stage: "HEVC tile",
                ..
            }
        ))
    ));
}

#[test]
fn heic_grid_aggregate_budget_includes_every_tile() {
    let grid = grid_with_filler(2, 12 * 1024 * 1024);
    let result = geometry::preflight_grid(&grid, &DecodeGuardrails::default());
    assert!(matches!(
        result,
        Err(DecodeError::Guardrail(
            DecodeGuardrailError::DecodedByteCountExceeded {
                stage: "HEIC grid",
                ..
            }
        ))
    ));
}

#[test]
fn avif_coded_geometry_mismatch_is_checked_without_a_pixel_limit() {
    let input = image_fixture(
        b"av01",
        (16, 16),
        boxed(b"av1C", &[0x81, 0x04, 0x0c, 0]),
        av1_payload(),
        None,
    );
    let (result, largest) =
        tracked(|| decode_bytes_to_rgba_with_guardrails(&input, DecodeGuardrails::default()));
    assert!(matches!(
        result,
        Err(DecodeError::AvifDecode(
            DecodeAvifError::DecodedGeometryMismatch {
                expected_width: 16,
                expected_height: 16,
                actual_width: 256,
                actual_height: 2048
            }
        ))
    ));
    assert!(largest < 256 * 2048);
}
