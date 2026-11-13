# Problem 7: compiled code spends several times HotSpot's work per operation

Filed 2026-11-12. Timings only: the host that produced them has no
instruction counter. Step 1 of the plan is to replace this table with
instruction counts.

## Measurement

`tools/probes/OpCostRows.java`: 20 kernels, each a method with its own loop,
called 3,000 times to warm and then timed over 1,000 calls of 10,000
operations (best of five), so the timed body is a normal compiled entry and
not an on-stack-replacement body.

Windows x64, HotSpot 25.0.3 against the `dev` release binary built
2026-10-27, default collector on both, three runs each, best run shown.
Nanoseconds per operation.

| row | what it does | HotSpot | CratonVM | ratio |
|---|---|---|---|---|
| `throwCatch` | throw a preallocated exception, catch it in the same method | 0.21 | 64.5 | 307x |
| `lambda` | call through an `IntUnaryOperator` | 0.36 | 10.5 | 29x |
| `charAt` | `String.charAt` | 0.38 | 8.1 | 21x |
| `sbAppend` | `StringBuilder.append(char)` | 1.16 | 14.2 | 12x |
| `virt2` | virtual call, two receiver classes | 1.21 | 13.2 | 11x |
| `listGet` | `ArrayList.get` and unbox | 1.26 | 12.2 | 9.7x |
| `mapGet` | `HashMap.get`, `Integer` keys | 2.91 | 27.4 | 9.4x |
| `arraycopy` | `System.arraycopy`, 16 ints | 13.4 | 105 | 7.9x |
| `typeCheck` | `instanceof` and cast | 1.23 | 9.6 | 7.8x |
| `aastore` | reference array store | 1.38 | 6.3 | 4.5x |
| `allocObj` | allocate a small object | 4.26 | 16.9 | 4.0x |
| `getfield` | field load, pointer chase | 1.40 | 5.5 | 4.0x |
| `iface4` | interface call, four receiver classes | 4.34 | 13.6 | 3.1x |
| `box` | box an `int` and read it back | 0.34 | 1.03 | 3.0x |
| `arith` | `s = s * 31 + i` | 1.07 | 2.2 | 2.1x |
| `putInt` | `p.x += i` | 0.36 | 0.71 | 2.0x |
| `allocArr` | allocate `int[4]` | 10.1 | 13.8 | 1.4x |
| `sync` | uncontended `synchronized` block | 5.06 | 6.1 | 1.2x |
| `throwNew` | throw and catch a new exception | 885 | 1005 | 1.1x |

`putRef` (a reference field store) is in the probe but not in the table:
HotSpot reported 0.00 ns, so it removed the loop. CratonVM's 4.9 ns is still
the cost of one barriered store; the row needs a body HotSpot cannot fold.

The ratios say where to look. They do not say how many instructions to cut:
the binary is two weeks old, and time is not instructions.

## What the rows have in common

1. **Calls are not inlined.** Every row at 9x or worse, except `throwCatch`,
   contains a call that HotSpot inlines and CratonVM makes, at about 10 to 13
   ns. Checked on `virt2` with `CRATONVM_DBG=jit-method-stats`: the kernel was
   in the optimizing tier and `Sq.area` and `Ci.area` were compiled as
   separate bodies. The JDK rows (`charAt`, `listGet`, `sbAppend`, `mapGet`)
   were not checked the same way: a registered native in front of the
   bytecode would be an inlining barrier (problem 4), and whether one stands
   there for each method in the default mode is the first thing to look at.
2. **The empty loop costs double.** `arith` and `putInt` have no call and no
   object work and are still 2x. Every other row pays this as well. Earlier
   Linux work found the mechanism on other rows: 14 to 17 frame-slot stores
   per iteration that exist to keep stale words out of the root scan
   (problem 3).
3. **Object operations cost about 4x** (`getfield`, `aastore`, `allocObj`,
   `typeCheck`). The 2026-11-02 Linux instruction count put allocation at
   71.6 instructions against HotSpot's 22.0, and `int[4]` at 59.9 against
   26.0.
4. **A throw caught in the same compiled body leaves compiled code.** HotSpot
   turns it into a branch.

Not a cause: switch reads. `CRATONVM_DBG_FLAGREADS` on an allocation loop
showed them concentrated in the compiler, not in steady-state code.

## Plan

1. **Turn the table into instruction counts.** The probe has
   `count <row> <ops>`: run it at N and 3N operations under
   `perf stat -e instructions:u` and take `(I(3N) - I(N)) / 2N`. Do it on
   current `dev`, Linux, both VMs, all three collectors. Check the table in as
   a budget; a row may not exceed its budget by more than 5 percent. Fix the
   `putRef` row first.
2. **Inline calls** (rows at 9x to 29x).
   - In the optimizing tier, inline a virtual or interface site with one or
     two profiled receivers behind a class check. A lambda site has one.
   - Let `String.charAt`, `ArrayList.get`, `StringBuilder.append` and
     `HashMap.get` run as bytecode so the compiler can inline them. This is
     problem 4's retirement order, chosen by this table instead of by package.
   - For calls that stay calls, count the instructions of one
     compiled-to-compiled call and trim the sequence.
3. **Fix the loop baseline** (2x on every row). Keep loop-carried values in
   registers across the back-edge, move frame-slot clears out of loop bodies,
   and make the safepoint poll one instruction. The clears can only go where
   the map says which slots are dead, so this depends on problem 3.
4. **Object operations** (about 4x).
   - Retire the legacy 16-byte-cell object layout so the layout guards on
     field access disappear.
   - Bring allocation from 71.6 instructions toward 30.
   - Give every store shape a lean inline barrier on each collector.
   - Inline the exact-class compare for `instanceof` and `checkcast`.
5. **Exceptions caught in the same compiled body** (307x). Compile the throw
   as a jump to the handler when both are in one body, inlined callees
   included.
6. **Small array copies** (105 ns for 16 ints). Inline constant-length copies
   and call typed stubs for the rest.
7. **Natives that stay native.** Give leaf natives a typed direct-call path
   with no tagged argument array and no root snapshot.

Steps 2 and 3 first: they move the most rows.

## Done when

- Every row in the instruction table is within 2x of HotSpot on all three
  collectors, and the budget gate is in CI.
- No row at parity today (`sync`, `throwNew`, `allocArr`) has moved by more
  than 5 percent.

## Not measured

- Interpreter cost per bytecode: every row here is compiled code.
- Multi-threaded rows, and any collector other than each VM's default.
