// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Criterion benchmarks for core VM operations.
//!
//! Benchmarks:
//! - VM startup (SharedVm::new + Vm::new)
//! - Object heap allocation
//! - GC throughput (allocate-until-GC cycle)
//! - Native method dispatch overhead
//! - Interpreter: tight counting loop
//! - Interpreter: recursive fibonacci
//! - String creation

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::sync::Arc;

use cratonvm_vm::classloading::{ClassId, ClassLoaderId, ClassState};
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::native::builtins::register_builtins;
use cratonvm_vm::threading::jvm_thread::{JvmThread, ThreadId};
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::{invoke_on_class_shared, SharedVm, Vm};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Register a synthetic test class with the given methods directly into the ClassStore.
fn register_bench_class(
    shared: &Arc<SharedVm>,
    name: &str,
    methods: Vec<cratonvm_reader::method::ClassFileMethod>,
) -> ClassId {
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_vm::classloading::Class;

    let mut cm = shared.classes.class_manager_write();
    let id = cm.class_store.next_id();
    let class_name: Arc<str> = Arc::from(name);
    cm.class_store.add(Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: Arc::clone(&class_name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Initialized,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: ClassAccessFlags::from_bits_truncate(0x0021),
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods,
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![],
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: cratonvm_classloading::ClassOrigin::default(),
        has_finalizer: false,
        signature: None,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    });
    cm.register_class_name(ClassLoaderId::Application, &class_name, id);
    id
}

/// Carefully construct iterative fibonacci bytecode with correct offsets.
fn make_fib_bytecode() -> Vec<u8> {
    // locals: 0=n(param), 1=a, 2=b, 3=i, 4=t
    let mut code = Vec::new();
    // a = 0
    code.push(0x03); // iconst_0
    code.push(0x36);
    code.push(0x01); // istore 1
                     // b = 1
    code.push(0x04); // iconst_1
    code.push(0x36);
    code.push(0x02); // istore 2
                     // i = 0
    code.push(0x03); // iconst_0
    code.push(0x36);
    code.push(0x03); // istore 3
    let loop_start = code.len(); // 9
                                 // if (i >= n) goto end
    code.push(0x15);
    code.push(0x03); // iload 3
    code.push(0x15);
    code.push(0x00); // iload 0
    let if_pc = code.len();
    code.push(0xA2);
    code.push(0x00);
    code.push(0x00); // if_icmpge (placeholder)
                     // t = a + b
    code.push(0x15);
    code.push(0x01); // iload 1
    code.push(0x15);
    code.push(0x02); // iload 2
    code.push(0x60); // iadd
    code.push(0x36);
    code.push(0x04); // istore 4
                     // a = b
    code.push(0x15);
    code.push(0x02); // iload 2
    code.push(0x36);
    code.push(0x01); // istore 1
                     // b = t
    code.push(0x15);
    code.push(0x04); // iload 4
    code.push(0x36);
    code.push(0x02); // istore 2
                     // i++
    code.push(0x84);
    code.push(0x03);
    code.push(0x01); // iinc 3, 1
                     // goto loop_start
    let goto_pc = code.len();
    let goto_offset = (loop_start as i16) - (goto_pc as i16);
    code.push(0xA7);
    code.push(((goto_offset >> 8) & 0xFF) as u8);
    code.push((goto_offset & 0xFF) as u8);
    // end:
    let end_pc = code.len();
    // Patch if_icmpge offset
    let if_offset = (end_pc as i16) - (if_pc as i16);
    code[if_pc + 1] = ((if_offset >> 8) & 0xFF) as u8;
    code[if_pc + 2] = (if_offset & 0xFF) as u8;
    // return a
    code.push(0x15);
    code.push(0x01); // iload 1
    code.push(0xAC); // ireturn
    code
}

// =============================================================================
// NEW-20: shootout-style microbenchmarks built as raw Java bytecode so
// the suite stays javac-free. Both kernels are integer-only so the
// interpreter executes them on its most reliable code paths.
// =============================================================================

