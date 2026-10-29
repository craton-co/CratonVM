//! HotSpot 25's `-XX:` names that exist on one operating system only, over the
//! generated tables ([`hotspot25_flags`], captured from the Windows x64 JDK, and
//! [`hotspot25_develop_flags`]).
//!
//! Hand-reviewed, not generated: a listing taken on another OS (the Linux x64
//! listing used here is Ubuntu 25.0.4.1) also differs by update release, and
//! only the names HotSpot declares in `globals_linux.hpp` / `globals_windows.hpp`
//! (or under `LINUX_ONLY`) belong here. Measured with `-XX:+PrintFlagsFinal`
//! / `-XX:+PrintFlagsRanges` (both unlocks on) and `-XX:+<Name> -version` on
//! both systems (gc-known-issues retire, 2026-10-25): each name below is
//! `Unrecognized VM option` on the other system.
//!
//! [`hotspot25_flags`]: crate::hotspot25_flags
//! [`hotspot25_develop_flags`]: crate::hotspot25_develop_flags

use crate::hotspot25_flags::{self, F, L, R, T};

/// Product, diagnostic and experimental flags of Linux only.
#[rustfmt::skip]
const LINUX_ONLY: &[F] = &[
    F { name: "AdjustStackSizeForTLS", ty: T::Bool, lock: L::None, range: R::None },
    F { name: "DumpPerfMapAtExit", ty: T::Bool, lock: L::Diagnostic, range: R::None },
    F { name: "DumpPrivateMappingsInCore", ty: T::Bool, lock: L::Diagnostic, range: R::None },
    F { name: "DumpSharedMappingsInCore", ty: T::Bool, lock: L::Diagnostic, range: R::None },
    F { name: "LoadExecStackDllInVMThread", ty: T::Bool, lock: L::None, range: R::None },
    F { name: "PrintMemoryMapAtExit", ty: T::Bool, lock: L::Diagnostic, range: R::None },
    F { name: "THPStackMitigation", ty: T::Bool, lock: L::Diagnostic, range: R::None },
    F { name: "TimerSlack", ty: T::Intx, lock: L::Experimental, range: R::None },
    F { name: "UseContainerSupport", ty: T::Bool, lock: L::None, range: R::None },
    F { name: "UseCpuAllocPath", ty: T::Bool, lock: L::Diagnostic, range: R::None },
    F { name: "UseMadvPopulateWrite", ty: T::Bool, lock: L::Diagnostic, range: R::None },
    F { name: "UseOprofile", ty: T::Bool, lock: L::None, range: R::None },
    F { name: "UseTransparentHugePages", ty: T::Bool, lock: L::None, range: R::None },
];

/// Flags of the generated table that only the Windows JDK has.
const WINDOWS_ONLY: &[&str] = &[
    "EnableAllLargePageSizesForWindows",
    "UseAllWindowsProcessorGroups",
    "UseOSErrorReporting",
];

/// `develop` flags of Linux only (`true`: `bool`).
const LINUX_ONLY_DEVELOP: &[(&str, bool)] = &[("DelayThreadStartALot", true)];

const ON_LINUX: bool = cfg!(target_os = "linux");
const ON_WINDOWS: bool = cfg!(target_os = "windows");

/// The flag named `name`, if HotSpot 25 has it on this operating system.
pub(crate) fn lookup(name: &str) -> Option<&'static F> {
    if !ON_WINDOWS && WINDOWS_ONLY.contains(&name) {
        return None;
    }
    hotspot25_flags::lookup(name).or_else(|| {
        ON_LINUX
            .then(|| LINUX_ONLY.iter().find(|f| f.name == name))
            .flatten()
    })
}

/// Every flag HotSpot 25 lists on this operating system, the generated
/// table's order first.
pub(crate) fn flags() -> impl Iterator<Item = &'static F> {
    hotspot25_flags::FLAGS
        .iter()
        .filter(|f| ON_WINDOWS || !WINDOWS_ONLY.contains(&f.name))
        .chain(LINUX_ONLY.iter().filter(|_| ON_LINUX))
}

/// Is `name` a `develop` flag on this operating system, and is it a `bool`?
pub(crate) fn develop(name: &str) -> Option<bool> {
    crate::hotspot25_develop_flags::develop(name).or_else(|| {
        ON_LINUX
            .then(|| {
                LINUX_ONLY_DEVELOP
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, b)| *b)
            })
            .flatten()
    })
}

/// Every `develop` flag on this operating system, as (name, `bool`).
pub(crate) fn develop_flags() -> impl Iterator<Item = (&'static str, bool)> {
    crate::hotspot25_develop_flags::DEVELOP
        .iter()
        .copied()
        .chain(LINUX_ONLY_DEVELOP.iter().copied().filter(|_| ON_LINUX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_os_lists_do_not_overlap_the_generated_tables() {
        for f in LINUX_ONLY {
            assert!(hotspot25_flags::lookup(f.name).is_none(), "{}", f.name);
        }
        for n in WINDOWS_ONLY {
            assert!(hotspot25_flags::lookup(n).is_some(), "{n}");
        }
        for (n, _) in LINUX_ONLY_DEVELOP {
            assert!(crate::hotspot25_develop_flags::develop(n).is_none(), "{n}");
        }
    }

    #[test]
    fn lookup_answers_for_this_os() {
        assert_eq!(lookup("UseTransparentHugePages").is_some(), ON_LINUX);
        assert_eq!(lookup("UseOSErrorReporting").is_some(), ON_WINDOWS);
        assert!(lookup("UseSerialGC").is_some());
        assert_eq!(develop("DelayThreadStartALot"), ON_LINUX.then_some(true));
        assert_eq!(develop("CountBytecodes"), Some(true));
    }
}
