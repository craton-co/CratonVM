<#
.SYNOPSIS
  Tests for `Resolve-EnvGatedStatus` in run-spring-boot-suite.ps1.

.DESCRIPTION
  `ENV-GATED` excuses a row, so the interesting cases are the ones where it must
  NOT fire. Every negative case below is a way a real CratonVM defect could have
  been hidden by a host-gap classifier that was one condition too loose:

    * a class where only SOME failures are symlink failures;
    * a class whose abort reason was never printed at all (a stale
      `SbRunner.class` in the fixture -- the actual state of both suite hosts on
      2026-08-11, and the reason the reason-strings looked unwired);
    * a container-level failure, which is not a per-test refusal;
    * a host that CAN create symbolic links, where these rows are real.

  The function definitions are lifted out of the runner by AST so this cannot
  drift from the implementation it tests.

  Run:  powershell -NoProfile -File .\tests\Test-EnvGatedStatus.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$runner = Join-Path (Split-Path $PSScriptRoot -Parent) 'run-spring-boot-suite.ps1'
if (-not (Test-Path $runner)) { throw "runner not found: $runner" }

$errs = $null; $tokens = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($runner, [ref]$tokens, [ref]$errs)
if ($errs -and $errs.Count -gt 0) { throw "runner does not parse: $($errs[0].Message)" }

# Pull in only what is under test, plus the signature it matches with.
# `Test-HostSymlinkSupport` comes along too, but every case below presets its
# cache (`$script:HostSymlinkSupport`), so it answers from that and never probes
# the real filesystem -- the test states the host capability instead of
# inheriting whichever one the machine running it happens to have.
foreach ($name in @('Test-HostSymlinkSupport', 'Resolve-EnvGatedStatus')) {
  $fn = $ast.FindAll({ param($n) $n -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq $name }, $true)
  if (-not $fn) { throw "function not found in runner: $name" }
  Invoke-Expression $fn[0].Extent.Text
}
$sigAssign = $ast.FindAll({
    param($n)
    $n -is [System.Management.Automation.Language.AssignmentStatementAst] -and
    $n.Left.Extent.Text -eq '$script:SymlinkGapSignature'
  }, $true)
if (-not $sigAssign) { throw 'SymlinkGapSignature not found in runner' }
Invoke-Expression $sigAssign[0].Extent.Text

$script:Failures = 0
function Check([string]$name, [string]$expected, [string]$actual) {
  if ($expected -eq $actual) {
    Write-Host ("  PASS  {0}" -f $name)
  } else {
    Write-Host ("  FAIL  {0}: expected '{1}', got '{2}'" -f $name, $expected, $actual)
    $script:Failures++
  }
}

# Real excerpts, trimmed. FileWatcherTests / ConfigTreePropertySourceTests fail
# with a bare `java.nio.file.FileSystemException` and NO message -- the OS's
# explanation is localized and the JUnit summary line carries none of it, which
# is why the detector matches the type, not the text.
$symlinkFailure = @'
SBRUNNER_FAILURE_DETAIL {0}
java.lang.RuntimeException: Failed to create symlink
	at java.base/java.nio.file.Files.createSymbolicLink(Files.java:1069)
Caused by: java.nio.file.FileSystemException: C:\Users\x\link.txt
	at java.base/sun.nio.fs.WindowsFileSystemProvider.createSymbolicLink(WindowsFileSystemProvider.java:597)
'@
$realFailure = @'
SBRUNNER_FAILURE_DETAIL somethingElseEntirely()
java.lang.AssertionError: expected 3 but was 2
	at org.assertj.core.api.Assertions.fail(Assertions.java:1)
'@

$threeSymlink = (($symlinkFailure -f 'a()'), ($symlinkFailure -f 'b()'), ($symlinkFailure -f 'c()')) -join "`n"

# --- host WITHOUT the privilege -------------------------------------------
$script:HostSymlinkSupport = $false

Check 'three symlink failures are env-gated' 'ENV-GATED' (
  Resolve-EnvGatedStatus -Status 'FAIL' -Combined $threeSymlink -Failed 3 -Aborted 0 -ContainersFailed 0)

Check 'one real failure among symlink ones stays FAIL' '' (
  Resolve-EnvGatedStatus -Status 'FAIL' -Combined (($symlinkFailure -f 'a()'), $realFailure -join "`n") `
    -Failed 2 -Aborted 0 -ContainersFailed 0)

Check 'fewer failure blocks than the counter stays FAIL' '' (
  Resolve-EnvGatedStatus -Status 'FAIL' -Combined ($symlinkFailure -f 'a()') -Failed 3 -Aborted 0 -ContainersFailed 0)

$abortLog = "SBRUNNER_ABORTED_DETAIL whenSymlinkExistsInDirectoryLocationGetDirThrows() : org.opentest4j.TestAbortedException: Symlink creation not supported`nSBRUNNER_SKIPPED_DETAIL other() : Disabled on operating system: Windows 11"
Check 'a symlink abort is env-gated' 'ENV-GATED' (
  Resolve-EnvGatedStatus -Status 'FAIL' -Combined $abortLog -Failed 0 -Aborted 1 -ContainersFailed 0)

# The stale-SbRunner.class case: aborted=1 and not a single reason line emitted.
Check 'an abort with no printed reason stays FAIL' '' (
  Resolve-EnvGatedStatus -Status 'FAIL' -Combined 'nothing useful here' -Failed 0 -Aborted 1 -ContainersFailed 0)

Check 'an abort for an unrelated reason stays FAIL' '' (
  Resolve-EnvGatedStatus -Status 'FAIL' -Combined 'SBRUNNER_ABORTED_DETAIL x() : org.opentest4j.TestAbortedException: Docker not available' `
    -Failed 0 -Aborted 1 -ContainersFailed 0)

Check 'a container failure stays FAIL' '' (
  Resolve-EnvGatedStatus -Status 'FAIL' -Combined $threeSymlink -Failed 3 -Aborted 0 -ContainersFailed 1)

Check 'a CRASH is never env-gated' '' (
  Resolve-EnvGatedStatus -Status 'CRASH' -Combined $threeSymlink -Failed 3 -Aborted 0 -ContainersFailed 0)

Check 'a PASS is never env-gated' '' (
  Resolve-EnvGatedStatus -Status 'PASS' -Combined '' -Failed 0 -Aborted 0 -ContainersFailed 0)

# --- host WITH the privilege ----------------------------------------------
# Same evidence, opposite verdict: on Linux these rows are real failures.
$script:HostSymlinkSupport = $true
Check 'symlink failures on a capable host stay FAIL' '' (
  Resolve-EnvGatedStatus -Status 'FAIL' -Combined $threeSymlink -Failed 3 -Aborted 0 -ContainersFailed 0)

if ($script:Failures -gt 0) {
  Write-Host "ENV-GATED TESTS: $script:Failures failed"
  exit 1
}
Write-Host 'ENV-GATED TESTS: all passed'
