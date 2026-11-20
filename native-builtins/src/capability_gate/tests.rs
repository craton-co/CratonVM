use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
// `fd_table` is declared on `NativeSystemAccess`, not `NativeContext`; the
// trait must be in scope for the mock to expose it.
use cratonvm_native_api::NativeSystemAccess;
use cratonvm_native_api::{
    capability_audit, install_capabilities, uninstall_capabilities, CapabilityKind,
    CapabilitySet, VmId,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Hands out a VM identity nothing else in the suite uses.
///
/// `install_capabilities` is a process-global index keyed on
/// `vm_identity()`, and every other mock context in the crate reports the
/// default `0`. Installing an `Enforce` policy under `0` would be visible
/// to every test running in parallel and could refuse *their* I/O. A
/// private identity per test keeps the blast radius at zero and lets these
/// tests run unserialized.
fn ctx_with_private_vm() -> (MockNativeContext, VmId) {
    static NEXT: AtomicUsize = AtomicUsize::new(0xCA9_0001);
    let raw = NEXT.fetch_add(1, Ordering::Relaxed);
    let ctx = mock_ctx();
    ctx.set_vm_identity(raw);
    (ctx, VmId::from_raw(raw))
}

/// Installs a policy for one VM and removes it on drop.
struct PolicyGuard {
    vm: VmId,
}

impl PolicyGuard {
    fn install(set: CapabilitySet) -> PolicyGuard {
        let vm = set.vm();
        install_capabilities(Arc::new(set));
        // The raw-memory memo is keyed by VM, so a private identity is
        // already a miss — reset anyway so a policy re-installed for the
        // same VM inside one test is picked up.
        reset_raw_memory_gate_memo();
        PolicyGuard { vm }
    }
}

impl Drop for PolicyGuard {
    fn drop(&mut self) {
        uninstall_capabilities(self.vm);
        reset_raw_memory_gate_memo();
    }
}

fn policy(vm: VmId, mode: CapabilityMode, grants: &str) -> CapabilitySet {
    let mut set = CapabilitySet::new(vm, mode);
    if !grants.is_empty() {
        let bad = set.grant_from_list(grants);
        assert!(bad.is_empty(), "unparseable grants: {bad:?}");
    }
    set
}

static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

fn temp_path(tag: &str) -> String {
    let n = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join(format!(
            "cratonvm_capgate_{}_{}_{}.bin",
            std::process::id(),
            n,
            tag
        ))
        .to_string_lossy()
        .into_owned()
}

fn write_file(path: &str, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("temp file must be writable");
}

/// How many checks the audit recorded for `kind`, across every scope.
fn recorded(vm: VmId, kind: CapabilityKind) -> u64 {
    capability_audit(vm)
        .map(|r| {
            r.uses
                .iter()
                .filter(|u| u.capability.kind() == kind)
                .map(|u| u.count)
                .sum()
        })
        .unwrap_or(0)
}

// -----------------------------------------------------------------------
// No policy at all — the configuration the whole suite runs in today
// -----------------------------------------------------------------------

#[test]
fn no_policy_installed_means_the_helpers_are_the_raw_openers() {
    let (ctx, vm) = ctx_with_private_vm();
    let path = temp_path("nopolicy");
    write_file(&path, b"craton");

    let fd = open_read_gated(&ctx, &path).expect("read must succeed with no policy");
    let mut buf = [0u8; 6];
    let n = ctx.fd_table().read_bytes(fd, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"craton");
    let _ = ctx.fd_table().close(fd);

    gate_raw_memory(&ctx, RAW_MEMORY_UNSAFE_ADDRESS)
        .expect("raw memory is ungated when no policy is installed");
    assert!(
        capability_audit(vm).is_none(),
        "no policy means nothing to report"
    );
    let _ = std::fs::remove_file(&path);
}

// -----------------------------------------------------------------------
// Permissive — allows, and is observably identical to the ungated path
// -----------------------------------------------------------------------

#[test]
fn permissive_allows_a_read_and_changes_nothing_observable() {
    let (ctx, vm) = ctx_with_private_vm();
    let path = temp_path("permissive");
    write_file(&path, b"0123456789");

    // Baseline: the raw opener, no policy installed.
    let mut baseline = [0u8; 10];
    let baseline_len = {
        let fd = ctx.fd_table().open_read(&path).unwrap();
        let n = ctx.fd_table().read_bytes(fd, &mut baseline).unwrap();
        let _ = ctx.fd_table().close(fd);
        n
    };

    // The same operation through the gate under a `Permissive` policy with
    // no grants at all: same success, same length, same bytes.
    let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Permissive, ""));
    let fd = open_read_gated(&ctx, &path).expect("permissive must allow");
    let mut gated = [0u8; 10];
    let n = ctx.fd_table().read_bytes(fd, &mut gated).unwrap();
    let _ = ctx.fd_table().close(fd);

    assert_eq!(n, baseline_len, "gated read returned a different length");
    assert_eq!(gated, baseline, "gated read returned different bytes");
    assert!(
        recorded(vm, CapabilityKind::FileRead) >= 1,
        "permissive must still record the file-read"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn track_caller_names_the_calling_native_not_the_helper() {
    let (ctx, vm) = ctx_with_private_vm();
    let path = temp_path("site");
    write_file(&path, b"x");

    let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Permissive, ""));
    let fd = open_read_gated(&ctx, &path).unwrap();
    let _ = ctx.fd_table().close(fd);

    let report = capability_audit(vm).expect("a policy is installed");
    let row = report
        .uses
        .iter()
        .find(|u| u.capability.kind() == CapabilityKind::FileRead)
        .expect("file-read must appear in the report");
    assert!(
        row.first_site.file == file!(),
        "the recorded site should be the caller, got {}",
        row.first_site
    );
    let _ = std::fs::remove_file(&path);
}

