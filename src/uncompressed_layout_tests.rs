use super::isobmff::test_support as fixtures;
use super::tests::zlib_compress;
use super::{
    UNCOMPRESSED_INTERLEAVE_COMPONENT, UNCOMPRESSED_INTERLEAVE_MIXED,
    UNCOMPRESSED_INTERLEAVE_MULTI_Y, UNCOMPRESSED_INTERLEAVE_PIXEL, UNCOMPRESSED_INTERLEAVE_ROW,
    UNCOMPRESSED_INTERLEAVE_TILE_COMPONENT, UNCOMPRESSED_SAMPLING_420, UNCOMPRESSED_SAMPLING_422,
    UNCOMPRESSED_SAMPLING_NO_SUBSAMPLING, decode_bytes_to_rgba,
};

const LAYOUT_COUNT: usize = 3_000;
const LARGEST_PAYLOAD_BYTES: usize = 8_192;
const INTERLEAVE_TYPES: [u8; 6] = [
    UNCOMPRESSED_INTERLEAVE_COMPONENT,
    UNCOMPRESSED_INTERLEAVE_PIXEL,
    UNCOMPRESSED_INTERLEAVE_MIXED,
    UNCOMPRESSED_INTERLEAVE_ROW,
    UNCOMPRESSED_INTERLEAVE_TILE_COMPONENT,
    UNCOMPRESSED_INTERLEAVE_MULTI_Y,
];
const COMPONENT_MONO: u16 = 0;
const COMPONENT_Y: u16 = 1;
const COMPONENT_CB: u16 = 2;
const COMPONENT_CR: u16 = 3;
const COMPONENT_RED: u16 = 4;
const COMPONENT_GREEN: u16 = 5;
const COMPONENT_BLUE: u16 = 6;
const COMPONENT_ALPHA: u16 = 7;
const COMPONENT_PADDED: u16 = 12;

struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn pick<T: Copy>(&mut self, choices: &[T]) -> T {
        choices[self.below(choices.len())]
    }

    fn one_in(&mut self, bound: usize) -> bool {
        self.below(bound) == 0
    }
}

#[derive(Debug)]
struct RandomLayout {
    width: u32,
    height: u32,
    component_types: Vec<u16>,
    components: Vec<(u16, u8, u8)>,
    sampling_type: u8,
    interleave_type: u8,
    pixel_size: u32,
    row_align_size: u32,
    tile_align_size: u32,
    tile_grid: (u32, u32),
}

impl RandomLayout {
    fn generate(rng: &mut Xorshift) -> Self {
        let interleave_type = rng.pick(&INTERLEAVE_TYPES);
        let sampling_type = random_sampling_type(rng, interleave_type);
        let component_types = random_component_types(rng, interleave_type, sampling_type);
        let components = random_components(rng, &component_types);
        let (subsample_x, subsample_y) = match sampling_type {
            UNCOMPRESSED_SAMPLING_422 => (2, 1),
            UNCOMPRESSED_SAMPLING_420 => (2, 2),
            _ => (1, 1),
        };
        let width = (1 + rng.below(8) as u32) * subsample_x * rng.pick(&[1, 1, 2]);
        let height = (1 + rng.below(5) as u32) * subsample_y;
        let tile_cols = rng.pick(&[1_u32, 1, 2, 3]);
        let tile_rows = rng.pick(&[1_u32, 1, 2]);
        let tile_grid_fits = width.is_multiple_of(tile_cols)
            && (width / tile_cols).is_multiple_of(subsample_x)
            && height.is_multiple_of(tile_rows)
            && (height / tile_rows).is_multiple_of(subsample_y);
        let pixel_size = if matches!(
            interleave_type,
            UNCOMPRESSED_INTERLEAVE_PIXEL | UNCOMPRESSED_INTERLEAVE_MULTI_Y
        ) && rng.one_in(2)
        {
            largest_group_bytes(&components) + rng.pick(&[0, 0, 1, 2, 4])
        } else {
            0
        };
        let (row_align_choices, tile_align_choices): (&[u32], &[u32]) = match sampling_type {
            UNCOMPRESSED_SAMPLING_422 => (&[0, 0, 2, 4, 8, 16], &[0, 0, 2, 4, 8, 16]),
            UNCOMPRESSED_SAMPLING_420 => (&[0, 0, 2, 4, 8, 16], &[0, 0, 4, 8, 16]),
            _ => (&[0, 0, 1, 2, 3, 4, 8, 16], &[0, 0, 1, 2, 3, 4, 8, 16]),
        };
        Self {
            width,
            height,
            component_types,
            components,
            sampling_type,
            interleave_type,
            pixel_size,
            row_align_size: rng.pick(row_align_choices),
            tile_align_size: rng.pick(tile_align_choices),
            tile_grid: if tile_grid_fits {
                (tile_cols, tile_rows)
            } else {
                (1, 1)
            },
        }
    }

    fn file(&self, payload: &[u8], compressed: bool) -> Vec<u8> {
        let mut properties = vec![
            fixtures::ispe_box(self.width, self.height),
            fixtures::cmpd_box(&self.component_types),
            fixtures::uncc_v0_box_with_layout(
                &self.components,
                self.sampling_type,
                self.interleave_type,
                self.pixel_size,
                self.row_align_size,
                self.tile_align_size,
                self.tile_grid,
            ),
        ];
        if compressed {
            properties.push(fixtures::cmpc_box(b"zlib", 0));
            return fixtures::minimal_uncompressed_heif_custom_layout(
                &properties,
                &zlib_compress(payload),
            );
        }
        fixtures::minimal_uncompressed_heif_custom_layout(&properties, payload)
    }

