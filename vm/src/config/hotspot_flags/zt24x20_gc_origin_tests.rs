use super::resolved_flags;
use crate::config::{GcAlgorithm, VmConfig};

fn origin_of(config: &VmConfig, name: &str) -> &'static str {
    resolved_flags(config)
        .into_iter()
        .find(|row| row.name == name)
        .map(|row| row.origin)
        .expect("a modelled row")
}

