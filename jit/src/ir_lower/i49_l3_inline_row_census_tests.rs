use super::{classify_inline_row_census, InlineRowCensus, InlineRowCensusVerdict};

/// A call is the node's whose code it lies in (the last mark strictly
/// before its return address); only a spliced node's calls count, a row
/// at the return address clears one, and a call past the last node is
/// out of line.
#[test]
fn calls_are_attributed_to_their_node_and_checked_against_the_rows() {
    let census = InlineRowCensus {
        // prologue node at 0 (own code), spliced node pc 40 at 10, own
        // node at 30, an empty spliced node pc 41 at 50, spliced pc 42 at 50.
        marks: vec![
            (0, None),
            (10, Some(40)),
            (30, None),
            (50, Some(41)),
            (50, Some(42)),
        ],
        closed_at: Some(70),
    };
    let filed = [20u32].into_iter().collect();
    let calls = [5, 20, 30, 35, 55, 70, 80];
    assert_eq!(
        classify_inline_row_census(&census, &calls, &filed),
        InlineRowCensusVerdict {
            // 20 (filed), 30 (ends exactly at the own node's start: still
            // the spliced node's), 55 and 70 (pc 42's).
            spliced: 4,
            rowless: vec![(30, 40), (55, 42), (70, 42)],
            out_of_line: 1,
        }
    );
}
