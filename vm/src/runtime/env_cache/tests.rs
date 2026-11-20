/// gch h4/wr item 7: only `2` / `list` print the watched addresses; the
/// ordinary on-words (`1`, `true`, `on`, `yes`) print the count alone.
#[test]
fn gci_i47w_oc47_watchref_list_needs_the_second_level() {
    use super::watchref_list_level;
    for v in ["2", "list", " LIST ", "2\n"] {
        assert!(watchref_list_level(v), "{v:?} is the list level");
    }
    for v in ["", "1", "true", "on", "yes", "0", "off", "3"] {
        assert!(!watchref_list_level(v), "{v:?} is not the list level");
    }
}

/// The guarded-inline default, pinned — N8.
///
/// This is the tree's ONLY devirtualization: with it off, `plan_inline`
/// answers `InlineRefusal::GuardNotEmittable` for every speculative plan
/// and no method whose hot work is behind an interface ever gets its
/// callee inlined. The default has been wrong before, in the other
/// direction and by accident, and nothing failed when it was.
///
/// `=0` is the one-variable bisect and must keep working; so must
/// `=false`, because `vm/tests/pgo02_guarded_virtual_inline.rs` and
/// `jit_guarded_inline_native_shadow.rs` set this variable by name and a
/// reader who knows one spelling will try the other.
#[test]
fn guarded_virtual_inlining_is_on_unless_explicitly_switched_off() {
    assert!(
        super::guarded_virtual_inline_from_env(None),
        "the shipped default must perform devirtualization"
    );
    assert!(super::guarded_virtual_inline_from_env(Some("1")));
    assert!(super::guarded_virtual_inline_from_env(Some("true")));
    assert!(!super::guarded_virtual_inline_from_env(Some("0")));
    assert!(!super::guarded_virtual_inline_from_env(Some("false")));
    assert!(!super::guarded_virtual_inline_from_env(Some("FALSE")));
}

/// The CHA default, pinned, and pinned to the SAME spellings — C3.
///
/// `CRATONVM_JIT_CHA` exists so a bisect can ask "is class-hierarchy
/// analysis the cause" without also withdrawing the receiver-profile
/// devirtualization that `guarded-virtual-inline` controls. That is only
/// useful if it is default-ON — an opt-OUT bisects the shipped
/// configuration, an opt-IN bisects one nobody runs — and only usable if
/// `=0` and `=false` both work, because an operator who learned one
/// spelling from the flag above will try it here.
#[test]
fn cha_is_on_unless_explicitly_switched_off() {
    assert!(
        super::cha_from_env(None),
        "the shipped default must perform class-hierarchy analysis"
    );
    assert!(super::cha_from_env(Some("1")));
    assert!(super::cha_from_env(Some("true")));
    assert!(!super::cha_from_env(Some("0")));
    assert!(!super::cha_from_env(Some("false")));
    assert!(!super::cha_from_env(Some("FALSE")));
}

use super::parse_osr_backedge_enabled;
use super::{effective_enforce_shadow_scope, parse_enforce_shadow_scope, EnforceShadowScope};

/// The pure-bytecode lever, modelled as a parameter: lever off is exactly
/// the dial's own parse and the default kind rule (Bridge only); lever on
/// arms the dial for every receiver and lets a reviewed `Intrinsic` yield.
#[test]
fn pure_bytecode_lever_is_inert_off_and_total_on() {
    for raw in [None, Some(""), Some("0"), Some("1"), Some("java/util/")] {
        assert_eq!(
            effective_enforce_shadow_scope(raw, false),
            parse_enforce_shadow_scope(raw),
            "lever off must not change the dial parse for {raw:?}"
        );
        assert_eq!(
            effective_enforce_shadow_scope(raw, true),
            EnforceShadowScope::All
        );
    }
}


