#![allow(deprecated)]
// A.8.a: this module defines or implements legacy ProviderCodec types; the deprecation warning is the signal for external callers, not this internal impl. Removal under A.8.b.

//! Body and event stream type aliases.
//!
//! Defining these as boxed `Stream` here keeps `ProviderCodec` and other
//! ports framework-free. The driving adapter (rama service) is
//! responsible for converting between rama's `Body` and `BodyStream`
//! at the boundary.

use std::pin::Pin;

use bytes::Bytes;
use futures::Stream;

use crate::{BoxError, NormalizedEvent};

pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, BoxError>> + Send + 'static>>;

pub type EventStream =
    Pin<Box<dyn Stream<Item = Result<NormalizedEvent, BoxError>> + Send + 'static>>;
