// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The assertion switches of one VM, as HotSpot's `JavaAssertions`
//! (`src/hotspot/share/classfile/javaAssertions.cpp`) keeps them.
//!
//! Interpreter round i5 wave 107, lane L5
//! (`docs/internal/fixed-bugs/interpreter-i106-L5-assertion-switches-are-one-global-FIXED-20261016.md`).
//! A per-VM value (`VmConfig::assertion_directives`, read through
//! `NativeContext::assertion_directives`), never a process global: two VMs in
//! one process may be launched with different `-ea` options.

/// A user default (`-ea` / `-da`), a system default (`-esa` / `-dsa`, the boot
/// loader's classes) and the per-class / per-package options of
/// `-ea:<name>` / `-da:<name>`, in command-line order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssertionDirectives {
    /// The last unscoped `-ea` / `-da` (`-enableassertions` /
    /// `-disableassertions`); `None` when none was given, in which case the
    /// caller's fallback (`CRATONVM_ENABLE_ASSERTIONS`, the embedders' switch)
    /// is the user default.
    pub user_default: Option<bool>,
    /// The last `-esa` / `-dsa`; HotSpot's default is `false`.
    pub system_default: bool,
    /// `-ea:<class>` / `-da:<class>`: internal (slash) names, command-line
    /// order; the last occurrence of a name wins.
    pub classes: Vec<(String, bool)>,
    /// `-ea:<pkg>...` / `-da:<pkg>...` (the `...` removed; `""` is the unnamed
    /// package of `-ea:...`): internal names, command-line order.
    pub packages: Vec<(String, bool)>,
}

impl AssertionDirectives {
    /// Apply one launcher token the way `Arguments::parse_each_vm_init_arg`
    /// does. Returns `false` (and changes nothing) when `arg` is not an
    /// assertion option: `-esa` / `-dsa` take no `:` tail, the user options
    /// take none or `:<name>`.
    pub fn apply_option(&mut self, arg: &str) -> bool {
        const USER: [(&str, bool); 4] = [
            ("-da", false),
            ("-ea", true),
            ("-disableassertions", false),
            ("-enableassertions", true),
        ];
        for (name, enable) in USER {
            if let Some(tail) = arg.strip_prefix(name) {
                if tail.is_empty() {
                    self.user_default = Some(enable);
                    return true;
                }
                if let Some(option) = tail.strip_prefix(':') {
                    self.add_option(option, enable);
                    return true;
                }
            }
        }
        match arg {
            "-esa" | "-enablesystemassertions" => {
                self.system_default = true;
                true
            }
            "-dsa" | "-disablesystemassertions" => {
                self.system_default = false;
                true
            }
            _ => false,
        }
    }

    /// `JavaAssertions::addOption`: a name ending in `...` goes on the package
    /// list without it, any other on the class list; dots become slashes.
    pub fn add_option(&mut self, name: &str, enable: bool) {
        match name.strip_suffix("...") {
            Some(package) => self
                .packages
                .push((package.replace('.', "/"), enable)),
            None => self.classes.push((name.replace('.', "/"), enable)),
        }
    }

    /// Whether any option at all was recorded (an unscoped or scoped switch).
    pub fn is_empty(&self) -> bool {
        self.user_default.is_none()
            && !self.system_default
            && self.classes.is_empty()
            && self.packages.is_empty()
    }

    /// `JavaAssertions::enabled(classname, systemClass)`: an exact class
    /// option, else the most specific enclosing package option, else the
    /// system default for a boot-loader class and the user default (or
    /// `user_fallback` when no `-ea` / `-da` was given) otherwise. Later
    /// options win. `class_name` may be internal or dotted.
    pub fn enabled(&self, class_name: &str, system_class: bool, user_fallback: bool) -> bool {
        let owned;
        let name = if class_name.contains('.') {
            owned = class_name.replace('.', "/");
            owned.as_str()
        } else {
            class_name
        };
        if let Some((_, on)) = self.classes.iter().rev().find(|(c, _)| c == name) {
            return *on;
        }
        if let Some(on) = self.match_package(name) {
            return on;
        }
        if system_class {
            self.system_default
        } else {
            self.user_default.unwrap_or(user_fallback)
        }
    }

