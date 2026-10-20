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
