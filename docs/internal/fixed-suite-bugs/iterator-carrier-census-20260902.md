# The iterator-carrier census

`native-collections` binds `native_al_itr_*` to a table of
(collection class → its real iterator class) pairs, `VALUES_ITR_CARRIERS`, so a
family hands out the class HotSpot hands out, minted with three snapshot fields
past its declared ones and the fail-fast door reading its `expectedModCount`.

Three comments in that file defer work to "a census": the dormant
`ArrayDeque$Itr` rows are "recorded as a retirement candidate rather than
deleted here, because deleting registrations is the shadow-retirement lane's
edit and wants its own census". This is that census.

## Method

Two halves, because neither half is sufficient.

**Static.** Every `java/util/*$*Itr*` / `*Iterator*` name in `native-collections`
and `native-builtins`, classified as REGISTERED (natives bound to it) and/or
MINTED (an allocation site names it). The interesting cell is
registered-and-not-minted.

A first pass keyed only on literal `register("java/util/X$Y"` was WRONG and is
worth recording: most rows bind through a variable — `for (_, itr) in
VALUES_ITR_CARRIERS`, `for itr_class in &[…]`, `let c = …` — so the scan
reported live carriers as dormant. `ArrayList$Itr` came back "registered, never
minted" while the runtime hands one out for every `ArrayList`. **A static scan
of this file has to follow the variable, and the check that it does is a
control row whose answer you already know.**

**Dynamic.** `apps/probes/ItrCarrierCensus` asks 24 collection families what
class `iterator()` returns, what it walks, and whether it is fail-fast, on both
VMs. Fail-fast is what says whether the natives bound to a carrier are reading
the shape it actually has, and it cannot be read off a registration list.

One probe-design correction, because the first version measured nothing for the
rows that matter: a map view refuses `add`, so provoking its fail-fast means
mutating the BACKING MAP. Testing the view's own `add` reported
`n/a-immutable` for all eight view rows — the very rows the carrier table exists
for.

## Result: 24 rows, 21 identical to HotSpot 25.0.3

Three differ, all of them class-NAME only, with correct contents and correct
fail-fast:

| family | HotSpot | CratonVM |
|---|---|---|
| `Vector` | `Vector$Itr` | `ArrayList$Itr` |
| `List.of` | `ImmutableCollections$ListItr` | `cratonvm/internal/UnmodifiableListItr` |
| `unmodifiableList` | `Collections$UnmodifiableCollection$1` | `cratonvm/internal/UnmodifiableListItr` |

A fourth row differed when the census was first run and is fixed — see below.

## What the census found that a registration list could not

**`ArrayDeque` was not fail-fast on `add`.** Same iterator class as HotSpot
(`ArrayDeque$DeqIterator`, so every class-name check passed), same walk, same
answers to `size()` and `peekFirst()` — and `add`-during-iteration raised no
`ConcurrentModificationException` where HotSpot raises one. `clear` matched,
and `addFirst`/`removeLast` correctly matched HotSpot's *non*-detection, so
three of four cells agreed.

The cause was a defect introduced HOURS EARLIER, by the `ArrayDeque(Collection)`
constructor added in the same session's previous commit. `DeqIterator.next()`
detects modification only through `nonNullElementAt` — a null read — so a plain
`add` cannot trip it unless the add GROWS and reallocates `elements`. HotSpot's
`ArrayDeque(Collection)` is `this(c.size()); copyElements(c)`, so a 3-element
deque gets 4 slots and the 4th add grows. The new constructor called the no-arg
init instead, which allocates the default 16 + 1: contents identical, capacity
17, no grow, no null, no CME.

Every element-level assertion agreed. **The capacity was observable only through
the fail-fast column**, which is the column this probe exists for and the one a
"does it iterate correctly" test would not have. `native_ad_init_capacity`'s own
comment already stated the contract it needed — "this constructor is the one
`ArrayDeque(Collection)` calls: `this(c.size())`" — one screen from the new code.
Fixed; `apps/probes/AdFailFast` (4 modification kinds) is now identical to
HotSpot.

## The dormant row, and its one real consumer

`java/util/ArrayDeque$Itr` is registered — `hasNext`/`next`/`remove` bound to the
snapshot natives — and **nothing mints it**. Confirmed both ways: no allocation
site names it (the `ArrayList$Itr` control proves the scan finds mints), and the
runtime hands out the real `DeqIterator`.