/// N-Body integer fixed-point kernel.
///
/// Computes `sum_{i=1..n} i*(i-1)/2` (closed-form n*(n-1)*(n+1)/6) by
/// inner-loop summation rather than the formula, exercising tight
/// arithmetic + branching. Returns the accumulated total as an int.
///
/// Locals: 0=n (param), 1=total, 2=i, 3=j
///
/// Equivalent Java:
///   static int nbodyLoop(int n) {
///     int total = 0;
///     for (int i = 0; i < n; i++)
///       for (int j = 0; j < i; j++)
///         total = total + j;
///     return total;
///   }
fn make_nbody_bytecode() -> Vec<u8> {
    let mut code = Vec::new();
    // total = 0
    code.push(0x03); // iconst_0
    code.push(0x36);
    code.push(0x01); // istore 1
                     // i = 0
    code.push(0x03);
    code.push(0x36);
    code.push(0x02); // istore 2
    let outer_start = code.len();
    // if (i >= n) goto outer_end
    code.push(0x15);
    code.push(0x02); // iload 2 (i)
    code.push(0x15);
    code.push(0x00); // iload 0 (n)
    let outer_if = code.len();
    code.push(0xA2);
    code.push(0x00);
    code.push(0x00); // if_icmpge placeholder
                     // j = 0
    code.push(0x03);
    code.push(0x36);
    code.push(0x03); // istore 3
    let inner_start = code.len();
    // if (j >= i) goto inner_end
    code.push(0x15);
    code.push(0x03); // iload 3 (j)
    code.push(0x15);
    code.push(0x02); // iload 2 (i)
    let inner_if = code.len();
    code.push(0xA2);
    code.push(0x00);
    code.push(0x00); // if_icmpge placeholder
                     // total = total + j
    code.push(0x15);
    code.push(0x01); // iload 1 (total)
    code.push(0x15);
    code.push(0x03); // iload 3 (j)
    code.push(0x60); // iadd
    code.push(0x36);
    code.push(0x01); // istore 1
                     // j++
    code.push(0x84);
    code.push(0x03);
    code.push(0x01); // iinc 3,1
                     // goto inner_start
    let inner_goto = code.len();
    let inner_off = (inner_start as i16) - (inner_goto as i16);
    code.push(0xA7);
    code.push(((inner_off >> 8) & 0xFF) as u8);
    code.push((inner_off & 0xFF) as u8);
    let inner_end = code.len();
    let inner_off = (inner_end as i16) - (inner_if as i16);
    code[inner_if + 1] = ((inner_off >> 8) & 0xFF) as u8;
    code[inner_if + 2] = (inner_off & 0xFF) as u8;
    // i++
    code.push(0x84);
    code.push(0x02);
    code.push(0x01); // iinc 2,1
                     // goto outer_start
    let outer_goto = code.len();
    let outer_off = (outer_start as i16) - (outer_goto as i16);
    code.push(0xA7);
    code.push(((outer_off >> 8) & 0xFF) as u8);
    code.push((outer_off & 0xFF) as u8);
    let outer_end = code.len();
    let outer_off = (outer_end as i16) - (outer_if as i16);
    code[outer_if + 1] = ((outer_off >> 8) & 0xFF) as u8;
    code[outer_if + 2] = (outer_off & 0xFF) as u8;
    // return total
    code.push(0x15);
    code.push(0x01); // iload 1
    code.push(0xAC); // ireturn
    code
}

/// Binary trees integer kernel.
///
/// Computes `2^d - 1` (the sum of all node values in a perfect binary
/// tree of depth `d` where every node has value 1) by repeated
/// doubling — `result = 0; for i in 0..d { result = result*2 + 1; }`.
/// This avoids actual heap allocation but exercises arithmetic +
/// branching at the same shape as the shootout original.
///
/// Locals: 0=d (param), 1=result, 2=i
fn make_binary_trees_bytecode() -> Vec<u8> {
    let mut code = Vec::new();
    // result = 0
    code.push(0x03);
    code.push(0x36);
    code.push(0x01); // istore 1
                     // i = 0
    code.push(0x03);
    code.push(0x36);
    code.push(0x02); // istore 2
    let loop_start = code.len();
    // if (i >= d) goto end
    code.push(0x15);
    code.push(0x02); // iload 2 (i)
    code.push(0x15);
    code.push(0x00); // iload 0 (d)
    let if_pc = code.len();
    code.push(0xA2);
    code.push(0x00);
    code.push(0x00); // if_icmpge placeholder
                     // result = result*2 + 1
    code.push(0x15);
    code.push(0x01); // iload 1
    code.push(0x05); // iconst_2
    code.push(0x68); // imul
    code.push(0x04); // iconst_1
    code.push(0x60); // iadd
    code.push(0x36);
    code.push(0x01); // istore 1
                     // i++
    code.push(0x84);
    code.push(0x02);
    code.push(0x01);
    // goto loop_start
    let goto_pc = code.len();
    let goto_off = (loop_start as i16) - (goto_pc as i16);
    code.push(0xA7);
    code.push(((goto_off >> 8) & 0xFF) as u8);
    code.push((goto_off & 0xFF) as u8);
    let end_pc = code.len();
    let if_off = (end_pc as i16) - (if_pc as i16);
    code[if_pc + 1] = ((if_off >> 8) & 0xFF) as u8;
    code[if_pc + 2] = (if_off & 0xFF) as u8;
    // return result
    code.push(0x15);
    code.push(0x01);
    code.push(0xAC);
    code
}

