use crate::*;

pub(super) fn heic_error(error: DecodeError) -> DecodeHeicError {
    match error {
        DecodeError::HeicDecode(error) => error,
        error => DecodeHeicError::InvalidDecodedFrame {
            detail: error.to_string(),
        },
    }
}

fn invalid(detail: impl Into<String>) -> DecodeError {
    DecodeHeicError::InvalidDecodedFrame {
        detail: detail.into(),
    }
    .into()
}

fn padded_extent(value: u32, alignment: u32) -> u64 {
    u64::from(value).div_ceil(u64::from(alignment)) * u64::from(alignment)
}

fn byte_limit(stage: &'static str, actual_bytes: u64, max_decoded_bytes: u64) -> DecodeError {
    DecodeGuardrailError::DecodedByteCountExceeded {
        stage,
        actual_bytes,
        max_decoded_bytes,
    }
    .into()
}

fn decoded_byte_budget(pixels: u64) -> u64 {
    pixels
        .saturating_mul(12)
        .saturating_add(GRID_TILE_DECODE_MEMORY_BUDGET)
}

fn enforce_decoded_bytes(
    stage: &'static str,
    actual_bytes: u64,
    max_decoded_bytes: u64,
) -> Result<(), DecodeError> {
    if actual_bytes > max_decoded_bytes {
        return Err(byte_limit(stage, actual_bytes, max_decoded_bytes));
    }
    Ok(())
}

fn check_sps(
    sps: &heic_decoder::hevc::params::Sps,
    expected: Option<(u32, u32)>,
    guardrails: &DecodeGuardrails,
) -> Result<u64, DecodeError> {
    let metadata = hevc_metadata_from_sps(sps)?;
    guardrails.enforce_pixel_count(
        sps.pic_width_in_luma_samples,
        sps.pic_height_in_luma_samples,
    )?;
    if let Some((width, height)) = expected {
        if (metadata.width, metadata.height) != (width, height) {
            return Err(invalid(
                "auxiliary HEVC geometry does not match primary image",
            ));
        }
        let alignment = sps.ctb_size();
        if u64::from(sps.pic_width_in_luma_samples) > padded_extent(width, alignment)
            || u64::from(sps.pic_height_in_luma_samples) > padded_extent(height, alignment)
        {
            return Err(invalid(
                "auxiliary HEVC coded geometry exceeds primary image padding",
            ));
        }
    }
    Ok(estimate_heic_grid_sps_decode_bytes(sps)?)
}

