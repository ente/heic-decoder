use alloc::vec::Vec;

use super::{HevcError, Result};

pub(super) fn filled<T: Clone>(value: T, len: usize) -> Result<Vec<T>> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(len)
        .map_err(|_| HevcError::DecodingError("allocation failed"))?;
    result.resize(len, value);
    Ok(result)
}

pub(super) fn copy<T: Copy>(source: &[T]) -> Result<Vec<T>> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(source.len())
        .map_err(|_| HevcError::DecodingError("allocation failed"))?;
    result.extend_from_slice(source);
    Ok(result)
}
