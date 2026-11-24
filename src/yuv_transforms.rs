use crate::isobmff::{ImageMirrorDirection, PrimaryItemTransformProperty};
use crate::{DecodeHeicError, DecodedHeicImage, HeicPixelLayout, HeicPlane};

pub(crate) fn libheif_converts_to_yuv444_before(
    layout: HeicPixelLayout,
    width: u32,
    height: u32,
    transform: &PrimaryItemTransformProperty,
) -> bool {
    let odd_width = !width.is_multiple_of(2);
    let odd_height = !height.is_multiple_of(2);
    match *transform {
        PrimaryItemTransformProperty::CleanAperture(clean_aperture) => {
            !crate::heic_clean_aperture_crop_preserves_chroma_phase(
                layout,
                width,
                height,
                clean_aperture,
            )
        }
        PrimaryItemTransformProperty::Rotation(rotation) => {
            match (layout, rotation.rotation_ccw_degrees % 360) {
                (HeicPixelLayout::Yuv422, 90 | 270) => true,
                (HeicPixelLayout::Yuv422, 180) => odd_height,
                (HeicPixelLayout::Yuv420, 90) => odd_width,
                (HeicPixelLayout::Yuv420, 180) => odd_width || odd_height,
                (HeicPixelLayout::Yuv420, 270) => odd_height,
                _ => false,
            }
        }
        PrimaryItemTransformProperty::Mirror(mirror) => match layout {
            HeicPixelLayout::Yuv422 => {
                mirror.direction == ImageMirrorDirection::Horizontal && odd_width
            }
            HeicPixelLayout::Yuv420 => odd_width || odd_height,
            _ => false,
        },
    }
}

pub(crate) fn libheif_yuv_transform_count(
    layout: HeicPixelLayout,
    mut width: u32,
    mut height: u32,
    transforms: &[PrimaryItemTransformProperty],
) -> usize {
    if crate::heic_chroma_subsampling(layout) == (1, 1) {
        return 0;
    }
    let mut count = 0;
    for (index, transform) in transforms.iter().enumerate() {
        if libheif_converts_to_yuv444_before(layout, width, height, transform) {
            return index + 1;
        }
        if !reorientation_preserves_chroma_phase(layout, width, height, transform) {
            count = index + 1;
        }
        let Some(dimensions) = transformed_dimensions(width, height, transform) else {
            return count;
        };
        (width, height) = dimensions;
    }
    count
}

pub(crate) fn transform_heic_planes(
    decoded: DecodedHeicImage,
    transform: &PrimaryItemTransformProperty,
) -> Result<DecodedHeicImage, DecodeHeicError> {
    match *transform {
        PrimaryItemTransformProperty::CleanAperture(clean_aperture) => {
            crate::crop_heic_by_clean_aperture(decoded, clean_aperture)
        }
        _ => reorient_heic_image(decoded, transform),
    }
}

