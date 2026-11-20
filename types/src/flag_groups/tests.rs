use super::*;

/// `resolve` borrows its source, so every test needs the source to outlive
/// the resolution. This owns both.
struct Case {
    src: MapSource,
}

impl Case {
    fn new(pairs: &[(&str, &str)]) -> Self {
        Self {
            src: MapSource::new(pairs.iter().copied()),
        }
    }
    fn resolve(&self) -> Resolved<'_> {
        resolve(&self.src)
    }
}

fn case(pairs: &[(&str, &str)]) -> Case {
    Case::new(pairs)
}

/// Round 18 wave 11 (lane osrfollow,
/// `r18-unroll-minus-token-leaves-the-ir-unroller-on-FIXED-20261006.md`):
/// `CRATONVM_JIT=-unroll` turns off BOTH unrollers -- the single-pass
/// paths' opt-out key and the optimizing tier's default-ON key (written
/// `0`, not unset) -- and `unroll` turns both back on.


/// The two names declared on 2026-08-11 reach their consumers through the
/// GROUPED spelling, which is the whole point of declaring them.
///
/// Both were read by code and named nowhere, so each was served by a live
/// `getenv` rather than the latched snapshot: `CRATONVM_DBG=overlay-gate`
/// and `CRATONVM_REAL=-memoryusage-tostring` reached neither, and
/// `flags::with_thread_overrides` could not arrange either in a test. A row
/// in `INVENTORY` is what fixes that, so assert the expansion rather than
/// the row's existence — `every_token_is_unique` and the surface guards
/// already cover the row.
///
/// `memoryusage-tostring` is an opt-OUT: the real JDK bytecode is the
/// default, so the token has no `on_key` and turning it OFF is what sets
/// the `CRATONVM_SYNTHETIC_*` key. Same shape as `mxbean-mapping` and
/// `aqs` above.

/// Same for the two GC diagnostics declared on 2026-08-17.
///
/// `CRATONVM_DBG_MAPGEN` (`vm/src/threading/gc_barrier.rs`) and
/// `CRATONVM_DBG_VACATED_FRAMES` (`gc/src/gc_quiescence.rs`) shipped as
/// `runtime_var_os` reads with no row, so each was served by a live
/// `getenv`. Both are plain opt-in diagnostics, so the assertion is just
/// that the grouped spelling reaches the legacy key — which is the fact a
/// consumer depends on and the row's existence alone does not establish.
///
/// The wider cost of leaving them undeclared was not the two flags: while
/// `flag_declaration_guard` was red it could not catch the NEXT one.

#[test]
fn every_token_is_unique() {
    // Uniqueness is on the PAIR. The same token in two different groups is
    // deliberate and has its own test
    // (`a_token_shared_between_groups_stays_two_keys`), so a bare token
    // count would condemn 13 legitimate rows.
    let mut seen: Vec<(Group, &str)> = INVENTORY.iter().map(|e| (e.group, e.token)).collect();
    seen.sort_unstable();
    let dups: Vec<String> = seen
        .windows(2)
        .filter(|w| w[0] == w[1])
        .map(|w| format!("{:?}/{}", w[0].0, w[0].1))
        .collect();
    assert!(
        dups.is_empty(),
        "these (group, token) pairs are claimed twice: {}. Merge each into \
         ONE entry carrying both an on_key and an off_key, or delete the \
         duplicate — two sessions declaring the same knob independently is \
         how this happens, and the row is usually verbatim-identical.",
        dups.join(", ")
    );
}

#[test]
fn every_legacy_key_is_claimed_once() {
    let mut keys: Vec<&str> = INVENTORY
        .iter()
        .flat_map(|e| [e.on_key, e.off_key].into_iter().flatten())
        .collect();
    keys.sort_unstable();
    let dups: Vec<&str> = keys
        .windows(2)
        .filter(|w| w[0] == w[1])
        .map(|w| w[0])
        .collect();
    assert!(
        dups.is_empty(),
        "these legacy variables are claimed by more than one token, so \
         their canonical spelling is ambiguous: {dups:?}"
    );
    for s in SCALARS {
        assert!(
            !keys.contains(s),
            "{s} is both a scalar and a grouped token"
        );
    }
    // A legacy key may not be spelled the same as a group variable: the
    // resolver would then both consume it as a token list and expand a
    // token into it. `CRATONVM_DBG` and `CRATONVM_REAL` were exactly this,
    // and both are handled at their entries above.
    for g in Group::ALL {
        assert!(
            !keys.contains(&g.var()),
            "{} is a group variable and also a legacy key",
            g.var()
        );
    }
}