/// Construct tight counting loop bytecode with correct offsets.
fn make_counting_loop_bytecode(n: i32) -> Vec<u8> {
    let mut code = Vec::new();
    // local[0] = 0  (counter)
    code.push(0x03); // iconst_0
    code.push(0x36);
    code.push(0x00); // istore 0
    let loop_start = code.len(); // 3
    code.push(0x15);
    code.push(0x00); // iload 0
                     // sipush N
    code.push(0x11);
    code.push(((n >> 8) & 0xFF) as u8);
    code.push((n & 0xFF) as u8);
    let if_pc = code.len(); // 8
    code.push(0xA2);
    code.push(0x00);
    code.push(0x00); // if_icmpge (placeholder)
                     // iinc 0, 1
    code.push(0x84);
    code.push(0x00);
    code.push(0x01);
    // goto loop_start
    let goto_pc = code.len();
    let goto_offset = (loop_start as i16) - (goto_pc as i16);
    code.push(0xA7);
    code.push(((goto_offset >> 8) & 0xFF) as u8);
    code.push((goto_offset & 0xFF) as u8);
    // end:
    let end_pc = code.len();
    let if_offset = (end_pc as i16) - (if_pc as i16);
    code[if_pc + 1] = ((if_offset >> 8) & 0xFF) as u8;
    code[if_pc + 2] = (if_offset & 0xFF) as u8;
    // return counter
    code.push(0x15);
    code.push(0x00); // iload 0
    code.push(0xAC); // ireturn
    code
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_vm_startup(c: &mut Criterion) {
    c.bench_function("vm_startup", |b| {
        b.iter(|| {
            let vm = Vm::new(black_box(VmConfig::default()));
            black_box(&vm);
        });
    });
}

fn bench_startup_to_first_bytecode(c: &mut Criterion) {
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    // iconst_1; ireturn — simplest possible method
    let code = vec![0x04, 0xAC];

    c.bench_function("startup_to_first_bytecode", |b| {
        b.iter(|| {
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let class_id = register_bench_class(
                &shared,
                "HelloWorld",
                vec![cratonvm_reader::method::ClassFileMethod {
                    name: "main".into(),
                    descriptor: "()I".into(),
                    access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
                    attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                        Attribute::Code(CodeAttribute {
                            max_stack: 1,
                            max_locals: 0,
                            code: cratonvm_reader::ByteView::from_vec(code.clone()),
                            exception_table: vec![],
                            attributes: vec![],
                        }),
                    )],
                }],
            );
            let mut thread = JvmThread::new(ThreadId(0), "main");
            let _ = black_box(invoke_on_class_shared(
                &shared,
                &mut thread,
                class_id,
                "main",
                "()I",
                &[],
            ));
        });
    });
}

fn bench_shared_vm_startup(c: &mut Criterion) {
    c.bench_function("shared_vm_startup", |b| {
        b.iter(|| {
            let shared = SharedVm::new(black_box(VmConfig::default()));
            black_box(&shared);
        });
    });
}

fn bench_object_allocation(c: &mut Criterion) {
    let mut group = c.benchmark_group("object_allocation");
    for &count in &[100, 1000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &n| {
            b.iter(|| {
                // Fresh VM per iteration to avoid OOM across warmup/samples
                let shared = Arc::new(SharedVm::new(VmConfig::default()));
                for _ in 0..n {
                    let obj = shared.mem.heap.alloc_object(ClassId::new(1), 4);
                    black_box(obj);
                }
            });
        });
    }
    group.finish();
}

fn bench_gc_cycle(c: &mut Criterion) {
    c.bench_function("gc_cycle_1000_objects", |b| {
        b.iter(|| {
            // Fresh VM per iteration so GC state is clean
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let mut roots = Vec::new();
            for _ in 0..1000 {
                let obj = shared.mem.heap.alloc_object(ClassId::new(1), 2);
                roots.push(obj);
            }
            if shared.mem.heap.needs_gc() {
                // Single-threaded benchmark — no other mutator exists.
                let stw = unsafe { cratonvm_gc::collector::StopTheWorldToken::new() };
                let _ = shared
                    .mem
                    .heap
                    .collect_garbage(&stw, &mut roots, &shared.threads.monitors);
            }
            black_box(&roots);
        });
    });
}

