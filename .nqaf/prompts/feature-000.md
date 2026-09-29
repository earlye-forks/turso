# Bootstrap: mark this repo as a fork tracked via NQAF

## Problem [feature-000]

This repo (`fork.txt`) is a straight mirror of `upstream.txt`
(`tursodatabase/turso`) with no indication in the repo itself that it
carries locally-applied fixes, or where to find them.

## Fix

Add a short `## About This Fork` section near the top of `README.md`: after
the centred logo/badge block and its `---` separator, and before the
`## About` section. State:

- This repository is a fork of the upstream project (link to
  `https://github.com/tursodatabase/turso`).
- It carries a set of security and correctness fixes applied on top of
  upstream, tracked as prompts rather than as a diverging code history.
- Anyone re-mirroring this fork from a newer upstream release should look at
  the prompts that produced the current fixes (do not hardcode a path to the
  `nqaf` repo here — just say "see the fork's NQAF prompt history").

Do not modify any `Cargo.toml` `repository`/`homepage` metadata fields, or
package metadata in the bindings (`package.json`, `pyproject.toml`, etc.).
This project is consumed as a git dependency, not published under a new
name, so there is no registry discoverability problem to solve by rewriting
package metadata — doing so would be speculative scope creep.

Do not make any other changes.
