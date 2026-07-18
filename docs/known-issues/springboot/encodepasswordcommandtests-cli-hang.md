# `EncodePasswordCommandTests` (spring-boot-cli) — ~4 minute stall before "Unknown algorithm" error

**Status: OPEN — found 2026-07-17, not root-caused**

## Symptom

Module `cli/spring-boot-cli`, class `EncodePasswordCommandTests` — HANG.

`.out.log` (2 lines) shows two successful test outputs before the stall — a SHA-256 "default" digest command and a bcrypt command:

```
c528d11ccc665a4df24f2cc3e61c63038719a1369e68fd582d634edf934c444971193e507b802adc3ca9adfcb7031780
{bcrypt}$2a$10$UqnKWlUxzBwGGKtWRl94M.I72ogcKaIVfrXlhhLraLaQb7dEdsXem
```

`.err.log` timeline:
- `19:51:57.925` → `19:52:01.124`: normal Post-clinit + `gc::guard` `InterceptingExecutableInvoker` noise (documented-ignorable, `class_id 733`).
- **`19:52:01.124` → `19:56:01.002` — a 4-minute gap with zero log lines.** This is the actual hang window.
- `19:56:01.002` onward: a fresh burst of the same noise, then at `19:56:01.048181Z`: `Unknown algorithm, valid options are: default,bcrypt,pbkdf2` (plain output, not WARN-tagged), then 3 more noise lines ending `19:56:01.077824Z`, then nothing further (file ends — the harness killed the process here after the overall timeout).

Log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/cli_spring-boot-cli.org.springframework.boot.cli.command.encodepassword.EncodePasswordCommandTests.{out,err}.log`.

## Root cause

**Not root-caused.** The hang is the **~4-minute stall before** the "Unknown algorithm" message appears, not after it — i.e. whatever test method eventually produces that message (almost certainly a test asserting CLI behavior for an unsupported/invalid algorithm argument) stalls for ~4 minutes first. Source files exist at
`apps/spring-boot/cli/spring-boot-cli/src/test/java/org/springframework/boot/cli/command/encodepassword/EncodePasswordCommandTests.java`
and
`apps/spring-boot/cli/spring-boot-cli/src/main/java/org/springframework/boot/cli/command/encodepassword/EncodePasswordCommand.java`
but were not read.

**Hypothesis (unconfirmed):** something in the algorithm-validation/error
path — possibly `SecureRandom` initialization for bcrypt/pbkdf2 salt
generation, or a `PasswordEncoder` factory lookup — stalls for ~4 minutes
before throwing `IllegalArgumentException("Unknown algorithm...")`.
Alternatively the process could be hanging waiting on stdin/console (this
is a CLI test, and could involve `System.console()` or piped process I/O).
Neither hypothesis was verified against CratonVM source
(`SecureRandom`/console/native-process code).

## Affected classes

- `cli/spring-boot-cli` | `EncodePasswordCommandTests`
