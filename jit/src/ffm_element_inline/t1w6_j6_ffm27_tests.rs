use super::*;
use std::collections::HashMap;

const SEG: &str = "java/lang/foreign/MemorySegment";
const PUT: &str = "(Ljava/lang/foreign/ValueLayout$OfInt;JI)V";


type Prof<'a> = Option<&'a crate::profile::MethodProfile>;

fn facts(carrier: u8) -> HashMap<(u32, u32), FfmLayoutFacts> {
    let mut m = HashMap::new();
    m.insert(
        (7, 1),
        FfmLayoutFacts {
            carrier,
            width: 4,
            align: 4,
            big_endian: false,
        },
    );
    m
}