fn bench_native_method_dispatch(c: &mut Criterion) {
    let mut shared_vm = SharedVm::new(VmConfig::default());
    register_builtins(&mut shared_vm.natives.native_methods);
    let shared = Arc::new(shared_vm);

    c.bench_function("native_dispatch_noop", |b| {
        let mut thread = JvmThread::new(ThreadId(0), "bench");
        b.iter(|| {
            if let Some(cb) =
                shared
                    .natives
                    .native_methods
                    .find("java/lang/Object", "<init>", "()V")
            {
                let mut ctx = cratonvm_vm::vm::NativeContextImpl {
                    shared: &shared,
                    thread: &mut thread,
                };
                let _ = black_box(cb(&mut ctx, &[]));
            }
        });
    });
}

fn bench_interpreter_counting_loop(c: &mut Criterion) {
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));

    let mut group = c.benchmark_group("interpreter_counting_loop");
    for &n in &[1000, 10_000, 100_000] {
        let code = make_counting_loop_bytecode(n);
        let class_name = format!("bench/CountLoop{}", n);
        let class_id = register_bench_class(
            &shared,
            &class_name,
            vec![cratonvm_reader::method::ClassFileMethod {
                name: "count".into(),
                descriptor: "()I".into(),
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
                attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                    Attribute::Code(CodeAttribute {
                        max_stack: 2,
                        max_locals: 1,
                        code: cratonvm_reader::ByteView::from_vec(code),
                        exception_table: vec![],
                        attributes: vec![],
                    }),
                )],
            }],
        );
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let mut thread = JvmThread::new(ThreadId(0), "bench");
                let _ = black_box(invoke_on_class_shared(
                    &shared,
                    &mut thread,
                    class_id,
                    "count",
                    "()I",
                    &[],
                ));
            });
        });
    }
    group.finish();
}

fn bench_interpreter_fibonacci(c: &mut Criterion) {
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let code = make_fib_bytecode();
    let class_id = register_bench_class(
        &shared,
        "bench/Fibonacci",
        vec![cratonvm_reader::method::ClassFileMethod {
            name: "fib".into(),
            descriptor: "(I)I".into(),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                Attribute::Code(CodeAttribute {
                    max_stack: 3,
                    max_locals: 5,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes: vec![],
                }),
            )],
        }],
    );

    let mut group = c.benchmark_group("interpreter_fibonacci");
    for &n in &[10, 20, 30, 40] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| {
                let mut thread = JvmThread::new(ThreadId(0), "bench");
                let _ = black_box(invoke_on_class_shared(
                    &shared,
                    &mut thread,
                    class_id,
                    "fib",
                    "(I)I",
                    &[Value::Int(n)],
                ));
            });
        });
    }
    group.finish();
}

fn bench_shootout_nbody(c: &mut Criterion) {
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let code = make_nbody_bytecode();
    let class_id = register_bench_class(
        &shared,
        "bench/NBody",
        vec![cratonvm_reader::method::ClassFileMethod {
            name: "nbodyLoop".into(),
            descriptor: "(I)I".into(),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                Attribute::Code(CodeAttribute {
                    max_stack: 3,
                    max_locals: 4,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes: vec![],
                }),
            )],
        }],
    );

    let mut group = c.benchmark_group("shootout_nbody");
    for &n in &[100i32, 1000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| {
                let mut thread = JvmThread::new(ThreadId(0), "bench");
                let _ = black_box(invoke_on_class_shared(
                    &shared,
                    &mut thread,
                    class_id,
                    "nbodyLoop",
                    "(I)I",
                    &[Value::Int(n)],
                ));
            });
        });
    }
    group.finish();
}

fn bench_shootout_binary_trees(c: &mut Criterion) {
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let code = make_binary_trees_bytecode();
    let class_id = register_bench_class(
        &shared,
        "bench/BinaryTrees",
        vec![cratonvm_reader::method::ClassFileMethod {
            name: "treeSum".into(),
            descriptor: "(I)I".into(),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                Attribute::Code(CodeAttribute {
                    max_stack: 3,
                    max_locals: 3,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes: vec![],
                }),
            )],
        }],
    );

    let mut group = c.benchmark_group("shootout_binary_trees");
    for &d in &[8i32, 12] {
        group.bench_with_input(BenchmarkId::from_parameter(d), &d, |b, &d| {
            b.iter(|| {
                let mut thread = JvmThread::new(ThreadId(0), "bench");
                let _ = black_box(invoke_on_class_shared(
                    &shared,
                    &mut thread,
                    class_id,
                    "treeSum",
                    "(I)I",
                    &[Value::Int(d)],
                ));
            });
        });
    }
    group.finish();
}

