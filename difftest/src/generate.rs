// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Corpus generator (design §3.1, tier 2).
//!
//! A small **type-directed** Java generator: it emits self-contained,
//! always-compilable programs whose `main` prints a deterministic transcript of
//! every intermediate, so the **same `.class` runs on both VMs** with no
//! wrapper. Generation is weighted toward the bug history (design §4 "first
//! targets"): arithmetic edge cases, `invokedynamic` string concat, and caught
//! exception identity.
//!
//! Determinism is two-fold and load-bearing:
//! * the **generator** is driven by a seeded xorshift PRNG, so
//!   `gen --seed N` reproduces the exact corpus; and
//! * every **emitted program** is itself deterministic (no wall-clock, no
//!   identity hashes, no map iteration order), so an admitted program's
//!   CratonVM≠HotSpot diff is a real bug, not a coin flip (design §3.3).
//!
//! ## Status: Step 4
//!
//! Wired for the three families below. The bytecode-mutation tier (§3.1 tier 3)
//! is Step 5.

/// A generated program ready to compile + run on both VMs.
#[derive(Debug, Clone)]
pub struct GeneratedProgram {
    /// Class / file name (no extension); also the `public class` name.
    pub name: String,
    /// Java source whose `main` prints a deterministic transcript.
    pub source: String,
}

/// Which target family to bias generation toward (design §4 "first targets").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetFamily {
    /// Integer/long edge cases — overflow, `MIN_VALUE / -1`, `%` of negatives,
    /// shift-distance masking. Reliably proves parity (no FP nondeterminism).
    Arithmetic,
    /// `invokedynamic` string concat — mixed primitive widths, `null`, boxed
    /// values, and FP formatting (`NaN`/`Infinity`/`-0.0`) through the indy
    /// bootstrap.
    StringConcat,
    /// Caught-exception identity — `getClass()` + `getMessage()` for the common
    /// throwables (the JEP-358 / CCE / SIOOBE message-parity surface).
    Exceptions,
}

impl TargetFamily {
    /// The stable kebab-case label used on the CLI (`gen --family`).
    pub fn label(self) -> &'static str {
        match self {
            TargetFamily::Arithmetic => "arith",
            TargetFamily::StringConcat => "concat",
            TargetFamily::Exceptions => "exceptions",
        }
    }

    /// Parse a `--family` label. `all` is handled by the caller.
    pub fn from_label(s: &str) -> Option<TargetFamily> {
        match s {
            "arith" | "arithmetic" => Some(TargetFamily::Arithmetic),
            "concat" | "string-concat" => Some(TargetFamily::StringConcat),
            "exceptions" | "exc" => Some(TargetFamily::Exceptions),
            _ => None,
        }
    }

    /// Every family (for `--family all`).
    pub fn all() -> [TargetFamily; 3] {
        [
            TargetFamily::Arithmetic,
            TargetFamily::StringConcat,
            TargetFamily::Exceptions,
        ]
    }
}

// ---------------------------------------------------------------------------
// Deterministic PRNG (xorshift64*) — reproducible corpora without a `rand` dep.
// ---------------------------------------------------------------------------

/// A small, fast, deterministic PRNG. Seed `0` is remapped (xorshift can't
/// leave the zero state).
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 0x9E3779B97F4A7C15 } else { seed },
        }
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// A value in `0..n` (`n` must be > 0).
    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// Pick a reference from a non-empty slice.
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

/// Generate `count` programs of `family`, seeded by `seed` (so the corpus is
/// reproducible). Every program compiles and is deterministic.
pub fn generate(family: TargetFamily, count: usize, seed: u64) -> Vec<GeneratedProgram> {
    let mut rng = Rng::new(seed);
    (0..count)
        .map(|i| {
            let name = format!("Gen_{}_{seed}_{i}", family.label());
            let body = match family {
                TargetFamily::Arithmetic => gen_arithmetic_body(&mut rng),
                TargetFamily::StringConcat => gen_concat_body(&mut rng),
                TargetFamily::Exceptions => gen_exceptions_body(&mut rng),
            };
            GeneratedProgram {
                source: wrap_program(&name, family, &body),
                name,
            }
        })
        .collect()
}

