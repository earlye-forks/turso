---
status: accepted
---

# IO extensions are required methods on `IO`

The fork needs capabilities that upstream turso's `IO` trait doesn't
have. The first is `sync_parent_dir`, which the orphan-WAL discard
needs: it makes a newly created db file durable before the first WAL
frame is written. We add each such **IO extension** as a **required**
method on `IO`, not as a provided method with a default. The
behaviour of every in-tree impl is known. A compile error then forces
every other impl, including out-of-tree ones, to decide what the
capability means for it, rather than silently inheriting a default.
An impl that can't honestly provide the capability (such as `VfsMod`
or browser `Opfs` for directory sync) returns an unsupported error,
and the feature that needs the capability refuses to run rather than
falling back to a weaker guarantee.

## Considered Options

- **A provided method with an `Unsupported` default**, like the
  existing `supports_shared_wal_coordination()`. Rejected: an
  out-of-tree IO would pick up `Unsupported` without anyone deciding
  it, and the first sign would be a runtime refusal. Upstream is more
  likely to accept this shape, so it may come back if the extension
  is proposed upstream.
- **A single interface reached by a hop**
  (`fn dir_sync(&self) -> Option<&dyn DirSync>`). Rejected: this pays
  off only for a group of related capabilities behind one interface,
  and there is one method. The hop itself still has to go on `IO`,
  either with a default (the silent opt-out again) or required (the
  same breaking change plus an extra trait).
- **COM-style `query_interface(&self, iid) -> Option<..>`** covering
  many unrelated optional interfaces. Rejected for now: we expect
  neither many related extensions nor many unrelated ones.

## Consequences

- This fork is maintained by re-implementing each fix from scratch
  against fresh upstream, so the cost of a required method is that
  each re-apply touches every `impl IO` in the tree, including any
  that upstream has added since. It does not create merge conflicts.
- **Revisit when the extension set grows.** Reconsider
  `query_interface`, or a single hop interface, once the fork carries
  two or more IO extensions, or once one of them needs a group of
  related methods. The current set is listed under "Fork Features" in
  `README.md`.
- This ADR was added alongside the change that introduced the first
  IO extension. It is maintained in the fork's NQAF prompt history;
  change it there, not by editing this copy.
