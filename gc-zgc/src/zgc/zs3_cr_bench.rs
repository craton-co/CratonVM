use super::*;

/// The spawn/join cost of one `zgc_par_chunks_with` pass, against the
/// serial arm over the same (tiny) work. Ignored: a timing, not a check.
/// `cargo test -p cratonvm-gc --lib zs3cr_bench -- --ignored --nocapture`.
#[test]
#[ignore]
fn zs3cr_bench_scoped_pass_spawn_cost() {
    let items: Vec<usize> = (0..4096).collect();
    let iters = 100u32;
    // The bare OS cost, for scale: one empty scoped spawn + join.
    let t = std::time::Instant::now();
    for _ in 0..iters {
        std::thread::scope(|s| {
            s.spawn(|| std::hint::black_box(1usize));
        });
    }
    eprintln!(
        "ZS3CR spawn bare_scoped_spawn_join_us={:.1}",
        t.elapsed().as_secs_f64() * 1e6 / f64::from(iters)
    );
    for workers in [2usize, 4, 8] {
        let t = std::time::Instant::now();
        for _ in 0..iters {
            std::hint::black_box(zgc_par_chunks_with(
                &items,
                workers,
                8,
                None,
                |c: &[usize]| c.len(),
            ));
        }
        let par = t.elapsed();
        let t = std::time::Instant::now();
        for _ in 0..iters {
            std::hint::black_box(zgc_par_chunks_with(&items, 1, 8, None, |c: &[usize]| {
                c.len()
            }));
        }
        let ser = t.elapsed();
        eprintln!(
            "ZS3CR spawn workers={workers} scoped_pass_us={:.1} serial_pass_us={:.2}",
            par.as_secs_f64() * 1e6 / f64::from(iters),
            ser.as_secs_f64() * 1e6 / f64::from(iters),
        );
    }
}