pub(crate) fn reorient_samples(
    samples: &[u16],
    width: u32,
    height: u32,
    transform: &PrimaryItemTransformProperty,
    label: &'static str,
) -> Result<HeicPlane, DecodeHeicError> {
    let sample_count = crate::heic_sample_count(width, height, label)?;
    if samples.len() != sample_count {
        return Err(DecodeHeicError::InvalidDecodedFrame {
            detail: format!(
                "{label} plane has {} samples, expected {width}x{height}",
                samples.len()
            ),
        });
    }
    let (source_width, source_height) = (width as usize, height as usize);
    let quarter_turn = is_quarter_turn(transform);
    let (output_width, output_height) = if quarter_turn {
        (height, width)
    } else {
        (width, height)
    };
    let source: fn(usize, usize, usize, usize) -> (usize, usize) = match *transform {
        PrimaryItemTransformProperty::Rotation(rotation) => {
            match rotation.rotation_ccw_degrees % 360 {
                90 => |x, y, width, _| (width - 1 - y, x),
                180 => |x, y, width, height| (width - 1 - x, height - 1 - y),
                270 => |x, y, _, height| (y, height - 1 - x),
                _ => |x, y, _, _| (x, y),
            }
        }
        PrimaryItemTransformProperty::Mirror(mirror) => match mirror.direction {
            ImageMirrorDirection::Horizontal => |x, y, width, _| (width - 1 - x, y),
            ImageMirrorDirection::Vertical => |x, y, _, height| (x, height - 1 - y),
        },
        PrimaryItemTransformProperty::CleanAperture(_) => {
            return Err(DecodeHeicError::InvalidDecodedFrame {
                detail: format!("{label} plane cannot be reoriented by a clean aperture"),
            });
        }
    };
    let mut reoriented = Vec::new();
    reoriented
        .try_reserve_exact(sample_count)
        .map_err(|_| DecodeHeicError::AllocationFailed {
            stage: "plane reorientation",
        })?;
    for y in 0..output_height as usize {
        for x in 0..output_width as usize {
            let (column, row) = source(x, y, source_width, source_height);
            reoriented.push(samples[row * source_width + column]);
        }
    }
    Ok(HeicPlane {
        width: output_width,
        height: output_height,
        samples: reoriented,
    })
}

fn transformed_dimensions(
    width: u32,
    height: u32,
    transform: &PrimaryItemTransformProperty,
) -> Option<(u32, u32)> {
    match *transform {
        PrimaryItemTransformProperty::CleanAperture(clean_aperture) => {
            crate::clean_aperture_crop_bounds(width, height, clean_aperture)
                .ok()
                .map(|crop| (crop.width, crop.height))
        }
        _ if is_quarter_turn(transform) => Some((height, width)),
        _ => Some((width, height)),
    }
}

fn is_quarter_turn(transform: &PrimaryItemTransformProperty) -> bool {
    matches!(
        transform,
        PrimaryItemTransformProperty::Rotation(rotation)
            if matches!(rotation.rotation_ccw_degrees % 360, 90 | 270)
    )
}

fn reorientation_preserves_chroma_phase(
    layout: HeicPixelLayout,
    width: u32,
    height: u32,
    transform: &PrimaryItemTransformProperty,
) -> bool {
    let (reverses_columns, reverses_rows) = match *transform {
        PrimaryItemTransformProperty::Rotation(rotation) => {
            match rotation.rotation_ccw_degrees % 360 {
                90 => (true, false),
                180 => (true, true),
                270 => (false, true),
                _ => (false, false),
            }
        }
        PrimaryItemTransformProperty::Mirror(mirror) => match mirror.direction {
            ImageMirrorDirection::Horizontal => (true, false),
            ImageMirrorDirection::Vertical => (false, true),
        },
        PrimaryItemTransformProperty::CleanAperture(_) => (false, false),
    };
    let (subsample_x, subsample_y) = crate::heic_chroma_subsampling(layout);
    (!reverses_columns || subsample_x == 1 || width.is_multiple_of(2))
        && (!reverses_rows || subsample_y == 1 || height.is_multiple_of(2))
}

fn reorient_heic_image(
    decoded: DecodedHeicImage,
    transform: &PrimaryItemTransformProperty,
) -> Result<DecodedHeicImage, DecodeHeicError> {
    if matches!(
        transform,
        PrimaryItemTransformProperty::Rotation(rotation)
            if crate::is_identity_rotation(rotation.rotation_ccw_degrees)
    ) {
        return Ok(decoded);
    }
    crate::validate_heic_plane_dimensions(&decoded.y_plane, decoded.width, decoded.height, "Y")?;
    let y_plane = reorient_plane(&decoded.y_plane, transform, "Y")?;
    let (width, height) = (y_plane.width, y_plane.height);
    if decoded.layout == HeicPixelLayout::Yuv400 {
        return Ok(DecodedHeicImage {
            width,
            height,
            y_plane,
            ..decoded
        });
    }
    let (u_plane, v_plane, chroma_width, chroma_height) =
        crate::require_heic_chroma_planes(&decoded)?;
    crate::validate_heic_plane_dimensions(u_plane, chroma_width, chroma_height, "U")?;
    crate::validate_heic_plane_dimensions(v_plane, chroma_width, chroma_height, "V")?;
    let u_plane = reorient_plane(u_plane, transform, "U")?;
    let v_plane = reorient_plane(v_plane, transform, "V")?;
    let (expected_width, expected_height) =
        crate::heic_chroma_dimensions(width, height, decoded.layout);
    crate::validate_heic_plane_dimensions(&u_plane, expected_width, expected_height, "U")?;
    Ok(DecodedHeicImage {
        width,
        height,
        y_plane,
        u_plane: Some(u_plane),
        v_plane: Some(v_plane),
        ..decoded
    })
}

