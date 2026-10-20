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
output JSON path as its sole argument. Regeneration was byte-identical. These
traces document pending emitter-specific context and path-lifetime parity;
they are source evidence, not a claim that the native runtime matches every case.
