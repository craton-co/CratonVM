# Spring Framework Interleaved Evaluation Runbook (2026-11-09)

## Execution Parameters
- **Base Commit**: `78795f2e2f` (branch `dev`)
- **Binary**: `/root/custom-bins/cratonvm-spring-eval-20261109`
- **JDK Root**: `/usr/lib/jvm/java-25-openjdk-amd64`
- **Regression Probes**: 64 probes validated via `regression-suite/probes/check-probes.sh` (0 name mismatches, 0 compile failures).
- **Worktree**: `/root/wt-spring-eval-20261109`

## Test Configuration
- **Runner**: `scripts/run_spring_interleaved.py`
- **Classes**: `/root/spring_valid_classes.txt` (2,900 test classes)
- **Runner Directory**: `apps/spring-suite-runner`
- **Classpath Argfiles**: `apps/spring-suite-runner/.af_spring`
- **Arms**: Default (`cratonvm`) vs Fast-Boot (`cratonvm --fast-boot`) interleaved class-by-class.
- **Reporting Intervals**: First report at 10 minutes, subsequent reports hourly.
