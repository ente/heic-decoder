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
            hvcc: hvcc.into(),
            colr: Default::default(),
            transforms: Default::default(),
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
    grid.tiles[0].transforms = [isobmff::PrimaryItemTransformProperty::CleanAperture(
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
    )]
    .into();
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
        .unwrap()
        .into();
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
        .unwrap()
        .into();
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

fn with_allocations_failing_at<T>(size: usize, run: impl FnOnce() -> T) -> T {
    FAIL_ALLOCATION_AT.with(|limit| limit.set(Some(size)));
    let result = run();
    FAIL_ALLOCATION_AT.with(|limit| limit.set(None));
    result
}

#[test]
fn uncompressed_channel_allocation_failure_is_a_resource_error() {
    let payload = vec![0_u8; 128 * 1024];
    let file = isobmff::test_support::minimal_uncompressed_heif_custom_layout(
        &[
            isobmff::test_support::ispe_box(1024, 1024),
            isobmff::test_support::cmpd_box(&[0]),
            isobmff::test_support::uncc_v0_box(&[(0, 0)], 0, UNCOMPRESSED_INTERLEAVE_PIXEL),
        ],
        &payload,
    );

    let result = with_allocations_failing_at(1024 * 1024, || {
        decode_primary_uncompressed_to_channels_internal(&file, &mut None)
    });

    assert!(matches!(
        result,
        Err(DecodeUncompressedError::ResourceLimit { .. })
    ));
}

fn plain_mono8_file(width: u32, height: u32) -> Vec<u8> {
    isobmff::test_support::minimal_uncompressed_heif_custom_layout(
        &[
            isobmff::test_support::ispe_box(width, height),
            isobmff::test_support::cmpd_box(&[0]),
            isobmff::test_support::uncc_v0_box(&[(0, 7)], 0, UNCOMPRESSED_INTERLEAVE_PIXEL),
        ],
        &vec![0_u8; width as usize * height as usize],
    )
}

#[test]
fn uncompressed_rgba_allocation_failure_is_a_resource_error() {
    let file = plain_mono8_file(512, 512);

    let result =
        with_allocations_failing_at(1024 * 1024, || decode_primary_uncompressed_to_image(&file));

    assert!(matches!(
        result,
        Err(DecodeUncompressedError::ResourceLimit { .. })
    ));
}

#[test]
fn uncompressed_payload_allocation_failure_is_a_resource_error() {
    let file = plain_mono8_file(1024, 1024);

    let result = with_allocations_failing_at(1024 * 1024, || {
        decode_primary_uncompressed_to_channels_internal(&file, &mut None).map(|_| ())
    });

    let error = result.expect_err("payload allocation must fail");
    assert!(matches!(
        error,
        DecodeUncompressedError::ExtractPrimaryPayload(
            isobmff::ExtractUncompressedItemDataError::PayloadAllocationFailed { .. }
        )
    ));
    assert_eq!(error.category(), DecodeErrorCategory::ResourceLimit);
}

#[test]
fn uncompressed_tile_scratch_allocation_failure_is_a_resource_error() {
    let payload = vec![0_u8; 1024 * 1024];
    let specs = [UncompressedComponentDecodeSpec {
        role: UncompressedChannelRole::Monochrome,
        bit_depth: 8,
        component_align_size: 0,
    }];
    let tile_layout = UncompressedTileDecodeLayout {
        tile_rows: 1,
        tile_cols: 1,
        tile_width: 1024,
        tile_height: 1024,
        image_width: 1024,
        row_align_size: 0,
        tile_align_size: 0,
    };
    let mut channel_samples: [Option<Vec<u16>>; UNCOMPRESSED_CHANNEL_COUNT] =
        std::array::from_fn(|_| None);
    channel_samples[UNCOMPRESSED_CHANNEL_MONO] = Some(vec![0_u16; 1024 * 1024]);

    let result = with_allocations_failing_at(1024 * 1024, || {
        decode_uncompressed_tile_component_interleave(
            &payload,
            &specs,
            tile_layout,
            UNCOMPRESSED_SAMPLING_NO_SUBSAMPLING,
            &mut channel_samples,
        )
    });

    assert!(matches!(
        result,
        Err(DecodeUncompressedError::ResourceLimit { .. })
    ));
}

