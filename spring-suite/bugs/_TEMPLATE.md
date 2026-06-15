<!-- One file per CratonVM-unique crash/hang. Skip anything HotSpot also fails. -->
# spring-bug-NN: <one-line symptom>

| | |
|---|---|
| **Category** | VM-CRASH \| VM-HANG \| VM-LOADERR \| VM-CORRECTNESS |
| **Module** | spring-xxx |
| **Test class** | `org.springframework.…Tests` |
| **Failing test(s)** | `methodName()` |
| **CratonVM** | CRASH / TIMEOUT / FAIL — `<short signal>` |
| **HotSpot JDK 25** | OK (N tests) |
| **CratonVM HEAD** | `<sha>` |
| **Status** | OPEN / FIXED (commit `<sha>`) / HANDOFF |
| **Suggested owner** | you / handoff |

## Symptom
<what CratonVM does: exact error / stack / panic / hang location>

## Reproduce
```bash
VM=C:/craton/CratonVM/target/release/cratonvm.exe
JDK='C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot'
CP="<harness>;<module cratonvm-testcp.txt>"
"$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.…Tests
# HotSpot (passes):
"$JDK\bin\java.exe" -cp "$CP" KRun org.springframework.…Tests
```

## Minimal trigger
<smallest Java snippet / JDK API that reproduces, if isolated>

## Root cause
<CratonVM component + file:line once known>

## Fix
<what changed; commit link>

## Notes
<related bugs [[spring-bug-MM]], memory links>
