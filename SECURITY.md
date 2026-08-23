# Security Policy

## Supported Versions

This repository is an actively developed pre-production codebase. Security fixes
are applied to the default branch; no stable release branch is currently
supported.

## Reporting a Vulnerability

Do not open a public issue for a suspected vulnerability. Send a private report
to `security@subhost.xyz` with the affected commit, exact file and line, impact,
reproduction steps, and any proposed mitigation. Do not include real private
keys, credentials, or production data.

We will acknowledge reports within five business days and coordinate disclosure
after a fix or mitigation is available.

## Review Scope

The maintained root-workspace scope is limited to packages under `crates/`. The
legacy `omnichain-*` and `explorer/` directories are not members of the root
workspace. No package in this repository should currently be treated as a
production network deployment.