#[test]
fn uncompressed_sample_conversion_allocation_failure_is_a_resource_error() {
    for bit_depth in [8, 10] {
        let decoded = DecodedUncompressedImage {
            width: 512,
            height: 512,
            bit_depth,
            rgba: vec![0_u16; 1024 * 1024],
            icc_profile: None,
        };

        let result = with_allocations_failing_at(1024 * 1024, || {
            decoded_uncompressed_to_rgba_image(decoded, &[])
        });

        let error = result.expect_err("conversion allocation must fail");
        assert_eq!(error.category(), DecodeErrorCategory::ResourceLimit);
    }
}

#[test]
fn rgba_transform_allocation_failure_is_a_resource_error() {
    let pixels = vec![0_u8; 4 * 1024 * 1024];
    let crop_source = pixels.clone();
    let clean_aperture = isobmff::ImageCleanApertureProperty {
        clean_aperture_width_num: 1024,
        clean_aperture_width_den: 1,
        clean_aperture_height_num: 768,
        clean_aperture_height_den: 1,
        horizontal_offset_num: 0,
        horizontal_offset_den: 1,
        vertical_offset_num: 0,
        vertical_offset_den: 1,
    };

    let results = with_allocations_failing_at(3 * 1024 * 1024, || {
        [
            rotate_rgba_ccw(1024, 1024, &pixels, 0).map(|_| ()),
            rotate_rgba_ccw(1024, 1024, &pixels, 90).map(|_| ()),
            mirror_rgba(
                1024,
                1024,
                &pixels,
                isobmff::ImageMirrorDirection::Horizontal,
            )
            .map(|_| ()),
            crop_rgba_by_clean_aperture(1024, 1024, crop_source, clean_aperture).map(|_| ()),
        ]
    });

    for result in results {
        let error = result.expect_err("transform allocation must fail");
        assert_eq!(error.category(), DecodeErrorCategory::ResourceLimit);
    }
}

#[test]
fn large_window_brotli_is_rejected_before_its_ring_buffer_is_requested() {
    let stream = [&[0x11_u8, 0x1E, 0x1E, 0x00, 0x02][..], &[7; 16], &[0]].concat();
    let file =
        isobmff::test_support::minimal_uncompressed_rgb3_heif_generic_compression(b"brot", &stream);

    let (result, largest_request) = tracked(|| decode_primary_uncompressed_to_image(&file));

    assert!(largest_request < 1024 * 1024);
    let error = result
        .map(|_| ())
        .expect_err("large-window stream must be rejected");
    assert_eq!(error.category(), DecodeErrorCategory::UnsupportedFeature);
}

#[test]
fn generic_compression_reserve_failure_is_a_resource_error() {
    let properties = crate::tests::zlib_full_item_properties();
    let compressed = crate::tests::zlib_compress(&[0_u8; 8]);

    let result = with_allocations_failing_at(1024 * 1024, || {
        maybe_decode_primary_uncompressed_generic_compression_payload(
            1,
            &properties,
            &compressed,
            || Ok(GENERIC_COMPRESSION_LIMITS),
        )
    });

    assert!(matches!(
        result,
        Err(DecodeUncompressedError::ResourceLimit { .. })
    ));
}