#[test]
fn every_entry_can_be_switched_both_ways() {
    for e in INVENTORY {
        assert!(
            e.on_key.is_some() || e.off_key.is_some(),
            "{}/{} expands to nothing",
            e.group.var(),
            e.token
        );
        if e.off_word.is_some() {
            // Round 18 wave 11 (lane osrfollow): with an `off_key` too,
            // `apply` writes `on_key = off_word` beside setting `off_key`
            // (the `unroll` row: two readers, one default-ON).
            assert!(
                e.on_key.is_some(),
                "off_word is the value written to on_key, so it needs an on_key"
            );
        }
    }
}

#[test]
fn token_names_are_canonical() {
    for e in INVENTORY {
        assert!(
            !e.token.is_empty()
                && e.token
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "token {:?} is not lower-kebab-case",
            e.token
        );
        assert!(
            !e.token.starts_with("no-") && !e.token.starts_with("disable-"),
            "token {:?} states a negative; invert it and put the legacy \
             name in off_key, so switching it off is spelled with a leading -",
            e.token
        );
    }
}






/// **EVERY default-ON knob's off token must actually write the off word.**
///
/// `default_on_knob_is_switched_off_by_value_not_by_unsetting` above pins
/// this for one row, which is one row's worth of protection: a NEW row
/// written in the wrong shape is not covered by it. Three were --
/// `xt-helper-window-discharge`, `xt-pinned-peer-depth` and
/// `xt-peer-shadow-scan` all named their own variable in `off_key` as well
/// as `on_key`, so `apply` took the "dedicated opt-out spelling" branch,
/// wrote `VAR=1`, then cleared `on_key` -- the same key -- and the knob came
/// out UNSET. Their parsers read unset as ON, so `CRATONVM_JIT=-<token>`
/// silently left three default-ON GC-root features running. Kill switches
/// that do not kill, which is worse than none: an arm that reads as
/// "the feature is not the cause" while the feature is still on.
///
/// The structural gates caught the shape, and their message -- "off_word
/// only applies to a default-ON knob with no opt-out key" -- describes the
/// table, not the damage. This asserts the CONSEQUENCE, over every row that
/// has an `off_word` now or later, so the next one is reported as what it
/// costs rather than as a schema violation.
#[test]
fn every_default_on_knob_resolves_its_off_token_to_the_off_word() {
    for e in INVENTORY {
        let Some(word) = e.off_word else { continue };
        let key = e
            .on_key
            .expect("checked by every_entry_can_be_switched_both_ways");
        let c = case(&[(e.group.var(), &format!("-{}", e.token))]);
        assert_eq!(
            c.resolve().get(key),
            Some(OsString::from(word)),
            "{}=-{} must write {key}={word}. It resolved to {:?} instead, \
             which this knob's parser reads as ON -- the off token is inert.",
            e.group.var(),
            e.token,
            c.resolve().get(key),
        );
    }
}

#[test]
fn following_switches_minus_tokens_override_their_enabled_parents() {
    for &(child, parent) in FOLLOWS {
        let e = INVENTORY.iter().find(|e| e.on_key == Some(child)).unwrap();
        for inherited in ["0", "1"] {
            let spec = format!("-{}", e.token);
            let c = case(&[
                (e.group.var(), &spec),
                (parent, "1"),
                (child, inherited),
            ]);
            assert_eq!(c.resolve().get(child), Some(OsString::from("0")), "{child}");
            assert_eq!(c.resolve().get(parent), Some(OsString::from("1")), "{parent}");
        }
    }
}

#[test]
fn thread_local_minus_token_disables_the_compatible_default() {
    let c = case(&[("CRATONVM_THREADS", "-tl-intrinsic")]);
    assert_eq!(
        c.resolve().get("CRATONVM_TL_INTRINSIC"),
        Some(OsString::from("0"))
    );
}

