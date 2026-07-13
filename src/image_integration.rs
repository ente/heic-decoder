//! Optional `image` crate integration helpers.
//!
//! Enable the `image-integration` feature to:
//! - register HEIF/HEIC/AVIF decoder hooks for `image::ImageReader`
//! - convert [`DecodedRgbaImage`] into `image` buffers
//!   and `DynamicImage` values.
//!
//! See `API.md` in the crate root for end-to-end examples.

use crate::{
    DecodeError, DecodeGuardrails, DecodedRgbaImage, DecodedRgbaLayout, DecodedRgbaPixels,
    HeifInputFamily, decode_bytes_to_rgba_layout_with_hint_and_guardrails,
    decode_bytes_to_rgba8_slice_with_hint_and_guardrails,
    decode_bytes_to_rgba16_native_endian_bytes_with_hint_and_guardrails,
};
use image::error::{
    DecodingError, ImageFormatHint, ParameterError, ParameterErrorKind, UnsupportedError,
    UnsupportedErrorKind,
};
use image::hooks;
use image::metadata::Orientation;
use image::{ColorType, DynamicImage, ImageBuffer, ImageDecoder, ImageError, ImageResult, Rgba};
use std::error::Error;
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
use std::io::{Read, Seek, SeekFrom};
use std::sync::Once;

const HOOK_EXTENSION_HEIC: &str = "heic";
const HOOK_EXTENSION_HEIF: &str = "heif";
const HOOK_EXTENSION_AVIF: &str = "avif";

const FTYP_MASK_12: [u8; 12] = [
    0x00, 0x00, 0x00, 0x00, // size field is ignored
    0xFF, 0xFF, 0xFF, 0xFF, // "ftyp"
    0xFF, 0xFF, 0xFF, 0xFF, // major_brand
];

const FTYP_SIG_AVIF: [u8; 12] = *b"\0\0\0\0ftypavif";
const FTYP_SIG_AVIS: [u8; 12] = *b"\0\0\0\0ftypavis";

const FTYP_SIG_HEIC: [u8; 12] = *b"\0\0\0\0ftypheic";
const FTYP_SIG_HEIX: [u8; 12] = *b"\0\0\0\0ftypheix";
const FTYP_SIG_HEVC: [u8; 12] = *b"\0\0\0\0ftyphevc";
const FTYP_SIG_HEVX: [u8; 12] = *b"\0\0\0\0ftyphevx";
const FTYP_SIG_HEIM: [u8; 12] = *b"\0\0\0\0ftypheim";
const FTYP_SIG_HEIS: [u8; 12] = *b"\0\0\0\0ftypheis";
const FTYP_SIG_MIF1: [u8; 12] = *b"\0\0\0\0ftypmif1";
const FTYP_SIG_MSF1: [u8; 12] = *b"\0\0\0\0ftypmsf1";
const FTYP_SIG_MIAF: [u8; 12] = *b"\0\0\0\0ftypmiaf";

static REGISTER_IMAGE_FORMAT_DETECTION_HOOKS: Once = Once::new();

pub type Rgba8ImageBuffer = ImageBuffer<Rgba<u8>, Vec<u8>>;
pub type Rgba16ImageBuffer = ImageBuffer<Rgba<u16>, Vec<u16>>;

/// Apply EXIF orientation (`1..=8`) to an `image::DynamicImage`.
///
/// This helper is intended for callers that decode with libheif-parity behavior
/// and then apply orientation explicitly at the application layer.
pub fn apply_exif_orientation_dynamic(image: DynamicImage, exif_orientation: u8) -> DynamicImage {
    match exif_orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.fliph().rotate270(),
        6 => image.rotate90(),
        7 => image.fliph().rotate90(),
        8 => image.rotate270(),
        _ => image,
    }
}

struct LazyHeifImageDecoder {
    input: Vec<u8>,
    hint: Option<HeifInputFamily>,
    guardrails: DecodeGuardrails,
    layout: DecodedRgbaLayout,
    // Primary-item payload extraction the layout probe already performed;
    // `read_image` consumes it instead of copying every payload out of the
    // container a second time.
    preextracted_heic: Option<crate::isobmff::HeicPrimaryItemDataWithGrid>,
}

