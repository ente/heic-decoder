extern crate alloc;
extern crate heic_decoder as api;
#[path = "../heic-decoder/mod.rs"]
mod heic_decoder;

use api::isobmff::{HeicPrimaryItemDataWithGrid, HevcDecoderConfigurationBox};
use heic_decoder::hevc::bitstream::parse_single_nal_bounded;
use heic_decoder::hevc::params::{parse_pps_bounded, parse_sps_bounded};

fn inspect(id: u32, config: &HevcDecoderConfigurationBox, data: &[u8]) {
    let nals: Vec<_> = config
        .nal_arrays
        .iter()
        .flat_map(|a| a.nal_units.iter())
        .collect();
    let parameter = |kind| nals.iter().find(|n| n[0] >> 1 & 63 == kind).unwrap();
    let sps_nal = parse_single_nal_bounded(parameter(33)).unwrap();
    let sps = parse_sps_bounded(&sps_nal.payload).unwrap();
    let pps_nal = parse_single_nal_bounded(parameter(34)).unwrap();
    let pps = parse_pps_bounded(&pps_nal.payload).unwrap();
    let mut types = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let n = usize::from(config.nal_length_size);
        let len = data[pos..pos + n]
            .iter()
            .fold(0usize, |v, b| v << 8 | usize::from(*b));
        pos += n;
        types.push(data[pos] >> 1 & 63);
        pos += len;
    }
    println!(
        "{{\"item\":{id},\"width\":{},\"height\":{},\"depth\":{},\"chroma\":{},\"ctu\":{},\"wpp\":{},\"tiles\":{},\"dependent_slices\":{},\"pcm\":{},\"sao\":{},\"transform_skip\":{},\"lossless_bypass\":{},\"nals\":{:?}}}",
        sps.pic_width_in_luma_samples,
        sps.pic_height_in_luma_samples,
        sps.bit_depth_y(),
        sps.chroma_format_idc,
        sps.ctb_size(),
        pps.entropy_coding_sync_enabled_flag,
        pps.tiles_enabled_flag,
        pps.dependent_slice_segments_enabled_flag,
        sps.pcm_enabled_flag,
        sps.sample_adaptive_offset_enabled_flag,
        pps.transform_skip_enabled_flag,
        pps.transquant_bypass_enabled_flag,
        types
    );
}

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(path).unwrap();
    match api::isobmff::extract_primary_heic_item_data_with_grid(&bytes).unwrap() {
        HeicPrimaryItemDataWithGrid::Coded(item) => {
            let props = api::isobmff::parse_primary_heic_item_preflight_properties(&bytes).unwrap();
            inspect(item.item_id, &props.hvcc, &item.payload);
        }
        HeicPrimaryItemDataWithGrid::Grid(grid) => {
            for tile in grid.tiles {
                inspect(tile.item_id, &tile.hvcc, &tile.payload);
            }
        }
    }
}