pub(super) fn preflight_hevc(
    hvcc: &isobmff::HevcDecoderConfigurationBox,
    payload: &[u8],
    expected: Option<(u32, u32)>,
    guardrails: &DecodeGuardrails,
) -> Result<u64, DecodeError> {
    let mut bytes = None;
    let mut failure = None;
    let mut stream_memory = HeicGridTileStreamMemoryEstimate::default();
    walk_hevc_nals_from_hvcc_or_payload(hvcc, payload, |offset, nal| {
        stream_memory.add_nal(nal)?;
        let unit = LengthPrefixedHevcNalUnit { offset, bytes: nal };
        if unit.nal_unit_type() == Some(NALUnitType::SpsNut) {
            let sps = hevc_sps_from_nal(nal, offset)?;
            match check_sps(&sps, expected, guardrails) {
                Ok(candidate) => bytes = Some(bytes.unwrap_or(0u64).max(candidate)),
                Err(error) => {
                    failure = Some(error);
                    return Ok(true);
                }
            }
        }
        Ok(false)
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    let bytes = bytes
        .ok_or(DecodeHeicError::MissingSpsNalUnit)?
        .saturating_add(stream_memory.estimated_decoder_bytes());
    let pixels = guardrails
        .max_pixels
        .into_iter()
        .chain(expected.map(|(width, height)| u64::from(width) * u64::from(height)))
        .min();
    if let Some(pixels) = pixels {
        enforce_decoded_bytes("HEVC frame", bytes, decoded_byte_budget(pixels))?;
    }
    Ok(bytes)
}

pub(super) fn preflight_grid(
    grid: &isobmff::HeicGridPrimaryItemData,
    guardrails: &DecodeGuardrails,
) -> Result<(), DecodeError> {
    validate_heic_grid_descriptor_and_tile_count(grid)?;
    let descriptor = &grid.descriptor;
    guardrails.enforce_pixel_count(descriptor.output_width, descriptor.output_height)?;
    let output_pixels = u64::from(descriptor.output_width) * u64::from(descriptor.output_height);
    let pixel_budget = guardrails
        .max_pixels
        .unwrap_or(output_pixels)
        .min(output_pixels);
    let decoded_budget = decoded_byte_budget(pixel_budget);
    let mut aggregate = 0u64;
    for tile in &grid.tiles {
        let mut tile_bytes = None;
        let mut failure = None;
        let mut stream_memory = HeicGridTileStreamMemoryEstimate::default();
        walk_hevc_nals_from_hvcc_or_payload(&tile.hvcc, &tile.payload, |offset, nal| {
            stream_memory.add_nal(nal)?;
            let unit = LengthPrefixedHevcNalUnit { offset, bytes: nal };
            if unit.nal_unit_type() == Some(NALUnitType::SpsNut) {
                let sps = hevc_sps_from_nal(nal, offset)?;
                let metadata = hevc_metadata_from_sps(&sps)?;
                let plan = RgbaTransformPlan::from_primary_transforms(
                    metadata.width,
                    metadata.height,
                    &tile.transforms,
                );
                let result = check_sps(&sps, None, guardrails).and_then(|bytes| {
                    let plan = plan?;
                    let alignment = sps.ctb_size();
                    let max_width = if descriptor.columns > 1 {
                        u64::from(descriptor.output_width - 1) / u64::from(descriptor.columns - 1)
                    } else {
                        padded_extent(descriptor.output_width, alignment)
                    };
                    let max_height = if descriptor.rows > 1 {
                        u64::from(descriptor.output_height - 1) / u64::from(descriptor.rows - 1)
                    } else {
                        padded_extent(descriptor.output_height, alignment)
                    };
                    if u64::from(plan.destination_width) > max_width
                        || u64::from(plan.destination_height) > max_height
                    {
                        return Err(invalid("HEVC tile geometry exceeds grid output"));
                    }
                    let mut coded_width = u64::from(sps.pic_width_in_luma_samples);
                    let mut coded_height = u64::from(sps.pic_height_in_luma_samples);
                    if coded_width > padded_extent(metadata.width, alignment)
                        || coded_height > padded_extent(metadata.height, alignment)
                    {
                        return Err(invalid(
                            "HEVC tile coded geometry exceeds visible geometry padding",
                        ));
                    }
                    for transform in &tile.transforms {
                        if let isobmff::PrimaryItemTransformProperty::Rotation(rotation) = transform
                            && rotation.rotation_ccw_degrees % 180 == 90
                        {
                            std::mem::swap(&mut coded_width, &mut coded_height);
                        }
                    }
                    let alignment = u64::from(alignment);
                    if coded_width > max_width.div_ceil(alignment) * alignment
                        || coded_height > max_height.div_ceil(alignment) * alignment
                    {
                        return Err(invalid("HEVC tile coded geometry exceeds grid output"));
                    }
                    enforce_decoded_bytes("HEVC tile", bytes, decoded_budget)?;
                    Ok(bytes)
                });
                match result {
                    Ok(bytes) => tile_bytes = Some(tile_bytes.unwrap_or(0u64).max(bytes)),
                    Err(error) => {
                        failure = Some(error);
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        })?;
        if let Some(error) = failure {
            return Err(error);
        }
        let tile_bytes = tile_bytes
            .ok_or(DecodeHeicError::MissingSpsNalUnit)?
            .saturating_add(stream_memory.estimated_decoder_bytes());
        enforce_decoded_bytes("HEVC tile", tile_bytes, decoded_budget)?;
        aggregate = aggregate.saturating_add(tile_bytes);
        enforce_decoded_bytes("HEIC grid", aggregate, decoded_budget)?;
    }
    Ok(())
}
