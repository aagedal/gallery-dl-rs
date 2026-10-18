# gallery-dl-rs

An early Rust implementation of a gallery downloader, inspired by
[gallery-dl](https://github.com/mikf/gallery-dl). The first target sites are
**Instagram, Facebook, and Twitter/X**.

**This is a working foundation, not a native Rust replacement for gallery-dl yet.**
The CLI, direct-link extraction, and HTTP download pipeline are Rust. Social-site
extraction defaults to a separately installed `gallery-dl` executable and reads
its JSON messages. An experimental native Instagram single-post extractor is
available with `--extractor native`; it has not yet been verified against a live
authenticated post.

## Build and run

Requires a current stable Rust toolchain. The default social extractor additionally
requires `gallery-dl` on `PATH` (for example, install it with `pipx install gallery-dl`).
See [upstream installation instructions](https://github.com/mikf/gallery-dl#installation).

```sh
cargo build --release --locked

# List media URLs from a post without downloading them
./target/release/gallery-dl-rs -g 'https://www.instagram.com/p/SHORTCODE/'

# Extract using your session cookies, then download using Rust
./target/release/gallery-dl-rs -C cookies.txt 'https://www.instagram.com/p/SHORTCODE/'
./target/release/gallery-dl-rs -C cookies.txt 'https://www.facebook.com/photo/?fbid=PHOTO_ID'
./target/release/gallery-dl-rs -C cookies.txt 'https://x.com/USERNAME/status/POST_ID'

# Native direct media downloads need no Python or gallery-dl installation
./target/release/gallery-dl-rs --jobs 4 -d downloads 'https://example.com/photo.jpg'

# Save available post metadata beside downloaded media
./target/release/gallery-dl-rs --write-metadata -C cookies.txt --extractor native 'https://www.instagram.com/p/SHORTCODE/'

# Keep completed-media history across runs, including refreshed social media URLs
./target/release/gallery-dl-rs --download-archive history.sqlite3 -C cookies.txt 'https://x.com/USERNAME/status/POST_ID'

# Choose native filenames from available post metadata
./target/release/gallery-dl-rs -f '{post_id}_{media_id}_{index:03}.{extension}' -C cookies.txt --extractor native 'https://www.instagram.com/p/SHORTCODE/'

# Inspect extracted media jobs as JSON; private request options are omitted
./target/release/gallery-dl-rs -j 'https://x.com/USERNAME/status/POST_ID'

# Experimental Instagram extraction in Rust, without gallery-dl
./target/release/gallery-dl-rs --extractor native -C cookies.txt -j 'https://www.instagram.com/p/SHORTCODE/'

./target/release/gallery-dl-rs --help
```

The URLs above illustrate argument syntax; replace placeholders with real posts.
`--cookies` accepts a Netscape cookie file. `--config` accepts an explicit gallery-dl
configuration for site-specific settings. Default gallery-dl config files are
ignored. `--extractor-command /path/to/gallery-dl` selects another executable;
it is invoked directly, without a shell.

`--extractor compatibility` is the default. `--extractor native` supports Instagram
`/p/`, `/reel/`, and `/tv/` single-post URLs only; Facebook, Twitter/X, profiles,
stories, and albums still need the compatibility backend. There is no automatic
fallback. Native mode rejects `--config`, which contains gallery-dl settings.

The native extractor reads Netscape cookies scoped to the API URL's domain, path,
HTTPS requirement, and expiry. Response `Set-Cookie` updates, including session
and CSRF rotation and deletions, are retained in memory across redirects, retries,
and later posts on the same extractor instance. Updates validate domain scope
against a compiled public-suffix list, honor path/expiry rules, and protect secure
cookies from insecure replacement. Server cookies are limited to 4 KiB each;
new identities are accepted while the jar has fewer than 256 entries (larger
input files are still loaded). The input file is never rewritten. This does not
log in, renew an expired session, or import browser sessions.
It follows up to five redirects within the same origin and
`/api/` paths, reselecting cookies and the CSRF token for each URL. Redirects to
login/challenge pages fail explicitly, as do authentication/session failures and
missing posts. `--retries` controls additional attempts for native HTTP 429,
HTTP 5xx, and send/read transport failures as well as downloads. Authentication,
missing-post, rejected-redirect, and parser errors are not retried; this includes
rate-limit payloads returned with HTTP 200. `--extraction-timeout` bounds the entire
native operation, including backoff and cooldown waits, with a 16 MiB response
limit per attempt. Library callers retain zero retries with `Instagram::new` and can opt in
with `with_retries`.

Native responses include all carousel children in order and select the largest
available photo or direct video variant by pixel area. Missing children or video
variants fail extraction rather than silently downloading a partial post or video
thumbnail. JSON includes post/media IDs, shortcode, username, caption, timestamp,
and a one-based carousel index. Media requests carry user-agent/referer headers;
Instagram session cookies are never copied to media hosts.

## Current behavior

- By default Instagram, Facebook, and Twitter/X URLs use the compatibility backend, including
  its child-extractor resolution. Actual URL coverage and login requirements
  depend on the installed gallery-dl version and the account's access.
- Direct media links use a native extractor. Arbitrary HTML galleries are not
  implemented in this milestone.
- Download mode pulls extraction results one input at a time and feeds at most
  `--jobs` active downloads. Downloads can finish while the next input is being
  extracted; a full download pipeline pauses extraction of later inputs.
  `-g` emits deduplicated URLs as inputs finish; `-j` retains one ordered JSON array.
- Media bodies stream to disk with bounded download concurrency. Each completed
  file is published from a temporary file without overwriting an existing file.
- `--write-metadata` saves `<media filename>.json` beside completed media, including
  files skipped because they already exist. Sidecars have `schema_version: 1`,
  `category`, the saved `filename`, and optional `metadata` (post/media IDs,
  shortcode, username, caption, timestamp, and carousel index). Post metadata is
  provided by native Instagram and allowlisted compatibility Instagram,
  Twitter/X, and Facebook fields when available. Compatibility timestamps are
  retained only when numeric; formatted backend dates are omitted. Instagram
  uses site post/media IDs, Twitter uses tweet ID and file number, and Facebook
  uses `id.extension` as media ID to distinguish audio and video. Non-Instagram
  shortcodes are empty; unavailable fields stay empty/null. URLs, request headers,
  cookies, and private adapter options are excluded.
  Existing sidecars are never overwritten. Sidecar publication is atomic but is
  separate from media publication: a sidecar failure returns a nonzero status and
  keeps the media; rerunning backfills missing sidecars without downloading again.
  Cancellation between the two publications can also leave a missing sidecar.
  This flag conflicts with `-g` and `-j`; inspection JSON retains its existing format.
- `--download-archive FILE` (alias `--archive`) keeps completed-media history in
  SQLite. Social identities use stable, site-specific post/media identifiers,
  independent of signed URLs or headers. Missing or ambiguous social IDs are
  downloaded normally without an archive entry. Direct links use a hash of the
  full URL and request context, so changed direct URLs remain distinct. Entries
  store only namespaced hashes and completion times, without URLs or credentials.
  Archive mode serializes downloads of the same stable identity within the run:
  completed items are skipped, while a failed URL leaves later alternatives usable. Files skipped
  because they already exist are recorded after any requested metadata repair;
  failed downloads or sidecars are never recorded. A database write failure keeps
  completed media and returns a nonzero exit status; rerunning can record it.
  Archive hits skip without checking the filesystem, even if files were removed
  or `--write-metadata` was enabled later. Omit the archive to backfill sidecars.
  Extraction still runs to discover media identities. Separate simultaneous runs
  can both download an archive miss; inserts are idempotent. The parent directory
  must exist, and this archive format is separate from Python gallery-dl archives.
  This flag conflicts with `-g` and `-j`.
- Filenames use a SHA-256 digest of the media URL and job/request context plus a
  sanitized filename, under `destination/category/`. Exact duplicate jobs within
  a run are removed, and existing paths are skipped for direct downloads. Bare
  direct links keep their original URL-only digest; media with headers now use a
  different digest and may be downloaded again after upgrading.
- `-f` / `--filename` replaces the default digest-prefixed basename with a native
  template under `destination/category/`. Available fields are `{filename}`
  (including extension), `{name}` (without the final extension), `{extension}`
  (without the dot), `{category}`, `{post_id}`, `{media_id}`, `{shortcode}`,
  `{username}`, `{index}`, and `{timestamp}`. Only index supports zero-padding,
  such as `{index:03}` or `{index:03d}`. `{{` / `}}` escape braces; all output then
  uses safe ASCII filename characters, so literal braces become underscores.
  Unknown fields/formats, missing values, and literal directory paths fail
  explicitly. Substituted values are sanitized; names are limited to 240 bytes
  while preserving the final extension. Include enough IDs to make names unique:
  files at colliding paths are assumed complete and skipped without overwriting.
  Changing the template does not clear archive history. This is a limited native
  syntax; gallery-dl expressions, conversions, conditionals, and directory
  templates are not supported. This flag conflicts with `-g` and `-j`.
- Transport errors, HTTP 429 rate limits, and HTTP 5xx responses have bounded
  retries controlled by `--retries`. Retry-After accepts seconds or an HTTP date;
  missing/invalid values use exponential backoff. Concurrent downloads share
  cooldowns by origin (scheme, host, and port), including cooldowns reported on a
  final attempt. Native Instagram reuses the same scheduler, sharing cooldowns
  across calls on an extractor instance and recording them even on a final
  attempt. Requests already in flight can finish. `--timeout` bounds each media
  attempt, including its redirect chain and cooldown waits; longer cooldowns fail
  with retry-later advice.
  Native extraction uses its overall `--extraction-timeout` deadline. The
  compatibility backend retains its own retries and separate extraction timeout.
- Failed inputs do not prevent other inputs from being processed. Any failed
  extraction or download results in a nonzero exit code. Status goes to stderr;
  `-g` and `-j` output goes to stdout.
- Ctrl-C cancels extraction, active downloads, and retry/cooldown waits, removes
  temporary downloads, and stops the directly owned compatibility process. Files
  already published are kept. Cancellation exits with status 130.

## Limitations

Facebook and Twitter/X extraction is **not native Rust**, and live authenticated
social downloads have not been verified in this repository. The native Instagram
endpoint is undocumented and may change; fixture tests establish parser and
transport behavior, not current live coverage. Tests exercise all three routing paths
with a fixture backend and HTTP downloads with a local server. Facebook coverage
in particular must be checked against the installed backend before relying on it.

Only direct HTTP(S) media is downloaded. Extraction retains typed direct HTTP,
video-page, and HLS/DASH jobs in input order. `-g` preserves the original `ytdl:`
source; `-j` includes the job kind and manifest protocol/base URL. Inline manifest
data, headers, cookies, and other private handoff options are omitted from JSON.
The JSON output is for inspection, not a lossless job export. Different inline
manifests with the same source can therefore look identical in inspection output.

Manifest links (`.m3u8`/`.mpd`, case insensitive) can be inspected directly.
Compatibility extraction retains `_ytdl_*` and `_fallback` options for a future
adapter, including inline manifests and manifest-specific headers. Handoffs
require an HTTP(S) target/base URL, including synthetic `ytdl:https://…` URLs;
synthetic non-HTTP identifiers and unknown manifest types fail extraction.
Video-page and manifest downloads fail explicitly before any request or file
creation, even if a destination file exists. A stable archive hit can skip an
already completed logical media item before download-type validation.
No companion dependency or video
adapter is enabled yet. The parser follows the
[upstream handoff protocol](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/downloader/ytdl.py).

HLS/DASH downloads, video merging, fallback URL handling, browser cookie import,
profile pagination in Rust, and full gallery-dl filename/config compatibility are
not implemented.
Media downloads follow up to five redirects within the same origin (scheme,
host, and effective port), retaining the original destination name and request
headers. Cross-origin redirects, HTTPS downgrades, URL credentials, malformed
redirects, and loops fail without requesting the target. The Rust downloader
forwards per-item `_http_headers`, but does not import the backend's session cookie
jar or TLS/browser impersonation. Media requiring those features may fail even
when extraction succeeds.

Each social input still buffers its entire extraction result before handing media
to the download pipeline. Very large profiles can exhaust memory or hit
`--extraction-timeout`; start with individual posts. Download mode retains at most
one input’s pending media plus active jobs, while the deduplication set keeps a
fixed-size SHA-256 digest per unique job across all inputs. `-j` additionally buffers
all unique media for its JSON array. True per-item extraction streaming and native
pagination remain planned. Signed URL changes produce new filenames; use the
download archive to skip previously saved social media by stable IDs. Existing files are assumed complete,
without revalidation.

## Development

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

`src/extract.rs` owns URL dispatch and the compatibility protocol.
`src/download.rs` owns request headers, downloads, retries, and file publication.
`src/archive.rs` owns SQLite completed-media history and archive-aware downloads.
`src/naming.rs` owns native filename template parsing and safe basename rendering.
`src/transport.rs` owns redirect validation, per-origin cooldowns for downloads
and native extraction, and Retry-After parsing.
`src/main.rs` owns CLI orchestration and process exit status.
`src/instagram.rs` owns the experimental native Instagram extractor;
`src/cookies.rs` owns scoped Netscape cookie selection. See the
[fixture and manual-check notes](tests/fixtures/instagram/README.md) before
promoting native extraction to the default.

See [ROADMAP.md](ROADMAP.md) for the native extractor work. This repository's
existing [GPL-3.0 license](LICENSE) is retained. The gallery-dl backend is a separate
program; this project does not bundle its Python source.
