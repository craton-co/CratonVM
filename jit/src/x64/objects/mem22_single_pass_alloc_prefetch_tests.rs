use super::*;


const CURSOR_OFF: usize = 0x40;
const END_OFF: usize = 0x48;



/// The position of the first occurrence of `needle` in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}






