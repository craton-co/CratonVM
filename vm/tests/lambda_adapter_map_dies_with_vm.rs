// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A VM's lambda thunks, and the compiled code they reach, die with the VM.
//!
//! An embedder (`libcratonvm`, `cratonvm-embed`) can create and drop VMs in a
//! loop on ONE thread. Two things used to keep each dropped VM's code mapped
//! for the life of the process:
//!
//! * the lambda-adapter thunk map was a process `OnceLock`, so every thunk,
//!   and through its callee roots the impl body it jumps into, stayed in it.
//!   It is now owned by each VM's `JitCache`;
//! * the calling thread's dispatch memos and lambda site caches own compiled
//!   bodies, keyed by VM identity, and nothing emptied them for a VM that was
//!   gone. The callers kept there own the thunk through their inline caches.
//!   `Drop for Vm` now releases them (`release_thread_jit_memos_for_vm`).
//!
//! See `docs/internal/jit/t1-jitic-lambda-thunk-map-outlives-its-vm-20261025.md`.
//!
//! This creates and drops several VMs on the test thread, each running the
//! lambda-dense `LambdaJitTierUp.warmChecksum`, and asserts after each drop:
//!
//! * the VMs built at least one thunk between them (else nothing below says
//!   anything), and each new VM's map starts empty;
//! * its `SharedVm` is gone (a stopping worker may hold it for a moment);
//! * every thunk it built is gone (`Arc` count 0);
//! * no compiled body is left registered at all, so committed JIT code does
//!   not grow from one VM to the next.
//!
//! Its own file, and so its own process: the counters are process-wide.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const VMS: usize = 6;

fn test_resources_dir() -> String {
    format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR"))
}

fn real_jdk_vm() -> Option<Vm> {
    let java_home = cratonvm_vm::config::resolve_java_home_public(None)?;
    let mut config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    config.use_synthetic_jdk = false;
    Some(Vm::new(
        config.with_java_home(java_home.to_string_lossy().into_owned()),
    ))
}

/// Wait, bounded, for `done`: a stopping worker can hold the last
/// `Arc<SharedVm>` for a moment, and a released body leaves through the
/// retirement queue.
fn wait_for(done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !done() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn every_thunk_dies_with_its_vm() {
    if !std::path::Path::new(&format!(
        "{}/cratonvm/LambdaJitTierUp.class",
        test_resources_dir()
    ))
    .exists()
    {
        eprintln!("Skipping: .class files not available (javac not on PATH?)");
        return;
    }
    if cratonvm_vm::config::resolve_java_home_public(None).is_none() {
        eprintln!(
            "Skipping: needs a class library (CRATONVM_JAVA_HOME / JAVA_HOME / `java` on PATH)."
        );
        return;
    }
    let expected: i32 = (0..400_000i32).map(|i| ((i & 0xFF) + 3) * 2).sum();
    let mut built_total = 0usize;
    let mut committed_after_first = None;
    for round in 0..VMS {
        let mut vm = real_jdk_vm().expect("class library resolved above");
        assert!(
            vm.shared.jit.jit_cache.lambda_adapters().is_empty(),
            "VM {round} starts with thunks it never built: the map is shared across VMs"
        );
        let result = vm.invoke("cratonvm/LambdaJitTierUp", "warmChecksum", "()I", &[]);
        assert_eq!(result.ok().flatten(), Some(Value::Int(expected)));
        let thunks = vm.shared.jit.jit_cache.lambda_adapters().weak_thunks();
        built_total += thunks.len();
        let shared = std::sync::Arc::downgrade(&vm.shared);
        drop(vm);

        wait_for(|| shared.strong_count() == 0);
        assert_eq!(
            shared.strong_count(),
            0,
            "VM {round}: its SharedVm (and so its JitCache and thunk map) outlived it by 20 s"
        );
        wait_for(|| cratonvm_jit::live_jit_entry_owners().is_empty());
        let live = cratonvm_jit::live_jit_entry_owners();
        let committed =
            cratonvm_jit::COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "VM {round}: {} thunk(s); {} compiled bodies left; committed JIT code {committed} bytes",
            thunks.len(),
            live.len(),
        );
        let alive = thunks.iter().filter(|t| t.strong_count() > 0).count();
        assert_eq!(
            alive,
            0,
            "VM {round}: {alive} of {} thunks outlived their VM",
            thunks.len()
        );
        assert!(
            live.is_empty(),
            "VM {round}: compiled bodies outlived their VM (entry, label, holders): {live:?}"
        );
        let first = *committed_after_first.get_or_insert(committed);
        assert!(
            committed <= first,
            "VM {round}: committed JIT code grew from {first} to {committed} bytes"
        );
    }
    assert!(
        built_total > 0,
        "no VM built a single lambda thunk, so this test proved nothing \
         (see lambda_jit_adapter_engagement_tests)"
    );
}
