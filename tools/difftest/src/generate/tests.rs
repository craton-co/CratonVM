use super::*;

#[test]
fn rng_is_deterministic_for_a_seed() {
    let mut a = Rng::new(42);
    let mut b = Rng::new(42);
    for _ in 0..100 {
        assert_eq!(a.next_u64(), b.next_u64());
    }
    // A different seed diverges.
    let mut c = Rng::new(43);
    assert_ne!(Rng::new(42).next_u64(), c.next_u64());
}

#[test]
fn generation_is_reproducible() {
    let a = generate(TargetFamily::Arithmetic, 5, 7);
    let b = generate(TargetFamily::Arithmetic, 5, 7);
    assert_eq!(a.len(), 5);
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.name, y.name);
        assert_eq!(x.source, y.source);
    }
}

#[test]
fn arithmetic_programs_are_well_formed() {
    for p in generate(TargetFamily::Arithmetic, 10, 1) {
        assert!(p.source.contains(&format!("public class {}", p.name)));
        assert!(p.source.contains("public static void main"));
        assert!(p.source.contains("System.out.println"));
        assert!(balanced_braces(&p.source), "unbalanced: {}", p.name);
    }
}

#[test]
fn concat_programs_are_well_formed() {
    for p in generate(TargetFamily::StringConcat, 10, 2) {
        assert!(p.source.contains("public static void main"));
        assert!(balanced_braces(&p.source));
    }
}

#[test]
fn family_labels_round_trip() {
    for f in TargetFamily::all() {
        assert_eq!(TargetFamily::from_label(f.label()), Some(f));
    }
    assert_eq!(TargetFamily::from_label("nope"), None);
}

fn balanced_braces(s: &str) -> bool {
    let mut depth = 0i32;
    for ch in s.chars() {
        match ch {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return false;
        }
    }
    depth == 0
}