// Manual impl: a derived Debug would dump the entire encoded input.
impl std::fmt::Debug for LazyHeifImageDecoder {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LazyHeifImageDecoder")
            .field("input_len", &self.input.len())
            .field("hint", &self.hint)
            .field("guardrails", &self.guardrails)
            .field("layout", &self.layout)
            .finish()
    }
}

impl LazyHeifImageDecoder {
    fn from_encoded_input(
        input: Vec<u8>,
        hint: Option<HeifInputFamily>,
        guardrails: DecodeGuardrails,
        probe: crate::RgbaLayoutProbe,
    ) -> Self {
        Self {
            input,
            hint,
            guardrails,
            layout: probe.layout,
            preextracted_heic: probe.preextracted_heic,
        }
    }

    fn storage_color_type(&self) -> ColorType {
        storage_color_type_from_bit_depth(self.layout.storage_bit_depth)
    }

    fn expected_total_bytes(&self) -> ImageResult<usize> {
        expected_rgba_total_bytes(
            self.layout.width,
            self.layout.height,
            self.layout.storage_bit_depth,
        )
    }
}

impl ImageDecoder for LazyHeifImageDecoder {
    fn dimensions(&self) -> (u32, u32) {
        (self.layout.width, self.layout.height)
    }

    fn color_type(&self) -> ColorType {
        self.storage_color_type()
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.layout.icc_profile.clone())
    }

    // Decode does not bake EXIF orientation into pixels (libheif parity;
    // orientation is applied at the application layer), so expose the EXIF
    // block through the trait: `ImageDecoder::orientation()` derives from it,
    // and without this override image-crate callers would always see
    // `Orientation::NoTransforms` for EXIF-only-rotated files.
    fn exif_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(crate::primary_exif_tiff_payload(&self.input))
    }

    // Container transforms (`irot`/`imir`) are baked into the decoded pixels,
    // so reporting the EXIF orientation on top would make generic
    // `orientation()` + `apply_orientation` callers double-rotate. Mirror the
    // gate in `ExifOrientationHint::should_apply_exif_orientation`.
    fn orientation(&mut self) -> ImageResult<Orientation> {
        if crate::primary_item_has_orientation_transform(&self.input) {
            return Ok(Orientation::NoTransforms);
        }
        Ok(self
            .exif_metadata()?
            .and_then(|chunk| Orientation::from_exif_chunk(&chunk))
            .unwrap_or(Orientation::NoTransforms))
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()>
    where
        Self: Sized,
    {
        let expected_total_bytes = self.expected_total_bytes()?;
        if buf.len() != expected_total_bytes {
            return Err(ImageError::Parameter(ParameterError::from_kind(
                ParameterErrorKind::DimensionMismatch,
            )));
        }

        match self.layout.storage_bit_depth {
            8 => decode_bytes_to_rgba8_slice_with_hint_and_guardrails(
                &self.input,
                self.hint,
                self.guardrails,
                self.preextracted_heic,
                buf,
            )
            .map_err(decode_error_to_image_error),
            16 => decode_bytes_to_rgba16_native_endian_bytes_with_hint_and_guardrails(
                &self.input,
                self.hint,
                self.guardrails,
                self.preextracted_heic,
                buf,
            )
            .map_err(decode_error_to_image_error),
            other => {
                unreachable!("validated storage bit depth must be 8 or 16, got {other}")
            }
        }
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

/// Build a hook decoder that probes metadata up front and defers pixel decode
/// until `read_image` supplies the destination buffer.
///
/// Memory invariant: every accepted format writes into the caller's buffer;
/// there is no eager owned-RGBA fallback. Codec-native planes and bounded grid
/// tile scratch buffers may still be allocated during decode.
fn decoder_from_seekable_with_hint_and_guardrails<R: Read + Seek>(
    input_reader: R,
    hint: Option<HeifInputFamily>,
    guardrails: DecodeGuardrails,
) -> ImageResult<Box<dyn ImageDecoder>> {
    let input = read_seekable_input_to_vec(input_reader, &guardrails)?;
    let probe =
        decode_bytes_to_rgba_layout_with_hint_and_guardrails(&input, hint, guardrails.clone())
            .map_err(decode_error_to_image_error)?;
    Ok(Box::new(LazyHeifImageDecoder::from_encoded_input(
        input, hint, guardrails, probe,
    )))
}

/// Default `max_input_bytes` applied by [`register_image_decoder_hooks`].
///
/// Hook decodes buffer the entire encoded input, so an unbounded default
/// would let a single oversized file (e.g. a motion-photo HEIC with a
/// multi-gigabyte `mdat`) allocate its full size. Callers that need larger
/// inputs can register with explicit guardrails via
/// [`register_image_decoder_hooks_with_guardrails`].
///
/// This limit also serves as the pre-allocation ceiling for the
/// seek-reported input length in [`read_seekable_input_to_vec`]: the
/// reported length is untrusted until the bytes are actually read, and a
/// lying or corrupt reader could otherwise trigger a multi-gigabyte
/// allocation (or a capacity-overflow panic) before a single byte arrives.
/// `read_to_end` still grows the buffer past this ceiling for genuinely
/// larger, guardrail-permitted inputs.
pub const DEFAULT_HOOK_MAX_INPUT_BYTES: u64 = 128 * 1024 * 1024;

/// Read the whole encoded input into memory.
///
/// This is a deliberate trade: the lazy decoder needs the full input as a
/// byte slice so `read_image` can decode directly into the caller's buffer
/// (without an additional full-frame owned RGBA allocation, which would
/// dominate peak memory). The cost is that the encoded input — usually far
/// smaller than the decoded RGBA — is held in memory for the decoder's
/// lifetime, bounded by `guardrails.max_input_bytes`.
fn read_seekable_input_to_vec<R: Read + Seek>(
    mut input_reader: R,
    guardrails: &DecodeGuardrails,
) -> ImageResult<Vec<u8>> {
    let input_len = input_reader
        .seek(SeekFrom::End(0))
        .map_err(ImageError::IoError)?;
    guardrails
        .enforce_input_bytes(input_len)
        .map_err(decode_error_to_image_error)?;

    input_reader
        .seek(SeekFrom::Start(0))
        .map_err(ImageError::IoError)?;
    let prealloc_len = input_len.min(DEFAULT_HOOK_MAX_INPUT_BYTES);
    let capacity = usize::try_from(prealloc_len).map_err(|_| {
        parameter_error(format!(
            "input size {prealloc_len} bytes does not fit in memory on this platform"
        ))
    })?;
    let mut input = Vec::with_capacity(capacity);
    match guardrails
        .max_input_bytes
        .and_then(|max| max.checked_add(1))
    {
        Some(read_limit) => input_reader
            .by_ref()
            .take(read_limit)
            .read_to_end(&mut input)
            .map_err(ImageError::IoError)?,
        // `None` is intentionally unbounded. A `u64::MAX` limit also has no
        // representable sentinel byte, so reading it without `take` preserves
        // the configured semantics without overflowing `max + 1`.
        None => input_reader
            .read_to_end(&mut input)
            .map_err(ImageError::IoError)?,
    };

    // Re-check the actual byte count: a reader may yield more bytes than its
    // seek-reported length claimed.
    guardrails
        .enforce_input_bytes(input.len() as u64)
        .map_err(decode_error_to_image_error)?;

    Ok(input)
}

/// Result of attempting to install `image` crate decoder hooks for this crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageHookRegistration {
    pub heic_decoder_hook_registered: bool,
    pub heif_decoder_hook_registered: bool,
    pub avif_decoder_hook_registered: bool,
}

impl ImageHookRegistration {
    pub fn any_decoder_hook_registered(self) -> bool {
        self.heic_decoder_hook_registered
            || self.heif_decoder_hook_registered
            || self.avif_decoder_hook_registered
    }

    pub fn all_decoder_hooks_registered(self) -> bool {
        self.heic_decoder_hook_registered
            && self.heif_decoder_hook_registered
            && self.avif_decoder_hook_registered
    }
}

/// Register HEIF/HEIC/AVIF decoder hooks with `image::hooks`.
///
/// After registration, `image::ImageReader` can decode `.heic`, `.heif`, and
/// `.avif` inputs through this crate's pure-Rust decode path, including direct
/// extension-based dispatch and content-based `ftyp` guesses for common brands.
///
/// Memory: hook decodes buffer the entire encoded input in memory before
/// decoding (in exchange, pixels decode straight into the caller's buffer
/// without an additional full-frame RGBA allocation). Codec-native planes
/// and a single grid-tile scratch buffer may still be allocated. The encoded
/// input buffer is capped at [`DEFAULT_HOOK_MAX_INPUT_BYTES`]; callers that
/// need a different bound (or none) should use
/// [`register_image_decoder_hooks_with_guardrails`] with
/// [`DecodeGuardrails::max_input_bytes`] set accordingly.
pub fn register_image_decoder_hooks() -> ImageHookRegistration {
    register_image_decoder_hooks_with_guardrails(DecodeGuardrails {
        max_input_bytes: Some(DEFAULT_HOOK_MAX_INPUT_BYTES),
        ..DecodeGuardrails::default()
    })
}

/// Register HEIF/HEIC/AVIF decoder hooks with `image::hooks`, applying the provided guardrails to all hook decodes.
///
/// Hook decodes buffer the entire encoded input in memory (see
/// [`register_image_decoder_hooks`]); `guardrails.max_input_bytes` bounds
/// that buffer.
pub fn register_image_decoder_hooks_with_guardrails(
    guardrails: DecodeGuardrails,
) -> ImageHookRegistration {
    let heif_guardrails = guardrails.clone();
    let heic_decoder_hook_registered = hooks::register_decoding_hook(
        OsString::from(HOOK_EXTENSION_HEIC),
        Box::new(move |reader| {
            decoder_from_seekable_with_hint_and_guardrails(
                reader,
                Some(HeifInputFamily::Heif),
                heif_guardrails.clone(),
            )
        }),
    );
    let heif_guardrails = guardrails.clone();
    let heif_decoder_hook_registered = hooks::register_decoding_hook(
        OsString::from(HOOK_EXTENSION_HEIF),
        Box::new(move |reader| {
            decoder_from_seekable_with_hint_and_guardrails(
                reader,
                Some(HeifInputFamily::Heif),
                heif_guardrails.clone(),
            )
        }),
    );
    let avif_decoder_hook_registered = hooks::register_decoding_hook(
        OsString::from(HOOK_EXTENSION_AVIF),
        Box::new(move |reader| {
            decoder_from_seekable_with_hint_and_guardrails(
                reader,
                Some(HeifInputFamily::Avif),
                guardrails.clone(),
            )
        }),
    );

    REGISTER_IMAGE_FORMAT_DETECTION_HOOKS.call_once(register_image_format_detection_hooks);

    ImageHookRegistration {
        heic_decoder_hook_registered,
        heif_decoder_hook_registered,
        avif_decoder_hook_registered,
    }
}

fn register_image_format_detection_hooks() {
    hooks::register_format_detection_hook(
        OsString::from(HOOK_EXTENSION_AVIF),
        &FTYP_SIG_AVIF,
        Some(&FTYP_MASK_12),
    );
    hooks::register_format_detection_hook(
        OsString::from(HOOK_EXTENSION_AVIF),
        &FTYP_SIG_AVIS,
        Some(&FTYP_MASK_12),
    );

    hooks::register_format_detection_hook(
        OsString::from(HOOK_EXTENSION_HEIF),
        &FTYP_SIG_HEIC,
        Some(&FTYP_MASK_12),
    );
    hooks::register_format_detection_hook(
        OsString::from(HOOK_EXTENSION_HEIF),
        &FTYP_SIG_HEIX,
        Some(&FTYP_MASK_12),
    );
    hooks::register_format_detection_hook(
        OsString::from(HOOK_EXTENSION_HEIF),
        &FTYP_SIG_HEVC,
        Some(&FTYP_MASK_12),
    );
    hooks::register_format_detection_hook(
        OsString::from(HOOK_EXTENSION_HEIF),
        &FTYP_SIG_HEVX,
        Some(&FTYP_MASK_12),
    );
    hooks::register_format_detection_hook(
        OsString::from(HOOK_EXTENSION_HEIF),
        &FTYP_SIG_HEIM,
        Some(&FTYP_MASK_12),
    );
    hooks::register_format_detection_hook(
        OsString::from(HOOK_EXTENSION_HEIF),
        &FTYP_SIG_HEIS,
        Some(&FTYP_MASK_12),
    );
    hooks::register_format_detection_hook(
        OsString::from(HOOK_EXTENSION_HEIF),
        &FTYP_SIG_MIF1,
        Some(&FTYP_MASK_12),
    );
    hooks::register_format_detection_hook(
        OsString::from(HOOK_EXTENSION_HEIF),
        &FTYP_SIG_MSF1,
        Some(&FTYP_MASK_12),
    );
    hooks::register_format_detection_hook(
        OsString::from(HOOK_EXTENSION_HEIF),
        &FTYP_SIG_MIAF,
        Some(&FTYP_MASK_12),
    );
}

/// ImageBuffer variants produced by `DecodedRgbaImage` conversion helpers.
#[derive(Debug)]
pub enum ImageBufferKind {
    Rgba8(Rgba8ImageBuffer),
    Rgba16(Rgba16ImageBuffer),
}

/// `image::ImageBuffer` conversion output plus metadata that cannot be stored
/// directly inside `ImageBuffer`.
#[derive(Debug)]
pub struct ImageBufferWithMetadata {
    pub image: ImageBufferKind,
    pub source_bit_depth: u8,
    pub icc_profile: Option<Vec<u8>>,
}

impl ImageBufferWithMetadata {
    pub fn storage_bit_depth(&self) -> u8 {
        match self.image {
            ImageBufferKind::Rgba8(_) => 8,
            ImageBufferKind::Rgba16(_) => 16,
        }
    }

    pub fn into_dynamic_image_with_metadata(self) -> DynamicImageWithMetadata {
        let image = match self.image {
            ImageBufferKind::Rgba8(buffer) => DynamicImage::ImageRgba8(buffer),
            ImageBufferKind::Rgba16(buffer) => DynamicImage::ImageRgba16(buffer),
        };
        DynamicImageWithMetadata {
            image,
            source_bit_depth: self.source_bit_depth,
            icc_profile: self.icc_profile,
        }
    }
}

/// `image::DynamicImage` conversion output plus metadata that cannot be stored
/// directly inside `DynamicImage`.
#[derive(Debug)]
pub struct DynamicImageWithMetadata {
    pub image: DynamicImage,
    pub source_bit_depth: u8,
    pub icc_profile: Option<Vec<u8>>,
}

/// Conversion failures while handing off decoded RGBA buffers to the `image` crate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImageConversionError {
    SampleCountOverflow {
        width: u32,
        height: u32,
    },
    SampleCountMismatch {
        storage_bit_depth: u8,
        width: u32,
        height: u32,
        expected_samples: usize,
        actual_samples: usize,
    },
}

