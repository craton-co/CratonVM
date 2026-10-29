# A loop root moves into a caller through receiver resolution (the W13-6 `matchonly` loss), not through an admission rule

Status: OPEN (proposal). Area: JIT IR splice planning, chain splice rules (round rx27 lane F owns the code).
Severity: performance (`matchonly` +8.3 % instructions per op on c26 with `CRATONVM_JIT_IR_SPLICE_ANEWARRAY=1`,
which is default ON since j27). Filed by round rx27 wave 1 (lane matcher, 2026-10-28) from the W13-6 capture on
`r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`. Nothing measured here.

## Evidence

W13-6 explained two losses of the `anewarray` splice. Both are a loop root moving up into a caller whose budget
it had already spent:

* `cached`: `Pattern.split(CharSequence,int,boolean)` became spliceable itself. The W13-6 rule
  (`splice_anewarray_refuses_loop_callee`) keeps a looping `anewarray` callee a call. It was inert at j27
  because the mass flip turned `CRATONVM_JIT_IR_SPLICE_ANEWARRAY_LOOP_CALLEES` ON. Rx27 lane matcher put that
  switch back to opt-in.
* `matchonly`: no callee changed admission. `Matcher.search`'s `root.match` site (pc 121) went from 27 bytes /
  1 body to 244 bytes / 7 bodies (a profile guard on class 592, a chain tree, 3 pruned branches), and the
  `Pattern$Start.match` share went from 56 % to 64 %. The likely cause: with `Matcher.<init>` spliced, the
  `parentPattern` it stored is visible, the receiver of `parentPattern.root.match` resolves, and the chain
  splice of `Start.match` (a scan loop root) is admitted at that site. No admission rule fires, so the W13-6
  rule cannot see it.

Rx27 lane matcher's `CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM` makes the same move more likely on every Matcher
row: `Pattern.matcher` (and with it `new Matcher`) splices into the row's lambda, so the `Matcher` there is a
fresh `Op::New` whose `parentPattern` / `root` fields the lambda's compile can forward.

## Lines needed first (both arms of the row)

```
CRATONVM_DBG_JITC=1 [ENV] cratonvm -cp <p> R24RegexSplitRows matchonly 3 1 2>&1 \
  | grep -E 'inline-plan|ir-splice-bytes|ir-splice-refused|inline-resolve REFUSED|ir-splice-nested|chain|pruned|guard' \
  | grep -E 'Matcher\.search|Pattern\$Start\.match|Pattern\.matcher|Matcher\.<init>|lambda\$row\$'
```

The `in <root>` suffix of the `pc=121` lines names the compile that carries the chain. Add
`CRATONVM_DBG_DEOPT=1` in a second run to see whether its profile guard misses.

## What would close it

A planner rule in the chain splice (lane F's area): a chain or receiver-resolved splice whose root body has a
back edge, and that would be planted in a compile which is not that body's own root, is charged the in-loop
sites it gives up (the callee's own standalone plan size) against the caller's remaining budget. If the caller
cannot carry them, it stays a call and keeps its own compile. That is the W13-6 rule generalised from "this
admission is new" to "this placement loses the loop's plan". Confirm by `matchonly` returning to the
`SPLICE_ANEWARRAY=0` count with the switch on, and by `regex-matches` keeping its gain.
