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
    HeifInputFamily, decode_bufread_to_rgba_with_guardrails,
    decode_bytes_to_rgba_layout_with_hint_and_guardrails, decode_bytes_to_rgba_with_guardrails,
    decode_bytes_to_rgba8_slice_with_hint_and_guardrails,
    decode_bytes_to_rgba16_native_endian_bytes_with_hint_and_guardrails,
    decode_path_to_rgba_with_guardrails, decode_read_to_rgba_with_guardrails,
    decode_seekable_to_rgba_with_hint_and_guardrails,
};
use image::error::{
    DecodingError, ImageFormatHint, ParameterError, ParameterErrorKind, UnsupportedError,
    UnsupportedErrorKind,
};
use image::hooks;
use image::{ColorType, DynamicImage, ImageBuffer, ImageDecoder, ImageError, ImageResult, Rgba};
use std::error::Error;
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::Path;
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

/// Dedicated `image::ImageDecoder` adapter backed by decoded RGBA samples.
///
/// This adapter decodes HEIF/HEIC/AVIF inputs directly into in-memory RGBA and
/// exposes the buffer via the `image` crate's decoder trait without any PNG
/// intermediate transcode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeifImageDecoder {
    decoded: DecodedRgbaImage,
}

impl HeifImageDecoder {
    /// Build an adapter from an already decoded RGBA image.
    pub fn from_decoded(decoded: DecodedRgbaImage) -> ImageResult<Self> {
        validate_decoded_rgba_image(&decoded)?;
        Ok(Self { decoded })
    }

    /// Decode HEIF/HEIC/AVIF bytes into an `image::ImageDecoder` adapter.
    pub fn from_bytes(input: &[u8]) -> ImageResult<Self> {
        Self::from_bytes_with_guardrails(input, DecodeGuardrails::default())
    }

    /// Decode HEIF/HEIC/AVIF bytes into an `image::ImageDecoder` adapter with configurable guardrails.
    pub fn from_bytes_with_guardrails(
        input: &[u8],
        guardrails: DecodeGuardrails,
    ) -> ImageResult<Self> {
        let decoded = decode_bytes_to_rgba_with_guardrails(input, guardrails)
            .map_err(decode_error_to_image_error)?;
        Self::from_decoded(decoded)
    }

    /// Decode a `Read` source into an `image::ImageDecoder` adapter.
    pub fn from_read<R: Read>(input_reader: R) -> ImageResult<Self> {
        Self::from_read_with_guardrails(input_reader, DecodeGuardrails::default())
    }

    /// Decode a `Read` source into an `image::ImageDecoder` adapter with configurable guardrails.
    pub fn from_read_with_guardrails<R: Read>(
        input_reader: R,
        guardrails: DecodeGuardrails,
    ) -> ImageResult<Self> {
        let decoded = decode_read_to_rgba_with_guardrails(input_reader, guardrails)
            .map_err(decode_error_to_image_error)?;
        Self::from_decoded(decoded)
    }

    /// Decode a seekable `Read` source into an `image::ImageDecoder` adapter.
    pub fn from_seekable<R: Read + Seek>(input_reader: R) -> ImageResult<Self> {
        Self::from_seekable_with_guardrails(input_reader, DecodeGuardrails::default())
    }

    /// Decode a seekable `Read` source into an `image::ImageDecoder` adapter with configurable guardrails.
    pub fn from_seekable_with_guardrails<R: Read + Seek>(
        input_reader: R,
        guardrails: DecodeGuardrails,
    ) -> ImageResult<Self> {
        Self::from_seekable_with_hint_and_guardrails(input_reader, None, guardrails)
    }

    /// Decode a `BufRead` source into an `image::ImageDecoder` adapter.
    pub fn from_bufread<R: BufRead>(input_reader: R) -> ImageResult<Self> {
        Self::from_bufread_with_guardrails(input_reader, DecodeGuardrails::default())
    }

    /// Decode a `BufRead` source into an `image::ImageDecoder` adapter with configurable guardrails.
    pub fn from_bufread_with_guardrails<R: BufRead>(
        input_reader: R,
        guardrails: DecodeGuardrails,
    ) -> ImageResult<Self> {
        let decoded = decode_bufread_to_rgba_with_guardrails(input_reader, guardrails)
            .map_err(decode_error_to_image_error)?;
        Self::from_decoded(decoded)
    }

    /// Decode a file path into an `image::ImageDecoder` adapter.
    pub fn from_path(input_path: &Path) -> ImageResult<Self> {
        Self::from_path_with_guardrails(input_path, DecodeGuardrails::default())
    }

    /// Decode a file path into an `image::ImageDecoder` adapter with configurable guardrails.
    pub fn from_path_with_guardrails(
        input_path: &Path,
        guardrails: DecodeGuardrails,
    ) -> ImageResult<Self> {
        let decoded = decode_path_to_rgba_with_guardrails(input_path, guardrails)
            .map_err(decode_error_to_image_error)?;
        Self::from_decoded(decoded)
    }

    /// Consume the adapter and return the owned decoded RGBA buffer.
    pub fn into_decoded_rgba(self) -> DecodedRgbaImage {
        self.decoded
    }

    fn from_seekable_with_hint_and_guardrails<R: Read + Seek>(
        input_reader: R,
        hint: Option<HeifInputFamily>,
        guardrails: DecodeGuardrails,
    ) -> ImageResult<Self> {
        let decoded =
            decode_seekable_to_rgba_with_hint_and_guardrails(input_reader, hint, guardrails)
                .map_err(decode_error_to_image_error)?;
        Self::from_decoded(decoded)
    }

