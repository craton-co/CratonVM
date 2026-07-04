# KcAdmV2HelpTest: generated --help text missing env-var mentions

Status: open — unclear, possibly stale test vs. current help text

Date observed: 2026-07-04

## Summary

`integration/client-cli/admin-cli :: KcAdmV2HelpTest` fails 2 of its 46
sub-tests:

- `testKeystoreOptionsAvailable`: `AssertionError: --storepass should mention KC_CLI_STORE_PASSWORD`
- `testPasswordDescriptionMentionsEnvVar`: `AssertionError: --password should mention KC_CLI_PASSWORD`

Both assert that generated Picocli `--help` output for `kcadm` mentions a
specific environment-variable name in the option description text.

## Why this is filed as "unclear" rather than a specific bug

Two plausible explanations, not yet distinguished:
1. **Real CratonVM/build-time divergence**: Picocli or an annotation
   processor generates the help text with environment-variable
   interpolation baked in at compile/build time; something in that
   generation pipeline behaves differently under CratonVM's build than under
   a real JDK build, dropping the env-var mention.
2. **Stale test vs. current CLI text**: this project has a documented
   pattern (see `reference_stale_test_vs_real_bug_resource_traversal` in
   project memory) of tests asserting on behavior/text that changed
   upstream for legitimate reasons, unrelated to CratonVM. The two assertions
   here are exactly the kind of literal-text check that drifts when a CLI's
   help copy is edited upstream.

## Next steps

Diff the actual generated `--help` output (run `kcadm.sh help` or the
equivalent under both CratonVM and a real JDK from the *same* Keycloak
checkout) against what the test expects, to see whether the text is simply
missing on both VMs (a stale test) or differs specifically between CratonVM
and HotSpot (a real VM-level divergence).

## Evidence

`/data/wt-keycloak-full-20260704/apps/keycloak-suite-runner/.suite/results/kcfull-others-1124-20260704-v2/others-jit/logs/integration_client-cli_admin-cli.org.keycloak.client.admin.cli.commands.v2.KcAdmV2HelpTest.out.log`
