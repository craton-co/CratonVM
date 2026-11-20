use super::{withdrawal_census_line, WITHDRAWAL_RECOMPILE_CUTOFF};

/// JIT round 22 (lane sync, CODECACHE21-3): nothing when no method was
/// charged; otherwise the top five by count, ties by name, bailed marked.
#[test]
fn the_withdrawal_census_names_the_top_five() {
    assert_eq!(withdrawal_census_line(Vec::new()), None);
    let rows = vec![
        (3, false, "a.m()V".to_string()),
        (WITHDRAWAL_RECOMPILE_CUTOFF, true, "z.storm()V".to_string()),
        (7, false, "b.m()V".to_string()),
        (7, false, "a.n()V".to_string()),
        (1, false, "c.m()V".to_string()),
        (2, false, "d.m()V".to_string()),
    ];
    let line = withdrawal_census_line(rows).expect("rows were charged");
    assert_eq!(
        line,
        format!(
            "[cratonvm] JIT withdrawal census: methods=6 bailed=1 cutoff={WITHDRAWAL_RECOMPILE_CUTOFF} \
             | top: z.storm()V={WITHDRAWAL_RECOMPILE_CUTOFF}(bailed) a.n()V=7 b.m()V=7 a.m()V=3 d.m()V=2"
        )
    );
}