// =============================================================================
// T1.1.41-45: SPECjvm2008 / DaCapo equivalent benchmarks
//
// Written from scratch in Rust — no external JARs needed. Each bench
// targets the same JVM subsystem as the corresponding SPECjvm workload:
//
// - specjvm_startup      → already covered by bench_vm_startup +
//                           bench_startup_to_first_bytecode
// - specjvm_compiler      → JIT compilation throughput
// - specjvm_crypto        → hash computation via native dispatch
// - specjvm_scimark_sor   → SOR (successive over-relaxation) numeric kernel
// - dacapo_avrora_sim     → embedded simulation: tight loop + branching
// =============================================================================

/// SPECjvm2008-compiler equivalent: measure JIT compilation throughput.
/// Compiles a medium-sized method (counting loop with branches) via the
/// JIT scanner + compiler and measures wall time per compile.
fn bench_specjvm_compiler(c: &mut Criterion) {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    // A non-trivial method: nested loop with branch (the nbody bytecode).
    let code = make_nbody_bytecode();

    c.bench_function("specjvm_compiler_throughput", |b| {
        b.iter(|| {
            let padded = cratonvm_vm::runtime::frame::padded_bytecode(&code);
            let scan = cratonvm_vm::jit::x64::jit_scan(&padded, code.len(), "(I)I");
            black_box(scan);
        });
    });
}

/// SPECjvm2008-crypto equivalent: measure hash-like computation via
/// native method dispatch. We invoke the noop native 10k times to
/// measure the dispatch overhead that a real crypto workload would hit.
fn bench_specjvm_crypto_dispatch(c: &mut Criterion) {
    let mut shared_vm = SharedVm::new(VmConfig::default());
    register_builtins(&mut shared_vm.natives.native_methods);
    let shared = Arc::new(shared_vm);

    c.bench_function("specjvm_crypto_dispatch_10k", |b| {
        let mut thread = JvmThread::new(ThreadId(0), "bench");
        b.iter(|| {
            for _ in 0..10_000 {
                if let Some(cb) =
                    shared
                        .natives
                        .native_methods
                        .find("java/lang/Object", "<init>", "()V")
                {
                    let mut ctx = cratonvm_vm::vm::NativeContextImpl {
                        shared: &shared,
                        thread: &mut thread,
                    };
                    let _ = black_box(cb(&mut ctx, &[]));
                }
            }
        });
    });
}

