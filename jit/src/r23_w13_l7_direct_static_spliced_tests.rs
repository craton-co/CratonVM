use super::{ir_direct_static_spliced_feed_enabled, ir_direct_static_spliced_rows};
use crate::ir::{Graph, IrType, Op};

fn load_static(g: &mut Graph, class_id: u32, field_index: u32, is_volatile: bool) {
    let start = g.entry;
    g.add(
        Op::LoadStatic {
            class_id,
            field_index,
            type_tag: b'L',
            is_volatile,
        },
        IrType::Ref,
        vec![start, start],
        Some(1),
    );
}

#[test]
fn ir_trusted_array_facts_preserve_the_final_reference_fact_for_spliced_reads() {
    let mut g = Graph::empty(0);
    g.entry = g.add(Op::Start, IrType::Control, vec![], None);
    load_static(&mut g, 579, 2, false);
    load_static(&mut g, 579, 2, false); // repeated nested read
    load_static(&mut g, 579, 5, false); // already known final
    load_static(&mut g, 579, 7, true); // volatile: never folded
    load_static(&mut g, 600, 1, false); // holder not initialized
    load_static(&mut g, 579, 9, false); // no trusted length
    g.trusted_final_static_fields.push((579, 5));
    super::ir_feed_trusted_static_arrays(&mut g, |c, _| c == 579,
        &|c, i| match (c, i) {
            (579, 2) => Some(128),
            (579, 5) => Some(0),
            (579, 7) | (600, 1) => Some(32),
            _ => None,
        });
    assert_eq!(g.trusted_static_array_lengths, vec![((579, 2), 128), ((579, 5), 0)]);
    assert_eq!(g.trusted_final_static_fields, vec![(579, 5), (579, 2)],
        "a trusted array length also proves its initialized final reference, including a spliced read");
}