/// Wrap a `main` body into a compilable, header-pragma'd class.
fn wrap_program(name: &str, family: TargetFamily, body: &str) -> String {
    format!(
        "// difftest: strict (generated, family={})\n\
         public class {name} {{\n\
         {INDENT}public static void main(String[] args) {{\n\
         {body}\
         {INDENT}}}\n\
         }}\n",
        family.label(),
        INDENT = "    ",
    )
}

const INT_LITERALS: &[&str] = &[
    "Integer.MIN_VALUE",
    "Integer.MAX_VALUE",
    "-1",
    "0",
    "1",
    "2",
    "3",
    "7",
    "-7",
    "31",
    "32",
    "33",
    "65535",
];
const INT_OPS: &[&str] = &["+", "-", "*", "&", "|", "^"];
const SHIFT_OPS: &[&str] = &["<<", ">>", ">>>"];
const LONG_LITERALS: &[&str] = &[
    "Long.MIN_VALUE",
    "Long.MAX_VALUE",
    "-1L",
    "0L",
    "1L",
    "9000000000L",
    "-9000000000L",
];

/// Arithmetic family: seed two `int` and two `long` vars from the edge-literal
/// pool, then emit a sequence of operations (overflow, shift masking, guarded
/// division) printing each result. Always int/long ⇒ no FP nondeterminism.
fn gen_arithmetic_body(rng: &mut Rng) -> String {
    let mut s = String::new();
    let a = rng.pick(INT_LITERALS);
    let b = rng.pick(INT_LITERALS);
    let la = rng.pick(LONG_LITERALS);
    let lb = rng.pick(LONG_LITERALS);
    s.push_str(&format!("        int a = {a}, b = {b};\n"));
    s.push_str(&format!("        long la = {la}, lb = {lb};\n"));

    let int_terms = ["a", "b", "1", "-1", "2", "31", "Integer.MIN_VALUE"];
    let steps = 6 + rng.below(8);
    for i in 0..steps {
        let kind = rng.below(5);
        let expr = match kind {
            0 => {
                // binary int op
                let x = rng.pick(&int_terms);
                let y = rng.pick(&int_terms);
                let op = rng.pick(INT_OPS);
                format!("({x} {op} {y})")
            }
            1 => {
                // shift (distance masked; any int term is valid)
                let x = rng.pick(&int_terms);
                let y = rng.pick(&int_terms);
                let op = rng.pick(SHIFT_OPS);
                format!("({x} {op} {y})")
            }
            2 => {
                // guarded division/modulo (never divides by zero)
                let x = rng.pick(&int_terms);
                let y = rng.pick(&int_terms);
                let op = if rng.below(2) == 0 { "/" } else { "%" };
                format!("({x} {op} (({y}) == 0 ? 1 : ({y})))")
            }
            3 => {
                // long op (widened), printed as long
                let x = rng.pick(&["la", "lb", "(long)a", "1L"]);
                let y = rng.pick(&["la", "lb", "(long)b", "2L"]);
                let op = rng.pick(INT_OPS);
                format!("({x} {op} {y})")
            }
            _ => {
                // narrowing cast round-trip (exercises (int)/(short)/(byte)/
                // (char)). Print through `(int)` so a `(char)` result is its
                // numeric code, not an encoding-sensitive raw character —
                // keeping the arithmetic family fully numeric and a clean
                // parity-prover. (Raw-char stdout encoding is exercised, on
                // purpose, by the `concat` family instead.)
                let x = rng.pick(&["la", "lb", "(a * b)", "(a << 16)"]);
                let cast = rng.pick(&["(int)", "(short)", "(byte)", "(char)"]);
                format!("(int)({cast} {x})")
            }
        };
        s.push_str(&format!(
            "        System.out.println(\"r{i}=\" + {expr});\n"
        ));
    }
    s
}