// -----------------------------------------------------------------------
// Audit — allows, records, and prices the Enforce flip
// -----------------------------------------------------------------------

#[test]
fn audit_allows_but_tallies_the_ungranted_use() {
    let (ctx, vm) = ctx_with_private_vm();
    let path = temp_path("audit");
    write_file(&path, b"audit");

    let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Audit, ""));
    let fd = open_read_gated(&ctx, &path).expect("audit mode must still allow");
    let _ = ctx.fd_table().close(fd);

    let report = capability_audit(vm).expect("a policy is installed");
    assert_eq!(report.mode, CapabilityMode::Audit);
    assert!(report.total_checks() >= 1, "audit mode must record the use");
    assert!(
        report.total_ungranted() >= 1,
        "with no grants, audit must price the Enforce flip"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn audit_counts_every_raw_memory_access_not_just_the_first() {
    let (ctx, vm) = ctx_with_private_vm();
    let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Audit, ""));

    for _ in 0..5 {
        gate_raw_memory(&ctx, RAW_MEMORY_UNSAFE_ADDRESS).expect("audit allows");
    }
    assert_eq!(
        recorded(vm, CapabilityKind::RawMemory),
        5,
        "Audit mode takes the Checked arm and must not memoize"
    );
}

#[test]
fn permissive_raw_memory_records_the_capability_then_goes_transparent() {
    let (ctx, vm) = ctx_with_private_vm();
    let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Permissive, ""));

    for _ in 0..64 {
        gate_raw_memory(&ctx, RAW_MEMORY_UNSAFE_ADDRESS).expect("permissive allows");
    }
    // Present in the report (so the capability is discoverable) but counted
    // once per thread — the documented trade on `gate_raw_memory`.
    assert_eq!(
        recorded(vm, CapabilityKind::RawMemory),
        1,
        "permissive raw-memory is recorded once per thread by design"
    );
}