/// Each of these knobs is default-ON: its reader treats an UNSET key as on and
/// `0` as off. Without an `off_word` the minus token unset the key and the
/// feature stayed on (2026-09-12), so each must write `0`.

/// The three rows that were wrong, by name, because a table-driven test
/// keeps passing if a row is deleted. Each is a default-ON cross-thread
/// root-scan feature whose kill switch is the first thing an investigation
/// reaches for.


#[test]
fn all_enables_every_token_in_the_group() {
    let c = case(&[("CRATONVM_DBG", "all")]);
    let r = c.resolve();
    let mut valued_seen = 0;
    for e in entries(Group::DBG) {
        if let Some(k) = e.on_key {
            // A valued key (a path) has no `1` to take: `all` leaves it
            // unset (JIT r23 w3 L6, `VALUED_KEYS`).
            if VALUED_KEYS.contains(&k) {
                valued_seen += 1;
                assert_eq!(r.get(k), None, "{k} is valued: `all` must not set it to 1");
                continue;
            }
            assert_eq!(
                r.get(k),
                Some(OsString::from("1")),
                "{k} not enabled by all"
            );
        }
    }
    // The exemption is exercised (`CRATONVM_EXIT_REPORT_FILE` is a DBG key),
    // and covers nothing else.
    assert!(valued_seen >= 1);
    assert_eq!(r.get("CRATONVM_JIT_PROFILE_SAVE"), None);
}


/// The exact mistake that produced a confident, wrong "hypothesis
/// falsified" (see the `math-floormod-long-int-returns-minus-one` doc):
/// `no-X` looks like English but is not a spelling this parser knows, so
/// the knob silently never applies.


#[test]
fn suggest_declines_when_the_match_is_ambiguous() {
    // A wrong hint invites a second inert run, so an ambiguous token gets
    // no guess. "s" is a substring of many tokens.
    assert_eq!(suggest(Group::JIT, "s"), None);
    // And a token resembling nothing at all gets none either.
    assert_eq!(suggest(Group::JIT, "zzzzzzzz-not-close-to-anything"), None);
}


/// JIT r23 w3 L6: a valued key set directly is supported (no legacy
/// advice), `token=<value>` sets it, and the bare token (value `1`) and
/// `all` do not.


#[test]
fn unclaimed_names_fall_through() {
    let c = case(&[("CRATONVM_SOMETHING_BRAND_NEW", "7")]);
    assert_eq!(
        c.resolve().get("CRATONVM_SOMETHING_BRAND_NEW"),
        Some(OsString::from("7"))
    );
}

// ── Supersession (Real-JDK mode, contract §9) ──────────────────────────

/// The load-bearing constraint of the whole supersession change: the token
/// keeps doing **exactly** what it did. `--real-jdk` is a superset, but the
/// people already exporting `CRATONVM_REAL=-stubs` in runbooks get no
/// behaviour change — only a note. If this row ever expanded to a second
/// key, every one of those runbooks would quietly start doing something new.
#[test]
fn stubs_still_expands_to_exactly_one_key() {
    let c = case(&[("CRATONVM_REAL", "-stubs")]);
    let r = c.resolve();
    let one = OsString::from("1");
    let applied: Vec<(&str, Option<&OsString>)> = r.overrides().collect();
    assert_eq!(
        applied,
        vec![("CRATONVM_NO_STUBS", Some(&one))],
        "-stubs must set CRATONVM_NO_STUBS=1 and nothing else"
    );
}

/// This feature is a *command-line* policy. If it ever grows an environment
/// variable, the fifteen-name promise is broken and
/// `types/tests/flag-surface.txt` needs a matching edit — neither of which
/// should happen by accident, so fail here first with a message that says
/// what to do about it.
#[test]
fn jdk_only_adds_no_environment_variable() {
    let names = INVENTORY
        .iter()
        .flat_map(|e| [e.on_key, e.off_key].into_iter().flatten())
        .chain(SCALARS.iter().copied())
        .chain(Group::ALL.iter().map(|g| g.var()));
    for name in names {
        assert!(
            !name.contains("JDK_ONLY"),
            "{name} looks like a Real-JDK environment variable. The feature \
             is a runtime policy chosen per invocation (contract §9): add a \
             CliFlag to JDK_ONLY_CLI_FLAGS instead. If a variable really is \
             wanted, types/tests/flag-surface.txt has to gain it in the \
             same commit."
        );
    }
    // Supersession is a note about an existing knob, never a new one.
    for s in SUPERSEDED {
        assert!(
            lookup(s.group, s.token).is_some(),
            "{} supersedes a token that is not in the inventory",
            s.spelling()
        );
    }
    // And the CLI table is flags, not variables.
    for f in JDK_ONLY_CLI_FLAGS {
        assert!(f.flag.starts_with("--"), "{} is not a flag", f.flag);
        assert!(!f.flag.contains("CRATONVM"), "{} is not a flag", f.flag);
    }
}

