#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(clippy::all)]
#![allow(deprecated)]
#![deny(duplicate_macro_attributes)]

pub mod g1;
pub mod g1_cards;
pub mod g1_concurrent;

pub use g1::{g1_object_address_census, G1CollectionType, G1Collector, G1CollectorConfig};
pub use g1_concurrent::{ConcurrentMarkController, ConcurrentMarkState};