impl Display for ImageConversionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageConversionError::SampleCountOverflow { width, height } => {
                write!(
                    f,
                    "image sample count overflow for dimensions {width}x{height}"
                )
            }
            ImageConversionError::SampleCountMismatch {
                storage_bit_depth,
                width,
                height,
                expected_samples,
                actual_samples,
            } => write!(
                f,
                "decoded RGBA{storage_bit_depth} sample count mismatch for {width}x{height}: expected {expected_samples}, got {actual_samples}"
            ),
        }
    }
}

impl Error for ImageConversionError {}

impl DecodedRgbaImage {
    /// Convert decoded pixels into `image::ImageBuffer` while carrying metadata.
    pub fn into_image_buffer_with_metadata(
        self,
    ) -> Result<ImageBufferWithMetadata, ImageConversionError> {
        let expected_samples = expected_rgba_sample_count(self.width, self.height).ok_or(
            ImageConversionError::SampleCountOverflow {
                width: self.width,
                height: self.height,
            },
        )?;

        let source_bit_depth = self.source_bit_depth;
        let icc_profile = self.icc_profile;

        let image = match self.pixels {
            DecodedRgbaPixels::U8(pixels) => {
                let actual_samples = pixels.len();
                let buffer =
                    ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(self.width, self.height, pixels)
                        .ok_or(ImageConversionError::SampleCountMismatch {
                            storage_bit_depth: 8,
                            width: self.width,
                            height: self.height,
                            expected_samples,
                            actual_samples,
                        })?;
                ImageBufferKind::Rgba8(buffer)
            }
            DecodedRgbaPixels::U16(pixels) => {
                let actual_samples = pixels.len();
                let buffer =
                    ImageBuffer::<Rgba<u16>, Vec<u16>>::from_raw(self.width, self.height, pixels)
                        .ok_or(ImageConversionError::SampleCountMismatch {
                        storage_bit_depth: 16,
                        width: self.width,
                        height: self.height,
                        expected_samples,
                        actual_samples,
                    })?;
                ImageBufferKind::Rgba16(buffer)
            }
        };

        Ok(ImageBufferWithMetadata {
            image,
            source_bit_depth,
            icc_profile,
        })
    }

