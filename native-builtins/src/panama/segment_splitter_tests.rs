use super::{pe_split_bounds, PE_SPLITTER_CHARACTERISTICS};

#[test]
fn an_even_count_splits_down_the_middle() {
    // 8 ints: low half 4 elements / 16 bytes, high half 16 bytes.
    assert_eq!(pe_split_bounds(8, 4), (4, 16, 16));
}

#[test]
fn an_odd_element_stays_with_the_high_half() {
    // 5 ints: low half 2 elements / 8 bytes, high half 12 bytes (3 elements).
    // The JDK keeps the remainder on the side that is NOT handed out, so
    // `lo + hi` must still be the whole segment.
    let (split, lobound, hibound) = pe_split_bounds(5, 4);
    assert_eq!((split, lobound, hibound), (2, 8, 12));
    assert_eq!(
        lobound + hibound,
        5 * 4,
        "the two halves must tile the segment"
    );
}

#[test]
fn the_halves_always_tile_the_segment() {
    for count in 1..64_i64 {
        for size in [1_i64, 2, 4, 8] {
            let (split, lobound, hibound) = pe_split_bounds(count, size);
            assert_eq!(
                lobound + hibound,
                count * size,
                "count {count} size {size} leaves a gap or an overlap"
            );
            assert_eq!(split * size, lobound, "count {count} size {size}");
            assert!(
                split <= count - split,
                "the low half must never be the larger one"
            );
            // The element counts must tile too, and each half's count must
            // match its byte span. Checking only the BYTE bounds is what let
            // a bad high-half count through: the splitter kept claiming the
            // whole original count over the half-sized segment it had left,
            // and ran off the end on the first element past the middle.
            assert_eq!(
                split + (count - split),
                count,
                "count {count} size {size}: element counts must tile"
            );
            assert_eq!(
                (count - split) * size,
                hibound,
                "count {count} size {size}: the high half's count must match its bytes"
            );
        }
    }
}

#[test]
fn characteristics_match_the_jdk() {
    // Measured on HotSpot JDK 25: `seg.spliterator(JAVA_INT)
    // .characteristics()` is 17744 =
    // NONNULL|SUBSIZED|SIZED|IMMUTABLE|ORDERED.
    assert_eq!(PE_SPLITTER_CHARACTERISTICS, 17744);
}