/// String-concat (`invokedynamic`) family: mix primitive widths, `null`, boxed
/// values, chars, and FP specials through one or more concat expressions.
fn gen_concat_body(rng: &mut Rng) -> String {
    let mut s = String::new();
    s.push_str("        int i = 42; long l = 9000000000L; boolean bo = true; char c = 'Z';\n");
    s.push_str("        Object o = Integer.valueOf(7); String z = null;\n");
    let fp_terms = [
        "3.5",
        "0.0",
        "-0.0",
        "(1.0/3.0)",
        "(0.0/0.0)",
        "(1.0/0.0)",
        "(-1.0/0.0)",
        "100000.0",
    ];
    let f_terms = ["1.0f", "(1.0f/3.0f)", "0.0f", "Float.NaN"];
    let terms = [
        "i",
        "l",
        "bo",
        "c",
        "o",
        "z",
        "(i + 1)",
        "(l * 2)",
        "(char)(c + 1)",
    ];
    let lines = 3 + rng.below(4);
    for n in 0..lines {
        let parts = 3 + rng.below(4);
        let mut expr = String::from("\"\"");
        for _ in 0..parts {
            let pick = rng.below(10);
            let term = if pick < 6 {
                rng.pick(&terms).to_string()
            } else if pick < 8 {
                rng.pick(&fp_terms).to_string()
            } else {
                rng.pick(&f_terms).to_string()
            };
            expr.push_str(&format!(" + \"/\" + {term}"));
        }
        s.push_str(&format!(
            "        System.out.println(\"c{n}=\" + ({expr}));\n"
        ));
    }
    s
}

/// Exception-identity family: trigger a caught throwable and print its concrete
/// class + message (the JEP-358 / CCE / SIOOBE message-parity surface). Every
/// throw is caught and printed inline (`e<idx>=<class>:<message>`), so the
/// program exits 0 deterministically. The triggers use `args.length` (0 at
/// runtime) so `javac` can't constant-fold them into a compile error.
fn gen_exceptions_body(rng: &mut Rng) -> String {
    // `@PRINT@` expands to the inline catch-body print (keyed by statement idx).
    let snippets: &[&str] = &[
        "try { int x = args.length; int y = 5 / x; System.out.println(y); } \
         catch (ArithmeticException e) { @PRINT@ }",
        "try { Object o = \"s\"; Integer n = (Integer) o; System.out.println(n); } \
         catch (ClassCastException e) { @PRINT@ }",
        "try { int[] ar = new int[args.length]; System.out.println(ar[7]); } \
         catch (ArrayIndexOutOfBoundsException e) { @PRINT@ }",
        "try { String s = \"abc\"; System.out.println(s.charAt(9)); } \
         catch (StringIndexOutOfBoundsException e) { @PRINT@ }",
        "try { String s = args.length > 0 ? \"x\" : null; System.out.println(s.length()); } \
         catch (NullPointerException e) { @PRINT@ }",
        "try { int[] ar = args.length > 0 ? new int[1] : null; System.out.println(ar[0]); } \
         catch (NullPointerException e) { @PRINT@ }",
        "try { Object[] oa = new String[1]; oa[0] = Integer.valueOf(1); } \
         catch (ArrayStoreException e) { @PRINT@ }",
        "try { Integer.parseInt(\"xyz\"); } \
         catch (NumberFormatException e) { @PRINT@ }",
    ];
    let count = 3 + rng.below(4);
    let mut s = String::new();
    for idx in 0..count {
        let print = format!(
            "System.out.println(\"e{idx}=\" + e.getClass().getName() + \":\" + e.getMessage());"
        );
        let snip = rng.pick(snippets).replace("@PRINT@", &print);
        s.push_str(&format!("        {snip}\n"));
    }
    s.push_str("        System.out.println(\"done\");\n");
    s
}

#[cfg(test)]
mod tests {
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
}
