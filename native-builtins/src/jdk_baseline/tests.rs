use super::*;

/// `parse` takes `&'static str` because a baseline is an `include_str!`.
/// A planted mutant is built at runtime, so it is leaked; a test process
/// that leaks a few kilobytes and then exits is not a leak worth a design.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Run `parse` on a deliberately corrupted baseline and return the panic
/// message. Fails the test if `parse` *accepts* the corruption — that is
/// the planted bypass, and a parser that shrugs at it is the defect.
fn parse_must_reject(what: &str, text: &'static str) -> String {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(|| {
        let b = parse(text);
        b.rows.len()
    });
    std::panic::set_hook(hook);
    match outcome {
        Ok(n) => panic!(
            "parse() ACCEPTED a baseline corrupted by {what}, returning {n} rows. The \
             self-test that was supposed to catch it does not."
        ),
        Err(e) => e
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default(),
    }
}

/// Both directions: every `jdk25-*.tsv` on disk is included here, and every
/// include names a file that is still on disk.
///
/// The second half is the one that decays. A `STALE` include would fail to
/// compile, so what this really guards is the first: a baseline generated
/// into `scripts/baselines/` that nothing reads is a file that can never
/// go red, which is `generate.py --check`'s `STALE` classification seen
/// from the Rust side.
#[test]
fn every_checked_in_baseline_is_wired_in_and_parses() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("scripts")
        .join("baselines");
    let on_disk: BTreeSet<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("jdk25-") && n.ends_with(".tsv"))
        .collect();

    // Anti-vacuity: an empty or near-empty listing must not read as
    // agreement. Thirty were checked in on 2026-08-13 (E32/E37); E41 added
    // `sun.reflect.ReflectionFactory` and
    // `sun.management.ManagementFactoryHelper`, the second class rows 22
    // and 3 each register on.
    assert!(
        on_disk.len() >= 32,
        "found only {} jdk25-*.tsv in {} — this census is measuring its own reach, not \
         the baselines. `[reach≠defect]`",
        on_disk.len(),
        dir.display()
    );

    let included: BTreeSet<String> = ALL.iter().map(|(n, _)| (*n).to_string()).collect();
    assert_eq!(
        ALL.len(),
        included.len(),
        "ALL has a duplicate file name; one of the two consts is unread."
    );

    let missing: Vec<&String> = on_disk.difference(&included).collect();
    assert!(
        missing.is_empty(),
        "these baselines are checked in and no const includes them, so nothing can ever \
         read them and `--check` will keep regenerating them for nobody: {missing:?}. Add \
         an include_str! and an ALL row, or delete the file and its classes.txt entry."
    );
    let dead: Vec<&String> = included.difference(&on_disk).collect();
    assert!(
        dead.is_empty(),
        "ALL names files that are not on disk: {dead:?}"
    );

    // Parsing each one runs every header self-test in `parse` against every
    // checked-in file — the cheapest place to catch a hand edit.
    let mut total_rows = 0usize;
    for &(name, text) in ALL {
        let b = parse(text);
        assert!(!b.class.is_empty(), "{name}: parsed to an empty class name");
        total_rows += b.rows.len();
    }
    // F23-1: this floor was `> 1_000` against a corpus of 1380 rows, and the
    // widening took the corpus to 1908 — so the old floor is now cleared by
    // 900 rows and could not notice a third of the corpus vanishing. A floor
    // a widened baseline trivially clears is not a floor. Re-derived from the
    // measured total with the same ~30% headroom the original had, which is
    // enough to absorb a JDK point release and not enough to absorb the
    // version-1 filter coming back (that would take it to 1380 — RED).
    assert!(
        total_rows >= 1_800,
        "the 32 baselines parsed to only {total_rows} rows in total; 1908 were measured \
         on openjdk 25.0.3+9. A truncated include looks exactly like this, and so does \
         a generator that reverted to the format-version-1 public/protected filter \
         (1380). See `the_widened_population_is_present_and_is_not_a_rounding_error` \
         for the floor that names the population directly."
    );
}

/// **The anti-vacuity floor that a widened baseline does NOT trivially
/// clear**, and the measure of how much of the surface the version-1
/// instrument was not looking at.
///
/// `every_checked_in_baseline_is_wired_in_and_parses` counts rows, and a
/// row count is exactly the kind of number that drifts upward until it
/// stops meaning anything. This one counts the population version 1 could
/// not emit, so a version-1 corpus scores **10** against a floor of 500 —
/// red by a factor of fifty, no matter how many rows the files have.
///
/// Measured on openjdk 25.0.3+9. The v1 column is `git show HEAD:` over the
/// 30 tracked baselines (the other two were written the same day by the same
/// version-1 generator):
///
/// | | v1 | v2 |
/// |---|---|---|
/// | rows | 1380 | 1908 |
/// | member rows (METHOD+FIELD) | 779 | 1307 |
/// | non-public member rows | **10** | **538** |
/// | ...of which `protected` | 10 | 10 |
/// | **...of which private or package-private** | **0** | **528** |
/// | **native methods visible** | **3** | **13** |
///
/// Version 1 kept `ACC_PROTECTED` as well as `ACC_PUBLIC`, which is why the
/// v1 figure is 10 and not 0 — and why "non-public" is the wrong word for
/// what it hid. What it hid was every private and package-private member:
/// 528 rows, **27.7% of the surface**, and 10 of the 13 `native` methods.
/// That last number is the one that matters for a native registrar: the
/// version-1 baselines could see 3 of the JDK natives on the classes they
/// baseline.
#[test]
fn the_widened_population_is_present_and_is_not_a_rounding_error() {
    let mut member_rows = 0usize;
    let mut non_public = 0usize;
    let mut natives = 0usize;
    for &(_, text) in ALL {
        let b = parse(text);
        for r in &b.rows {
            if r.kind == "METHOD" || r.kind == "FIELD" {
                member_rows += 1;
                if !r.is_public() {
                    non_public += 1;
                }
            }
            if r.kind == "METHOD" && r.has_flag("native") {
                natives += 1;
            }
        }
    }
    assert!(
        member_rows >= 1_250,
        "only {member_rows} member rows across the 32 baselines; 1307 measured."
    );
    assert!(
        non_public >= 500,
        "only {non_public} of {member_rows} member rows are non-public. 538 were \
         measured, and a format-version-1 corpus scores 10 — every one of them \
         `protected`, because that filter kept ACC_PROTECTED and hid only the private \
         and package-private members. This assertion cannot be satisfied by a baseline \
         set that still carries that filter, however many rows it has. `[gate=FR]`"
    );
    assert!(
        natives >= 13,
        "only {natives} `native` methods visible across the 32 baselines; 13 measured, \
         of which 10 are non-public. A public-only corpus sees 3. Auditing a NATIVE \
         registrar against a surface that omits 10 of 13 natives is the defect F23-1 \
         fixed."
    );
}