    fn storage_color_type(&self) -> ColorType {
        storage_color_type_from_bit_depth(self.decoded.storage_bit_depth())
    }

    fn expected_total_bytes(&self) -> ImageResult<usize> {
        expected_rgba_total_bytes(
            self.decoded.width,
            self.decoded.height,
            self.decoded.storage_bit_depth(),
        )
    }
}

impl ImageDecoder for HeifImageDecoder {
    fn dimensions(&self) -> (u32, u32) {
        (self.decoded.width, self.decoded.height)
    }

    fn color_type(&self) -> ColorType {
        self.storage_color_type()
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.decoded.icc_profile.clone())
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

        match self.decoded.pixels {
            DecodedRgbaPixels::U8(pixels) => {
                buf.copy_from_slice(&pixels);
            }
            DecodedRgbaPixels::U16(pixels) => {
                write_rgba16_native_endian_bytes(&pixels, buf);
            }
        }

        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

struct LazyHeifImageDecoder {
    input: Vec<u8>,
    hint: Option<HeifInputFamily>,
    guardrails: DecodeGuardrails,
    layout: DecodedRgbaLayout,
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
        layout: DecodedRgbaLayout,
    ) -> Self {
        Self {
            input,
            hint,
            guardrails,
            layout,
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
                buf,
            )
            .map_err(decode_error_to_image_error),
            16 => decode_bytes_to_rgba16_native_endian_bytes_with_hint_and_guardrails(
                &self.input,
                self.hint,
                self.guardrails,
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
    let layout =
        decode_bytes_to_rgba_layout_with_hint_and_guardrails(&input, hint, guardrails.clone())
            .map_err(decode_error_to_image_error)?;
    Ok(Box::new(LazyHeifImageDecoder::from_encoded_input(
        input, hint, guardrails, layout,
    )))
}

/// Read the whole encoded input into memory.
///
/// This is a deliberate trade: the lazy decoder needs the full input as a
/// byte slice so `read_image` can decode directly into the caller's buffer
/// (without an additional full-frame owned RGBA allocation, which would
/// dominate peak memory). The cost is that the encoded input — usually far
/// smaller than the decoded RGBA — is held in memory for the decoder's
/// lifetime, bounded only by `guardrails.max_input_bytes`.
/// Pre-allocation ceiling for the seek-reported input length. The reported
/// length is untrusted until the bytes are actually read: a lying or corrupt
/// reader could otherwise trigger a multi-gigabyte allocation (or a
/// capacity-overflow panic) before a single byte arrives. `read_to_end` still
/// grows the buffer past this for genuinely larger, guardrail-permitted
/// inputs.
const MAX_INPUT_PREALLOCATION_BYTES: u64 = 64 * 1024 * 1024;

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
    let prealloc_len = input_len.min(MAX_INPUT_PREALLOCATION_BYTES);
    let capacity = usize::try_from(prealloc_len).map_err(|_| {
        parameter_error(format!(
            "input size {prealloc_len} bytes does not fit in memory on this platform"
        ))
    })?;
    let mut input = Vec::with_capacity(capacity);
    input_reader
        .read_to_end(&mut input)
        .map_err(ImageError::IoError)?;

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
/// and a single grid-tile scratch buffer may still be allocated. The default
/// guardrails leave the encoded input buffer unbounded; production callers
/// should prefer
/// [`register_image_decoder_hooks_with_guardrails`] with
/// [`DecodeGuardrails::max_input_bytes`] set.
pub fn register_image_decoder_hooks() -> ImageHookRegistration {
    register_image_decoder_hooks_with_guardrails(DecodeGuardrails::default())
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

fn validate_decoded_rgba_image(decoded: &DecodedRgbaImage) -> ImageResult<()> {
    if decoded.storage_bit_depth() != 8 && decoded.storage_bit_depth() != 16 {
        return Err(ImageError::Unsupported(
            UnsupportedError::from_format_and_kind(
                heif_image_format_hint(),
                UnsupportedErrorKind::GenericFeature(format!(
                    "unsupported decoded RGBA storage bit depth {}",
                    decoded.storage_bit_depth()
                )),
            ),
        ));
    }

    let expected_samples =
        expected_rgba_sample_count(decoded.width, decoded.height).ok_or_else(|| {
            parameter_error(format!(
                "decoded RGBA sample count overflow for {}x{} image",
                decoded.width, decoded.height
            ))
        })?;
    let actual_samples = match &decoded.pixels {
        DecodedRgbaPixels::U8(pixels) => pixels.len(),
        DecodedRgbaPixels::U16(pixels) => pixels.len(),
    };
    if actual_samples != expected_samples {
        return Err(parameter_error(format!(
            "decoded RGBA sample count mismatch for {}x{} image: expected {expected_samples}, got {actual_samples}",
            decoded.width, decoded.height
        )));
    }

    Ok(())
}

fn write_rgba16_native_endian_bytes(samples: &[u16], out: &mut [u8]) {
    for (sample, chunk) in samples.iter().zip(out.chunks_exact_mut(2)) {
        chunk.copy_from_slice(&sample.to_ne_bytes());
    }
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
        decoder_from_seekable_with_hint_and_guardrails,
    };
    use std::io::Cursor;

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
