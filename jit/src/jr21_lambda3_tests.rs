use super::*;

const PROXY: u32 = 0x8000_0031;

fn site(captures: usize, pcs: Vec<(usize, usize)>) -> IrLambdaImplSite {
    IrLambdaImplSite {
        pcs,
        class: "p/Host".to_string(),
        name: "lambda$k$0".to_string(),
        desc: "(II)I".to_string(),
        class_id: 0x7131,
        captures,
    }
}

fn plan(rows: Vec<(u16, IrLambdaImplSite)>) -> IrLambdaImplPlan {
    IrLambdaImplPlan {
        by_cp: rows.into_iter().collect(),
    }
}

#[test]
fn a_dropped_splice_unplans_only_the_capturing_call_there() {
    let p = plan(vec![
        (1, site(2, vec![(10, 5), (30, 5)])),
        (2, site(0, vec![(20, 15)])),
        (3, site(1, vec![(40, 35)])),
    ])
    .without_capture_pcs(&[10, 20, 40]);
    // The capturing entry keeps its other call; the non-capturing call at
    // a dropped pc is kept; an entry left with no call is gone.
    assert_eq!(p.site_count(), 2);
    assert_eq!(
        p.builder_sites(),
        HashMap::from([(30usize, (5usize, 2usize, 2usize)), (20, (15, 0, 2))])
    );
    assert!(p.names(3).is_none());
    assert_eq!(
        p.rewritten_invoke_ops(&[(10, 9, 0xb9), (30, 9, 0xb9), (40, 9, 0xb9)]),
        vec![(10, 9, 0xb9), (30, 1, 0xb8), (40, 9, 0xb9)]
    );
    // Nothing to drop: the plan is unchanged.
    let q = plan(vec![(1, site(2, vec![(10, 5)]))]).without_capture_pcs(&[]);
    assert_eq!(q.site_count(), 1);
}

#[test]
fn the_census_names_the_call_route_and_the_proxy() {
    let mut g = ir::Graph::empty(8);
    g.add(
        ir::Op::New {
            class_id: PROXY,
            num_fields: 2,
        },
        ir::IrType::Ref,
        vec![],
        Some(5),
    );
    g.lambda_proxy_alloc_classes.insert(PROXY);
    g.add(
        ir::Op::Call { info_ptr: 0 },
        ir::IrType::Int,
        vec![],
        Some(10),
    );
    let p = plan(vec![
        (1, site(2, vec![(10, 5)])),
        (2, site(0, vec![(20, 15)])),
    ]);
    let none: HashMap<usize, (usize, bool)> = HashMap::new();
    let no_cells: HashMap<usize, usize> = HashMap::new();
    let lines = p.census_lines(&g, &none, &no_cells);
    assert_eq!(lines.len(), 2);
    assert!(
        lines[0].starts_with("pc=10 indy_pc=5 impl=p/Host.lambda$k$0(II)I captures=2"),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].contains("call=generic calls=1 proxy=kept"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains("call=spliced calls=0 proxy=singleton"),
        "{}",
        lines[1]
    );
    let bound: HashMap<usize, (usize, bool)> = HashMap::from([(10usize, (0x1000usize, false))]);
    let lines = p.census_lines(&g, &bound, &no_cells);
    assert!(lines[0].contains("call=bound"), "{}", lines[0]);
    let cells: HashMap<usize, usize> = HashMap::from([(10usize, 0x2000usize)]);
    let lines = p.census_lines(&g, &none, &cells);
    assert!(lines[0].contains("call=late-cell"), "{}", lines[0]);
    // The proxy's `New` gone (scalar-replaced) and the call spliced.
    for n in g.nodes.iter_mut() {
        n.op = ir::Op::Dead;
    }
    let lines = p.census_lines(&g, &none, &no_cells);
    assert!(
        lines[0].contains("call=spliced calls=0 proxy=elided"),
        "{}",
        lines[0]
    );
}



/// The retry without the plan runs only after every other retry declined
/// (`ir_tier`'s last arm), and only for an attempt that planned a call.
#[test]
fn the_tier_retries_once_without_the_plan_last() {
    let src = include_str!("lib_parts/debugger_write_door.rs");
    let tier: String = src
        .split("fn ir_tier(")
        .nth(1)
        .expect("ir_tier is in this file")
        .split("\npub")
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let rebuild = tier
        .find("letSome(site)=rebuild_without")
        .expect("the splice rebuild arm");
    let retry = tier
        .find("lambda_plan_withheld=true;continue;")
        .expect("the lambda retry");
    assert!(rebuild < retry);
    assert!(tier.contains("iflambda_plan_built&&!lambda_plan_withheld"));
}