/// The known-answer counts, carried across the language boundary.
///
/// `generate.py`'s KAT asserts these against `javap`, an instrument
/// different from the class-file reader under test. This asserts the same
/// two numbers again from Rust, through a *third* implementation — this
/// parser's own recount — so a header, a Python counter and a Rust counter
/// must all three agree. E32 §4 has the `javap` transcripts.
#[test]
fn the_two_hand_measured_counts_survive_the_rust_parser() {
    assert_eq!(
        parse(MAC).public_methods().len(),
        17,
        "javax.crypto.Mac: `javap -public javax.crypto.Mac | grep -c '('` = 17 on \
         openjdk 25.0.3+9 (E25 §1.1, E32 §4)."
    );
    assert_eq!(
        parse(CHARACTER).public_methods().len(),
        96,
        "java.lang.Character: `javap -public` prints 97 lines, one of which is the \
         public deprecated constructor, so 96 public methods — including the \
         `compareTo(Ljava/lang/Object;)I` bridge, which is a real method-table entry."
    );
    // The denominator E32-4's rewrite is built on, pinned here so it cannot
    // move silently: 17 public methods + 3 public constructors.
    assert_eq!(parse(SUBMISSION_PUBLISHER).public_surface().len(), 20);

    // F23-1: the same three counts taken with the OTHER instrument.
    // `javap -public` is what produced the version-1 defect, so a KAT built
    // only on it agrees with a generator that drops every private member —
    // and did, for 32 files. These are `javap -p <class> | grep -c '('` on
    // the same JDK, same day (`generate.py::KNOWN_ANSWERS_ALL_ACCESS`).
    assert_eq!(parse(MAC).declared_surface().len(), 22);
    assert_eq!(parse(CHARACTER).declared_surface().len(), 104);
    assert_eq!(
        parse(CDS).declared_surface().len(),
        28,
        "13 members is what `javap -public jdk.internal.misc.CDS` prints, and 13 is what \
         every guard on this class saw until F23-1."
    );
}

/// The JDK-version pin, and the E25 §5 step 4 residual it closes.
#[test]
fn every_baseline_is_pinned_to_this_jdk() {
    for &(name, text) in ALL {
        let b = parse(text);
        assert!(
            b.java_version.starts_with(JAVA_VERSION_PIN),
            "{name} was taken on {}",
            b.java_version
        );
    }
}

// --- planted bypasses: five corruptions the parser must refuse ----------