#[test]
fn the_any_scope_gate_has_its_own_memo_and_the_same_modes() {
    // Permissive: the `RawMemory(Any)` use is recorded once per thread even
    // after `Unsafe.rawAddress` already recorded, because the memos are
    // separate slots (one verdict says nothing about the other scope).
    let (ctx, vm) = ctx_with_private_vm();
    {
        let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Permissive, ""));
        gate_raw_memory(&ctx, RAW_MEMORY_UNSAFE_ADDRESS).expect("permissive allows");
        for _ in 0..64 {
            gate_raw_memory_any(&ctx).expect("permissive allows");
        }
        assert_eq!(
            recorded(vm, CapabilityKind::RawMemory),
            2,
            "one row per scope, each recorded once on this thread"
        );
    }
    // Audit counts every call; Enforce refuses an ungranted one.
    let (ctx, vm) = ctx_with_private_vm();
    {
        let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Audit, ""));
        for _ in 0..5 {
            gate_raw_memory_any(&ctx).expect("audit allows");
        }
        assert_eq!(recorded(vm, CapabilityKind::RawMemory), 5);
    }
    let (ctx, vm) = ctx_with_private_vm();
    let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Enforce, ""));
    assert!(gate_raw_memory_any(&ctx).is_err(), "enforce refuses it");
    assert!(gate_raw_memory_any(&ctx).is_err(), "and keeps refusing it");
}

// -----------------------------------------------------------------------
// Enforce — denies, and a denial has no observable effect
// -----------------------------------------------------------------------

#[test]
fn enforce_denies_an_ungranted_read() {
    let (ctx, vm) = ctx_with_private_vm();
    let path = temp_path("denyread");
    write_file(&path, b"secret");

    let _guard =
        PolicyGuard::install(policy(vm, CapabilityMode::Enforce, "file-read:/nowhere"));
    let err = open_read_gated(&ctx, &path).expect_err("enforce must refuse");
    assert!(
        matches!(err, FdCapabilityError::Denied(_)),
        "refusal must be a capability denial, not an I/O error: {err}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_denied_open_is_refused_before_the_syscall() {
    let (ctx, vm) = ctx_with_private_vm();
    // A path that does not exist. The *raw* opener answers `NotFound` (it
    // reached `fs::File::open`); the gated opener must answer `Denied`
    // instead, which is only possible if the check ran first — so no fd
    // was reserved and no syscall was issued.
    let path = temp_path("missing");
    assert!(matches!(
        ctx.fd_table().open_read(&path).map_err(|e| e.kind()),
        Err(std::io::ErrorKind::NotFound)
    ));

    let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Enforce, ""));
    assert!(
        matches!(
            open_read_gated(&ctx, &path),
            Err(FdCapabilityError::Denied(_))
        ),
        "the refusal must precede the open, so it cannot surface as NotFound"
    );
}

#[test]
fn a_denied_write_creates_no_file() {
    let (ctx, vm) = ctx_with_private_vm();
    let path = temp_path("nocreate");
    assert!(!std::path::Path::new(&path).exists());

    let _guard =
        PolicyGuard::install(policy(vm, CapabilityMode::Enforce, "file-write:/nowhere"));
    let denied = open_write_gated(&ctx, &path, false);
    assert!(matches!(denied, Err(FdCapabilityError::Denied(_))));
    assert!(
        !std::path::Path::new(&path).exists(),
        "a refused write must not have created the file"
    );
}