    /// Convert decoded pixels into `image::ImageBuffer`.
    pub fn into_image_buffer(self) -> Result<ImageBufferKind, ImageConversionError> {
        Ok(self.into_image_buffer_with_metadata()?.image)
    }

    /// Convert decoded pixels into `image::DynamicImage` while carrying metadata.
    pub fn into_dynamic_image_with_metadata(
        self,
    ) -> Result<DynamicImageWithMetadata, ImageConversionError> {
        Ok(self
            .into_image_buffer_with_metadata()?
            .into_dynamic_image_with_metadata())
    }

    /// Convert decoded pixels into `image::DynamicImage`.
    pub fn into_dynamic_image(self) -> Result<DynamicImage, ImageConversionError> {
        Ok(self.into_dynamic_image_with_metadata()?.image)
    }
}

fn expected_rgba_sample_count(width: u32, height: u32) -> Option<usize> {
    (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)
}

fn expected_rgba_byte_count(width: u32, height: u32, storage_bit_depth: u8) -> Option<usize> {
    let bytes_per_sample = match storage_bit_depth {
        8 => 1,
        16 => 2,
        _ => return None,
    };
    expected_rgba_sample_count(width, height)?.checked_mul(bytes_per_sample)
}

/// `expected_rgba_byte_count` with the byte-count overflow mapped to the
/// decoder adapters' shared parameter error.
fn expected_rgba_total_bytes(width: u32, height: u32, storage_bit_depth: u8) -> ImageResult<usize> {
    expected_rgba_byte_count(width, height, storage_bit_depth).ok_or_else(|| {
        parameter_error(format!(
            "decoded RGBA buffer size overflow for {width}x{height} image"
        ))
    })
}