#[test]
fn superseded_lookup_is_polarity_sensitive() {
    let row = superseded_by(Group::REAL, "stubs", false)
        .expect("CRATONVM_REAL=-stubs is the one superseded knob");
    // The preferred spelling stopped being a flag on 2026-09-20: the
    // policy the token approximates is the launcher default, so the advice
    // is to stop opting out rather than to opt in.
    assert_eq!(
        row.prefer,
        "the default jdk-only policy (i.e. dropping --compatible)"
    );
    assert_eq!(row.spelling(), "CRATONVM_REAL=-stubs");
    assert!(row.note().contains("CRATONVM_REAL=-stubs"));
    assert!(row.note().contains("jdk-only"));
    assert!(row.note().contains("--compatible"));
    assert!(row.note().contains(row.because));

    // `stubs` (positive) asks for the default behaviour and is not
    // superseded by anything; neither is a token in another group.
    assert!(superseded_by(Group::REAL, "stubs", true).is_none());
    assert!(superseded_by(Group::JIT, "stubs", false).is_none());
    assert!(superseded_by(Group::REAL, "aqs", false).is_none());
}

#[test]
fn resolution_reports_the_superseded_spelling_both_ways() {
    // Grouped spelling.
    let c = case(&[("CRATONVM_REAL", "aqs,-stubs")]);
    let r = c.resolve();
    assert_eq!(r.superseded.len(), 1);
    assert_eq!(
        r.superseded[0].prefer,
        "the default jdk-only policy (i.e. dropping --compatible)"
    );
    // ...and the expansion is untouched by the note.
    assert_eq!(r.get("CRATONVM_NO_STUBS"), Some(OsString::from("1")));

    // Legacy key exported directly — the runbook case the note exists for.
    let c = case(&[("CRATONVM_NO_STUBS", "1")]);
    assert_eq!(c.resolve().superseded.len(), 1);

    // Tokenizer parity with `resolve`: spacing, `+`, `=value` and case.
    for spec in ["-stubs", " -stubs ", "-STUBS=1", "a5-mark-span,-stubs"] {
        assert!(
            spec_names(spec, "stubs", false),
            "{spec:?} names -stubs but was not recognised"
        );
    }
    assert!(!spec_names("stubs", "stubs", false));
    assert!(!spec_names("+stubs", "stubs", false));
    assert!(!spec_names("-stubsx", "stubs", false));

    // Nothing set: no note. Nagging on a clean environment is how a
    // deprecation line gets filtered out before the one that matters.
    assert!(case(&[]).resolve().superseded.is_empty());
    assert!(case(&[("CRATONVM_REAL", "aqs")])
        .resolve()
        .superseded
        .is_empty());
}

// ── The §9 command-line surface ────────────────────────────────────────

#[test]
fn the_jdk_only_cli_surface_is_the_eight_flags_of_the_contract() {
    let flags: Vec<&str> = JDK_ONLY_CLI_FLAGS.iter().map(|f| f.flag).collect();
    assert_eq!(
        flags,
        vec![
            "--real-jdk",
            "--compatible",
            "--fast-boot",
            "--synthetic-jdk",
            "--real-jdk-report",
            "--dump-class-origins",
            "--trace-real-jdk",
            "--explain-real-jdk",
        ]
    );

    let mut unique = flags.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), flags.len(), "a flag is listed twice");

    for f in JDK_ONLY_CLI_FLAGS {
        assert!(!f.help.is_empty(), "{} has no --help line", f.flag);
        assert!(
            f.help.ends_with('.'),
            "{} help must read as a sentence",
            f.flag
        );
        assert_eq!(
            jdk_only_flag(f.flag),
            Some(f),
            "{} is not findable by its own name",
            f.flag
        );
    }

    // Only the two that name a file take one; a value-taking flag parsed as
    // a boolean silently swallows the next argument.
    let with_value: Vec<&str> = JDK_ONLY_CLI_FLAGS
        .iter()
        .filter(|f| f.takes_value)
        .map(|f| f.flag)
        .collect();
    assert_eq!(
        with_value,
        vec!["--real-jdk-report", "--dump-class-origins"]
    );

    assert!(jdk_only_flag("--real-jdky").is_none());
    assert!(
        jdk_only_flag("--real-jdk-report=x").is_none(),
        "exact match"
    );
    assert!(jdk_only_flag("").is_none());
}