    /// `JavaAssertions::match_package`, including its quirk: the unnamed
    /// package option (`-ea:...`) is consulted only for a class that has no
    /// package; a packaged class stops after its outermost package.
    fn match_package(&self, name: &str) -> Option<bool> {
        if self.packages.is_empty() {
            return None;
        }
        let mut len = name.rfind('/').unwrap_or(0);
        loop {
            let package = &name[..len];
            if let Some((_, on)) = self.packages.iter().rev().find(|(p, _)| p == package) {
                return Some(*on);
            }
            len = name[..len].rfind('/').unwrap_or(0);
            if len == 0 {
                return None;
            }
        }
    }

    /// `JavaAssertions::createAssertionStatusDirectives`'s lists: dotted
    /// names, command-line order.
    pub fn dotted_classes(&self) -> Vec<(String, bool)> {
        self.classes
            .iter()
            .map(|(n, on)| (n.replace('/', "."), *on))
            .collect()
    }

    /// See [`Self::dotted_classes`].
    pub fn dotted_packages(&self) -> Vec<(String, bool)> {
        self.packages
            .iter()
            .map(|(n, on)| (n.replace('/', "."), *on))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::AssertionDirectives;

    fn parse(args: &[&str]) -> AssertionDirectives {
        let mut d = AssertionDirectives::default();
        for a in args {
            assert!(d.apply_option(a), "{a} is an assertion option");
        }
        d
    }

    // (user, inner, string, list) as L5W106AssertionScopes prints them.
    fn row(d: &AssertionDirectives) -> [bool; 4] {
        [
            d.enabled("L5W106AssertionScopes", false, false),
            d.enabled("L5W106AssertionScopes$Inner", false, false),
            d.enabled("java/lang/String", true, false),
            d.enabled("java/util/ArrayList", true, false),
        ]
    }

    #[test]
    fn hotspot_rows_of_the_probe() {
        assert_eq!(row(&parse(&[])), [false, false, false, false]);
        assert_eq!(row(&parse(&["-ea"])), [true, true, false, false]);
        assert_eq!(row(&parse(&["-esa"])), [false, false, true, true]);
        assert_eq!(
            row(&parse(&["-ea", "-da:L5W106AssertionScopes"])),
            [false, true, false, false]
        );
        assert_eq!(
            row(&parse(&["-ea", "-da:L5W106AssertionScopes$Inner"])),
            [true, false, false, false]
        );
        assert_eq!(row(&parse(&["-ea:..."])), [true, true, false, false]);
        assert_eq!(
            row(&parse(&["-ea:L5W106AssertionScopes"])),
            [true, false, false, false]
        );
        assert_eq!(row(&parse(&["-ea:java.util..."])), [false, false, false, true]);
        assert_eq!(row(&parse(&["-da", "-esa"])), [false, false, true, true]);
    }

    #[test]
    fn last_option_wins_and_packages_nest() {
        let d = parse(&["-ea:a.b...", "-da:a.b.c...", "-ea:a.b.c.D", "-da:a.b.c.D"]);
        assert!(d.enabled("a/b/X", false, false));
        assert!(!d.enabled("a/b/c/X", false, false));
        assert!(!d.enabled("a/b/c/D", false, false));
        assert!(!d.enabled("a/b/c/d/E", false, false), "a.b.c... covers a.b.c.d");
        assert!(!d.enabled("a/bc/X", false, false), "a.b... is not a prefix match");
        // The unnamed-package option does not reach a packaged class.
        let d = parse(&["-ea:..."]);
        assert!(!d.enabled("a/X", false, false));
        assert!(d.enabled("X", false, false));
    }

    #[test]
    fn non_options_are_refused() {
        let mut d = AssertionDirectives::default();
        for a in ["-esa:x", "-eax", "-e", "-enableassertionsX", "-Xmx1g"] {
            assert!(!d.apply_option(a), "{a}");
        }
        assert!(d.is_empty());
        assert!(d.apply_option("-enableassertions:p..."));
        assert_eq!(d.dotted_packages(), vec![("p".to_string(), true)]);
    }

    #[test]
    fn fallback_is_the_user_default_only() {
        let d = AssertionDirectives::default();
        assert!(d.enabled("p/A", false, true));
        assert!(!d.enabled("java/lang/String", true, true));
        let d = parse(&["-da"]);
        assert!(!d.enabled("p/A", false, true));
    }
}
