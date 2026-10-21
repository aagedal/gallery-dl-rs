# Postprocessor source oracle

`source.json` contains calls to the installed, unmodified gallery-dl 1.32.17
`HashPP` and `MtimePP` classes. It records effective hooks, metadata mutations,
filename rebuild requests, and directory timestamp effects. Source module SHA256
checksums and package version are embedded in the fixture.

Regenerate without network using a Python environment with gallery-dl installed:

```sh
python tests/fixtures/postprocessors/generate.py
```

Hash cases cover all supported fixed-output algorithms, aliases, option shape and
precedence, repeated metadata keys, empty mappings, chunk size zero/negative and
boolean behavior, filename truthiness, and custom events. Hash values are actual
source results for `abc`. Mtime cases cover file metadata, numeric strings,
missing/zero/null values, malformed input, key/template selection, directory
updates, and event selection. Test directories and timestamps are deterministic;
no personal files or remote services are used.

`job_context.json` records nine genuine `DownloadJob.run` traces with real
metadata modify/delete hooks and synthetic downloads. It distinguishes shared
metadata objects, precomputed independent dictionaries, and copies made after
the Directory message, as well as eager/lazy initialization, empty posts and
no-result jobs. Object identities use stable labels and temporary paths are
normalized. Source checksums and the complete replay script are embedded in
`generator_source`: write that string to a temporary `.py` file and pass the
output JSON path as its sole argument. Regeneration was byte-identical. The native keyword replay reconstructs the original emitter inputs, applies the
real native metadata hooks and ownership engine, and compares all 204 context
snapshots plus final contexts. Path-state and object-identity records remain
additional source evidence; the replay does not assert all native path/alias
behavior.

`pixiv_ownership.json` consumes six genuine Pixiv/Pixivision/Sketch/Novel `items()`
generators one yield at a time and applies actual metadata modify/delete hooks
between yields. It records shared roots, per-file overwrites, novel increments,
and nested aliases. Source checksums and a complete deterministic replay script
are embedded. Novel mutation-driven counters and cross-object nested aliases
remain native work; the fixture preserves that evidence independently.

`init.json` contains 16 actual `Job._init()` truthiness cases. `metadata_paths.json`
contains 13 actual MetadataPP/PathFormat destination cases. Both embed source
checksums and replay scripts.

`formatter_dates.json` calls the actual source formatter in default and identity
modes for 1,230 cases, runs 32 actual MetadataPP modifications, and publishes
nine genuine Danbooru DownloadJobs. `formatter_dates_generate.py` imports the
frozen date-bound and booru job generators under `../source_archive`; regenerate
with gallery-dl 1.32.17 and Python 3.15. Typed descriptors distinguish ordinary
strings and numbers from datetime objects and the falsy NullDatetime sentinel.
The jobs assert paths, published bytes, five hook phases, and SQLite identities;
45 additional typed callback snapshots preserve datetime identity before export.

The native replay covers 1,144 cases through the JSON-backed Template APIs and
80 preexisting datetime cases through the same renderer with typed values. It
also replays 28 modifications and all nine jobs. Explicit remaining boundaries
are four Python strings containing unpaired surrogates, two raw renderings with
fractional-second timezone offsets, and four preexisting subsecond datetime
modifications that the current whole-second JSON identity cannot represent.
These cases stay in the source fixture and are counted separately.

A separate `offset_cases` supplement contains 72 actual formatter evaluations
and 12 MetadataPP runs. It records shifted NullDatetime payloads, chained epoch,
string, repr, JSON and identity conversions, scalar attributes, and year-one /
year-9999 overflow. The original 1,230/32/9 cases remain unchanged. Native replay
retains the invalid sentinel's falsy value and public marker while checking its
underlying shifted timestamp through copied values.

`formatter_offsets.json` freezes the actual source `O` parser and formatter,
MetadataPP copies, and booru DownloadJobs. It separates parse errors from render
errors and records typed values, publication paths, bytes, and SQLite identities.
Arguments cover Unicode decimal digits, underscore and whitespace syntax, minute
sign rules, timedelta bounds, and arbitrary-size cancellation. Local offsets are
recorded in isolated Python subprocesses with `TZ=UTC`, `Europe/Oslo`, and
`Asia/Kathmandu`; the native replay also isolates each timezone in its own process.
Each group embeds the exact TZif bytes, length and SHA used by the source.
The generator verifies that copied files reproduce all source observations;
native 64-bit Unix subprocesses use those files and verify their hashes, so historical
expectations do not depend on the host timezone database. Other platforms remain explicit local-time replay boundaries; the captured dates
require a timestamp range spanning years one through 9998. Regenerate using `formatter_offsets_generate.py`
with gallery-dl 1.32.17 and Python 3.15; keep the adjacent date fixture generator
and the `../source_archive` generators in their relative layout.

The main offset corpus contains 594 direct evaluations, 100 modifications, and
four publication jobs. Six direct cases and two modifications retain explicit
aware-UTC representation boundaries; the rest are replayed. Each timezone group
contains 90 direct evaluations and 22 modifications, with six direct and two
modification awareness boundaries. Twenty-four direct cases per zone use typed
datetime inputs to preserve microseconds around a late-year DST transition.

An additional four numeric cases and two cases per timezone prove that `O`
arguments do not require a trailing slash. The final frozen JSON SHA-256 is
`d726fb2d565df2b3d444e0c1cb0a1745269e08de9a2db5467023ec9be9c67bf2`.