#[test]
fn jdk_only_conflicts_with_the_synthetic_library_and_the_compatible_policy() {
    assert_eq!(
        JDK_ONLY_CONFLICTS_WITH.to_vec(),
        vec!["--synthetic-jdk", "--compatible", "--fast-boot"]
    );
    for name in JDK_ONLY_CONFLICTS_WITH {
        assert!(
            jdk_only_flag(name).is_some(),
            "{name} is not a flag this table knows about"
        );
    }
    assert!(JDK_ONLY_CONFLICTS_WITH.contains(&"--fast-boot"));
    assert!(!JDK_ONLY_CONFLICTS_WITH.contains(&"--real-jdk"));
}

// ── The 2026-08-01 declaration audit ───────────────────────────────────
//
// Four JIT capabilities landed in this wave with their consumers committed
// and their declarations impossible, because this file was owned by another
// lane. The tests below pin what was verified at each consumer, not merely
// that a row exists — a row with the wrong polarity is worse than no row,
// since it reads as coverage while doing the opposite of what it says.

/// The default-OFF JIT capability rows below are switched off by **removing**
/// their key, never by writing a value into it.
///
/// Two distinct reasons converge on the same requirement, and the assertion
/// messages keep them apart because only one of them is a correctness bug:
///
/// * `range-bce` and `activation-global-mutex` are **presence-parsed**
///   (`runtime_var_os(..).is_some()`). Writing `"0"` leaves `is_some()`
///   answering `true`, so an `off_word` here makes `CRATONVM_JIT=-token`
///   turn the feature **on**. For `range-bce` that is not cosmetic: the gate
///   admits a new reason for *deleting* a bounds check, and a wrong elision
///   is an out-of-bounds heap write — which would then be reachable from the
///   documented surface by the very spelling that means "off".
///
/// (`vectorize` left this list on 2026-09-18, round 9 wave 4, when it went
/// default-ON with `off_word: Some("0")`.)

/// The whole bounds-check-elimination family, in one place.
///
/// `range-bce` arrived alongside three existing switches and the question
/// asked was whether *any* of them were declared. Three already were; a gap
/// in this family is how a bisection ends up unable to express the state it
/// needs, so the set is asserted rather than assumed.

/// Two knobs that were asked for and are deliberately **not** here.
///
/// `CRATONVM_TIER_BROKER`: `jit/src/tiered.rs` reads no such key — the
/// standalone `CompilationBroker` it would have gated was deleted
/// 2026-09-12 without ever being wired (`docs/feature-designs/wire-tiered-manager.md`).
/// `CRATONVM_JIT_BYTECODE_UNROLL`: the loop rewriter is armed by a
/// thread-local (`x64::set_bytecode_loop_rewriter_armed`) and reads no
/// environment at all.
///
/// A declared flag with no read site is worse than an absent one: it reads
/// as coverage, `CRATONVM_JIT=<token>` reports no unknown token, and
/// `docs/CONFIG.md` gains a knob that does nothing. That is exactly the
/// defect `the_documented_no_ops_now_work` above exists to have fixed once.
///
/// The two names are spelled with `concat!` on purpose. Written whole they
/// would be exact `"CRATONVM_…"` literals in a scanned source file, and
/// both `types/tests/flag_declaration_guard.rs` and
/// `tools/flag-census/check-surface.sh` would report them as undeclared
/// read sites — which is the correct behaviour for a *read* site and the
/// wrong answer for an assertion that the name does not exist. Splitting
/// before the underscore is enough: both scanners key on `"CRATONVM_`.
#[test]
fn knobs_without_a_consumer_are_not_declared() {
    for key in [
        concat!("CRATONVM", "_TIER_BROKER"),
        concat!("CRATONVM", "_JIT_BYTECODE_UNROLL"),
    ] {
        assert!(
            canonical_spelling(key).is_none(),
            "{key} is declared, but nothing reads it. Delete the row, or \
             land the consumer in the same commit."
        );
    }
    for token in ["compilation-broker", "bytecode-unroll"] {
        assert!(lookup(Group::JIT, token).is_none(), "{token} is declared");
    }
    // And the tokens that *do* exist are reported as unknown, so a runbook
    // exporting one is told rather than silently ignored.
    let c = case(&[("CRATONVM_JIT", "compilation-broker")]);
    assert_eq!(
        c.resolve().unknown_tokens,
        vec!["CRATONVM_JIT=compilation-broker"]
    );
}

