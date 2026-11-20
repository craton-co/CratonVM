use super::*;

fn replaceable_object() -> Graph {
    let mut g = Graph::new();
    let alloc = g.add_node(
        Op::New {
            class_id: 1,
            num_fields: 1,
        },
        vec![],
    );
    let c = g.add_node(Op::Const(42), vec![]);
    let _st = g.add_node(Op::Store(0), vec![alloc, c]);
    let _ld = g.add_node(Op::Load(0), vec![alloc]);
    g
}