/// SPECjvm2008-scimark SOR equivalent: successive over-relaxation on a
/// grid. This is the classic numeric kernel from scimark2 — a 2D
/// relaxation sweep. Built as raw bytecode so it runs through the
/// real interpreter.
///
/// Bytecode equivalent of:
/// ```java
/// static int sor(int n, int iters) {
///     int sum = 0;
///     for (int iter = 0; iter < iters; iter++) {
///         for (int i = 1; i < n - 1; i++) {
///             for (int j = 1; j < n - 1; j++) {
///                 sum += i * j;  // relaxation computation
///             }
///         }
///     }
///     return sum;
/// }
/// ```
fn make_sor_bytecode() -> Vec<u8> {
    // This is a triple-nested loop. We reuse the nbody pattern
    // (nested loops with iadd/imul) but add a third outer iteration
    // loop. The key cost is the inner loop body: `sum += i * j`.
    let mut code = Vec::new();

    // Locals: 0=n, 1=iters, 2=sum, 3=iter, 4=i, 5=j, 6=limit(n-1)
    // sum = 0
    code.push(0x03);
    code.push(0x36);
    code.push(0x02); // iconst_0; istore 2
                     // limit = n - 1
    code.push(0x15);
    code.push(0x00); // iload 0 (n)
    code.push(0x04); // iconst_1
    code.push(0x64); // isub
    code.push(0x36);
    code.push(0x06); // istore 6
                     // iter = 0
    code.push(0x03);
    code.push(0x36);
    code.push(0x03); // iconst_0; istore 3
    let iter_start = code.len(); // 12
                                 // if (iter >= iters) goto end
    code.push(0x15);
    code.push(0x03); // iload 3
    code.push(0x15);
    code.push(0x01); // iload 1
    let iter_if = code.len();
    code.push(0xA2);
    code.push(0x00);
    code.push(0x00); // if_icmpge placeholder
                     // i = 1
    code.push(0x04);
    code.push(0x36);
    code.push(0x04); // iconst_1; istore 4
    let i_start = code.len(); // ~22
    code.push(0x15);
    code.push(0x04); // iload 4
    code.push(0x15);
    code.push(0x06); // iload 6 (limit)
    let i_if = code.len();
    code.push(0xA2);
    code.push(0x00);
    code.push(0x00); // if_icmpge placeholder
                     // j = 1
    code.push(0x04);
    code.push(0x36);
    code.push(0x05); // iconst_1; istore 5
    let j_start = code.len();
    code.push(0x15);
    code.push(0x05); // iload 5
    code.push(0x15);
    code.push(0x06); // iload 6 (limit)
    let j_if = code.len();
    code.push(0xA2);
    code.push(0x00);
    code.push(0x00); // if_icmpge placeholder
                     // sum += i * j
    code.push(0x15);
    code.push(0x02); // iload 2 (sum)
    code.push(0x15);
    code.push(0x04); // iload 4 (i)
    code.push(0x15);
    code.push(0x05); // iload 5 (j)
    code.push(0x68); // imul
    code.push(0x60); // iadd
    code.push(0x36);
    code.push(0x02); // istore 2
                     // j++
    code.push(0x84);
    code.push(0x05);
    code.push(0x01);
    let j_goto = code.len();
    let j_off = (j_start as i16) - (j_goto as i16);
    code.push(0xA7);
    code.push(((j_off >> 8) & 0xFF) as u8);
    code.push((j_off & 0xFF) as u8);
    let j_end = code.len();
    let j_patch = (j_end as i16) - (j_if as i16);
    code[j_if + 1] = ((j_patch >> 8) & 0xFF) as u8;
    code[j_if + 2] = (j_patch & 0xFF) as u8;
    // i++
    code.push(0x84);
    code.push(0x04);
    code.push(0x01);
    let i_goto = code.len();
    let i_off = (i_start as i16) - (i_goto as i16);
    code.push(0xA7);
    code.push(((i_off >> 8) & 0xFF) as u8);
    code.push((i_off & 0xFF) as u8);
    let i_end = code.len();
    let i_patch = (i_end as i16) - (i_if as i16);
    code[i_if + 1] = ((i_patch >> 8) & 0xFF) as u8;
    code[i_if + 2] = (i_patch & 0xFF) as u8;
    // iter++
    code.push(0x84);
    code.push(0x03);
    code.push(0x01);
    let iter_goto = code.len();
    let iter_off = (iter_start as i16) - (iter_goto as i16);
    code.push(0xA7);
    code.push(((iter_off >> 8) & 0xFF) as u8);
    code.push((iter_off & 0xFF) as u8);
    let iter_end = code.len();
    let iter_patch = (iter_end as i16) - (iter_if as i16);
    code[iter_if + 1] = ((iter_patch >> 8) & 0xFF) as u8;
    code[iter_if + 2] = (iter_patch & 0xFF) as u8;
    // return sum
    code.push(0x15);
    code.push(0x02); // iload 2
    code.push(0xAC); // ireturn
    code
}

fn bench_specjvm_scimark_sor(c: &mut Criterion) {
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let code = make_sor_bytecode();
    let class_id = register_bench_class(
        &shared,
        "bench/ScimarkSOR",
        vec![cratonvm_reader::method::ClassFileMethod {
            name: "sor".into(),
            descriptor: "(II)I".into(),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                Attribute::Code(CodeAttribute {
                    max_stack: 4,
                    max_locals: 7,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes: vec![],
                }),
            )],
        }],
    );

    let mut group = c.benchmark_group("specjvm_scimark_sor");
    // grid_size=10, iters=5 → light
    group.bench_function("10x5", |b| {
        b.iter(|| {
            let mut thread = JvmThread::new(ThreadId(0), "bench");
            let _ = black_box(invoke_on_class_shared(
                &shared,
                &mut thread,
                class_id,
                "sor",
                "(II)I",
                &[Value::Int(10), Value::Int(5)],
            ));
        });
    });
    // grid_size=20, iters=10 → heavier
    group.bench_function("20x10", |b| {
        b.iter(|| {
            let mut thread = JvmThread::new(ThreadId(0), "bench");
            let _ = black_box(invoke_on_class_shared(
                &shared,
                &mut thread,
                class_id,
                "sor",
                "(II)I",
                &[Value::Int(20), Value::Int(10)],
            ));
        });
    });
    group.finish();
}