#[test]
fn no_presence_predicate_shadows_a_compound_flag_default() {
    const ENV_CACHE_SRC: &str = include_str!("../env_cache.rs");
    const FLAGS_SRC: &str = include_str!("../../../../types/src/flags.rs");

    // Every env var behind a cached presence predicate in this file.
    let mut presence_vars: Vec<&str> = Vec::new();
    for line in ENV_CACHE_SRC.lines() {
        let l = line.trim_start();
        if l.starts_with("cached_is_set!(") || l.starts_with("cached_is_ok!(") {
            if let Some(var) = l.split('"').nth(1) {
                presence_vars.push(var);
            }
        }
    }
    assert!(
        presence_vars.len() > 20,
        "the scanner found only {} presence predicates — it stopped matching \
         the macro call shape it audits, so this gate is inert",
        presence_vars.len()
    );

    // `include_str!` embeds the file's RAW bytes, and this repository is
    // checked out with CRLF on Windows. Every `,\n` boundary search below
    // would then match nothing (the bytes are `,\r\n`): `initializer_around`
    // would run from the last `{` in the whole file to EOF, hand back a
    // ~60 KB slab that trivially contains `||`, and report a long list of
    // "compound" offenders that are nothing of the sort — the first being
    // `CRATONVM_IAE_TRACE`, whose initializer is a plain
    // `present(src, ..)`. Normalise once so this gate asks the SAME
    // question on a CRLF and an LF checkout. (Only `flags.rs` needs it:
    // `str::lines()` already strips a trailing `\r` from the env-cache
    // scan above.)
    let flags_src = FLAGS_SRC.replace("\r\n", "\n");
    let flags_src = flags_src.as_str();

    // The flags.rs field initializer that resolves a given variable: from
    // just after the previous initializer's `,` up to this one's.
    fn initializer_around(src: &str, idx: usize) -> &str {
        let mut start = src[..idx].rfind(",\n").map(|p| p + 2).unwrap_or(0);
        if let Some(brace) = src[start..idx].rfind('{') {
            start += brace + 1;
        }
        let end = src[idx..].find(",\n").map(|p| idx + p).unwrap_or(src.len());
        &src[start..end]
    }

    let mut offenders: Vec<(&str, String)> = Vec::new();
    for var in &presence_vars {
        let needle = format!("present(src, \"{var}\")");
        let mut from = 0;
        while let Some(rel) = flags_src[from..].find(&needle) {
            let idx = from + rel;
            from = idx + needle.len();
            let init = initializer_around(flags_src, idx);
            // A default that can be true with the variable unset always
            // reaches for another term: `||`, `&&`, or a negated presence.
            if init.contains("||") || init.contains("&&") || init.contains("!present") {
                offenders.push((var, init.split_whitespace().collect::<Vec<_>>().join(" ")));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these env vars have a compound (non-presence) default in flags.rs \
         but are still answered by a presence predicate in env_cache.rs, so \
         the predicate now means \"explicitly requested\" and not \"active\": \
         {offenders:?}"
    );
}

#[test]
fn osr_backedge_defaults_on_and_has_explicit_opt_outs() {
    assert!(parse_osr_backedge_enabled(None));
    assert!(parse_osr_backedge_enabled(Some("")));
    assert!(parse_osr_backedge_enabled(Some("1")));
    assert!(parse_osr_backedge_enabled(Some("true")));
    assert!(!parse_osr_backedge_enabled(Some("0")));
    assert!(!parse_osr_backedge_enabled(Some("false")));
    assert!(!parse_osr_backedge_enabled(Some("OFF")));
    assert!(!parse_osr_backedge_enabled(Some(" no ")));
}
/// `CRATONVM_ENFORCE_NATIVE_SHADOW` is a §1.4 dial AND a subsystem
/// selector, and the two spellings that already meant something keep
/// meaning it.
#[test]
fn enforce_shadow_scope_parses_the_three_spellings() {
    assert_eq!(parse_enforce_shadow_scope(None), EnforceShadowScope::Off);
    assert_eq!(
        parse_enforce_shadow_scope(Some("")),
        EnforceShadowScope::Off
    );
    assert_eq!(
        parse_enforce_shadow_scope(Some("  ")),
        EnforceShadowScope::Off
    );
    assert_eq!(
        parse_enforce_shadow_scope(Some("0")),
        EnforceShadowScope::Off
    );
    for on in ["1", "all", "ALL", "true", "Yes", "on"] {
        assert_eq!(
            parse_enforce_shadow_scope(Some(on)),
            EnforceShadowScope::All,
            "{on}"
        );
    }
    assert_eq!(
        parse_enforce_shadow_scope(Some("javax/management/")),
        EnforceShadowScope::Prefixes(vec!["javax/management/".to_string()])
    );
    // Dotted spelling and whitespace both normalise, because a reader who
    // has just been looking at a census (dots) and one who has been looking
    // at the registry (slashes) must not get different behaviour.
    assert_eq!(
        parse_enforce_shadow_scope(Some(" java.util.logging. , javax/management/ ")),
        EnforceShadowScope::Prefixes(vec![
            "java/util/logging/".to_string(),
            "javax/management/".to_string()
        ])
    );
}

/// A prefix list that parses to NOTHING must read as off, not as armed —
/// otherwise a typo produces an inert run that looks like a clean result.
#[test]
fn an_empty_prefix_list_is_off_not_vacuously_armed() {
    let s = parse_enforce_shadow_scope(Some(",  , ,"));
    assert_eq!(s, EnforceShadowScope::Prefixes(vec![]));
    assert!(s.is_off(), "an empty prefix list must not read as armed");
    assert!(!s.covers("javax/management/MBeanServer"));
}

/// `covers` is what dispatch asks, and it must be narrow: arming one
/// subsystem must not enforce §1.4 on every other receiver in the VM.
#[test]
fn enforce_shadow_scope_covers_only_the_named_subsystem() {
    let all = EnforceShadowScope::All;
    assert!(all.covers("java/lang/String"));
    assert!(all.covers("javax/management/MBeanServer"));

    let jmx = parse_enforce_shadow_scope(Some("javax/management/"));
    assert!(!jmx.is_off());
    assert!(jmx.covers("javax/management/MBeanServer"));
    assert!(jmx.covers("javax/management/openmbean/CompositeDataSupport"));
    assert!(
        !jmx.covers("java/lang/String"),
        "the JMX dial must not reach java.lang"
    );
    assert!(!jmx.covers("java/util/logging/LogManager"));

    let two = parse_enforce_shadow_scope(Some("java/util/logging/,javax/management/"));
    assert!(two.covers("java/util/logging/LogManager"));
    assert!(two.covers("javax/management/MBeanServer"));
    assert!(!two.covers("java/lang/System"));

    assert!(!EnforceShadowScope::Off.covers("javax/management/MBeanServer"));
}