// Real-JDK Random is default-on; a minus token must disable it explicitly.

/// The two ZGC rows are **kill switches**, and `-token` has to reach the
/// value their own parsers read as false.
///
/// Both `zgc_tlab_enabled_by_default` and `zgc_start_bits_enabled_by_default`
/// return `true` for an unset key, so the `off_word: None` these rows
/// carried until 2026-08-13 made `CRATONVM_GC=-zgc-tlab` expand to *unset*
/// — i.e. to the ON state. That is the failure mode this test exists for,
/// and it is invisible from the outside: the spelling is accepted, no
/// unknown-token diagnostic fires, and the feature stays on. It also fed a
/// wrong `opt-in | off` row into the generated `flag-inventory.md`.
///
/// The exact edit that trips it: put `off_word: None` back on either row.

/// The rest of the audit: knobs that were read by a live `getenv` because
/// no row named them. Polarity is asserted per row, since these were
/// classified by reading each consumer.

/// The two OSR diagnosis levers declared 2026-08-04 resolve BOTH ways, and
/// are default-OFF.
///
/// `flag_declaration_guard` only asks whether a name appears in
/// [`INVENTORY`]; a row can be listed and still not resolve, and from there
/// the two look identical. So this drives the grouped spelling and checks
/// the legacy key it must set — and checks the polarity, because these two
/// sit four rows from `osr-dead-locals`, which is the opposite: default-ON
/// with `"0"` as its kill switch.
///
/// The exact edit that trips it: give either row an `off_word`. The
/// `off_word.is_none()` assertion fails, and so does the last block —
/// `-osr-single-pc` would start expanding to `=0` instead of doing nothing,
/// and for a consumer that accepts only `1`/`on`/`true`/`yes` that is an
/// opt-out spelling nobody wrote.

/// `ir-linear-scan` is one token name in two groups — the capability in
/// `JIT`, its trace in `DBG` — like `ir-long` and `xt-jit-root-scan` before
/// it. They must stay two distinct keys, or `CRATONVM_DBG=all` would arm a
/// register allocator.

/// The six GC/native-collections knobs brought inside the boundary on
/// 2026-08-04 are reachable BOTH ways, which is the whole point of
/// declaring them.
///
/// `flag_declaration_guard` only asks whether a name appears in
/// [`INVENTORY`]; it cannot tell a token that resolves from one that was
/// merely listed. Until this landed, all six were served by a live
/// `getenv`, so `CRATONVM_GC=…` could not reach them and
/// `flags::with_thread_overrides` could not arrange them in a test — which
/// is exactly how a flag-dependent test ends up measuring the developer's
/// ambient environment instead of what it claims to check.