#[test]
fn a_deleted_row_is_caught_by_the_rows_header() {
    let mutant = leak(
        MAC.lines()
            .filter(|l| !l.starts_with("METHOD\treset\t"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let msg = parse_must_reject("deleting one METHOD row", mutant);
    assert!(msg.contains("`# rows` header"), "wrong diagnosis: {msg}");
}

#[test]
fn a_row_edited_in_place_is_caught_by_the_public_methods_header() {
    // Row count unchanged — only the flags move. This is the corruption
    // the `# rows` check cannot see, which is why there are two counts.
    let mutant = leak(MAC.replace(
        "METHOD\treset\t()V\tpublic,final",
        "METHOD\treset\t()V\tprotected,final",
    ));
    let msg = parse_must_reject("demoting a public method to protected", mutant);
    assert!(
        msg.contains("`# public-methods` header"),
        "wrong diagnosis: {msg}"
    );
}

#[test]
fn a_baseline_regenerated_on_a_later_jdk_is_refused() {
    let mutant = leak(MAC.replace("# java.version\t25.0.3", "# java.version\t26.0.1"));
    let msg = parse_must_reject("a JDK 26 regeneration", mutant);
    assert!(msg.contains("26.0.1"), "wrong diagnosis: {msg}");
}

#[test]
fn a_truncated_include_is_refused_rather_than_auditing_nothing() {
    let header_only = leak(
        MAC.lines()
            .take_while(|l| l.starts_with("# "))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    parse_must_reject("truncating the file to its header", header_only);
    parse_must_reject("an include_str! that resolved to nothing", "");
}

/// **The planted bypass for F23-1 itself: a version-1 file must not be read
/// as though it were a whole class.**
///
/// Two mutants, because they are two different accidents:
///
/// 1. the version marker says `1`. A checkout that predates this change has
///    32 of these, and every one of them answers `declares()` from a
///    public-and-protected-only set.
/// 2. the version marker says `2` and the *content* is version 1 — a
///    generator whose filter came back, with headers rewritten to match its
///    own truncated output. This is the one a version check alone cannot
///    catch, and it is why `# declared-methods` exists.
#[test]
fn a_version_one_baseline_is_refused_in_both_of_its_shapes() {
    let old_marker = leak(CDS.replace("# jdk-baseline\t2", "# jdk-baseline\t1"));
    let msg = parse_must_reject("a format-version-1 marker", old_marker);
    assert!(msg.contains("format version"), "wrong diagnosis: {msg}");

    // Mutant 2. Drop every non-public METHOD row — which is what the
    // version-1 generator did — and rewrite `# rows` and `# declared-methods`
    // the way that generator would have, so the only header left disagreeing
    // is the one this change added. `# public-methods` is untouched and
    // correct in BOTH files, which is the whole point: it cannot notice.
    let kept: Vec<&str> = CDS
        .lines()
        .filter(|l| {
            !l.starts_with("METHOD\t")
                || l.split('\t')
                    .nth(3)
                    .is_some_and(|f| f.split(',').any(|x| x == "public" || x == "protected"))
        })
        .collect();
    let dropped = CDS.lines().count() - kept.len();
    assert!(
        dropped >= 15,
        "the mutant must actually remove the private members it is standing in for; it \
         removed {dropped}"
    );
    let rows = kept
        .iter()
        .filter(|l| !l.starts_with("# ") && !l.is_empty())
        .count();
    let refiltered = leak(
        kept.iter()
            .map(|l| {
                if l.starts_with("# rows\t") {
                    format!("# rows\t{rows}")
                } else {
                    (*l).to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let msg = parse_must_reject(
        "a generator that reverted to the public-only filter",
        refiltered,
    );
    assert!(
        msg.contains("`# declared-methods` header"),
        "the `# rows` and `# public-methods` headers were rewritten to be self-consistent, \
         exactly as a reverted generator would write them, so `# declared-methods` is the \
         only check that can fire — and it must: {msg}"
    );
}

/// The mutation check the widening needs in the other direction: a member
/// that is **known present and known private** must be found by
/// `declares`/`declared_surface` and must NOT be found by `public_surface`.
///
/// Without this the widening could be a no-op — `declares` would keep
/// answering `false` for every private member and every guard would stay
/// green while staying wrong, which is the state the tree was in.
#[test]
fn a_known_private_member_is_visible_and_a_known_absent_one_is_not() {
    let b = parse(CDS);
    let public: BTreeSet<_> = b.public_surface().into_iter().collect();
    let declared: BTreeSet<_> = b.declared_surface().into_iter().collect();

    // Present, private, native. `javap -p jdk.internal.misc.CDS`:
    //   private static native int getCDSConfigStatus();
    //   private static native void logLambdaFormInvoker(java.lang.String);
    //   private static native boolean needsClassInitBarrier0(java.lang.Class<?>);
    //   private static native void dumpClassList(java.lang.String);
    //   private static native void dumpDynamicArchive(java.lang.String);
    for (n, d) in [
        ("getCDSConfigStatus", "()I"),
        ("logLambdaFormInvoker", "(Ljava/lang/String;)V"),
        ("needsClassInitBarrier0", "(Ljava/lang/Class;)Z"),
        ("dumpClassList", "(Ljava/lang/String;)V"),
        ("dumpDynamicArchive", "(Ljava/lang/String;)V"),
    ] {
        assert!(b.declares(n, d), "{n}{d} is a real member of JDK 25's CDS");
        assert!(
            declared.contains(&(n, d)),
            "{n}{d} must be on the declared surface"
        );
        assert!(
            !public.contains(&(n, d)),
            "{n}{d} is private; it must NOT be on the public surface. If it is, the two \
             populations have been collapsed into one and `audit`'s UNCOVERED denominator \
             just grew by 538 members."
        );
        assert_eq!(b.flags_of(n, d), Some("private,static,native"));
    }

    // Absent at every access level. These are the two `cds.rs` really did
    // register on names JDK 25 does not have, and F17-1 really did remove.
    for (n, d) in [("isDumpingClassList", "()Z"), ("isSharingEnabled", "()Z")] {
        assert!(
            !b.declares(n, d) && b.descriptors_named(n).is_empty(),
            "{n}{d} is not a CDS member at ANY access level on 25.0.3+9 — the widening \
             must not turn a real absence into a false present"
        );
    }

    // And the population that matters for a native registrar.
    assert_eq!(
        b.native_surface().len(),
        8,
        "CDS declares 8 natives; a public-only baseline showed 3 of them: {:?}",
        b.native_surface()
    );
}

#[test]
fn a_reordered_column_grammar_is_refused() {
    let mutant = leak(MAC.replace(
        "# columns\tkind\tname\tdescriptor\tflags",
        "# columns\tkind\tname\tflags\tdescriptor",
    ));
    let msg = parse_must_reject("swapping two columns without bumping the version", mutant);
    assert!(msg.contains("`# columns`"), "wrong diagnosis: {msg}");
}

// --- the ratchet: one test per failure kind ----------------------------

/// A registrar that has exactly `getAlgorithm` and `reset`, for the four
/// synthetic-input tests below.
fn two_of_macs_methods(name: &str, _descriptor: &str) -> bool {
    name == "getAlgorithm" || name == "reset"
}

#[test]
fn kind_one_uncovered_a_jdk_member_with_no_row() {
    let b = parse(MAC);
    let problems = audit(&b, &[("reset", "()V", true, "")], two_of_macs_methods);
    // 17 public methods + 1 protected ctor(excluded) → 17 in the surface;
    // one is covered, so sixteen are not.
    assert_eq!(
        problems
            .iter()
            .filter(|p| p.starts_with("UNCOVERED"))
            .count(),
        16,
        "{problems:#?}"
    );
}

#[test]
fn kind_two_dropped_a_registration_that_is_gone() {
    let b = parse(MAC);
    let problems = audit(&b, &[("doFinal", "()[B", true, "")], two_of_macs_methods);
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("DROPPED: javax.crypto.Mac.doFinal()[B")),
        "{problems:#?}"
    );
}

#[test]
fn kind_three_closed_a_gap_the_record_still_calls_open() {
    let b = parse(MAC);
    let problems = audit(
        &b,
        &[("reset", "()V", false, "believed unregistered")],
        two_of_macs_methods,
    );
    let hit = problems
        .iter()
        .find(|p| p.starts_with("CLOSED"))
        .unwrap_or_else(|| panic!("{problems:#?}"));
    assert!(
        hit.contains("standing permission"),
        "the kind-3 message must say what a stale `false` row IS: {hit}"
    );
}

#[test]
fn kind_four_stale_a_row_naming_a_member_this_jdk_does_not_declare() {
    let b = parse(MAC);
    let problems = audit(
        &b,
        &[("getInstanceStrong", "()Ljavax/crypto/Mac;", true, "")],
        two_of_macs_methods,
    );
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("STALE row: javax.crypto.Mac")),
        "{problems:#?}"
    );
}

#[test]
fn an_unexplained_false_row_and_a_noisy_true_row_are_both_refused() {
    let b = parse(MAC);
    let problems = audit(
        &b,
        &[
            ("doFinal", "()[B", false, "   "),
            ("reset", "()V", true, "some reason"),
        ],
        two_of_macs_methods,
    );
    assert!(
        problems.iter().any(|p| p.starts_with("UNJUSTIFIED")),
        "{problems:#?}"
    );
    assert!(
        problems.iter().any(|p| p.starts_with("NOISY")),
        "{problems:#?}"
    );
}

#[test]
fn a_duplicated_triage_row_is_refused() {
    let b = parse(MAC);
    let problems = audit(
        &b,
        &[("reset", "()V", true, ""), ("reset", "()V", false, "x")],
        two_of_macs_methods,
    );
    assert!(
        problems.iter().any(|p| p.starts_with("DUPLICATE")),
        "{problems:#?}"
    );
}

/// **The argument for this whole capability, re-enacted against real data.**
///
/// `jdk25_concurrency.rs:1524` defines
/// `CLS_CONFIG = "java/util/concurrent/StructuredTaskScope$Config"`;
/// `s52_class_name_config` (`:5103`) asserts that spelling and
/// `s52_total_registration_count` (`:5130-5135`) counts six methods on it.
/// **There is no such type in the JDK 25 runtime image** — the generator
/// refused it on its first run (E32 §8), and the real nested type is
/// `$Configuration`. A hand-maintained list cannot report that one of its
/// own names is not a JDK name.
///
/// This test takes those six triples verbatim out of the working tree,
/// pretends every one of them is registered (which it is), and audits them
/// against the `$Configuration` baseline. **All six come back STALE**, and
/// three of them come back with the sharper `STALE row (descriptor)`
/// diagnosis, because the fabricated name is baked into their return types
/// as well: the JDK's `withName` returns `…$Configuration`, not `…$Config`.
/// The three real members come back `UNCOVERED`.
///
/// Kind 4 catches this twice over. The blunt way is here. The blunter way
/// is that a guard converted to read a baseline for `$Config` **does not
/// compile**: `include_str!` cannot resolve a file the generator refuses to
/// write. Either way the name is checked against the image instead of
/// against the code that invented it.
#[test]
fn kind_four_would_have_caught_structured_task_scope_config() {
    let b = parse(STRUCTURED_TASK_SCOPE_CONFIGURATION);
    assert_eq!(
        b.internal_name(),
        "java/util/concurrent/StructuredTaskScope$Configuration",
        "the one-line pin every converted guard should carry: \
         assert_eq!(baseline.internal_name(), CLS_CONFIG). Against \
         jdk25_concurrency.rs's CLS_CONFIG (\"…$Config\") this line alone is red."
    );

    // Verbatim from jdk25_concurrency.rs:5130-5135, the six the tree
    // registers on the fabricated class.
    const AS_THE_TREE_HAS_THEM: &[Triage] = &[
        ("<init>", "()V", true, ""),
        (
            "withName",
            "(Ljava/lang/String;)Ljava/util/concurrent/StructuredTaskScope$Config;",
            true,
            "",
        ),
        (
            "withThreadFactory",
            "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/StructuredTaskScope$Config;",
            true,
            "",
        ),
        (
            "withTimeout",
            "(Ljava/time/Duration;)Ljava/util/concurrent/StructuredTaskScope$Config;",
            true,
            "",
        ),
        ("getName", "()Ljava/lang/String;", true, ""),
        (
            "getThreadFactory",
            "()Ljava/util/concurrent/ThreadFactory;",
            true,
            "",
        ),
    ];

    let problems = audit(&b, AS_THE_TREE_HAS_THEM, |_, _| true);
    let stale = problems.iter().filter(|p| p.starts_with("STALE")).count();
    assert_eq!(
        stale, 6,
        "all six registrations name members java.util.concurrent.StructuredTaskScope$\
         Configuration does not have: {problems:#?}"
    );
    assert_eq!(
        problems
            .iter()
            .filter(|p| p.starts_with("STALE row (descriptor)"))
            .count(),
        3,
        "withName/withThreadFactory/withTimeout exist under the right NAME and the \
         wrong descriptor — the fabricated `$Config` is in their return types too: \
         {problems:#?}"
    );
    assert_eq!(
        problems
            .iter()
            .filter(|p| p.starts_with("UNCOVERED"))
            .count(),
        3,
        "and the three real members are covered by nothing: {problems:#?}"
    );
}

// --- the fifth check: the registry's own rows, audited against the JDK ---

/// Kind 5, on synthetic input, with the planted bypass in both directions.
#[test]
fn off_surface_catches_a_registration_the_jdk_does_not_declare() {
    let b = parse(MAC);
    // `getInstanceStrong` is a `SecureRandom` method, not a `Mac` one.
    let problems = audit_off_surface(
        &b,
        &[
            ("reset", "()V"),
            ("getInstanceStrong", "()Ljavax/crypto/Mac;"),
        ],
        &[],
    );
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(
        problems[0]
            .contains("OFF-SURFACE: this VM registers javax.crypto.Mac.getInstanceStrong"),
        "{problems:#?}"
    );
    // Recorded with a reason, the same input is silent — and that is the
    // whole point of the reason field.
    assert!(audit_off_surface(
        &b,
        &[
            ("reset", "()V"),
            ("getInstanceStrong", "()Ljavax/crypto/Mac;")
        ],
        &[(
            "getInstanceStrong",
            "()Ljavax/crypto/Mac;",
            "deliberate, for this test"
        )],
    )
    .is_empty());
}

/// The sharper sub-case: right name, wrong descriptor. This is the one
/// that reads as a plain absence if you only ever grep for the name.
#[test]
fn off_surface_reports_a_wrong_descriptor_as_a_wrong_descriptor() {
    let b = parse(MAC);
    let problems = audit_off_surface(&b, &[("reset", "(I)V")], &[]);
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(
        problems[0]
            .contains("OFF-SURFACE (descriptor): this VM registers javax.crypto.Mac.reset(I)V"),
        "{problems:#?}"
    );
    assert!(problems[0].contains("()V"), "{problems:#?}");
}

/// Kinds 6 and 7 — the record outliving what it records. These are the two
/// that get omitted, and they are the reason this is a ratchet and not a
/// filter.
#[test]
fn an_off_surface_row_that_rotted_is_refused_in_both_directions() {
    let b = parse(MAC);
    // Kind 6: nothing registers it any more.
    let dead = audit_off_surface(&b, &[], &[("gone", "()V", "was registered once")]);
    assert!(
        dead.iter().any(|p| p.starts_with("DEAD OFF-SURFACE row")),
        "{dead:#?}"
    );
    // Kind 7: the JDK declares it publicly, so it belongs in TRIAGE.
    let stale = audit_off_surface(
        &b,
        &[("reset", "()V")],
        &[("reset", "()V", "believed not a JDK member")],
    );
    assert!(
        stale.iter().any(|p| p.starts_with("STALE OFF-SURFACE row")),
        "{stale:#?}"
    );
    // An unexplained exemption is an omission wearing a record's clothes.
    let mute = audit_off_surface(&b, &[("gone", "()V")], &[("gone", "()V", "  ")]);
    assert!(
        mute.iter()
            .any(|p| p.starts_with("UNJUSTIFIED OFF-SURFACE")),
        "{mute:#?}"
    );
}

// --- live re-enactments: real registrations, real baselines -------------
//
// Each of these takes the (class, name, descriptor) triples verbatim out
// of a registrar in this crate and audits them against the checked-in
// baseline for the class they are registered on. They are re-enactments,
// not guards on those files: they prove the mechanism against real data
// and they do NOT make the originating test red. Converting the guards is
// E25 rows 3, 4-6 and 16, delivered as nominations in
// docs/feature-designs/native-dispatch-architecture.md.

/// **Rows 4-6.** `shared_secrets_bridge.rs`'s `FACTORIES` is 15 rows and
/// `all_factories_listed` asserts `FACTORIES.len() == 15` — a const against
/// a literal copied out of that const. Two of those fifteen name a method
/// `jdk.internal.access.SharedSecrets` does not have on JDK 25:
///
/// * `getJavaSecurityAccess` — `JavaSecurityAccess` and its accessor went
///   with the Security Manager (JEP 486). There is no such getter; the
///   three `getJavaSecurity*Access` methods that DO exist are
///   `Properties`, `Signature` and `Spec`.
/// * `getJavaUtilJarAccess` — the real spelling has never had the `get`
///   prefix. `javaUtilJarAccess()` is the method; `getJavaUtilJarAccess`
///   is a name this tree invented.
///
/// Both are registered on `jdk/internal/access/SharedSecrets` **and** on
/// the legacy `jdk/internal/misc/SharedSecrets` alias, so four
/// registrations are keyed on names no real call can produce, and
/// `every_factory_returns_access_interface` cannot say so because its
/// whole test is `starts_with("getJava")` — which both of them pass.
#[test]
fn kind_four_catches_two_of_the_fifteen_shared_secrets_factories() {
    let b = parse(SHARED_SECRETS);
    assert_eq!(b.internal_name(), "jdk/internal/access/SharedSecrets");
    assert_eq!(
        b.public_surface().len(),
        65,
        "64 public methods + the public no-arg constructor. E25 rows 4-6 quote `15 of 30` \
         for the `getJava*Access` getters alone; the class a native registrar can key on \
         is 65 members wide."
    );

    // Verbatim from shared_secrets_bridge.rs:61 `FACTORIES`, as
    // `register_factories` keys them: `(method, "()" + ret_desc)`.
    #[rustfmt::skip]
    const AS_THE_TREE_HAS_THEM: &[Triage] = &[
        ("getJavaLangAccess", "()Ljdk/internal/access/JavaLangAccess;", true, ""),
        ("getJavaLangInvokeAccess", "()Ljdk/internal/access/JavaLangInvokeAccess;", true, ""),
        ("getJavaLangRefAccess", "()Ljdk/internal/access/JavaLangRefAccess;", true, ""),
        ("getJavaLangReflectAccess", "()Ljdk/internal/access/JavaLangReflectAccess;", true, ""),
        ("getJavaIOAccess", "()Ljdk/internal/access/JavaIOAccess;", true, ""),
        ("getJavaIORandomAccessFileAccess", "()Ljdk/internal/access/JavaIORandomAccessFileAccess;", true, ""),
        ("getJavaIOFileDescriptorAccess", "()Ljdk/internal/access/JavaIOFileDescriptorAccess;", true, ""),
        ("getJavaNetInetAddressAccess", "()Ljdk/internal/access/JavaNetInetAddressAccess;", true, ""),
        ("getJavaNetUriAccess", "()Ljdk/internal/access/JavaNetUriAccess;", true, ""),
        ("getJavaNioAccess", "()Ljdk/internal/access/JavaNioAccess;", true, ""),
        ("getJavaSecurityAccess", "()Ljdk/internal/access/JavaSecurityAccess;", true, ""),
        ("getJavaUtilJarAccess", "()Ljdk/internal/access/JavaUtilJarAccess;", true, ""),
        ("getJavaUtilZipFileAccess", "()Ljdk/internal/access/JavaUtilZipFileAccess;", true, ""),
        ("getJavaNetHttpCookieAccess", "()Ljdk/internal/access/JavaNetHttpCookieAccess;", true, ""),
        ("getJavaUtilResourceBundleAccess", "()Ljdk/internal/access/JavaUtilResourceBundleAccess;", true, ""),
    ];

    let problems = audit(&b, AS_THE_TREE_HAS_THEM, |_, _| true);
    let stale: Vec<&String> = problems.iter().filter(|p| p.starts_with("STALE")).collect();
    assert_eq!(
        stale.len(),
        2,
        "exactly two FACTORIES rows name a member JDK 25's SharedSecrets does not \
         declare: {problems:#?}"
    );
    assert!(
        stale.iter().any(|p| p.contains("getJavaSecurityAccess")),
        "{stale:#?}"
    );
    assert!(
        stale.iter().any(|p| p.contains("getJavaUtilJarAccess")),
        "{stale:#?}"
    );
    // And the real spelling is right there in the same baseline, uncovered.
    assert!(
        b.declares(
            "javaUtilJarAccess",
            "()Ljdk/internal/access/JavaUtilJarAccess;"
        ),
        "the JDK's own spelling carries no `get` prefix"
    );
    assert!(
        !b.declares(
            "getJavaSecurityAccess",
            "()Ljdk/internal/access/JavaSecurityAccess;"
        ),
        "JEP 486 took JavaSecurityAccess out; nothing declares this getter"
    );
}

/// **Row 3, and the retraction.** This test used to be called
/// `kind_four_catches_five_of_the_ten_cds_registrations` and asserted
/// `stale.len() == 5`. **Three of those five were artifacts of the
/// version-1 baseline**, and the report they justified came within one
/// commit of deleting three real, reachable natives.
///
/// The ten triples below are `cds.rs::register_cds_natives` as E41/F8 found
/// it. Audited against the format-version-2 baseline:
///
/// | registered | v1 verdict | v2 verdict — measured with `javap -p` |
/// |---|---|---|
/// | `isDumpingClassList()Z` | STALE | **STALE.** No such name at any access level |
/// | `isSharingEnabled()Z` | STALE | **STALE.** No such name; `isUsingArchive()Z` is the JDK-true spelling |
/// | `logLambdaFormInvoker(Ljava/lang/String;)V` | STALE (descriptor) | **REAL.** `private static native`, and the public 4-String overload's whole body is `logLambdaFormInvoker(prefix+" "+holder+…)` — CDS.java:142-146 |
/// | `dumpClassList(Ljava/lang/String;)V` | STALE | **REAL.** `private static native` |
/// | `dumpDynamicArchive(Ljava/lang/String;)V` | STALE | **REAL.** `private static native` |
///
/// The `logLambdaFormInvoker` row is the one that shows why the sub-case
/// ORDER in `audit` mattered: the JDK declares that name twice, once
/// privately with one parameter and once publicly with four, so the
/// version-1 code path found the public overload and produced *"the name is
/// right and the descriptor is not"* — a specific, confident, wrong
/// instruction to change the registration to a body that drops three
/// arguments and to drop the only form with no bytecode fallback.
///
/// Two of five survive. `[triage=stale]`.
#[test]
fn kind_four_catches_two_of_the_ten_cds_registrations_not_five() {
    let b = parse(CDS);
    assert_eq!(b.internal_name(), "jdk/internal/misc/CDS");

    // Verbatim from cds.rs::register_cds_natives as of E41/F8, BEFORE
    // F17-1 edited it. Kept in that shape on purpose: this test is the
    // re-enactment of the wrong verdict, not a guard on today's file.
    #[rustfmt::skip]
    const AS_F8_FOUND_THEM: &[Triage] = &[
        ("<init>", "()V", true, ""),
        ("isDumpingClassList", "()Z", true, ""),
        ("isDumpingArchive", "()Z", true, ""),
        ("isSharingEnabled", "()Z", true, ""),
        ("initializeFromArchive", "(Ljava/lang/Class;)V", true, ""),
        ("getRandomSeedForDumping", "()J", true, ""),
        ("logLambdaFormInvoker", "(Ljava/lang/String;)V", true, ""),
        ("defineArchivedModules", "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V", true, ""),
        ("dumpClassList", "(Ljava/lang/String;)V", true, ""),
        ("dumpDynamicArchive", "(Ljava/lang/String;)V", true, ""),
    ];

    let problems = audit(&b, AS_F8_FOUND_THEM, |_, _| true);
    let stale: Vec<&String> = problems.iter().filter(|p| p.starts_with("STALE")).collect();
    assert_eq!(
        stale.len(),
        2,
        "two of the ten name something JDK 25 does not have, not five: {problems:#?}"
    );
    assert!(stale.iter().any(|p| p.contains("isDumpingClassList")));
    assert!(stale.iter().any(|p| p.contains("isSharingEnabled")));
    assert_eq!(
        stale
            .iter()
            .filter(|p| p.starts_with("STALE row (descriptor)"))
            .count(),
        0,
        "the single `STALE row (descriptor)` the version-1 run produced was \
         `logLambdaFormInvoker`, and it was wrong: {stale:#?}"
    );

    // The three retractions, stated as data rather than as prose.
    for (n, d) in [
        ("logLambdaFormInvoker", "(Ljava/lang/String;)V"),
        ("dumpClassList", "(Ljava/lang/String;)V"),
        ("dumpDynamicArchive", "(Ljava/lang/String;)V"),
    ] {
        assert_eq!(
            b.flags_of(n, d),
            Some("private,static,native"),
            "{n}{d} was reported off-surface and is a real native"
        );
    }
    assert!(
        b.declares("isDumpingStaticArchive", "()Z") && b.declares("isUsingArchive", "()Z"),
        "the two methods the tree's `isDumpingClassList`/`isSharingEnabled` were probably \
         meant to be are both right here in the same baseline"
    );
    // Kind 1 is unchanged by the widening, and that is the design: 13 public
    // members, 5 of them covered by a row above, 8 uncovered. Widening this
    // denominator to the declared surface would have made it 23.
    assert_eq!(
        problems
            .iter()
            .filter(|p| p.starts_with("UNCOVERED"))
            .count(),
        8
    );
}

/// The other half of the retraction: `cds.rs` **as F17-1 left it** is
/// entirely on JDK 25's declared surface — eleven registrations, zero
/// off-surface — and the version-1 oracle would have called five of them
/// fabrications.
///
/// This is the test that would have prevented the near-miss, and it is
/// mutation-checked in both directions below: a real private member must
/// pass, and a name the JDK does not have must not.
#[test]
fn f23_1_every_cds_registration_is_on_the_jdk25_declared_surface() {
    let b = parse(CDS);

    // Verbatim from cds.rs::register_cds_natives in the working tree.
    #[rustfmt::skip]
    const AS_THE_TREE_HAS_THEM: &[(&str, &str)] = &[
        ("<init>", "()V"),
        ("isDumpingArchive", "()Z"),
        ("isUsingArchive", "()Z"),
        ("getCDSConfigStatus", "()I"),
        ("needsClassInitBarrier0", "(Ljava/lang/Class;)Z"),
        ("initializeFromArchive", "(Ljava/lang/Class;)V"),
        ("getRandomSeedForDumping", "()J"),
        ("logLambdaFormInvoker", "(Ljava/lang/String;)V"),
        ("defineArchivedModules", "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V"),
        ("dumpClassList", "(Ljava/lang/String;)V"),
        ("dumpDynamicArchive", "(Ljava/lang/String;)V"),
    ];

    assert!(
        audit_off_surface(&b, AS_THE_TREE_HAS_THEM, &[]).is_empty(),
        "{:#?}",
        audit_off_surface(&b, AS_THE_TREE_HAS_THEM, &[])
    );

    // Mutation, direction 1 — plant a name JDK 25 does not have. If this
    // does not fire, the widening turned the check into a rubber stamp.
    let planted = {
        let mut v = AS_THE_TREE_HAS_THEM.to_vec();
        v.push(("isDumpingClassList", "()Z"));
        v
    };
    let problems = audit_off_surface(&b, &planted, &[]);
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(
        problems[0].contains("isDumpingClassList") && problems[0].contains("ANY ACCESS LEVEL"),
        "{problems:#?}"
    );

    // Mutation, direction 2 — five of the eleven are non-public, so under
    // the version-1 rule (public surface as the reachability test) this same
    // input produced five OFF-SURFACE reports. Recomputed here from the
    // public surface directly, because that is the number the deleted-
    // registration proposal was built on.
    let public: BTreeSet<_> = b.public_surface().into_iter().collect();
    let would_have_been_reported: Vec<_> = AS_THE_TREE_HAS_THEM
        .iter()
        .filter(|k| !public.contains(&(k.0, k.1)))
        .collect();
    assert_eq!(
        would_have_been_reported.len(),
        5,
        "the version-1 rule reports five real natives as fabrications: {would_have_been_reported:#?}"
    );
}

/// Kind 7b — an off-surface exemption written while the oracle was blind.
///
/// Every such row in the tree is now a lie in the safe direction ("we
/// knowingly register something the JDK does not have", about something the
/// JDK has), and the repair is the opposite of what the message for kind 7a
/// asks for: delete the ROW, keep the registration.
#[test]
fn kind_seven_b_an_exemption_written_against_a_blind_oracle_is_refused() {
    let b = parse(CDS);
    let problems = audit_off_surface(
        &b,
        &[("logLambdaFormInvoker", "(Ljava/lang/String;)V")],
        &[(
            "logLambdaFormInvoker",
            "(Ljava/lang/String;)V",
            "the JDK declares a four-String overload under this name",
        )],
    );
    let hit = problems
        .iter()
        .find(|p| p.starts_with("STALE OFF-SURFACE row (declared, not public)"))
        .unwrap_or_else(|| panic!("{problems:#?}"));
    assert!(
        hit.contains("private,static,native") && hit.contains("DELETE THE ROW"),
        "the message must quote the access level and say which of the two things to \
         delete: {hit}"
    );
}

/// **Row 3, the other class.** `register_cds_natives` also registers six
/// natives on `sun/management/CDSMetrics` and a seventh,
/// `ManagementFactoryHelper.getCDSMetrics()Lsun/management/CDSMetrics;`,
/// that returns one.
///
/// `sun.management.CDSMetrics` **is not in the JDK 25 runtime image** — a
/// `jrt:/` walk finds no such class file, which is the same refusal the
/// generator gave for `StructuredTaskScope$Config`. So there is no
/// baseline to convert that half of row 3 against, and there never will
/// be; the missing file is the finding.
///
/// What CAN be checked is the owner, and it says the same thing from the
/// other side: `sun.management.ManagementFactoryHelper` is in the image,
/// declares 22 public methods, and `getCDSMetrics` is not among them. A
/// native registered under a name its own owner class does not declare is
/// a native no bytecode can reach.
#[test]
fn management_factory_helper_does_not_declare_get_cds_metrics() {
    let b = parse(SUN_MANAGEMENT_FACTORY_HELPER);
    assert_eq!(b.internal_name(), "sun/management/ManagementFactoryHelper");
    assert_eq!(b.public_methods().len(), 22);
    let problems = audit_off_surface(
        &b,
        &[("getCDSMetrics", "()Lsun/management/CDSMetrics;")],
        &[],
    );
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(
        problems[0].contains("registers sun.management.ManagementFactoryHelper.getCDSMetrics"),
        "{problems:#?}"
    );
    assert!(
        !b.declares("getCDSMetrics", "()Lsun/management/CDSMetrics;"),
        "not at any access level either — this is not a visibility question"
    );
}

/// **Row 16, the half that is not `$Config`.**
/// `kind_four_would_have_caught_structured_task_scope_config` covers the
/// fabricated nested type. This covers the outer class, which is real, and
/// which the tree also gets wrong in four places at once — three names JDK
/// 25 does not declare and one that is declared with a different return
/// type:
///
/// * `isShutdown()Z` — JEP 505 shipped `isCancelled()Z`.
/// * `shutdown()V` — gone; cancellation is the joiner's business now.
/// * `joinUntil(Ljava/time/Instant;)…` — gone; a deadline is a
///   `Configuration.withTimeout`.
/// * `join()` — declared, but it returns `Ljava/lang/Object;`, not
///   `Ljava/util/concurrent/StructuredTaskScope;`. The preview API returned
///   the scope for chaining; the final one returns the joiner's result.
///
/// `s52_total_registration_count` asserts none of these: its fourteen
/// triples are seven `$Joiner`, six `$Config` and exactly one on the outer
/// class (`open(Joiner)`), so every row above is outside its population.
#[test]
fn kind_four_catches_the_outer_structured_task_scope_surface() {
    let b = parse(STRUCTURED_TASK_SCOPE);
    assert_eq!(
        b.internal_name(),
        "java/util/concurrent/StructuredTaskScope"
    );

    // Verbatim from jdk25_concurrency.rs's registrations on CLS_TASK_SCOPE.
    #[rustfmt::skip]
    const AS_THE_TREE_HAS_THEM: &[Triage] = &[
        ("close", "()V", true, ""),
        ("fork", "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;", true, ""),
        ("isShutdown", "()Z", true, ""),
        ("join", "()Ljava/util/concurrent/StructuredTaskScope;", true, ""),
        ("joinUntil", "(Ljava/time/Instant;)Ljava/util/concurrent/StructuredTaskScope;", true, ""),
        ("open", "()Ljava/util/concurrent/StructuredTaskScope;", true, ""),
        ("open", "(Ljava/util/concurrent/StructuredTaskScope$Joiner;)Ljava/util/concurrent/StructuredTaskScope;", true, ""),
        ("shutdown", "()V", true, ""),
    ];

    let problems = audit(&b, AS_THE_TREE_HAS_THEM, |_, _| true);
    let stale: Vec<&String> = problems.iter().filter(|p| p.starts_with("STALE")).collect();
    assert_eq!(stale.len(), 4, "{problems:#?}");
    assert!(
        stale
            .iter()
            .any(|p| p.starts_with("STALE row (descriptor)") && p.contains(".join()")),
        "`join` exists under a different return type, which is the diagnosis that says \
         `fix the descriptor` rather than `delete the row`: {stale:#?}"
    );
    // The three the JDK declares and this VM registers nothing for.
    assert!(b.declares("isCancelled", "()Z"));
    assert!(b.declares("join", "()Ljava/lang/Object;"));
    assert!(b.declares(
        "fork",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;"
    ));
}

/// The two `ReflectionFactory` types are not one surface, and row 22's
/// guard names triples on both.
#[test]
fn the_two_reflection_factories_are_different_classes() {
    let internal = parse(REFLECTION_FACTORY);
    let unsupported = parse(SUN_REFLECTION_FACTORY);
    assert_eq!(internal.public_surface().len(), 25);
    assert_eq!(unsupported.public_surface().len(), 14);
    // Same name, different descriptor, on the two types — a guard that
    // reads one baseline for both would report a false STALE.
    assert!(internal.declares(
        "newOptionalDataExceptionForSerialization",
        "()Ljava/lang/reflect/Constructor;"
    ));
    assert!(unsupported.declares(
        "newOptionalDataExceptionForSerialization",
        "(Z)Ljava/io/OptionalDataException;"
    ));
    // `getConstantPool` is registered on the jdk.internal type by
    // `register_essential_natives_with_shims` (lib.rs) and is not a member
    // of either. It is `SharedSecrets.getJavaLangAccess().getConstantPool`.
    for b in [&internal, &unsupported] {
        assert!(
            !b.declares(
                "getConstantPool",
                "(Ljava/lang/Class;)Ljdk/internal/reflect/ConstantPool;"
            ),
            "{} declares getConstantPool",
            b.class
        );
    }
}

/// The module accessors, and the internal/binary spelling trap E32 §5
/// warns about.
#[test]
fn the_module_baseline_carries_the_real_export_denominator() {
    let b = parse(MODULE_JAVA_BASE);
    assert_eq!(b.kind, "module");
    assert_eq!(
        b.unqualified_exports().len(),
        58,
        "`java --describe-module java.base` reports 58 unqualified exports on \
         openjdk 25.0.3+9. `jdk25_language.rs:834` \
         test_java_base_exports_has_14_entries asserts 14 as fact (E25 row 13)."
    );
    assert!(b.unqualified_exports().contains(&"java.util.stream"));
    assert!(b
        .unqualified_exports_internal()
        .contains(&"java/util/stream".to_string()));
}

/// `audit` must refuse a module baseline rather than compute an empty
/// public surface and report every triage row as stale.
#[test]
fn audit_refuses_a_module_baseline() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(|| {
        let b = parse(MODULE_JAVA_BASE);
        audit(&b, &[], |_, _| false).len()
    });
    std::panic::set_hook(hook);
    assert!(
        outcome.is_err(),
        "audit() accepted a module baseline; its public surface is empty, so every row \
         would read as STALE and an empty triage would read as a pass."
    );
}