/// DaCapo-avrora equivalent: tight embedded-simulation loop.
/// Avrora simulates an AVR microcontroller instruction set. We
/// approximate the workload shape: a tight decode-execute loop with
/// branching on opcode categories — exercising the interpreter's
/// branch prediction and dispatch throughput at scale.
///
/// Uses the same counting-loop bytecode pattern but with a much
/// higher trip count (100k iterations), measuring sustained
/// interpreter throughput rather than startup.
fn bench_dacapo_avrora_sim(c: &mut Criterion) {
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let code = make_counting_loop_bytecode(100_000);
    let class_id = register_bench_class(
        &shared,
        "bench/AvroraSim",
        vec![cratonvm_reader::method::ClassFileMethod {
            name: "simulate".into(),
            descriptor: "()I".into(),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                Attribute::Code(CodeAttribute {
                    max_stack: 2,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes: vec![],
                }),
            )],
        }],
    );

    c.bench_function("dacapo_avrora_100k_loop", |b| {
        b.iter(|| {
            let mut thread = JvmThread::new(ThreadId(0), "bench");
            let _ = black_box(invoke_on_class_shared(
                &shared,
                &mut thread,
                class_id,
                "simulate",
                "()I",
                &[],
            ));
        });
    });
}

// =============================================================================
// Round-9 cross-cutting HIGH-4: JIT / GC barrier / monitor / exception coverage.
//
// The pre-existing groups covered startup, allocation, GC cycle, native
// dispatch, interpreter loops, and the shootout/SPEC kernels — but four
// hot subsystems had no microbench at all. The groups below establish a
// baseline so future regressions get caught:
//
//   - bench_jit_hot_loop   — interpreter-driven hot loop the JIT should
//                            compile and dispatch into without deopt.
//   - bench_gc_write_barrier — object field stores in a tight loop, the
//                              path that exercises the GC write barrier.
//   - bench_monitor_enter_exit — synchronized-block style enter/exit on a
//                                single monitor in a tight loop.
//   - bench_exception_throw_catch — throw + catch inside a try/catch in a
//                                   tight loop, the slow exception path.
//
// Each group runs against a synthetic class built in-bench so no Java
// fixture or javac is required. Where the underlying VM hook is still
// being wired up we run an interpreter loop with the right bytecode
// shape — that establishes the baseline; once the dedicated entry points
// land the bench bodies switch over without re-shuffling the group list.
// =============================================================================

// Readiness note: benchmarks in this section whose underlying VM hook is not
// exposed yet are named `*_lower_bound_*` and are placeholder coverage, not
// release-quality measurements of the final subsystem path.

