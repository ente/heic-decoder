use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{BoundedDecodeError, Result};

#[derive(Clone)]
pub(super) struct Budget(Arc<State>);

struct State {
    limit: usize,
    used: AtomicUsize,
}

pub(super) struct Reservation {
    budget: Budget,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.0.used.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

impl Budget {
    pub(super) fn new(limit: usize) -> Self {
        Self(Arc::new(State {
            limit,
            used: AtomicUsize::new(0),
        }))
    }

    pub(super) fn reserve(&self, bytes: usize, stage: &'static str) -> Result<Reservation> {
        let result = self
            .0
            .used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes)
                    .filter(|&total| total <= self.0.limit)
            });
        match result {
            Ok(_) => Ok(Reservation {
                budget: self.clone(),
                bytes,
            }),
            Err(used) => Err(BoundedDecodeError::MemoryBudgetExceeded {
                stage,
                required: used.saturating_add(bytes),
                limit: self.0.limit,
            }),
        }
    }

    pub(super) fn available(&self) -> usize {
        self.0.limit - self.0.used.load(Ordering::Relaxed)
    }

    pub(super) fn buffer<T>(&self, len: usize, stage: &'static str) -> Result<Buffer<T>> {
        let bytes = len
            .checked_mul(size_of::<T>())
            .ok_or(BoundedDecodeError::LimitExceeded("buffer size overflow"))?;
        let reservation = self.reserve(bytes, stage)?;
        let mut data = Vec::new();
        data.try_reserve_exact(len)
            .map_err(|_| BoundedDecodeError::AllocationFailed)?;
        Ok(Buffer {
            data,
            _reservation: reservation,
        })
    }

    pub(super) fn zeroed<T: Default + Clone>(
        &self,
        len: usize,
        stage: &'static str,
    ) -> Result<Buffer<T>> {
        let mut buffer = self.buffer(len, stage)?;
        buffer.data.resize(len, T::default());
        Ok(buffer)
    }
}

pub(super) struct Buffer<T> {
    data: Vec<T>,
    _reservation: Reservation,
}

impl<T> Buffer<T> {
    pub(super) fn push(&mut self, value: T) -> Result<()> {
        if self.data.len() == self.data.capacity() {
            return Err(BoundedDecodeError::LimitExceeded("index capacity"));
        }
        self.data.push(value);
        Ok(())
    }

    pub(super) fn into_vec(self) -> Vec<T> {
        self.data
    }
}

impl<T> Deref for Buffer<T> {
    type Target = [T];
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<T> DerefMut for Buffer<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}
