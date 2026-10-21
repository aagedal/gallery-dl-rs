# Prepare hash cached-path oracle

Regenerate offline with gallery-dl 1.32.17:

```sh
/private/tmp/gdlvenv/bin/python tests/fixtures/source_archive/booru_prepare_cache_generate.py
```

The generator executes actual `DanbooruTagExtractor.items()`, `DownloadJob.run()`,
`PathFormat`, `HashPP`, SQLite archive checks and file publication. Controlled
pagination provides synthetic API posts. Only media transport is replaced with
bounded `BODY-id` writes, or a false result after the ordinary target-path build.
Source module hashes and the shared input-schema hash are recorded in the JSON.
No network requests, credentials, external programs or source edits are involved.

Each case contains its URL, API inputs, options, configured processors, initial
files, archive entries and media bytes. Each `jobs` member represents a fresh
actual `DownloadJob`, with source messages, callback/path traces, status, final
archive entries and final files. The `published` list includes preseeded files
that survive the run; `download`, `file`, `after` and `skip` trace events identify
what the actual job attempted, published or skipped. Paths omit the temporary
base directory. Metadata snapshots retain the declared `metadata_keys`; native
job proxies and source archive-cache implementation details are outside this
cached-path proof.

The twelve cases demonstrate successful predecessors, directory changes between
posts, an existing-file skip, an archive hit with a missing target, a failed
transfer, Prepare `filename:true` rebuilding the candidates for the next callback,
and isolation between separate jobs sharing a filesystem. Failure records retain
exception classes rather than potentially private exception messages.

Setup previews use a separate actual extractor, `Job.update_kwdict()` and
`PathFormat` to obtain the source's own default paths for preseed files. They do
not write those files or participate in the recorded job's lifecycle. Every
recorded Danbooru post contains one file; this fixture does not claim a same-post
multi-file producer oracle.
