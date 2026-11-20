use super::*;

fn read(rel: &str) -> String {
    let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .replace("\r\n", "\n")
}

fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

/// The slice door retries the runner before either failure arm.
#[test]
fn gci_i30gv_run_on_runner_waits_for_a_runner_before_failing() {
    let src = read("src/vm/jdk_continuation.rs");
    let runner = body(&src, "fn run_on_runner(");
    assert!(runner.contains("match acquire_runner_with_backoff(runners, shared) {"));
    assert!(!runner.contains("runners.acquire(shared)"), "no un-retried acquire");
    let retry = body(&src, "fn acquire_runner_with_backoff(");
    let acquire = retry.find("runners.acquire(shared)").expect("acquire");
    let give_up = retry
        .find("if tries >= RUNNER_ACQUIRE_RETRIES => return Err(message)")
        .expect("bounded");
    let sleep = retry.find("std::thread::sleep(wait);").expect("backoff");
    assert!(acquire < give_up && give_up < sleep);
}

/// The backoff is bounded: about two seconds in all, never unbounded.
#[test]
fn gci_i30gv_the_runner_backoff_totals_about_two_seconds() {
    let mut wait = std::time::Duration::from_millis(1);
    let mut total = std::time::Duration::ZERO;
    for _ in 0..RUNNER_ACQUIRE_RETRIES {
        total += wait;
        wait = (wait * 2).min(std::time::Duration::from_millis(64));
    }
    assert!(total >= std::time::Duration::from_millis(1500), "{total:?}");
    assert!(total <= std::time::Duration::from_millis(2500), "{total:?}");
}