#[test]
fn enforce_admits_a_granted_path_and_refuses_a_traversal_out_of_it() {
    let (ctx, vm) = ctx_with_private_vm();
    let root = std::env::temp_dir().to_string_lossy().into_owned();
    let path = temp_path("granted");
    write_file(&path, b"ok");

    let grants = format!("file-read:{root}");
    let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Enforce, &grants));

    let fd = open_read_gated(&ctx, &path).expect("a file under the granted root is admitted");
    let _ = ctx.fd_table().close(fd);

    // `..` is resolved before the prefix test, so this is not under the root.
    let escape = format!("{root}/../etc/passwd");
    assert!(
        matches!(
            open_read_gated(&ctx, &escape),
            Err(FdCapabilityError::Denied(_))
        ),
        "a traversal out of the granted root must be refused, not merely fail to open"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn read_write_open_needs_both_capabilities() {
    let (ctx, vm) = ctx_with_private_vm();
    let root = std::env::temp_dir().to_string_lossy().into_owned();
    let path = temp_path("rwboth");
    write_file(&path, b"rw");

    // Read granted, write not: the fd could still write, so this is refused.
    let read_only = format!("file-read:{root}");
    let guard = PolicyGuard::install(policy(vm, CapabilityMode::Enforce, &read_only));
    assert!(
        matches!(
            open_read_write_gated(&ctx, &path, false),
            Err(FdCapabilityError::Denied(_))
        ),
        "an fd that can write needs file-write too"
    );
    drop(guard);

    let both = format!("file-read:{root};file-write:{root}");
    let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Enforce, &both));
    let fd = open_read_write_gated(&ctx, &path, false).expect("both granted");
    let _ = ctx.fd_table().close(fd);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn enforce_denies_an_ungranted_bind_and_binds_nothing() {
    let (ctx, vm) = ctx_with_private_vm();
    let _guard = PolicyGuard::install(policy(
        vm,
        CapabilityMode::Enforce,
        "network:127.0.0.1:9000-9100",
    ));
    // Port 0 is outside the granted range, so the bind never happens.
    assert!(
        matches!(
            open_tcp_listener_gated(&ctx, "127.0.0.1:0"),
            Err(FdCapabilityError::Denied(_))
        ),
        "a bind outside the granted port range must be refused"
    );
    assert!(
        recorded(vm, CapabilityKind::Network) >= 1,
        "the refused bind must still be recorded"
    );
}

#[test]
fn enforce_denies_raw_memory_and_keeps_denying_it() {
    let (ctx, vm) = ctx_with_private_vm();
    let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Enforce, ""));
    for _ in 0..3 {
        let err = gate_raw_memory(&ctx, RAW_MEMORY_UNSAFE_ADDRESS)
            .expect_err("enforce with no raw-memory grant must refuse");
        assert!(
            format!("{err:?}").contains("SecurityException"),
            "a denial must map to SecurityException, got {err:?}"
        );
    }
    assert_eq!(
        recorded(vm, CapabilityKind::RawMemory),
        3,
        "Enforce must not memoize the verdict away"
    );
}

#[test]
fn a_raw_memory_grant_admits_the_unsafe_address_scope() {
    let (ctx, vm) = ctx_with_private_vm();
    let grants = format!("raw-memory:{RAW_MEMORY_UNSAFE_ADDRESS}");
    let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Enforce, &grants));
    gate_raw_memory(&ctx, RAW_MEMORY_UNSAFE_ADDRESS).expect("the exact scope is granted");
}

#[test]
fn spawn_and_foreign_gates_record_a_scope_that_round_trips_as_a_grant() {
    let (ctx, vm) = ctx_with_private_vm();
    let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Audit, ""));

    gate_process_spawn(&ctx, "/bin/sh").expect("audit allows");
    gate_foreign_upcall(&ctx, "com.example.Callback::onEvent").expect("audit allows");
    gate_foreign_downcall(&ctx, "snprintf").expect("audit allows");
    gate_library_load(&ctx, "ssl").expect("audit allows");
    gate_network(&ctx, "example.invalid:443").expect("audit allows");

    let report = capability_audit(vm).unwrap();
    let grants = report.suggested_grants();
    assert!(grants.contains("process-spawn:"), "{grants}");
    assert!(
        grants.contains("foreign-upcall:com.example.Callback::onEvent"),
        "{grants}"
    );
    assert!(grants.contains("foreign-downcall:snprintf"), "{grants}");
    assert!(grants.contains("library-load:ssl"), "{grants}");
    assert!(grants.contains("network:example.invalid:443"), "{grants}");

    // The derived grant list, loaded into a fresh `Enforce` set, admits
    // exactly what the run did — the gates and the report agree.
    let (ctx2, vm2) = ctx_with_private_vm();
    let _guard2 = PolicyGuard::install(policy(vm2, CapabilityMode::Enforce, &grants));
    gate_process_spawn(&ctx2, "/bin/sh").expect("derived grant must admit the spawn");
    gate_foreign_downcall(&ctx2, "snprintf").expect("derived grant must admit the downcall");
    assert!(
        gate_foreign_downcall(&ctx2, "system").is_err(),
        "the derived grant must not admit a symbol the run never used"
    );
}
