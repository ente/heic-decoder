extern crate alloc;
extern crate heic_decoder as api;
#[path = "../heic-decoder/mod.rs"]
mod heic_decoder;

fn main() {
    let paths: Vec<_> = std::env::args().skip(1).collect();
    assert!(!paths.is_empty(), "Usage: incremental-check INPUT...");
    let annex_b_output = std::env::var_os("ANNEX_B_OUTPUT");
    assert!(annex_b_output.is_none() || paths.len() == 1);
    for path in paths {
        let bytes = std::fs::read(&path).unwrap();
        let item = api::isobmff::extract_primary_heic_item_data(&bytes).unwrap();
        let props = api::isobmff::parse_primary_heic_item_preflight_properties(&bytes).unwrap();
        let config = &props.hvcc;
        let mut all: Vec<&[u8]> = config
            .nal_arrays
            .iter()
            .flat_map(|a| a.nal_units.iter().map(Vec::as_slice))
            .collect();
        let mut pos = 0;
        while pos < item.payload.len() {
            let n = usize::from(config.nal_length_size);
            let len = item.payload[pos..pos + n]
                .iter()
                .fold(0usize, |v, b| (v << 8) | usize::from(*b));
            pos += n;
            all.push(&item.payload[pos..pos + len]);
            pos += len;
        }
        let parameter = |kind| *all.iter().find(|n| (n[0] >> 1) & 63 == kind).unwrap();
        if let Some(output) = &annex_b_output {
            use std::io::Write;
            let mut file = std::fs::File::create(output).unwrap();
            for nal in &all {
                file.write_all(&[0, 0, 0, 1]).unwrap();
                file.write_all(nal).unwrap();
            }
        }
        let Some(slice) = all.iter().find(|n| matches!((n[0] >> 1) & 63, 19 | 20)) else {
            continue;
        };
        let prepare = || {
            heic_decoder::hevc::bounded::Prepared::new(
                parameter(32),
                parameter(33),
                parameter(34),
                slice,
            )
            .unwrap()
        };
        let reference = prepare().decode().unwrap();
        let independent = std::env::var("REFERENCE_YUV")
            .ok()
            .map(|p| std::fs::read(p).unwrap());
        if let Some(bytes) = &independent {
            assert_eq!(
                bytes.len(),
                reference.width as usize * reference.height as usize * 3 / 2
            );
        }
        let result = prepare().decode_incremental(|start, band| {
            for component in 0..3 {
                let sub = if component == 0 {1} else {2};
                let (expected, stride) = reference.plane(component);
                let (actual, _) = band.plane(component);
                let offset = (start / sub) as usize * stride;
                let len = (band.height / sub) as usize * stride;
                if let Some(bytes) = &independent {
                    let plane_offset = match component { 0 => 0, 1 => reference.width as usize * reference.height as usize, _ => reference.width as usize * reference.height as usize * 5 / 4 };
                    let expected = &bytes[plane_offset + offset..plane_offset + offset + len];
                    if let Some(i) = actual[..len].iter().zip(expected).position(|(&a,&b)| a != u16::from(b)) {
                        panic!("independent YUV difference: component {component} ({},{}) actual={} expected={}", i%stride, start/sub + (i/stride) as u32, actual[i],expected[i]);
                    }
                }
                if let Some(i) = actual[..len]
.iter().zip(&expected[offset..offset+len]).position(|(a,b)| a != b) {
                    panic!("{path}: component {component} ({},{}) actual={} expected={}", i%stride, start/sub + (i/stride) as u32, actual[i],expected[offset+i]);
                }
            }
            Ok(())
        }).unwrap();
        assert_eq!(result.rows, reference.height);
        println!("{path}: {result:?}");
    }
}
