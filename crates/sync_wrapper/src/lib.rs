//! A small, independently written stand-in for the `sync_wrapper` crate,
//! providing the API reqwest and tower use under the MIT license.
//!
//! [`SyncWrapper`] only hands out its value through `&mut self` or by
//! value. A shared `&SyncWrapper<T>` therefore gives no access to `T` at
//! all, so the wrapper can be `Sync` even when `T` is not.
#![no_std]

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// A value reachable only through exclusive access, and therefore `Sync`.
#[repr(transparent)]
pub struct SyncWrapper<T>(T);

// SAFETY: no method takes `&self`, so a shared reference cannot reach `T`;
// sharing a `&SyncWrapper<T>` between threads exposes nothing.
unsafe impl<T> Sync for SyncWrapper<T> {}

impl<T> SyncWrapper<T> {
    /// Wrap `value`.
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Exclusive access to the value.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.0
    }

    /// Pinned exclusive access to the value.
    pub fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut T> {
        // SAFETY: the value is never moved out of a pinned wrapper; this is a
        // plain structural projection to the only field.
        unsafe { self.map_unchecked_mut(|wrapper| &mut wrapper.0) }
    }

    /// Unwrap the value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for SyncWrapper<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("SyncWrapper")
    }
}

impl<T: Default> Default for SyncWrapper<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> From<T> for SyncWrapper<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

/// A future that is `Sync` whatever the future it wraps.
pub struct SyncFuture<F> {
    inner: SyncWrapper<F>,
}

impl<F: Future> SyncFuture<F> {
    pub fn new(inner: F) -> Self {
        Self {
            inner: SyncWrapper::new(inner),
        }
    }

    pub fn into_inner(self) -> F {
        self.inner.into_inner()
    }
}

impl<F: Future> Future for SyncFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: structural projection to the only field.
        let inner = unsafe { self.map_unchecked_mut(|future| &mut future.inner) };
        inner.get_pin_mut().poll(cx)
    }
}

impl<F> fmt::Debug for SyncFuture<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("SyncFuture")
    }
}

/// A stream that is `Sync` whatever the stream it wraps.
#[cfg(feature = "futures")]
pub struct SyncStream<S> {
    inner: SyncWrapper<S>,
}

#[cfg(feature = "futures")]
impl<S: futures_core::Stream> SyncStream<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner: SyncWrapper::new(inner),
        }
    }

    pub fn into_inner(self) -> S {
        self.inner.into_inner()
    }
}

#[cfg(feature = "futures")]
impl<S: futures_core::Stream> futures_core::Stream for SyncStream<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<S::Item>> {
        // SAFETY: structural projection to the only field.
        let inner = unsafe { self.map_unchecked_mut(|stream| &mut stream.inner) };
        inner.get_pin_mut().poll_next(cx)
    }
}

#[cfg(feature = "futures")]
impl<S> fmt::Debug for SyncStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("SyncStream")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    fn assert_sync<T: Sync>(_: &T) {}

    #[test]
    fn wraps_values_that_are_not_sync() {
        let mut wrapper = SyncWrapper::new(Cell::new(1));
        assert_sync(&wrapper);
        wrapper.get_mut().set(2);
        assert_eq!(wrapper.into_inner().get(), 2);
        assert_eq!(SyncWrapper::<u8>::default().into_inner(), 0);
        assert_eq!(SyncWrapper::from(3).into_inner(), 3);
    }

    #[test]
    fn polls_the_wrapped_future() {
        let future = SyncFuture::new(core::future::ready(Cell::new(7)));
        assert_sync(&future);
        let mut future = core::pin::pin!(future);
        let waker = core::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => assert_eq!(value.get(), 7),
            Poll::Pending => panic!("ready future was pending"),
        }
    }
}