    fn plain_decodes(&self, payload_bytes: usize) -> bool {
        decode_bytes_to_rgba(&self.file(&sample_payload(payload_bytes), false)).is_ok()
    }
}

fn random_sampling_type(rng: &mut Xorshift, interleave_type: u8) -> u8 {
    match interleave_type {
        UNCOMPRESSED_INTERLEAVE_PIXEL | UNCOMPRESSED_INTERLEAVE_ROW => {
            UNCOMPRESSED_SAMPLING_NO_SUBSAMPLING
        }
        UNCOMPRESSED_INTERLEAVE_MULTI_Y => UNCOMPRESSED_SAMPLING_422,
        _ => rng.pick(&[
            UNCOMPRESSED_SAMPLING_NO_SUBSAMPLING,
            UNCOMPRESSED_SAMPLING_422,
            UNCOMPRESSED_SAMPLING_420,
        ]),
    }
}

fn random_component_types(rng: &mut Xorshift, interleave_type: u8, sampling_type: u8) -> Vec<u16> {
    if interleave_type == UNCOMPRESSED_INTERLEAVE_MULTI_Y {
        let mut types = rng
            .pick(&[
                [COMPONENT_Y, COMPONENT_CB, COMPONENT_Y, COMPONENT_CR],
                [COMPONENT_CB, COMPONENT_Y, COMPONENT_CR, COMPONENT_Y],
                [COMPONENT_Y, COMPONENT_CR, COMPONENT_Y, COMPONENT_CB],
                [COMPONENT_CR, COMPONENT_Y, COMPONENT_CB, COMPONENT_Y],
            ])
            .to_vec();
        if rng.one_in(3) {
            types.insert(rng.below(types.len() + 1), COMPONENT_PADDED);
        }
        return types;
    }

    let mut types = if sampling_type != UNCOMPRESSED_SAMPLING_NO_SUBSAMPLING
        || interleave_type == UNCOMPRESSED_INTERLEAVE_MIXED
    {
        vec![COMPONENT_Y, COMPONENT_CB, COMPONENT_CR]
    } else if rng.one_in(3) {
        vec![COMPONENT_MONO]
    } else {
        vec![COMPONENT_RED, COMPONENT_GREEN, COMPONENT_BLUE]
    };
    if rng.one_in(3) {
        types.push(COMPONENT_ALPHA);
    }
    if rng.one_in(3) {
        types.push(COMPONENT_PADDED);
    }
    for index in (1..types.len()).rev() {
        types.swap(index, rng.below(index + 1));
    }
    types
}

fn random_components(rng: &mut Xorshift, component_types: &[u16]) -> Vec<(u16, u8, u8)> {
    let depths = [1_u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 14, 16];
    let luma_depth = rng.pick(&depths);
    let uniform_depth = rng.one_in(2);
    component_types
        .iter()
        .enumerate()
        .map(|(index, component_type)| {
            let depth = if *component_type == COMPONENT_Y || uniform_depth {
                luma_depth
            } else {
                rng.pick(&depths)
            };
            let smallest_alignment = depth.div_ceil(8);
            let alignment = match rng.below(5) {
                0 => smallest_alignment,
                1 => smallest_alignment + 1,
                2 => 4,
                _ => 0,
            };
            (index as u16, depth - 1, alignment)
        })
        .collect()
}

fn largest_group_bytes(components: &[(u16, u8, u8)]) -> u32 {
    let group_bits: u32 = components
        .iter()
        .map(|(_, depth_minus_one, alignment)| match alignment {
            0 => u32::from(*depth_minus_one) + 1,
            aligned => u32::from(*aligned) * 8 + 7,
        })
        .sum();
    group_bits.div_ceil(8)
}

fn sample_payload(bytes: usize) -> Vec<u8> {
    (0..bytes)
        .map(|index| (index as u8).wrapping_mul(37))
        .collect()
}

#[test]
fn generic_compression_budget_equals_the_reader_minimum_across_layouts() {
    let mut rng = Xorshift(0x9E37_79B9_7F4A_7C15);
    let payload_sizes: Vec<usize> = (0..=LARGEST_PAYLOAD_BYTES).collect();
    let mut decoded_per_interleave = [0_usize; INTERLEAVE_TYPES.len()];

    for _ in 0..LAYOUT_COUNT {
        let layout = RandomLayout::generate(&mut rng);
        if !layout.plain_decodes(LARGEST_PAYLOAD_BYTES) {
            continue;
        }
        let reader_minimum = payload_sizes.partition_point(|bytes| !layout.plain_decodes(*bytes));
        let payload = sample_payload(reader_minimum);

        let plain = decode_bytes_to_rgba(&layout.file(&payload, false))
            .unwrap_or_else(|err| panic!("plain {layout:?}: {err}"));
        let compressed = decode_bytes_to_rgba(&layout.file(&payload, true))
            .unwrap_or_else(|err| panic!("compressed {layout:?}: {err}"));
        assert_eq!(compressed.pixels, plain.pixels, "{layout:?}");

        let oversized = sample_payload(reader_minimum + 1);
        let Err(err) = decode_bytes_to_rgba(&layout.file(&oversized, true)) else {
            panic!("budget exceeds the reader minimum of {reader_minimum}: {layout:?}");
        };
        assert!(
            err.to_string().contains("output budget"),
            "{layout:?}: {err}"
        );

        decoded_per_interleave[usize::from(layout.interleave_type)] += 1;
    }

    assert!(
        decoded_per_interleave.iter().all(|decoded| *decoded >= 100),
        "too few decodable layouts: {decoded_per_interleave:?}"
    );
}