That is the "trap armed for whoever produces one later" the file names, and it
is not hypothetical: `PriorityQueue$Itr` sat in exactly this state until
`native_pq_iterator` started minting the real class, at which point the dormant
row won the slot over the registration matching the shape actually produced and
**iteration reported every queue EMPTY**. The trap was sprung again during THIS
census: a first attempt at an ArrayDeque iterator for `--synthetic-jdk` minted
`ArrayDeque$Itr`, and was reverted.

It has one consumer, and it is not Java code: `vm/src/vm/tests.rs::array_deque_iterator`
calls the natives directly on a hand-built receiver, under
`#[cfg(all(test, feature = "synthetic-jdk"))]`. So the registration is
unreachable from any program and exercised only by a unit test that supplies its
own object.

### The consumer was already broken, and so was everything around it

`vm::tests::array_deque_iterator` fails at its FIRST line —
`java/util/ArrayDeque.iterator() not registered` — so it never reaches the
`$Itr` natives either. It has been failing since the 2026-08-30 retirement.

Nobody saw it, because its whole `#[cfg(all(test, feature = "synthetic-jdk"))]`
module had not compiled since `InlineSite` grew an `ir_new_info` field: one
missing initializer entry in one round-trip test took **4291 tests** out of the
build. `cargo test -p cratonvm-vm --lib array_deque_iterator` reported
`0 passed; 0 failed` and looked like a clean run.

Fixed here — it is a one-line addition — and with the module building again the
same run reports **4144 passed, 30 failed**. Twenty-nine of those thirty are
nothing to do with iterators; they are simply the first sight anyone has had of
this module since it broke, and they cluster rather than scatter:

```text
cyclic_barrier_*                  4   basic, await_sequence, reset, zero_parties_throws
p86_thread_group_* / thread_group 4   active_count, enumerate, hierarchy, basics_p71
m3_* functional composition       4   predicate and/or/negate, consumer and_then
scanner_*                         2   close, from_bais
http_*_enums_p60                  2   redirect, version
one each                         13   async_socket_channel_p67, basic_file_attributes_p59,
                                      countdown_latch_await_timeout, enum_map_put_get_size,
                                      hex_format_from_hex_digits_p64, input_stream_read_all_bytes_p72,
                                      log_manager_p61, object_output_stream_p70,
                                      optional_or_present_returns_self, proxy_new_instance_stores_handler,
                                      sb_repeat_string_p64, system_get_property_native,
                                      u6_datagram_channel_connect_disconnect
```

The clustering is the useful part: four barrier tests and four thread-group
tests failing together is one defect each, not eight. Not triaged here — this
census's scope is the carrier table — but they are now VISIBLE, which they were
not this morning.

**A test module that does not compile reports zero failures**, which is
indistinguishable at a glance from a module that passes. `--lib` name filters
make it worse: they print a green `0 passed; 0 failed` line for a target that
never built.

## Verdict: retired

The deferral asked for evidence and now has it, so the rows are gone:

* nothing mints `java/util/ArrayDeque$Itr` — no allocation site names it, and
  the runtime hands out the real `DeqIterator`;
* its only caller was a unit test that hand-builds its receiver, and that test
  already failed before reaching it;
* the hazard is demonstrated twice, not asserted.

`native_ad_itr_remove` went with them: it existed to serve
`ArrayDeque$Itr.remove()` and had no other caller. The stale test is `#[ignore]`d
rather than deleted, with its reason, because its assertions are the right ones
if an ArrayDeque iterator is ever minted here again.

`no_natives_are_bound_to_a_retired_iterator_carrier` is the durable half. It is
a deny-list, not the general property, and the doc comment says why: a registry
test sees NAMES, not allocations, so "every registered carrier has a mint site"
is not a question it can ask. It carries a control row over
`VALUES_ITR_CARRIERS` so it cannot pass by the registry being empty, and it was
checked by re-arming the trap — it reds on exactly the restored row.

The three class-name rows are left as measured. `Vector` is the one with a real
fix available — a `("java/util/Vector", "java/util/Vector$Itr")` row — and it is
the one that most deserves the caution the table documents:
`Vector$ListItr extends Vector$Itr`, so java.base becomes a second producer of
the carrier class, which is the `DescendingIterator`/`DeqIterator` shape that
walked `descendingIterator()` to `[]` the last time it happened. The guard for
it exists (`run_receiver_own_bytecode_when_inherited`), so this is a tractable
change and not a blocked one — but the gain is a class NAME, with contents and
fail-fast already correct, and that is not worth spending the table's risk
budget on inside a census.
