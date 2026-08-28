# Task 14 — Implement natural-language temporal reasoning

## Roadmap position

This task makes phrases such as “last week,” “before Tuesday,” “the latest,” and “before it disappeared” usable by deterministic catalog and revision APIs. The model may extract a normalized constraint, but Rust must resolve dates, boundaries, time zones, and revisions.

## Objective

Create a temporal interpreter and resolver that convert supported natural-language expressions into typed constraints, evaluate them against backup timestamps and revision intervals, and return deterministic selections with explicit ambiguity and completeness status.

## Current repository analysis

The repository uses Unix-second timestamps in src/core/indexes.rs and catalog metadata. src/core/catalog/model.rs stores present_from/present_until timestamps, first/last seen values, and latest restorable backups. Cargo.toml already includes chrono, but there is no temporal AI module or timezone policy. Search/explore currently accept user-facing path/query/history flags but do not expose a general temporal constraint language.

Task 03 supplies structured generation and Task 13 consumes the normalized result. Task 12 supplies histories and metadata-only scans. Do not let a model compare raw date strings, infer local time from prose during execution, or choose a revision by recency without a typed resolver.

## Temporal contract

Define a TemporalConstraint enum with explicit variants, for example:

- Instant at a resolved UTC timestamp;
- Interval with start/end and half-open/closed boundary policy;
- Before or after an instant;
- RelativeCalendarPeriod such as previous week/month;
- LatestBefore an event or cutoff;
- RevisionBeforeDisappearance;
- Unknown/Ambiguous requiring clarification.

The extracted form should include original phrase, granularity, calendar/time-zone assumptions, resolved UTC range, source of reference time, and an interpretation version. User-visible responses may include the assumed local date/time zone. Never hide a default that materially changes the result.

Define a ReferenceClock and TimeZonePolicy. Use the user’s configured timezone or system local timezone when explicitly supported, and convert calendar boundaries to UTC with a documented DST policy. A fixed offset or chrono-tz strategy is acceptable; the important requirement is that repeated resolution receives the same clock, timezone, locale policy, and interpreter version. Tests must use a fixed clock rather than the wall clock.

## Extraction and resolution flow

Use a versioned structured prompt to extract only supported temporal fields. It may classify “last week” as a relative period, but must not output final timestamps without the reference context supplied by Rust. Reject unsupported phrases or return a clarification request instead of silently mapping them to a broad interval.

Rust then:

1. normalizes the extracted fields;
2. resolves the reference date and timezone;
3. computes the UTC start/end using documented calendar arithmetic;
4. validates ordering and maximum span;
5. applies the constraint to backup timestamps, revision presence intervals, or event windows;
6. returns matches with the exact predicate and assumptions used.

Use half-open intervals [start, end) for calendar periods unless a specific user-visible API says otherwise. Define “before Tuesday” as before the start of Tuesday in the chosen timezone, not before an arbitrary midday. Define “latest” as the newest eligible item under a deterministic sort; distinguish latest indexed from latest restorable. Define “before it disappeared” as a relation to a disappearance window from Task 18, not as a guessed deletion instant.

Revision selection must use interval arithmetic. A revision is present at time t when present_from is at or before t and present_until is absent or after t, with the exact boundary policy documented. If multiple revisions tie, sort by backup timestamp, backup ID, revision ID, and path according to a fixed rule. Filter restorable results only after resolving the temporal relation, and explain when the best historical match is not restorable.

## Ambiguity and completeness

Handle “last,” “recent,” “Tuesday,” locale-specific dates, missing year, and daylight-saving transitions explicitly. If a phrase cannot be resolved from the policy, return a typed AmbiguousTemporalConstraint containing candidate interpretations and ask the user in interactive mode. JSON mode returns that object and never waits.

Propagate catalog indexed-through timestamps and degraded/pending state. A temporal query beyond the indexed range must state that the answer is incomplete. Do not produce “no file existed” from a period the catalog did not cover.

## Tests and acceptance criteria

Use a fixed reference clock and timezone fixtures to test:

- previous week/month boundaries across month/year changes;
- “before Tuesday,” “after,” exact instants, and date-only phrases;
- UTC conversion, DST spring/fall transitions, and fixed-offset fallback;
- latest-before-cutoff selection and restorable versus non-restorable results;
- revision intervals at exact start/end boundaries;
- disappearance-relative constraints with missing/ambiguous event windows;
- unsupported/ambiguous phrases, maximum spans, and malformed model output;
- deterministic serialization, interpretation version, and repeatability;
- JSON clarification responses with no prompt and interactive clarification through the shared confirmation/input abstraction.

The task is complete when every accepted temporal phrase becomes a typed, testable constraint, every revision choice is reproducible from explicit inputs, and limitations or assumptions are visible. The language model may interpret wording, but it must never be the authority for a timestamp or revision selection.

## References

- [chrono FixedOffset documentation](https://docs.rs/chrono/latest/chrono/struct.FixedOffset.html) — timezone-aware timestamp handling.
- [chrono documentation](https://docs.rs/chrono/latest/chrono/) — date and duration operations.
- [GIB catalog model](../src/core/catalog/model.rs) — revision presence intervals and timestamps.
- [GIB backup indexes](../src/core/indexes.rs) — Unix-second backup time semantics.