fn storage_color_type_from_bit_depth(storage_bit_depth: u8) -> ColorType {
    match storage_bit_depth {
        8 => ColorType::Rgba8,
        16 => ColorType::Rgba16,
        other => {
            unreachable!("validated storage bit depth must be 8 or 16, got {other}")
        }
    }
}

fn heif_image_format_hint() -> ImageFormatHint {
    ImageFormatHint::Name("heif/heic/avif".to_string())
}

fn parameter_error(message: String) -> ImageError {
    ImageError::Parameter(ParameterError::from_kind(ParameterErrorKind::Generic(
        message,
    )))
}

fn decode_error_to_image_error(err: DecodeError) -> ImageError {
    match err {
        DecodeError::Io(io_err) => ImageError::IoError(io_err),
        DecodeError::Unsupported(message) => {
            ImageError::Unsupported(UnsupportedError::from_format_and_kind(
                heif_image_format_hint(),
                UnsupportedErrorKind::GenericFeature(message),
            ))
        }
        other => ImageError::Decoding(DecodingError::new(heif_image_format_hint(), other)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ColorType, DecodeGuardrails, HeifInputFamily, ImageDecoder,
        decoder_from_seekable_with_hint_and_guardrails, read_seekable_input_to_vec,
    };
    use std::cell::Cell;
    use std::io::Cursor;
    use std::rc::Rc;

    /// Locks the uncompressed half of the lazy-adapter contract: layout
    /// probing and caller-buffer decoding must stay pixel-identical to the
    /// direct owned API.
    #[test]
    fn hook_decoder_decodes_uncompressed_heif_lazily() {
        let (file, expected_rgba) = crate::isobmff::test_support::minimal_uncompressed_rgb3_heif();

        let decoder = decoder_from_seekable_with_hint_and_guardrails(
            Cursor::new(file),
            Some(HeifInputFamily::Heif),
            DecodeGuardrails::default(),
        )
        .expect("hook construction must accept the lazy uncompressed decoder");
        assert_eq!(decoder.dimensions(), (2, 1));
        assert_eq!(decoder.color_type(), ColorType::Rgba8);

        let mut pixels = vec![0_u8; expected_rgba.len()];
        decoder
            .read_image_boxed(&mut pixels)
            .expect("lazy uncompressed decode should succeed");
        assert_eq!(pixels, expected_rgba);
    }

    /// A reader whose seek-reported length lies wildly. The hook must not
    /// size an allocation from the untrusted length (that would panic or
    /// abort before a single byte is read) and must still decode the bytes
    /// the reader actually yields.
    struct LyingLengthReader(Cursor<Vec<u8>>);

    impl std::io::Read for LyingLengthReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl std::io::Seek for LyingLengthReader {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            match pos {
                std::io::SeekFrom::End(0) => {
                    self.0.seek(std::io::SeekFrom::End(0))?;
                    Ok(u64::MAX)
                }
                other => self.0.seek(other),
            }
        }
    }

    struct UnderreportedLengthReader {
        inner: Cursor<Vec<u8>>,
        bytes_read: Rc<Cell<usize>>,
    }

    impl std::io::Read for UnderreportedLengthReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let read = self.inner.read(buf)?;
            self.bytes_read.set(self.bytes_read.get() + read);
            Ok(read)
        }
    }

    impl std::io::Seek for UnderreportedLengthReader {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            match pos {
                std::io::SeekFrom::End(0) => {
                    self.inner.seek(std::io::SeekFrom::End(0))?;
                    Ok(0)
                }
                other => self.inner.seek(other),
            }
        }
    }

    #[test]
    fn hook_input_read_enforces_limit_while_buffering() {
        let guardrails = DecodeGuardrails {
            max_input_bytes: Some(8),
            ..DecodeGuardrails::default()
        };

        let exact_reads = Rc::new(Cell::new(0));
        let exact_input = vec![1_u8; 8];
        let input = read_seekable_input_to_vec(
            UnderreportedLengthReader {
                inner: Cursor::new(exact_input.clone()),
                bytes_read: Rc::clone(&exact_reads),
            },
            &guardrails,
        )
        .expect("an input exactly at max_input_bytes should be accepted");
        assert_eq!(input, exact_input);
        assert_eq!(exact_reads.get(), 8);

        let oversized_reads = Rc::new(Cell::new(0));
        let error = read_seekable_input_to_vec(
            UnderreportedLengthReader {
                inner: Cursor::new(vec![2_u8; 64]),
                bytes_read: Rc::clone(&oversized_reads),
            },
            &guardrails,
        )
        .expect_err("an input larger than max_input_bytes should be rejected");
        assert_eq!(oversized_reads.get(), 9);
        assert!(
            error
                .to_string()
                .contains("input exceeds configured max_input_bytes")
        );
    }

    /// Decode does not bake EXIF orientation into pixels, so hook callers
    /// rely on `exif_metadata`/`orientation` to rotate correctly; without
    /// the override they would always see `Orientation::NoTransforms`.
    #[test]
    fn hook_decoder_exposes_exif_orientation() {
        let (file, expected_rgba, expected_tiff) =
            crate::isobmff::test_support::minimal_uncompressed_rgb3_heif_with_exif_orientation(6);

        let mut decoder = decoder_from_seekable_with_hint_and_guardrails(
            Cursor::new(file),
            Some(HeifInputFamily::Heif),
            DecodeGuardrails::default(),
        )
        .expect("hook construction should succeed");
        assert_eq!(
            decoder
                .exif_metadata()
                .expect("exif metadata read should succeed"),
            Some(expected_tiff)
        );
        assert_eq!(
            decoder
                .orientation()
                .expect("orientation read should succeed"),
            image::metadata::Orientation::Rotate90
        );

        // Pixels stay unrotated; orientation is applied by the caller.
        let mut pixels = vec![0_u8; expected_rgba.len()];
        decoder
            .read_image_boxed(&mut pixels)
            .expect("lazy uncompressed decode should succeed");
        assert_eq!(pixels, expected_rgba);
    }

    /// When the primary item carries `irot`/`imir`, decode bakes that
    /// transform into the pixels, so `orientation()` must report
    /// `NoTransforms` — otherwise generic `orientation()` +
    /// `apply_orientation` callers double-rotate. Mirrors
    /// `ExifOrientationHint::should_apply_exif_orientation`.
    #[test]
    fn hook_decoder_suppresses_exif_orientation_when_transforms_bake_rotation() {
        let (file, unrotated_rgba, expected_tiff) = crate::isobmff::test_support::
            minimal_uncompressed_rgb3_heif_with_exif_orientation_and_transforms(
                6,
                &[crate::isobmff::test_support::irot_box(1)],
            );

        let mut decoder = decoder_from_seekable_with_hint_and_guardrails(
            Cursor::new(file),
            Some(HeifInputFamily::Heif),
            DecodeGuardrails::default(),
        )
        .expect("hook construction should succeed");

        // The EXIF block itself stays exposed (camera metadata and friends);
        // only the derived orientation is suppressed.
        assert_eq!(
            decoder
                .exif_metadata()
                .expect("exif metadata read should succeed"),
            Some(expected_tiff)
        );
        assert_eq!(
            decoder
                .orientation()
                .expect("orientation read should succeed"),
            image::metadata::Orientation::NoTransforms
        );

        // The irot is baked into the pixels: 2x1 rotated 90 degrees CCW.
        assert_eq!(decoder.dimensions(), (1, 2));
        let mut pixels = vec![0_u8; unrotated_rgba.len()];
        decoder
            .read_image_boxed(&mut pixels)
            .expect("lazy uncompressed decode should succeed");
        let expected_rotated: Vec<u8> = unrotated_rgba[4..8]
            .iter()
            .chain(&unrotated_rgba[0..4])
            .copied()
            .collect();
        assert_eq!(pixels, expected_rotated);
    }

    /// An identity `irot` (angle 0) bakes nothing into the pixels, so it
    /// must NOT suppress the EXIF-derived orientation: suppressing here
    /// would leave generic `orientation()` + `apply_orientation` callers
    /// with an unrotated image. Locks the identity gate shared with
    /// `ExifOrientationHint::should_apply_exif_orientation`.
    #[test]
    fn hook_decoder_keeps_exif_orientation_for_identity_irot() {
        let (file, expected_rgba, expected_tiff) = crate::isobmff::test_support::
            minimal_uncompressed_rgb3_heif_with_exif_orientation_and_transforms(
                6,
                &[crate::isobmff::test_support::irot_box(0)],
            );

        let mut decoder = decoder_from_seekable_with_hint_and_guardrails(
            Cursor::new(file),
            Some(HeifInputFamily::Heif),
            DecodeGuardrails::default(),
        )
        .expect("hook construction should succeed");

        assert_eq!(
            decoder
                .exif_metadata()
                .expect("exif metadata read should succeed"),
            Some(expected_tiff)
        );
        assert_eq!(
            decoder
                .orientation()
                .expect("orientation read should succeed"),
            image::metadata::Orientation::Rotate90
        );

        // The identity irot leaves the pixels untouched and unrotated.
        assert_eq!(decoder.dimensions(), (2, 1));
        let mut pixels = vec![0_u8; expected_rgba.len()];
        decoder
            .read_image_boxed(&mut pixels)
            .expect("lazy uncompressed decode should succeed");
        assert_eq!(pixels, expected_rgba);
    }

    #[test]
    fn hook_decoder_survives_lying_seek_length() {
        let (file, expected_rgba) = crate::isobmff::test_support::minimal_uncompressed_rgb3_heif();

        let decoder = decoder_from_seekable_with_hint_and_guardrails(
            LyingLengthReader(Cursor::new(file)),
            Some(HeifInputFamily::Heif),
            DecodeGuardrails::default(),
        )
        .expect("a lying seek length must not fail hook construction");

        let mut pixels = vec![0_u8; expected_rgba.len()];
        decoder
            .read_image_boxed(&mut pixels)
            .expect("decode should use the bytes the reader actually yields");
        assert_eq!(pixels, expected_rgba);
    }
}
