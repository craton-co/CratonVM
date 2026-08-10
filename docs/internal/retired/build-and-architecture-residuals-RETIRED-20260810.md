# Eight build / architecture residuals — RETIRED 2026-08-10

Closing record for the eight items left standing after `a01ccc442` ("the build
break that reached the shipped cdylib, and eleven defects under it"). All eight
are fixed on `fix/build-residuals-20260809`; this page exists so the reasoning
survives the branch.

Read this top-down as a list of *what was actually wrong*, not as a changelog.
Several of the eight turned out to be a different shape than the one-line
summary suggested, and those are the interesting rows.

---

## 1. `--all-targets` was red — 664 errors in `#[cfg(test)]` modules

`cargo check --workspace` was green and `cargo check --workspace --all-targets`
was not, so `cargo test`, `cargo clippy --all-targets` and coverage were all
dead for `cratonvm-native-builtins`. Same root cause as the parent commit: the
`alloc_concurrent_synthetic -> try_alloc_concurrent_synthetic` funnel migration
(WIP commit `532925266`) rewrote 1,904 call sites and left the rest.

Fixed by driving the rewrite from **rustc's own byte spans**, not from a regex,
in three shapes:

| shape | count | fix |
|---|---:|---|
| `expr?` in a `#[test]` returning `()` | 403 | `?` becomes `.unwrap()` |
| a local bound to a now-fallible call, flagged at its *uses* | 175 uses / 114 bindings | unwrap at the **binding** |
| `?` applied to something that was never a `Result` | 7 | drop the operator |

Unwrapping at the binding rather than at each use is the difference between 114
edits and 175; a local rebound in forty test bodies needs forty fixes, not one,
so the nearest `let` above *each* flagged use is what gets the `.unwrap()`.

Three sites needed a decision rather than a rewrite, and each is a case where
the mechanical answer was wrong:

* `tls.rs`'s `ssl_context_get_instance_refuses_an_unsupported_protocol` asserts
  `result.is_err()`. The migration's `?` had turned the refusal the test exists
  to observe into an early return; `.unwrap()` would have turned it into a
  panic. The call is left un-unwrapped.
* `net_phase_e.rs`'s scripted-publisher mock answers `Option<MethodCallResult>`,
  where `?` means "no such method", not "the allocation failed". A refused
  allocation now answers `Some(Err(..))`, matching the sibling three lines below.
* `build_object_stream_class` is `Result<Option<_>, _>` and the `Option` is a
  real answer (a non-`Serializable` class), so its tests unwrap the `Result` and
  keep asserting on the `Option`.

Verified: `cargo check --workspace --all-targets` green; `cargo test -p
cratonvm-native-builtins --lib` **3,384 passed, 0 failed**.

Building the `synthetic-jdk` feature later (see §6) found two more of the same
migration's leftovers in code that only that feature compiles.

---

## 2. `-Xverify:all` parsed and did nothing

`XverifyMode::All` was written at CLI parse time and read by **nothing**. The
dispatcher branches on `skip_verification` (which only `none` sets), and boot
skipping was decided by `verifier_skip_eligible`, a predicate that takes no
configuration. Even `bytecode_verifier::verify_bytecode_strict` — documented as
this flag's entry point — had no production caller.

The doc called wiring it up "a product decision with real regression surface".
That is true of *turning it on by default*, which nothing here does. It is not
an argument for a flag that lies.

Wired as `ClassManager::strict_verification`, set from `VmConfig::xverify_mode`
in `vm_init` before any class is loaded — per-manager, not process-global, so
two VMs in one process can differ. It withdraws three shortcuts:

* `class_is_bootstrap_trusted` stops earning the lenient branch-target path, so
  the JDK image is checked against the spec-literal JVMS §4.10.1 rule;
* `defer_loader_sensitive_pass3` stops withholding the Pass-3 type-state verdict
  for user-loader classes — every Spring / Tomcat / H2 application class. That
  deferral is how a `multianewarray` with more dimensions than its descriptor
  has brackets reached the interpreter unchallenged;
* `verifier_skip_eligible` stops skipping link-time Pass 2 for bootstrap classes.

`Remote` stays the default.

The test that matters for a flag like this is a *disagreement* test: a
bootstrap-trusted class with a frameless branch target must be accepted under
`remote` and rejected under `all`. If the two ever agree about it the flag is
inert again, and the test says so. It asserts the fixture is in the trusted
population first, so it cannot pass vacuously.

---

## 3. VM creation blanket-wiped four GC registries

`reset_loader_singletons`, called from `Vm::new`, wiped `loader_pin`,
`mirror_pin`, `metadata_pin` and `jit_activation` — **sixteen lines below** the
note explaining why the two registries above them are deliberately not wiped
there. A Rust test binary runs `#[test]`s on several threads, so a second VM's
construction was reaching into a live VM's state.

Losing a pin is the dangerous direction. All three pin registries only *extend*
reachability from an already-live object, so a wiped row is a **missing root**:
the marker collects a loader or a `Class` mirror that is still reachable.

Each row now carries the `vm_identity` that wrote it, and
`forget_vm_loader_singletons` drops that VM's rows at teardown — which is also
the *right* time, since the addresses in them belong to the heap that is going
away and were previously left dangling until the next VM happened to be created.

The keys did **not** change, so no collector changes:

* `mirror_pin` / `metadata_pin` are keyed by heap address, unique across every
  live VM, so the marker's read path already cannot confuse two VMs.
* `loader_pin` is keyed by `class_id`, which restarts at 0 per VM. Two live VMs
  can collide and the row resolves to whichever wrote last — handing the marker
  an address in another heap, which its own bounds check rejects. That is an
  over-approximation, the safe direction, and it is why threading a VM identity
  through `gen_heap` / `g1` / `zgc` buys nothing over it.

`jit_activation` needed no VM key and no wipe at all: a slot is owned by the
thread running the compiled frame and cleared by that thread's own `exit`, so a
foreign wipe was the only way to lose a record whose frame was still running. A
record stranded by a thread that died mid-frame over-retains one loader for one
collection, and `vm::memory::roots` already filters every id it reads back
through `defining_loader_for(vm_identity, ..)`.

`set_metadata_weak_mode(false)` carried the same bug in miniature and now drops
only the calling VM's rows. There is deliberately no `clear_loader_pins` /
`clear_mirror_pins` any more, so the wipe cannot come back.

---

## 4. JIT `checkcast` accepted a same-named class from another loader

The premise in the code was "CratonVM has a flat global class store". The
dictionary has been keyed by `(ClassLoaderId, name)` for some time, so the
justification was stale — but the *behaviour* was not merely undocumented, it
was a real hole: a compiled site carried only the target class NAME, so
`jit_typecheck_resolve` re-resolved it at run time, could land on the wrong
copy, and covered for that with `is_assignable_to_name`, a loader-blind walk
that accepts **either** copy.

The compiler already had the answer and threw it away. `cp_new_resolver`
resolves each site's `CONSTANT_Class` entry through the compiling class's loader
and returns `JitNewSite::Resolved { class_id, .. }`; all three producers read it
only as a yes/no "is it loaded".

Sites are now interned by `(name, resolved ClassId)`. Two loaders' copies get
two distinct `(ptr, len)` identities — which also un-conflates the per-thread
memos keyed on that pointer — and `typecheck_target_for_site` hands the id back
to the helper, which answers by identity and does **not** run the name walk.

Two guards, because a `ClassId` is a per-VM number in a process-wide table: the
reader confirms the id still names this site's class in *its* class manager
(which also covers a redefinition retiring it), and array receivers are excluded
so the descriptor-based and `Object`/`Serializable`/`Cloneable` carve-outs at
either end of the function keep their say.

`is_assignable_to_name` survives, now reachable only from a site whose target
was not loaded at compile time. Its doc comment says that instead of citing the
flat store.

---

## 5. Two heap-sizing implementations disagreed

`vm-cli`'s `ergonomic_default_max_heap`: basis `min(physical RAM, cgroup limit)`,
cap 4 GiB, floor 256 MB, two env knobs.
`SharedVm::new`'s `suggested_default_max_heap`: basis the cgroup limit alone,
cap 8 GiB, floor 16 MiB, no host-RAM basis, no knobs.

So one container got two different default heaps depending on which entry point
started the VM, and only one of the two caps carried the reasoning that
motivates having a cap at all (the generational heap commits its arenas
eagerly, so an uncapped quarter-of-RAM default charges that much commit per
process).

The clamp, constants, knobs and `physical_ram_bytes` all move into
`vm::runtime::container`; the launcher calls them. A test asserts the two agree
across six container sizes.

One difference remains and is now deliberate: an **uncontained** embedder still
gets the fixed 256 MB. The launcher owns the process it sizes; a library that
commits eagerly does not get to claim a quarter of the machine inside an
application's address space. A cgroup limit *is* an explicit statement about the
process's budget, which is why that case is sized and the bare-metal case is not.

---

## 6. `vm/src/vm.rs` was 76,799 lines

Of which the production content — module docs, four `mod` declarations, four
`pub use`s — was **60**. Everything from line 84 to the end was one
`#[cfg(all(test, feature = "synthetic-jdk"))] mod tests` holding 1,528 tests
that no default build compiles.

Moved verbatim to `vm/src/vm/tests.rs`. The extraction deliberately did **not**
re-indent: the module's raw-string fixtures contain Java sources that several
tests assert on, and shifting them four columns to make the new file look native
would edit those assertions. The extra indentation level is an artifact and says
so in the file's header.

The tests were **not** split further into themed files. They share helpers
defined at arbitrary points in the module, so a themed split is a visibility
refactor of 72k lines rather than a file move — a separate change from making
the orchestrator readable.

Building `synthetic-jdk` to verify the move found that feature's build already
red, with two more funnel-migration leftovers: `epoch_day_to_ymd` widened to
`Result` although every path ends in `Ok` (narrowed back), and two
`ctx.get_field(..)?` where `get_field` answers `Value` (operator dropped).

---

## 7. `lock_discipline_ratchet` was red — 436 against a baseline of 432

Four raw locks had landed in `native-builtins` since the freeze. The gate's own
instruction is "do NOT raise the baseline", so four were converted instead:
`ds_side_table`, `ds_peer_table`, `ssc_side_table` (`net_phase_e.rs`) and
`pending_connect_sockets` (`phases_late/ssl_security.rs`), all
`LockLevel::Scratch`, each earned by reading every acquisition site — no `ctx`
call under any guard.

Two of the four newcomers were deliberately **not** converted.
`boot_layer_memo` and `p60_current_handle_memo` hold their guard across
`ctx.add_global_root`, the re-entrant shape a level exists to forbid. Giving
them a level would encode the false hierarchy the ratchet's own doc calls more
dangerous than none. They need their publish restructured first, and are now
named in the §A6 backlog so the next reader does not re-derive it.

---

## 8. `class_name_of_id` allocated at 761 call sites

`NativeContext::class_name_of_id` answers `Option<String>` and the VM implements
it as a `class_manager` read lock plus `c.name.to_string()`. `Class::name` is
already an `Arc<str>`, so the copy is pure waste at the shape almost every call
site uses: compare to a literal, drop.

`class_name_arc_of_id` returns a clone of the `Arc` — one refcount increment, no
allocation, no copy. Default-implemented in terms of `class_name_of_id` so all
six test mocks compile unchanged; the VM overrides it.

151 call sites converted — the whole `…class_name_of_id(x).as_deref()` idiom
across the workspace. That shape is a drop-in (`Option<Arc<str>>::as_deref()`
yields the same `Option<&str>`), so the conversion is mechanical *and*
compiler-checked, which is what makes converting the idiom rather than the sites
safe at this scale. The 144 `.unwrap_or_default()` sites genuinely want an owned
`String` and are left alone.

`native-collections`'s `class_name_rc` was the hand-rolled dodge the doc named.
Its miss path allocated a `String` purely to copy it into an `Rc` and now builds
the `Rc` straight from the `Arc`'s bytes. **The memo stays**, and its header now
says why: it also removes the `class_manager` read lock, which this does not.

**Not measured as a benchmark delta, and not claimed as one.** What it removes
is exact and countable — one heap allocation and one memcpy per call at 151
sites — but the sites are spread across classification predicates that no single
CratonBench row isolates, and asserting a number a rerun would not reproduce is
worse than saying this.

---

## Verification

* `cargo check --workspace --all-targets` — green (was 664 errors).
* `cargo check -p cratonvm-vm --all-targets --features synthetic-jdk` — green
  (was 3 errors before this branch touched it).
* `cargo test -p cratonvm-native-builtins --lib` — 3,384 passed, 0 failed.
* `cargo test -p cratonvm-types --lib` — 536 passed, 0 failed.
* `cargo test -p cratonvm-jit --lib` — 1,972 passed, 0 failed.
* `cargo test -p cratonvm-classloading --lib` — 783 passed, 0 failed.
* `cargo test -p cratonvm-cli --bins` — 127 passed, 0 failed.
* `cargo test -p cratonvm-native-builtins --test lock_discipline_ratchet` —
  432 raw, green.
* `cargo test -p cratonvm-vm --lib --features synthetic-jdk` — 3,979 passed,
  1 failed (see below). That suite had not been runnable at all: the feature's
  own build was red on `dev`.
* `cargo test --workspace --no-fail-fast` over the tree with `origin/dev`
  merged in — 13 failures, **all of which reproduce identically on pristine
  `origin/dev`** and none of which this branch touches.

### The reds that were already there

Attribution matters more than the count, so each was reproduced on a detached
worktree at `origin/dev` before being dismissed.

**13 JIT intrinsic integration failures** — `intrinsic_arraycopy` (1),
`intrinsic_string_access` (6), `intrinsic_string_narrow_oops` (1),
`intrinsic_string_search` (5). Same names, same counts, on `origin/dev` with no
part of this branch present. These tests hand-build heap objects from restated
layout constants (`HEADER_SIZE`, `SLOT_SIZE`, a `kind` byte at offset 4), and
`d7965af6a feat: HEADER_SIZE 24 -> 16` moved that layout underneath them —
offset 4..8 is `shape` now, not a kind byte. A fixture that restates a layout
constant is the failure mode this repository has recorded before; these are
instances of it, and fixing them is its own change.

**`vm::tests::server_socket_lifecycle`** binds `0.0.0.0:8080`, and on this host
port 8080 is held by a release binary from an unrelated worktree
(`CratonVM-symlink-20260809`). Not asserted — measured: `dev`'s own new
`probes/ServerSocketPortContentionProbe.java`, run on Temurin 25.0.3 with the
same occupant present, answers

```text
wildcard reuse=false : java.net.BindException: Address already in use: bind
wildcard reuse=true  : java.net.BindException: Address already in use: bind
loopback reuse=false : OK localAddr=/127.0.0.1:8080
control port=18087   : OK localAddr=/0.0.0.0:18087
```

so HotSpot refuses the same wildcard bind this test performs. The control row
confirms the run measures contention and not something else.