#[test]
fn the_whole_surface_is_twenty_variables() {
    // This is the number the refactor exists to hold down. Raising it wants
    // an argument, not a merge.
    //
    // 15 -> 17 -> 18 on 2026-09-01, and every argument was made at the
    // entry itself, in `SCALARS`, by the change that added it:
    // `CRATONVM_JFR_ENABLE_EVENTS` (B10), the `native.encoding` override
    // (D3), and `CRATONVM_STDOUT_ENCODING`.
    //
    // The argument for the third, since this is where it is owed: it is the
    // opt-out for deriving `stdout.encoding` / `stderr.encoding` /
    // `stdin.encoding` from the host instead of pinning UTF-8, which
    // `docs/feature-designs/real-jdk-mode.md` §9 asked for by name when it recommended following the
    // host — that key decides the `Charset` stamped on `System.out`, so the
    // change moves BYTES and not only a property string, and a change that
    // moves bytes on every non-UTF-8 host needs a same-binary way back.
    // A scalar rather than an `INVENTORY` token for the same reason as the
    // two above: it carries a VALUE, and the `E` model is presence-only.
    //
    // Two GROUPS were not added — `Group::ALL` is still ten. Adding one of
    // those is the move that would want a fresh argument.
    //
    // 18 -> 19 on 2026-09-26: `CRATONVM_MONITOR_LAZY_PARK_US` (JIT round 11
    // wave 19), argued at its entry in `SCALARS`: it carries a VALUE (a
    // park bound in microseconds), which the presence-only `E` model
    // cannot express. That change did not update this count.
    //
    // 19 -> 20 on 2026-09-27: `CRATONVM_MONITOR_MAX_SPINNERS` (JIT round 12
    // wave 1), a cap on contended spinners per monitor: it carries a VALUE,
    // which the presence-only `E` model cannot express. That change did not
    // update this count either; gen round 5 wave 5 did.
    //
    // 20 -> 21 on 2026-09-30: `CRATONVM_FFM_AUTO_ARENA_GC_BUDGET_MB` (JIT
    // round 15 wave 2, FFM3W-2), the megabytes of auto-arena memory
    // allocated between collections before one is requested: it carries a
    // VALUE, which the presence-only `E` model cannot express. Unset = off.
    //
    // 21 -> 24 on 2026-10-29 (Phase 5 boot image): `CRATONVM_DBG_IMAGE_CENSUS`
    // (WP0, a report file path; that change did not update this count),
    // `CRATONVM_BOOT_IMAGE` (off|capture|auto plus tokens) and
    // `CRATONVM_BOOT_IMAGE_DIR` (a cache path): each carries a VALUE.
    assert_eq!(Group::ALL.len() + SCALARS.len(), 24);
}
/// The repository's first commit. No knob can predate it, so a `since:`
/// earlier than this is a typo rather than a very old flag.
const REPOSITORY_EPOCH: &str = "2026-04-26";

/// A `DBG` knob declared on or after this date must have a live consumer.
///
/// # Why the rule is stated forwards rather than backwards
///
/// The obvious retirement rule — "a knob older than N months must justify
/// itself" — is red on arrival here: **43** `DBG` tokens declared before
/// 2026-08-01 are named nowhere outside `types/`, the two generated
/// documents and `docs/feature-designs/real-jdk-mode.md` (the historical bug-write-up archive,
/// which is stripped from published history, so a mention there is not a
/// consumer anybody can reach). Landing a test that fails on 43 rows nobody
/// in this branch is allowed to delete produces a red gate everyone learns
/// to skip, which is worse than no gate at all.
///
/// So the horizon runs the other way: rows declared **before** it are
/// grandfathered and enumerated in
/// `docs/feature-designs/real-jdk-mode.md`; rows declared **on
/// or after** it have to earn their place. That is the half that can be
/// green today and still bite, because every new flag is on the biting side
/// of the line from the day it is added.
///
/// # Why this exact date
///
/// It is measured, not chosen for roundness. The newest consumer-less `DBG`
/// row in the tree is `dupclass-filter`, dated 2026-07-27; 2026-08-01 is the
/// first month boundary above it. That leaves **126** `DBG` rows inside the
/// rule today, all of them passing, and every row added from now on. Moving
/// the horizon *earlier* is the retirement work itself: each step back has
/// to be paid for by deleting or documenting the rows it newly covers, and
/// this test names exactly which those are.
const DBG_CONSUMER_HORIZON: &str = "2026-08-01";

/// Directories never searched for a consumer: build output, VCS metadata,
/// and the Java application harnesses under `apps/`.
///
/// `types/` and the internal write-up archive are excluded too, but by
/// name at the walk's top level rather than here — see
/// [`names_with_a_possible_consumer`].
const NOT_A_CONSUMER: &[&str] = &["target", ".git", "apps", "node_modules"];