fn reorient_plane(
    plane: &HeicPlane,
    transform: &PrimaryItemTransformProperty,
    label: &'static str,
) -> Result<HeicPlane, DecodeHeicError> {
    reorient_samples(&plane.samples, plane.width, plane.height, transform, label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isobmff::{ImageMirrorProperty, ImageRotationProperty};

    fn rotation(degrees: u16) -> PrimaryItemTransformProperty {
        PrimaryItemTransformProperty::Rotation(ImageRotationProperty {
            rotation_ccw_degrees: degrees,
        })
    }

    fn mirror(direction: ImageMirrorDirection) -> PrimaryItemTransformProperty {
        PrimaryItemTransformProperty::Mirror(ImageMirrorProperty { direction })
    }

    fn clap(width: u32, height: u32) -> PrimaryItemTransformProperty {
        PrimaryItemTransformProperty::CleanAperture(crate::isobmff::ImageCleanApertureProperty {
            clean_aperture_width_num: width,
            clean_aperture_width_den: 1,
            clean_aperture_height_num: height,
            clean_aperture_height_den: 1,
            horizontal_offset_num: -(8 - width as i32),
            horizontal_offset_den: 2,
            vertical_offset_num: -(8 - height as i32),
            vertical_offset_den: 2,
        })
    }

    fn odd_origin_clap() -> PrimaryItemTransformProperty {
        PrimaryItemTransformProperty::CleanAperture(crate::isobmff::ImageCleanApertureProperty {
            clean_aperture_width_num: 7,
            clean_aperture_width_den: 1,
            clean_aperture_height_num: 8,
            clean_aperture_height_den: 1,
            horizontal_offset_num: 1,
            horizontal_offset_den: 2,
            vertical_offset_num: 0,
            vertical_offset_den: 1,
        })
    }

    #[test]
    fn conversion_rules_match_libheif_rotate_ccw_and_mirror_inplace() {
        use HeicPixelLayout::{Yuv400, Yuv420, Yuv422, Yuv444};
        use ImageMirrorDirection::{Horizontal, Vertical};
        let cases = [
            (Yuv420, 7, 6, rotation(90), true),
            (Yuv420, 6, 7, rotation(90), false),
            (Yuv420, 7, 6, rotation(180), true),
            (Yuv420, 6, 7, rotation(180), true),
            (Yuv420, 6, 6, rotation(180), false),
            (Yuv420, 7, 6, rotation(270), false),
            (Yuv420, 6, 7, rotation(270), true),
            (Yuv420, 7, 7, rotation(0), false),
            (Yuv420, 8, 8, odd_origin_clap(), true),
            (Yuv422, 8, 8, odd_origin_clap(), true),
            (Yuv420, 8, 8, clap(7, 8), false),
            (Yuv420, 7, 6, mirror(Vertical), true),
            (Yuv420, 6, 7, mirror(Horizontal), true),
            (Yuv420, 6, 6, mirror(Horizontal), false),
            (Yuv422, 6, 6, rotation(90), true),
            (Yuv422, 6, 6, rotation(270), true),
            (Yuv422, 6, 7, rotation(180), true),
            (Yuv422, 7, 6, rotation(180), false),
            (Yuv422, 7, 6, mirror(Horizontal), true),
            (Yuv422, 6, 7, mirror(Horizontal), false),
            (Yuv422, 7, 7, mirror(Vertical), false),
            (Yuv444, 7, 7, rotation(90), false),
            (Yuv444, 7, 7, mirror(Horizontal), false),
            (Yuv400, 7, 7, rotation(180), false),
        ];
        for (layout, width, height, transform, converts) in cases {
            assert_eq!(
                libheif_converts_to_yuv444_before(layout, width, height, &transform),
                converts,
                "{layout:?} {width}x{height} {transform:?}"
            );
        }
    }

    #[test]
    fn transform_count_tracks_dimensions_through_earlier_transforms() {
        use HeicPixelLayout::{Yuv420, Yuv422, Yuv444};
        use ImageMirrorDirection::{Horizontal, Vertical};
        let cases: [(
            HeicPixelLayout,
            u32,
            u32,
            Vec<PrimaryItemTransformProperty>,
            usize,
        ); 10] = [
            (Yuv420, 6, 7, vec![rotation(90)], 0),
            (Yuv420, 6, 7, vec![rotation(90), mirror(Vertical)], 2),
            (Yuv420, 6, 7, vec![rotation(90), rotation(90)], 2),
            (Yuv420, 8, 8, vec![clap(7, 8), rotation(270)], 0),
            (
                Yuv420,
                8,
                8,
                vec![clap(7, 8), rotation(90), mirror(Vertical)],
                2,
            ),
            (Yuv420, 8, 8, vec![rotation(180), clap(7, 8)], 0),
            (Yuv420, 8, 8, vec![rotation(180), odd_origin_clap()], 2),
            (Yuv422, 7, 6, vec![rotation(180), mirror(Vertical)], 1),
            (
                Yuv422,
                7,
                6,
                vec![mirror(Vertical), rotation(180), mirror(Horizontal)],
                3,
            ),
            (Yuv444, 7, 7, vec![rotation(90), mirror(Horizontal)], 0),
        ];
        for (layout, width, height, transforms, count) in cases {
            assert_eq!(
                libheif_yuv_transform_count(layout, width, height, &transforms),
                count,
                "{layout:?} {width}x{height} {transforms:?}"
            );
        }
    }

    #[test]
    fn reoriented_samples_match_rgba_transforms() {
        let (width, height) = (5_u32, 3_u32);
        let samples: Vec<u16> = (0..width * height).map(|index| index as u16).collect();
        let rgba: Vec<u16> = samples.iter().flat_map(|&sample| [sample; 4]).collect();
        for degrees in [90, 180, 270] {
            let plane = reorient_samples(&samples, width, height, &rotation(degrees), "test")
                .expect("rotation should succeed");
            let (rotated_width, rotated_height, rotated) =
                crate::rotate_rgba_ccw(width, height, &rgba, degrees).expect("RGBA rotation");
            assert_eq!((plane.width, plane.height), (rotated_width, rotated_height));
            assert_eq!(
                plane.samples,
                rotated.iter().step_by(4).copied().collect::<Vec<_>>(),
                "{degrees}"
            );
        }
        for direction in [
            ImageMirrorDirection::Horizontal,
            ImageMirrorDirection::Vertical,
        ] {
            let plane = reorient_samples(&samples, width, height, &mirror(direction), "test")
                .expect("mirror should succeed");
            let mirrored =
                crate::mirror_rgba(width, height, &rgba, direction).expect("RGBA mirror");
            assert_eq!((plane.width, plane.height), (width, height));
            assert_eq!(
                plane.samples,
                mirrored.iter().step_by(4).copied().collect::<Vec<_>>(),
                "{direction:?}"
            );
        }
    }

    #[test]
    fn reorientation_rejects_mismatched_planes() {
        assert!(reorient_samples(&[1, 2, 3], 2, 2, &rotation(90), "test").is_err());
        assert!(reorient_samples(&[1, 2, 3, 4], 2, 2, &clap(2, 2), "test").is_err());
    }
}
