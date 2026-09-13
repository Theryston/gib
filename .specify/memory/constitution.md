# Gib Constitution

## Core Principles

### I. Correctness and Recoverability First

- Correctness, recoverability, bounded resource usage, and contract stability MUST outrank
  convenience and implementation speed; no capability MAY be added by relaxing an existing
  guarantee.
- Persistent mutations MUST be atomic; interrupted operations MUST leave well-defined,
  resumable state, and multi-object mutations MUST be journaled and MUST recover
  idempotently without human repair.
- Rationale: the system holds other people's data, so an unrecoverable partial write is
  worse than a slow correct one.

### II. Layered Boundaries, Domain Purity, and Portability

- Dependencies MUST flow one way: delivery surfaces to the public programmatic interface,
  to use cases, to the domain and its ports; adapters depend on those ports, never the
  reverse.
- The domain MUST NOT depend on delivery mechanisms, concrete external systems,
  persistence or runtime technologies, presentation libraries, environment variables, or
  platform APIs, and the portable core MUST behave correctly on every supported platform
  family.
- Delivery surfaces MUST hold only input collection, parsing, and presentation; new
  capabilities MUST arrive through stable interfaces and typed extension points, never as
  vendor- or presentation-specific branches inside existing use cases. Platform-specific
  behavior MUST be isolated behind adapters and verified natively.

### III. Explicit, Single-Sourced Domain Model

- Domain concepts MUST be explicit validated types; raw strings, tuples, and ambiguous
  booleans MUST NOT carry domain meaning, and invalid states MUST be unrepresentable past
  the boundary.
- Every shared rule, default, limit, and serialized value MUST have exactly one
  authoritative definition.
- Shared behavior MUST live with the domain concept or adapter that owns it; catch-all
  modules and duplicated conventions MUST NOT be created.

### IV. Stable, Versioned, Additively Evolving Contracts

- Public interfaces, protocols, and persisted formats MUST stay backward compatible unless
  a breaking release is explicitly authorized.
- Public types MUST support additive evolution through private fields, validated newtypes,
  builders, opaque handles, and deliberate non-exhaustiveness.
- Every persisted format and public envelope MUST be explicitly versioned and kept
  separate from mutable domain models; unknown versions MUST fail with a typed
  unsupported-version error.
- Previously supported formats MUST keep migration support and committed fixtures, and
  capability selection MUST be additive, so unselected capabilities add no dependency.

### V. Fail-Closed Integrity, Bounded Resources, and Controlled Failure

- Externally supplied input, including paths, serialized plans, stored objects,
  credentials, and remote responses, MUST be treated as untrusted and validated; corrupt
  data MUST NEVER be silently repaired into valid data and integrity failures MUST surface
  as typed errors.
- Filesystem access MUST stay confined to an explicitly opened root, accounting for links,
  reparse points, and time-of-check/time-of-use races.
- Credentials MUST be encrypted at rest and MUST NEVER appear in diagnostics, output,
  temporary files, or fixtures; persisted and user-visible text MUST use one canonical
  language.
- Atomicity, cancellation, validation, encryption, and resource limits MUST NOT be
  weakened for simplicity, and resource usage MUST be explicitly bounded.
- Production paths MUST NOT abort or panic; unavoidable invariants MUST be documented and
  test-covered, and unsafe escapes MUST be narrow, isolated, and justified.

### VI. Verified, Focused, and Traceable Change

- Each change MUST address one behavior change, MUST NOT mix in unrelated formatting,
  renaming, dependency, or cleanup work, and MUST preserve uncommitted user work.
- Every fixed defect MUST gain a regression test at the lowest layer that can observe it.
- Focused tests and then the complete project gate set MUST pass; failures caused or
  exposed by the change MUST be fixed, and an inconclusive run MUST NEVER be reported as
  passing.

## Additional Constraints

- Changes touching trust boundaries, credentials, confinement, recovery, or resource
  limits MUST pass security scrutiny before merge.
- Architectural boundaries MUST be enforced by mechanical checks, not convention alone,
  and the complete gate set MUST include architectural, feature-matrix, fixture,
  fault-injection, and performance checks where the change touches them.

## Development Workflow and Quality Gates

- Relevant code, tests, and documentation MUST be inspected before editing, and the
  requested work MUST be implemented completely without expanding into unrelated changes.
- Public behavior MUST be added to the public programmatic interface first; public
  interfaces MUST be documented, with comments reserved for non-obvious invariants,
  security reasoning, protocol decisions, platform constraints, and compatibility notes.
- Documentation MUST change with the behavior it describes, and performance changes on
  critical paths MUST carry reproducible before/after measurements.
- Code MUST follow one canonical, automatically enforced formatting and naming convention.
  Small, safe, adjacent debt MAY be fixed when clearly understood and testable; larger or
  unrelated debt MUST be reported separately.
- Commits MUST stay focused on one behavior change, and handoff MUST report contract
  changes, validation results, compatibility, performance, security, and portability
  implications, debt fixed, and remaining blockers.

## Governance

- This constitution supersedes conflicting conventions; more specific local guidance MAY
  add stricter rules but MUST NOT weaken these principles.
- Amendments MUST state the modification, its rationale, the affected principles, and the
  compatibility impact, and take effect only after maintainer approval.
- Versioning is semantic: MAJOR removes or redefines a principle, MINOR adds or materially
  expands guidance, PATCH clarifies wording.
- Every change, review, and release MUST verify compliance; non-compliance MUST block merge
  until corrected or the constitution is amended as above.

**Version**: 1.0.0 | **Ratified**: 2026-09-13 | **Last Amended**: 2026-09-13
