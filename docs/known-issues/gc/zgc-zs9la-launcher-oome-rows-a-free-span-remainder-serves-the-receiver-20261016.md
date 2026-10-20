# ZGC: after zs s9/la, the L4 launcher rows that still enter `main` get their receiver from a free-span remainder

> **STATUS (2026-10-19, zt t6/nd4): OPEN, decided by reading which arm can close it; no code change.** On the default (no exhausted verdict) path the fill's final refusal goes through `alloc_raw`, which records the size at the current collection count, so `note_final_refusal` (gc/src/zgc.rs) arms; with `CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS=on` (threshold 2048) the dead 32-byte node the receiver ladder frees is set aside, and the fill's live set (static) is not freed by that collection, so the threshold does not disarm (`max(64 KiB, capacity/64)` progress rule). The literal `16` arm cannot close it (t1/sp F14). With the exhausted verdict on, `CRATONVM_ZGC_VERDICT_RECORDS_REFUSAL` (t2/al) is what lets it arm. Deciding rows: SP-1's `R` arm (`t1-sp-report.md`) and AL-4 (`t2-al-report.md`); flip if `R` is 20/20 on every cell D passes with SP-2 / SP-4 SAME and CPU <= 1.05x; else record the 16 bytes as an intended difference. Report: `docs/internal/zgc-round-zt-20261018/t6-nd4-report.md`.**

> **STATUS (2026-10-19, zt t5/pg3): OPEN (a flip decision with a local grid).** Both opt-ins are built (`CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS`, `CRATONVM_ZGC_VERDICT_RECORDS_REFUSAL`). Run row AL-4 of `docs/internal/zgc-round-zt-20261018/t2-al-report.md` (8 cells, 20 runs each, V+R vs V+R+K) on the Windows box; flip the fit predicate in gc/src/arena.rs (`CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS`) if V+R is 20/20 exact wherever D passes and SP-2 / RSS are neutral. If AL-4 refutes it, record the receiver's 16 bytes as an intended difference (no page allocator) and retire.

> **STATUS (2026-10-19, zt t3/tp): OPEN at the default; opt-in `CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS` (t1/sp) and `CRATONVM_ZGC_VERDICT_RECORDS_REFUSAL` (t2/al) are built.** The deciding grid is t2-al row AL-4 (the 8-cell L4 loop, V+R vs V+R+K, 20 runs per cell). Nothing else was changed this wave: the refusal and verdict functions are lane sp's code. Flip rule: V+R at 20/20 exact on every cell where D passes, and the SP-2 / RSS rows neutral.**

> **STATUS (2026-10-19, zt t1/sp): FIXED IN CODE OPT-IN, PENDING BUILD/PROBE.** Item 1 is implemented as `CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS` (zs s10/sp `f454d69c2b`, brought onto `zt1-sp` and reviewed: five defects fixed, see the report). Default OFF. The fix is a fit predicate, not a removal: blocks and bump tails below `T = max(S, floor)` stay listed but serve nothing until a collection changes the picture by `max(64 KiB, capacity/64)`. By reading, `on` (floor 2048) is the arm that can close the six cells; the literal `16` arm cannot, because the dead 32-byte node is not below `T = 32` and still serves the receiver. Rows SP-1 (the 8-cell L4 loop, arms D / R=on / R=16), SP-2 and SP-3 are in `docs/internal/zgc-round-zt-20261018/t1-sp-report.md`. A flip needs SP-1 at 20/20 on R=on for every cell D passes, SP-2 SAME, and the RSS/CPU rows neutral.

> **STATUS (2026-10-16, zs s9/la): OPEN, measured locally; the launcher's own text is at parity on every path.** zs s9/la removed one source of the bytes that let `Zs1NdLauncherOome` / `Zs2Nd2LauncherArgs` enter `main`: the four traced `OutOfMemoryError`s CratonVM built on the heap that had just refused (HotSpot preallocates them; `CRATONVM_ZGC_OOME_TRACED_POOL`). Locally that moved `Zs1NdLauncherOome -Xmx16m` to HotSpot's 27 bytes in both modes (0/5 -> 4/4 each). The other six cells still print the uncaught-handler line; this page names why, and what would close it. Report: `docs/internal/zgc-round-zs-20261014/s9-la-report.md`.

## What the still-failing cells print

All rc 1, stdout `start`. CratonVM stderr is
`\nException: java.lang.OutOfMemoryError thrown from the UncaughtExceptionHandler in thread "main"\n`
-- the text HotSpot itself prints on that path (`Zs4Nd4StaticArgs`, nd4). HotSpot 25.0.3 prints
`Exception in thread "main" ` (27 bytes, no newline) on all eight cells: its instance `main`'s
receiver (`NewObject` in `java.c`) is refused, and `ExceptionDescribe` writes the prefix and
then fails its own `printStackTrace()`.

## Why CratonVM's receiver is served (measured)

`CRATONVM_DBG_ZGC_POST_REFUSAL=12 CRATONVM_DBG_RTERR=OutOfMemory`, `Zs1NdLauncherOome
-Xmx32m`, debug build with the pool (s9/la):

```text
[zgc-post-refusal] served size=48 ... at=0xefb28 was=corpse ...    (the fill: long[4])
... refused size=32 (x12, the fill's Object[2], each after a collection) ...
[DBG_RTERR-STK 0] Zs1NdLauncherOome.<clinit> pc=17                  (the last refusal)
[zgc-post-refusal] served size=16 ... at=0xefb58 was=zero ...       (main's receiver)
[DBG_RTERR-STK 0] Zs1NdLauncherOome.main pc=19                      (then "main-ran true")
```

The receiver's 16 bytes start exactly where the fill's last 48-byte hand-out ended
(`0xefb28 + 48 = 0xefb58`) and read zero: by every sign the remainder of the free span that
hand-out split (not proven: the diagnostic logs hand-outs, not the free list), which the free
list kept and served to the next 16-byte request. The fill itself never asks for 16 bytes on CratonVM before it stops (its
smallest attempt is the 32-byte `Object[2]`, 24 bytes on HotSpot), so the hole survives the
fill and serves `main`.

HotSpot's ZGC has no such hole to give: small objects are bumped in TLABs carved from pages;
a TLAB whose tail cannot serve a request is retired (its tail filled, unusable) before the
refill that fails, and a page's dead bytes come back only when the page is relocated. A
16-byte gap is never handed to the next request.

## What would close it

1. **Allocator / exhausted-verdict side (owner: `no2`, refusal and exhausted-verdict
   functions; or the TLAB lane).** At the FINAL refusal of a ladder for size `S` (the one
   that becomes the `OutOfMemoryError`), take every free span smaller than `S` off the free
   list until the next relocating slide, as a retired TLAB tail is on HotSpot. Proposal:
   `zgc-zs9la-proposal-retire-sub-request-spans-at-the-final-refusal-20261016.md`.
2. **Probe side.** `Zs4Nd4LauncherExact` (fills the last 16-byte holes before `main`) is
   already at parity; keep the two older probes as rc != 134 rows until 1 lands.

The launcher report path (`vm-cli` `premain_exception_describe`,
`dispatch_main_uncaught_exception`) needs no change: each path prints HotSpot's bytes for
that path (nd2, nd4 rows; re-checked here).