/// File kinds that can carry a consumer: source, runbooks, CI, fixtures.
const CONSUMER_EXTENSIONS: &[&str] = &[
    "rs", "md", "sh", "py", "toml", "yml", "yaml", "txt", "ps1", "json", "java",
];

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("types/ always has a workspace root above it")
        .to_path_buf()
}

/// Every `CRATONVM_*` name mentioned anywhere a *consumer* could live, and
/// the number of files actually read.
///
/// Two directories are cut out of the walk. `types/` is the declaration
/// itself, and a row citing its own registry is not evidence of anything.
/// The `internal` subtree of `docs` is the historical write-up archive; it
/// is stripped from published history, so a name whose only mention is
/// there has no reader outside this working copy. The two generated
/// documents are cut for the same reason as `types/`: they are rendered
/// *from* this table, so every row is in them by construction. The
/// retirement-candidate list is cut for a third reason: it names a knob in
/// order to propose DELETING it, which is the opposite of evidence that
/// something still wants it. (It is also self-referential — it was written
/// by this same measurement, and counting it moved the candidate set from
/// 63 to 0 the first time it was left in.)
///
/// A mention, not a read site: the policy accepts a runbook, a CI step or a
/// known-issue page as evidence that a knob is still wanted, which is why
/// this scans Markdown and shell as well as Rust. It deliberately does not
/// require the quoting precision of
/// `flag_declaration_guard::exact_literals` — prose does not quote.
fn names_with_a_possible_consumer() -> (std::collections::BTreeSet<String>, usize) {
    let root = workspace_root();
    let derived = [
        "docs/config/flag-inventory.md",
        "docs/flag-tokens.md",
        "docs/config/flag-inventory.md",
    ];
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut files = 0usize;
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if path.is_dir() {
                if name.starts_with('.') || NOT_A_CONSUMER.contains(&name) {
                    continue;
                }
                if dir == root && name == "types" {
                    continue;
                }
                if name == "internal" && dir == root.join("docs") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !CONSUMER_EXTENSIONS.contains(&ext) {
                continue;
            }
            let relative = path.strip_prefix(&root).unwrap_or(path.as_path());
            let shown = relative.display().to_string().replace('\\', "/");
            if derived.contains(&shown.as_str()) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            files += 1;
            if !text.contains("CRATONVM_") {
                continue;
            }
            let mut rest = text.as_str();
            while let Some(at) = rest.find("CRATONVM_") {
                let tail = &rest[at..];
                let end = tail
                    .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                    .unwrap_or(tail.len());
                names.insert(tail[..end].to_string());
                rest = &rest[at + "CRATONVM_".len()..];
            }
        }
    }
    (names, files)
}

/// Every row says when its knob arrived, in a form that can be compared.
///
/// The date is what makes retirement possible at all, so a malformed one is
/// worse than a wrong one: `since: "aug 2026"` sorts below every real date
/// and would quietly drop its row out of every horizon check for good.
#[test]
fn every_row_states_when_it_arrived() {
    for e in INVENTORY {
        let s = e.since;
        let well_formed = s.len() == 10
            && s.as_bytes().iter().enumerate().all(|(i, c)| {
                if i == 4 || i == 7 {
                    *c == b'-'
                } else {
                    c.is_ascii_digit()
                }
            });
        assert!(
            well_formed,
            "{}={} has since: {s:?}, which is not an ISO YYYY-MM-DD date. \
             The field is compared as a string, so anything else sorts \
             silently outside every horizon instead of failing.",
            e.group.var(),
            e.token
        );
        assert!(
            s >= REPOSITORY_EPOCH,
            "{}={} claims since: {s:?}, which is before the repository's \
             first commit ({REPOSITORY_EPOCH}) — no knob can be older than \
             the tree it lives in",
            e.group.var(),
            e.token
        );
    }
}


// ── The 2026-09-21 declaration audit (round 9 wave 5) ──────────────────
//
// The three names `flag_declaration_guard` had been red on, each declared
// after reading its consumer rather than after reading its NAME. The same
// reasoning as the 2026-08-01 block above, and the same reason for pinning
// it: a row with the wrong polarity is worse than no row, because it reads
// as coverage while doing the opposite of what it says.