#[test]
fn generic_compression_growth_failure_is_a_resource_error() {
    let properties = crate::tests::zlib_full_item_properties();
    let compressed = crate::tests::zlib_compress(&vec![0_u8; 4 * 1024 * 1024]);

    let result = with_allocations_failing_at(2 * 1024 * 1024, || {
        maybe_decode_primary_uncompressed_generic_compression_payload(
            1,
            &properties,
            &compressed,
            || Ok(GENERIC_COMPRESSION_LIMITS),
        )
    });

    assert!(matches!(
        result,
        Err(DecodeUncompressedError::ResourceLimit { .. })
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
        .unwrap()
        .into();
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

fn bit_reader_ue(bits: &[bool], position: &mut usize) -> u32 {
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

fn bit_writer_ue(bits: &mut Vec<bool>, value: u32) {
    let value = value + 1;
    let width = 32 - value.leading_zeros();
    bits.extend(std::iter::repeat_n(false, (width - 1) as usize));
    for i in (0..width).rev() {
        bits.push(value & (1 << i) != 0);
    }
}

fn hidden_geometry_sps() -> Vec<u8> {
    let base = small_hevc_sps();
    let sps = crate::heic_decoder::hevc::bitstream::parse_single_nal(&base).unwrap();
    let mut bits: Vec<bool> = sps
        .payload
        .iter()
        .flat_map(|byte| (0..8).rev().map(move |i| byte & (1 << i) != 0))
        .collect();
    let mut cursor = 104;
    bit_reader_ue(&bits, &mut cursor);
    assert_eq!(bit_reader_ue(&bits, &mut cursor), 1);
    let start = cursor;
    assert_eq!(bit_reader_ue(&bits, &mut cursor), 16);
    assert_eq!(bit_reader_ue(&bits, &mut cursor), 16);
    assert!(!bits[cursor]);
    cursor += 1;
    let mut replacement = Vec::new();
    bit_writer_ue(&mut replacement, 16384);
    bit_writer_ue(&mut replacement, 16384);
    replacement.push(true);
    for crop in [0, 8184, 0, 8184] {
        bit_writer_ue(&mut replacement, crop);
    }
    bits.splice(start..cursor, replacement);
    while !bits.len().is_multiple_of(8) {
        bits.push(false);
    }
    let raw: Vec<u8> = bits
        .chunks_exact(8)
        .map(|b| b.iter().fold(0, |v, &bit| (v << 1) | u8::from(bit)))
        .collect();
    let mut result = base[..2].to_vec();
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

fn junk_nal_with_hidden_parameter_sets(length: usize) -> Vec<u8> {
    let mut nal = [
        &[0x4eu8, 0x01][..],
        &[0, 0, 0, 1][..],
        &hidden_geometry_sps()[..],
        &[0, 0, 1][..],
        &hex("4401c173c089")[..],
    ]
    .concat();
    nal.resize(length, 0xff);
    nal
}

fn hevc_configuration_with_leading_nals(sps: Vec<u8>, leading: &[Vec<u8>]) -> Vec<u8> {
    let mut config = vec![0u8; 23];
    config[0] = 1;
    config[1] = 1;
    config[16] = 1;
    config[17] = 1;
    config[18] = 0xf8;
    config[19] = 0xf8;
    config[21] = 3;
    config[22] = (leading.len() + 3) as u8;
    for nal in leading.iter().chain([
        &hex("40010c01ffff0408000003009fa800000300001eba0240"),
        &sps,
        &hex("4401c173c089"),
    ]) {
        config.push((nal[0] >> 1) & 63);
        config.extend_from_slice(&1u16.to_be_bytes());
        config.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        config.extend_from_slice(nal);
    }
    boxed(b"hvcC", &config)
}

#[test]
fn hidden_parameter_sets_cannot_bypass_pixel_limits() {
    let leading = vec![junk_nal_with_hidden_parameter_sets(300)];
    let input = image_fixture(
        b"hvc1",
        (16, 16),
        hevc_configuration_with_leading_nals(small_hevc_sps(), &leading),
        hevc_payload(),
        None,
    );
    let clean = image_fixture(
        b"hvc1",
        (16, 16),
        hevc_configuration(small_hevc_sps()),
        hevc_payload(),
        None,
    );
    let guardrails = DecodeGuardrails {
        max_pixels: Some(1024),
        ..DecodeGuardrails::default()
    };
    let (decoded, largest) =
        tracked(|| decode_bytes_to_rgba_with_guardrails(&input, guardrails.clone()).unwrap());
    assert_eq!(decoded.pixels, decode_bytes_to_rgba(&clean).unwrap().pixels);
    assert!(
        largest < 1024 * 1024,
        "hidden parameter sets allocated {largest} bytes past the pixel limit"
    );
}

#[test]
fn hevc_decode_uses_fixed_length_prefixed_framing_at_first_nal_boundaries() {
    let clean = image_fixture(
        b"hvc1",
        (16, 16),
        hevc_configuration(small_hevc_sps()),
        hevc_payload(),
        None,
    );
    let expected = decode_bytes_to_rgba(&clean).unwrap();
    for length in [1usize, 255, 256, 511, 512] {
        let mut junk = vec![0x4eu8, 0x01];
        junk.resize(length, 0xff);
        let mut leading = vec![junk];
        if length == 1 {
            leading.push(junk_nal_with_hidden_parameter_sets(300));
        }
        let input = image_fixture(
            b"hvc1",
            (16, 16),
            hevc_configuration_with_leading_nals(small_hevc_sps(), &leading),
            hevc_payload(),
            None,
        );
        let (decoded, largest) = tracked(|| {
            decode_bytes_to_rgba_with_guardrails(
                &input,
                DecodeGuardrails {
                    max_pixels: Some(1024),
                    ..DecodeGuardrails::default()
                },
            )
            .unwrap()
        });
        assert_eq!(decoded.pixels, expected.pixels, "first NAL length {length}");
        assert!(
            largest < 1024 * 1024,
            "first NAL length {length} allocated {largest} bytes"
        );
    }
}

#[test]
fn hidden_parameter_sets_in_auxiliary_alpha_stay_bounded() {
    let leading_alpha_configuration = vec![junk_nal_with_hidden_parameter_sets(300)];
    let input = image_fixture(
        b"hvc1",
        (16, 16),
        hevc_configuration(small_hevc_sps()),
        hevc_payload(),
        Some((
            (16, 16),
            hevc_configuration_with_leading_nals(small_hevc_sps(), &leading_alpha_configuration),
            hevc_payload(),
        )),
    );
    let clean = image_fixture(
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
    let (decoded, largest) = tracked(|| {
        decode_bytes_to_rgba_with_guardrails(
            &input,
            DecodeGuardrails {
                max_pixels: Some(1024),
                ..DecodeGuardrails::default()
            },
        )
        .unwrap()
    });
    assert_eq!(
        decoded.pixels,
        decode_bytes_to_rgba_with_guardrails(
            &clean,
            DecodeGuardrails {
                max_pixels: Some(1024),
                ..DecodeGuardrails::default()
            }
        )
        .unwrap()
        .pixels
    );
    assert!(
        largest < 1024 * 1024,
        "hidden alpha parameter sets allocated {largest} bytes"
    );
}

#[test]
fn grid_tile_streams_with_hidden_parameter_sets_decode_strictly() {
    let leading = vec![junk_nal_with_hidden_parameter_sets(300)];
    let config = hevc_configuration_with_leading_nals(small_hevc_sps(), &leading);
    let tile = isobmff::HeicGridTileItemData {
        item_id: 1,
        construction_method: 1,
        hvcc: isobmff::parse_boxes(&config).unwrap()[0]
            .parse_hvcc()
            .unwrap()
            .into(),
        colr: Default::default(),
        transforms: Default::default(),
        payload: hevc_payload(),
    };
    let (decoded, largest) = tracked(|| crate::decode_heic_grid_tile_to_image(&tile).unwrap());
    assert_eq!((decoded.width, decoded.height), (16, 16));
    assert!(
        largest < 1024 * 1024,
        "hidden tile parameter sets allocated {largest} bytes"
    );
}

#[test]
fn rgb8_coded_route_rejects_visible_oversized_sps_before_allocation() {
    let oversized = image_fixture(
        b"hvc1",
        (16, 16),
        hevc_configuration(hidden_geometry_sps()),
        hevc_payload(),
        None,
    );
    let guardrails = DecodeGuardrails {
        max_pixels: Some(1024),
        ..DecodeGuardrails::default()
    };
    let (result, largest) =
        tracked(|| decode_bytes_to_rgb8_with_guardrails(&oversized, guardrails));
    assert!(matches!(
        result,
        Err(DecodeError::Guardrail(
            DecodeGuardrailError::PixelCountExceeded { .. }
        ))
    ));
    assert!(
        largest < 1024 * 1024,
        "oversized coded frame allocated {largest} bytes before the pixel limit"
    );
    let clean = image_fixture(
        b"hvc1",
        (16, 16),
        hevc_configuration(small_hevc_sps()),
        hevc_payload(),
        None,
    );
    let decoded = decode_bytes_to_rgb8_with_guardrails(
        &clean,
        DecodeGuardrails {
            max_pixels: Some(1024),
            ..DecodeGuardrails::default()
        },
    )
    .unwrap();
    assert_eq!((decoded.width, decoded.height), (16, 16));
}

#[test]
fn rgb8_path_route_rejects_visible_oversized_sps_before_allocation() {
    let oversized = image_fixture(
        b"hvc1",
        (16, 16),
        hevc_configuration(hidden_geometry_sps()),
        hevc_payload(),
        None,
    );
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("heic-rgb8-{}-{nonce}.heic", std::process::id()));
    std::fs::write(&path, oversized).unwrap();
    let (result, largest) = tracked(|| {
        decode_path_to_rgb8_with_guardrails(
            &path,
            DecodeGuardrails {
                max_pixels: Some(1024),
                ..DecodeGuardrails::default()
            },
        )
    });
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(
        result,
        Err(DecodeError::Guardrail(
            DecodeGuardrailError::PixelCountExceeded { .. }
        ))
    ));
    assert!(
        largest < 1024 * 1024,
        "oversized coded frame allocated {largest} bytes before the pixel limit"
    );
}

fn assert_backend_decode_failure(input: &[u8], expected: &str) {
    for failure in [
        decode_bytes_to_rgba(input).err(),
        decode_bytes_to_rgb8(input).err(),
    ] {
        assert!(
            matches!(
                &failure,
                Some(DecodeError::HeicDecode(DecodeHeicError::BackendDecodeFailed { detail }))
                    if detail.contains(expected)
            ),
            "{failure:?}"
        );
    }
}

fn hevc_payload_with_nal_header(header: [u8; 2]) -> Vec<u8> {
    let mut payload = hevc_payload();
    payload[4..6].copy_from_slice(&header);
    payload
}

#[test]
fn streams_without_a_base_layer_slice_fail_instead_of_returning_a_blank_frame() {
    for header in [[0x28, 0x09], [0x2c, 0x01], [0xa8, 0x01], [0x28, 0x00]] {
        let input = image_fixture(
            b"hvc1",
            (16, 16),
            hevc_configuration(small_hevc_sps()),
            hevc_payload_with_nal_header(header),
            None,
        );
        assert_backend_decode_failure(&input, "no base layer slice");
    }
}

#[test]
fn enhancement_layer_nal_units_do_not_hide_the_base_layer_slice() {
    let input = image_fixture(
        b"hvc1",
        (16, 16),
        hevc_configuration(small_hevc_sps()),
        [hevc_payload_with_nal_header([0x28, 0x09]), hevc_payload()].concat(),
        None,
    );
    let clean = image_fixture(
        b"hvc1",
        (16, 16),
        hevc_configuration(small_hevc_sps()),
        hevc_payload(),
        None,
    );
    assert_eq!(
        decode_bytes_to_rgba(&input).unwrap().pixels,
        decode_bytes_to_rgba(&clean).unwrap().pixels
    );
}

#[test]
fn slices_ending_before_the_last_ctu_fail_instead_of_returning_blank_ctus() {
    let input = image_fixture(
        b"hvc1",
        (32, 16),
        hevc_configuration(sps_with_tile_dimensions(32, 16)),
        hevc_payload(),
        None,
    );
    assert_backend_decode_failure(&input, "slice ended before the last CTU");
}

fn sps_with_tile_dimensions(width: u32, height: u32) -> Vec<u8> {
    let base = small_hevc_sps();
    let sps = crate::heic_decoder::hevc::bitstream::parse_single_nal(&base).unwrap();
    let mut bits: Vec<bool> = sps
        .payload
        .iter()
        .flat_map(|byte| (0..8).rev().map(move |i| byte & (1 << i) != 0))
        .collect();
    let mut cursor = 104;
    bit_reader_ue(&bits, &mut cursor);
    assert_eq!(bit_reader_ue(&bits, &mut cursor), 1);
    let start = cursor;
    assert_eq!(bit_reader_ue(&bits, &mut cursor), 16);
    assert_eq!(bit_reader_ue(&bits, &mut cursor), 16);
    assert!(!bits[cursor]);
    cursor += 1;
    let mut replacement = Vec::new();
    bit_writer_ue(&mut replacement, width);
    bit_writer_ue(&mut replacement, height);
    replacement.push(false);
    bits.splice(start..cursor, replacement);
    while !bits.len().is_multiple_of(8) {
        bits.push(false);
    }
    let raw: Vec<u8> = bits
        .chunks_exact(8)
        .map(|b| b.iter().fold(0, |v, &bit| (v << 1) | u8::from(bit)))
        .collect();
    let mut result = base[..2].to_vec();
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

fn tiled_grid(
    columns: u16,
    rows: u16,
    output: (u32, u32),
    sps: Vec<u8>,
) -> isobmff::HeicGridPrimaryItemData {
    let config = hevc_configuration(sps);
    let hvcc = isobmff::parse_boxes(&config).unwrap()[0]
        .parse_hvcc()
        .unwrap();
    let first = isobmff::HeicGridTileItemData {
        item_id: 1,
        construction_method: 1,
        hvcc: hvcc.into(),
        colr: Default::default(),
        transforms: Default::default(),
        payload: hevc_payload(),
    };
    let tile_count = u32::from(columns) * u32::from(rows);
    isobmff::HeicGridPrimaryItemData {
        item_id: 2,
        construction_method: 1,
        descriptor: isobmff::HeicGridDescriptor {
            version: 0,
            rows,
            columns,
            output_width: output.0,
            output_height: output.1,
        },
        colr: Default::default(),
        tile_item_ids: (1..=tile_count).collect(),
        tiles: (0..tile_count)
            .map(|index| {
                let mut tile = first.clone();
                tile.item_id = index + 1;
                tile
            })
            .collect(),
    }
}

#[test]
fn heic_grid_single_span_accepts_fixed_tiles_beyond_padded_output() {
    let sps = sps_with_tile_dimensions(512, 512);
    for (columns, rows, output) in [
        (1u16, 2u16, (300u32, 1000u32)),
        (1, 16, (300, 8000)),
        (1, 24, (480, 12000)),
        (2, 1, (1000, 300)),
        (16, 1, (8000, 300)),
    ] {
        let grid = tiled_grid(columns, rows, output, sps.clone());
        assert!(
            geometry::preflight_grid(&grid, &DecodeGuardrails::default()).is_ok(),
            "{}x{} grid with output {}x{}",
            columns,
            rows,
            output.0,
            output.1
        );
    }
}

#[test]
fn heic_grid_single_span_rejects_tiles_beyond_the_fixed_extent_limit() {
    let sps = sps_with_tile_dimensions(600, 600);
    let grid = tiled_grid(1, 2, (300, 1000), sps);
    let result = geometry::preflight_grid(&grid, &DecodeGuardrails::default());
    assert!(matches!(
        result,
        Err(DecodeError::HeicDecode(DecodeHeicError::InvalidDecodedFrame { detail }))
            if detail.contains("tile geometry exceeds grid output")
    ));
}

#[test]
fn heic_grid_single_tile_keeps_the_padded_extent_bound() {
    let sps = sps_with_tile_dimensions(512, 512);
    let grid = tiled_grid(1, 1, (300, 8000), sps);
    assert!(geometry::preflight_grid(&grid, &DecodeGuardrails::default()).is_err());
}

#[test]
fn heic_grid_budget_counts_the_fixed_tile_span_of_narrow_grids() {
    let sps = sps_with_tile_dimensions(512, 512);
    for (columns, rows, output) in [
        (1u16, 59u16, (40u32, 30000u32)),
        (59, 1, (30000, 40)),
        (1, 79, (100, 40000)),
    ] {
        let grid = tiled_grid(columns, rows, output, sps.clone());
        assert!(
            geometry::preflight_grid(&grid, &DecodeGuardrails::default()).is_ok(),
            "{}x{} grid with output {}x{}",
            columns,
            rows,
            output.0,
            output.1
        );
    }
}

#[test]
fn heic_grid_narrow_budget_stays_capped_by_max_pixels() {
    let sps = sps_with_tile_dimensions(512, 512);
    let grid = tiled_grid(1, 59, (40, 30000), sps);
    let result = geometry::preflight_grid(
        &grid,
        &DecodeGuardrails {
            max_pixels: Some(40 * 30000),
            ..DecodeGuardrails::default()
        },
    );
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

fn assert_alpha_ramp(input: &[u8], width: u32, height: u32, x_step: u32) {
    let decoded = decode_bytes_to_rgba(input).unwrap();
    assert_eq!((decoded.width, decoded.height), (width, height));
    let DecodedRgbaPixels::U8(pixels) = decoded.pixels else {
        panic!("expected 8-bit RGBA output");
    };
    for (index, pixel) in pixels.chunks_exact(4).enumerate() {
        let (x, y) = (index as u32 % width, index as u32 / width);
        let expected = x_step * x + y;
        assert!(
            u32::from(pixel[3]).abs_diff(expected) <= 2,
            "alpha at ({x}, {y}) is {}, expected {expected}",
            pixel[3]
        );
    }
}

#[test]
fn apple_monochrome_alpha_plane_decodes() {
    assert_alpha_ramp(
        include_bytes!("testdata/apple-alpha-192x64.heic"),
        192,
        64,
        1,
    );
}

#[test]
fn apple_alpha_coded_at_the_minimum_frame_size_is_kept() {
    assert_alpha_ramp(include_bytes!("testdata/apple-alpha-40x30.heic"), 40, 30, 5);
}