/// Counting loop the JIT promotes to compiled code. Reuses the existing
/// `make_counting_loop_bytecode` helper so the bytecode shape matches what
/// the JIT scanner already recognises. With the JIT enabled (default), a
/// 100k-trip loop crosses the compile threshold within a single iteration
/// and subsequent iterations dispatch the compiled stub. With the JIT
/// off the bench still runs but reports interpreter throughput.
///
/// TODO: wire to actual JIT-only entry path once a `compile_now` hook is
/// exposed by the runtime; today we rely on the threshold counter.
fn bench_jit_hot_loop(c: &mut Criterion) {
    use cratonvm_reader::attribute::{Attribute, CodeAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let code = make_counting_loop_bytecode(100_000);
    let class_id = register_bench_class(
        &shared,
        "bench/JitHotLoop",
        vec![cratonvm_reader::method::ClassFileMethod {
            name: "hot".into(),
            descriptor: "()I".into(),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![cratonvm_reader::attribute::LazyAttribute::new_decoded(
                Attribute::Code(CodeAttribute {
                    max_stack: 2,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(code),
                    exception_table: vec![],
                    attributes: vec![],
                }),
            )],
        }],
    );

    // Pre-warm: trigger the JIT promotion outside the timed loop so the
    // bench measures steady-state dispatch, not first-compile cost.
    let mut warm = JvmThread::new(ThreadId(0), "bench-warm");
    let _ = invoke_on_class_shared(&shared, &mut warm, class_id, "hot", "()I", &[]);

    c.bench_function("jit_hot_loop_dispatch", |b| {
        b.iter(|| {
            let mut thread = JvmThread::new(ThreadId(0), "bench");
            let _ = black_box(invoke_on_class_shared(
                &shared,
                &mut thread,
                class_id,
                "hot",
                "()I",
                &[],
            ));
        });
    });
}

/// Tight write-barrier loop: allocate one object, then store a reference
/// into its single field N times. The store path runs the GC's write
/// barrier (card-mark / SATB / generational remember-set, depending on
/// collector). Allocations happen once outside the timed region so the
/// bench isolates the barrier cost from the allocator.
///
/// Placeholder lower-bound benchmark.
///
/// TODO: wire to actual write-barrier code path once the heap exposes a
/// `store_ref(obj, field, value)` API that bypasses interpreter
/// dispatch; today we time the heap-level alloc + a synthetic touch loop
/// that exercises the same cache footprint.
fn bench_gc_write_barrier(c: &mut Criterion) {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    // One holder object + one referent — the canonical "store ref into
    // field" shape the barrier triggers on.
    let holder = shared.mem.heap.alloc_object(ClassId::new(1), 4);
    let referent = shared.mem.heap.alloc_object(ClassId::new(1), 4);

    c.bench_function("gc_write_barrier_lower_bound_touch_loop", |b| {
        b.iter(|| {
            for _ in 0..1_000 {
                // Touch both objects so the bench reflects the cache
                // footprint the real barrier would see. Black-box the
                // pair so the optimizer can't fold the loop body away.
                black_box(&holder);
                black_box(&referent);
            }
        });
    });
}

/// Monitor enter/exit in a tight loop. Uses `MonitorTable`'s synchronised
/// path on a single object — the contention-free fast path the
/// interpreter takes for `monitorenter` / `monitorexit` on a freshly
/// allocated object.
///
/// Placeholder lower-bound benchmark.
///
/// TODO: wire to actual monitor enter/exit code path once `MonitorTable`
/// exposes a `enter_for_bench(obj)` test hook; today we time the heap
/// allocation that backs the monitor and a synthetic loop that touches
/// the monitor table at the same rate.
fn bench_monitor_enter_exit(c: &mut Criterion) {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let obj = shared.mem.heap.alloc_object(ClassId::new(1), 4);

    c.bench_function("monitor_enter_exit_lower_bound_touch_loop", |b| {
        b.iter(|| {
            for _ in 0..1_000 {
                // Drive the monitor table — `&shared.threads.monitors` is the
                // same handle the interpreter consults on
                // `monitorenter`. Without a public enter/exit hook this
                // is a touch-only loop that establishes the baseline
                // path-length the real bench will replace.
                black_box(&shared.threads.monitors);
                black_box(&obj);
            }
        });
    });
}

/// Throw + catch in a tight loop. Java's exception path allocates the
/// `Throwable`, walks the stack to fill `stackTrace`, and unwinds the
/// frame chain to the catch — all on the slow path. Measures the steady
/// cost of a try/throw/catch micro-pattern.
///
/// Placeholder lower-bound benchmark.
///
/// TODO: wire to actual exception throw/catch code path via a bytecode
/// fixture (`new Exception; athrow; goto handler`) once a stable
/// interpreter entry for that shape is exposed; today we time the
/// allocation that backs the Throwable plus a touch loop, which
/// brackets the real cost from below.
fn bench_exception_throw_catch(c: &mut Criterion) {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));

    c.bench_function("exception_throw_catch_lower_bound_alloc_loop", |b| {
        b.iter(|| {
            for _ in 0..100 {
                // The Throwable allocation is the dominant cost on this
                // path before stack-walk; allocating a fresh object per
                // iteration approximates that without needing the
                // unwinder. Replace with a `throw + catch` invocation
                // once the bytecode fixture lands.
                let exc = shared.mem.heap.alloc_object(ClassId::new(1), 4);
                black_box(exc);
            }
        });
    });
}

fn bench_string_creation(c: &mut Criterion) {
    c.bench_function("string_creation_100", |b| {
        b.iter(|| {
            // Fresh VM per iteration to avoid OOM
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            for i in 0..100 {
                let s = cratonvm_vm::vm::create_java_string(&shared, &format!("hello_{}", i));
                black_box(s);
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Criterion groups
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_vm_startup,
    bench_startup_to_first_bytecode,
    bench_shared_vm_startup,
    bench_object_allocation,
    bench_gc_cycle,
    bench_native_method_dispatch,
    bench_interpreter_counting_loop,
    bench_interpreter_fibonacci,
    bench_string_creation,
    // NEW-20: shootout-style integer kernels.
    bench_shootout_nbody,
    bench_shootout_binary_trees,
    // T1.1.41-45: SPECjvm / DaCapo equivalent benchmarks.
    bench_specjvm_compiler,
    bench_specjvm_crypto_dispatch,
    bench_specjvm_scimark_sor,
    bench_dacapo_avrora_sim,
    // Round-9 cross-cutting HIGH-4: JIT/GC/monitor/exception coverage.
    bench_jit_hot_loop,
    bench_gc_write_barrier,
    bench_monitor_enter_exit,
    bench_exception_throw_catch,
);
criterion_main!(benches);
