# Fix: oss-security-domain — align company-domain references to craton.com.ar

- **Id:** oss-security-domain
- **Date:** 2026-06-10
- **Scope:** documentation only (no code, no build)
- **Owned files edited:** `SECURITY.md`, `MAINTAINERS.md`, `SUPPORT.md`
- **Source report:** `docs/reviews/fable-2026-06-10/oss-readiness.md` §4 "contact-domain
  inconsistency vs. owner brief (`craton.co` vs `craton.com.ar`)" (lines 258-263) and §2
  warning (lines 137-142).

## Problem

The owner brief / audit names the canonical company site as **craton.com.ar**, but the three
community-health files used **`craton.co`** for every company-domain (email) contact. This was
also flagged by the `oss-meta` fix-note (lines 67-69): the root `Cargo.toml` `homepage` was
already updated to `https://craton.com.ar` (confirmed at `Cargo.toml:16`), leaving the email
contacts in these files out of sync and the canonical domain unreconciled.

## Decision: which references are "company-domain"

Two distinct kinds of `craton` references exist in the repo:

1. **Company-domain references (email addresses).** Unambiguously the corporate domain — these
   are the security/maintainer/support contacts. These were `@craton.co` and are now
   `@craton.com.ar`. **Fixed.**
2. **GitHub org/repo slugs** (`github.com/craton-co/cratonvm`, the `@craton-co/cratonvm-maintainers`
   GitHub team handle). These are GitHub **identifiers**, not a domain. They are referenced
   consistently across `Cargo.toml` `repository`/all-crate metadata, `.github/CODEOWNERS`,
   `FUNDING.yml`, and `README.md` (all owned by other agents/files). Rewriting the slug to
   `craton.com.ar` would (a) break the GitHub URLs/team handle, and (b) desync from those
   other-owned files. The audit itself lists `org craton-co` / `repo github.com/craton-co/cratonvm`
   separately from the email domain (oss-readiness.md:139). **Left unchanged — intentional.**

So this fix aligns the **company-domain (email) contacts** to `craton.com.ar` and leaves GitHub
slugs as GitHub identifiers.

## Changes

### SECURITY.md
- Reporting-a-Vulnerability email contact: `security@craton.co` → `security@craton.com.ar`
  (line 160). The section now provides a clear, working vulnerability-reporting contact at the
  craton.com.ar domain: GitHub Security Advisories (preferred private channel) **plus**
  `security@craton.com.ar` email, with the existing 7-day acknowledgement commitment.

### MAINTAINERS.md
- Steward contacts: `hello@craton.co` → `hello@craton.com.ar` (general) and
  `security@craton.co` → `security@craton.com.ar` (security) (lines 13-14).

### SUPPORT.md
- General-inquiries email: `hello@craton.co` → `hello@craton.com.ar` (line 41).
- Commercial-support email: `support@craton.co` → `support@craton.com.ar` (line 56).

## Verification

- Grep over the three owned files confirms **zero** remaining `@craton.co` email addresses; all
  company-domain contacts now use `craton.com.ar`.
- Remaining `craton-co` hits in these files are exclusively GitHub org/repo URLs and the
  `@craton-co/cratonvm-maintainers` team handle — intentionally preserved (see Decision above).
- Documentation-only; no Rust touched, build unaffected.

## Out-of-scope (not owned by this task) — for the owner / other agents

The same `@craton.co` email pattern still exists in files **not** owned by this task and should be
reconciled to `craton.com.ar` by their owners for full consistency:
- `CODE_OF_CONDUCT.md:48` — `conduct@craton.co`
- `fuzz/README.md:92,94` — `security@craton.co` (security-contact metadata)

If the owner later decides to also migrate the GitHub org slug (`craton-co` →
e.g. a `craton.com.ar`-aligned org), that is a coordinated, repo-wide change touching
`Cargo.toml` (all crates' `repository`/`homepage`), `.github/CODEOWNERS`, `FUNDING.yml`,
`README.md`, and the live GitHub org/team — out of scope for a documentation-only domain alignment.
