# KcAdmV2HelpTest: generated --help text missing env-var mentions

Status: fixed

Date observed: 2026-07-04
Date fixed: 2026-07-05

## Summary

`integration/client-cli/admin-cli :: KcAdmV2HelpTest` failed 2 of its 46
sub-tests under CratonVM:

- `testKeystoreOptionsAvailable`: `--storepass should mention KC_CLI_STORE_PASSWORD`
- `testPasswordDescriptionMentionsEnvVar`: `--password should mention KC_CLI_PASSWORD`

Both checks assert that generated Picocli `--help` output for `kcadm` mentions
specific `KC_CLI_*` environment-variable names in option descriptions.

## Root cause

This was a CratonVM `java.text.BreakIterator` divergence, not a stale Keycloak
test.

Picocli 4.7.7 wraps help text by replacing `-` with `U+00FF` before calling
`BreakIterator.getLineInstance()`, so long option names do not split at hyphens.
CratonVM's synthetic BreakIterator scanned Rust UTF-8 bytes and returned those
byte offsets as Java offsets. After Picocli inserted two non-ASCII `U+00FF`
characters for `--user`, later boundaries were two positions too large relative
to Java's UTF-16 string indexes.

For the failing help text, the line boundary before `KC_CLI_PASSWORD` was
reported after `KC`, so Picocli hard-wrapped the string as `KC` followed by
`_CLI_PASSWORD`. The same mechanism split `KC_CLI_STORE_PASSWORD` and
`KC_CLI_TRUSTSTORE_PASSWORD`. `KC_CLI_CLIENT_SECRET` survived because that
description did not have the same preceding `--` replacement and width shape.

## Fix

`native-builtins/src/phases_late.rs` now translates between Java UTF-16 text
positions and Rust UTF-8 byte indexes inside the synthetic BreakIterator helper.
The line iterator also reports boundaries after complete whitespace/hyphen runs
instead of at the separator character or inside a `--` run.

## Validation

- HotSpot baseline: `kcadmv2-help-hotspot-20260705` passed
  `org.keycloak.client.admin.cli.commands.v2.KcAdmV2HelpTest` 46/46.
- Pre-fix CratonVM reduced probe: `PrintKcAdmV2ListHelp` reported
  `HAS_KC_CLI_PASSWORD=false`, `HAS_KC_CLI_STORE_PASSWORD=false`, and
  `HAS_KC_CLI_TRUSTSTORE_PASSWORD=false`.
- Fixed CratonVM binary:
  `/data/cratonvm-keycloak-kcadmv2-help-remote-20260705-001/cratonvm-keycloak-kcadmv2-help-20260705-r2-utf16breakiter`.
- Fixed reduced probes:
  `PicocliDescriptionProbe` and `PrintKcAdmV2ListHelp` both reported all
  expected `KC_CLI_*` substrings present.
- Keycloak suite runner:
  `kcadmv2-help-r2-utf16breakiter-20260705/all-jit` passed
  `org.keycloak.client.admin.cli.commands.v2.KcAdmV2HelpTest` in 64.3s.
