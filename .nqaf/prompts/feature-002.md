# Run CI on GitHub-hosted runners instead of Blacksmith

## Problem [feature-002]

Upstream CI runs most jobs on [Blacksmith](https://blacksmith.sh) runners
(`runs-on: blacksmith-*`) and uses Blacksmith's forks of common actions
(`useblacksmith/*`). This fork (`fork.txt`) has no Blacksmith installation,
so every such job sits queued for 24 hours and is then cancelled; on a
typical push about 110 jobs never start. Jobs that need upstream's secrets,
or that publish, release or deploy, also cannot succeed here.

Patch `.github/workflows/*.yml` and `.github/actions/**` (and
`.github/shared/**` if it ever gains a reference) so CI runs on
GitHub-hosted runners, and gate the jobs that only make sense upstream.

The inventory below is from upstream rev `8cc059b`. Workflows will have
changed since; treat it as a starting point and work from what is in the
tree now.

### Inventory at `8cc059b`

About 130 case-insensitive `blacksmith` hits in 29 files under `.github/`:
72 jobs (about 165 after matrix expansion) run on Blacksmith, 29 jobs on
GitHub-hosted or other runners.

Runner labels (literal `runs-on:`, matrix lists and `include:` values such
as `os:`, `host:`, `runner:`):

| Label | Uses | Where |
|---|---|---|
| `blacksmith-4vcpu-ubuntu-2404` | ~58 | almost every workflow |
| `blacksmith-6vcpu-macos-latest` | 10 | rust, python, napi, go, java-publish, react-native |
| `blacksmith-4vcpu-windows-2025` | 9 | rust, napi, go, c-compat, dotnet-publish |
| `blacksmith-2vcpu-ubuntu-2404-arm` | 3 | python (`linux` matrix), napi (`build` matrix) |
| `blacksmith-8vcpu-windows-2025` | 2 | rust `build-native`, python `test` matrices |
| `blacksmith` (bare) | 3 | antithesis, build-sim, build-sqlancer |

Also: one commented-out `runs-on: blacksmith-4vcpu-ubuntu-2404` in
`rust.yml` (`stress-nondeterminism-check`), and about 10 comments that name
Blacksmith (Windows images lacking LLVM or MSVC ARM64 tools in `rust.yml`,
`python.yml`, `napi.yml`, `go.yml`, `dotnet-publish.yml`; sticky-disk
propagation in `codspeed-memory.yml`; images without rustup in
`.github/actions/setup-rust-toolchain/action.yml`).

Actions: `useblacksmith/setup-node@v5` (8), `setup-python@v6` (7),
`rust-cache@v3` (7), `setup-uv@v4` (5), `cache@v5` (2), `setup-java@v5` (1),
`build-push-action@v1` (1, `build-sim.yml`), `stickydisk@v1` (2) and
`stickydisk-delete@v1` (1, both in `codspeed-memory.yml`). Some of these sit
in jobs that already run on `ubuntu-latest` (`rust_perf.yml`) or on
self-hosted runners (`perf_nightly.yml`).

## Acceptance

`grep -riq blacksmith .github` finds nothing: no runner label, action
reference, input, comment or commented-out line under `.github/` mentions
Blacksmith, in any case. Files outside `.github/` (`README.md`,
`CHANGELOG.md`, test databases) are out of scope; leave them alone.

## Runner labels

Replace every Blacksmith label with a GitHub-hosted one, wherever it
appears (literal `runs-on`, matrix values, `include:` entries, expressions):

| Blacksmith label | GitHub-hosted label |
|---|---|
| `blacksmith-*-ubuntu-2404` (x86_64, any vCPU) | `ubuntu-24.04` |
| `blacksmith-*-ubuntu-2404-arm` (any vCPU) | `ubuntu-24.04-arm` |
| `blacksmith-*-ubuntu-2204` (if any appear) | `ubuntu-22.04` |
| `blacksmith-*-windows-2025` (any vCPU) | `windows-2025` |
| `blacksmith-*-macos-latest` (any vCPU) | `macos-latest` |
| `blacksmith` (bare) | `ubuntu-24.04` |

Upstream already uses `windows-2025`, `windows-11-arm` and `macos-26` for
the jobs it keeps on GitHub-hosted runners; leave those, and any other
non-Blacksmith labels, unchanged. Map a Blacksmith label not in this table
by its OS and architecture to the nearest GitHub-hosted label, and record
the mapping in your decision file.

Delete the commented-out `runs-on: blacksmith-…` line, or rewrite it to
the mapped label; do not otherwise touch the commented-out job.

## Actions

Replace each `useblacksmith/*` action with the upstream action it forks,
keeping its `with:` inputs unless noted:

| Blacksmith action | Replacement | Notes |
|---|---|---|
| `useblacksmith/setup-node@v5` | `actions/setup-node@v5` | same inputs |
| `useblacksmith/setup-python@v6` | `actions/setup-python@v6` | same inputs |
| `useblacksmith/setup-java@v5` | `actions/setup-java@v5` | same inputs |
| `useblacksmith/setup-uv@v4` | `astral-sh/setup-uv@v4` | same inputs (`enable-cache`) |
| `useblacksmith/rust-cache@v3` | `Swatinem/rust-cache@v2` | same inputs (`prefix-key`); `@v2` is what the rest of the tree uses |
| `useblacksmith/cache@v5` | `actions/cache@v4` | same inputs (`path`, `key`); `@v4` matches the rest of the tree |
| `useblacksmith/build-push-action@v1` | `docker/setup-buildx-action@v3` step, then `docker/build-push-action@v6` | same build inputs (`context`, `file`, `push`, `tags`, `build-args`); the Blacksmith action sets up its own builder, so the buildx step is new |
| `useblacksmith/stickydisk@v1`, `stickydisk-delete@v1` | `actions/upload-artifact` / `actions/download-artifact` | see below |

Use the action versions already used elsewhere in the tree where they
differ from the table.

**Sticky disk (`codspeed-memory.yml`).** The `build` job mounts a sticky disk
at `target/codspeed` and builds into it; the `run` shards mount a read-only
copy; `cleanup` deletes it. Replace with: in `build`, an
`actions/upload-artifact` step after the build that uploads
`target/codspeed` (`retention-days: 1`); in `run`, an
`actions/download-artifact` step to `target/codspeed` in place of the mount.
Remove the `sleep 90` propagation step and the `cleanup` job, which only
exist for the sticky disk. Keep the "Restore binary permissions" step:
artifacts drop the executable bit too. Use the upload/download versions
the tree already uses (`upload-artifact@v6` with `download-artifact@v7`, or
the `@v4` pair).

## Comments

Reword comments that name Blacksmith so they stay true on GitHub-hosted
runners, without changing the code they describe. For example "Blacksmith
Windows images ship VS Build Tools without LLVM" becomes "Some Windows runner
images ship VS Build Tools without LLVM". Keep the settings and steps those
comments justify (`CC_x86_64_pc_windows_msvc: cl.exe`, the `choco install
llvm` step, the rustup cache in `setup-rust-toolchain`, `windows-2025` for
ARM64 cross builds) even if a GitHub-hosted image doesn't need them; they are
harmless and keep the diff small.

## Runner capacity

GitHub-hosted runners for public repos are: Linux and Windows x64 4 vCPU,
16 GB RAM, about 14 GB free disk; Linux arm64 4 vCPU; `macos-latest` arm64
3 vCPU, 7 GB. That is the same vCPU as `blacksmith-4vcpu-*`, but fewer than
the `6vcpu-macos` and `8vcpu-windows` labels, and the disk is smaller than
Blacksmith's.

Blacksmith jobs at `8cc059b` set `timeout-minutes` between 10 and 60
(antithesis 30, build-sim 60, build-sqlancer 45, `rust.yml` `build-native`
60, `simulator` 35, `concurrent-simulator` 40, `codspeed` builds 60,
`conformance` `sqlite-tcl` 60, `elle-check` 60, `stack-sizes` 45, napi
`test-sync-windows` 45, react-native `build-example-ios` 45, most others
30). You may:

- raise `timeout-minutes` on a job that moved to a smaller runner (the
  `6vcpu-macos` and `8vcpu-windows` jobs), or that does a full or
  all-features release build, by up to 2x;
- add a disk-cleanup step (removing preinstalled toolchains under
  `/usr/share/dotnet`, `/usr/local/lib/android`, `/opt/ghc`,
  `/opt/hostedtoolcache/CodeQL`, or a pinned
  `jlumbroso/free-disk-space` action) at the start of a Linux job whose
  build is likely to exceed ~14 GB (all-features or multi-profile `cargo`
  builds, `clippy --all-targets`, Docker image builds).

Do this only where there is a concrete reason. Record each raised timeout
and each cleanup step, with its reason, in your decision file.

## Gate upstream-only jobs

Add a job-level `if: github.repository == 'tursodatabase/turso'` to jobs
that need upstream's secrets or credentials, publish, release or deploy, or
run on upstream's self-hosted runners. Where a job already has an `if:`,
combine them: `if: github.repository == 'tursodatabase/turso' && (<existing>)`,
keeping any `${{ }}` wrapping the existing condition uses. A job-level `if:`
also works on a job that calls a reusable workflow (`uses:`).

At `8cc059b` these are:

| Workflow | Jobs | Why |
|---|---|---|
| `antithesis.yml` | `test` | `ANTITHESIS_*` secrets |
| `antithesis-schedule.yml` | `test` (calls `antithesis.yml`) | as above |
| `aristo-verify.yml` | `verify` | `ARETTA_TOKEN` |
| `build-sim.yml` | `deploy` | AWS role, ECR push (`LIMBO_SIM_*`) |
| `build-sqlancer.yml` | `deploy` | AWS role, ECR push (`SQLANCER_*`) |
| `claude.yml` | `claude` | `ANTHROPIC_API_KEY` |
| `codspeed.yml` | every job (`memory`, `discover`, `build-stable`, `build-nightly`, `benchmarks-stable`, `benchmarks-nightly`) | CodSpeed OIDC upload, registered to upstream |
| `dotnet-publish.yml` | `publish-to-nuget` | NuGet publish |
| `go.yml` | `publish` | `TURSO_GO_PLATFORM_LIBS_TOKEN` |
| `java-publish.yml` | `publish` | `MAVEN_*` |
| `napi.yml` | `publish` | npm publish |
| `perf_nightly.yml` | every job | `NYRKIO_JWT_TOKEN`, self-hosted `nyrkio_perf_server_*` runners (they would queue the same way) |
| `publish-cli-npm.yml` | `publish` | `NPM_TOKEN` |
| `publish-crates.yml` | `publish` | `CARGO_REGISTRY_TOKEN` |
| `publish-serverless-python.yml` | `publish` | `PYPI_API_TOKEN` |
| `python.yml` | `release` | `PYPI_API_TOKEN` |
| `react-native.yml` | `publish` | npm publish |
| `serverless.yml` | `conformance` | `TURSO_API_TOKEN`, `TURSO_ORG`, `TURSO_GROUP` |
| `stale.yml` | `stale` | `GH_TOKEN` |

For a job not listed, apply the same rule: gate it if it reads a
`secrets.*` other than `GITHUB_TOKEN`, publishes or deploys anything, or runs
on a runner label this fork doesn't have. Record each such job in your
decision file.

Do not gate:

- jobs whose secret use is already behind an `if:` on a repo variable
  (`dotnet-publish.yml` `build-natives` Azure signing checks
  `vars.AZURE_SIGNING_ENABLED`);
- `release.yml`. It is generated by `cargo-dist` and has no Blacksmith
  references; editing it can make `dist plan` report the workflow as out of
  date. Its publishing jobs run only on version tags. Leave it unchanged
  unless it gains a Blacksmith reference.
- `rust_perf.yml`, `elle.yml`, `labeler.yml`: they need no upstream secret
  and run fine on GitHub-hosted runners.

## Constraints

- Change only runner labels, the `useblacksmith/*` steps (and the steps the
  sticky-disk replacement needs), comments that name Blacksmith, job-level
  gating `if:`s, and any timeout or disk-cleanup changes recorded in your
  decision file. Do not change job logic, test commands, matrices (other
  than their runner labels), triggers, permissions or concurrency.
- Keep the diff small and line-local so this patch carries cleanly onto
  later upstream revisions: edit labels in place, don't reformat YAML, and
  don't factor runner labels out into variables or reusable workflows.
- Validate every changed workflow parses as YAML (for example
  `python3 -c 'import sys,yaml; [yaml.safe_load(open(f)) for f in sys.argv[1:]]' .github/workflows/*.yml`)
  and, if available, run `actionlint` on them. Do not run the full
  `make test`.

Out of scope: an upstreamable form would keep Blacksmith as the default and
let a fork override it, e.g.
`runs-on: ${{ vars.LINUX_RUNNER || 'blacksmith-4vcpu-ubuntu-2404' }}`. It
leaves `blacksmith` strings in `.github/` (failing the acceptance check
above) and still needs the `useblacksmith/*` actions swapped, so this fork
does a plain replacement.

## Update README

In the "Fork Features" subsection of "About This Fork" in `README.md`
(added by feature-000/feature-001; create it if missing), add a bullet: CI
runs on GitHub-hosted runners instead of Blacksmith, and jobs that need
upstream secrets or publish/deploy run only in `tursodatabase/turso`.
