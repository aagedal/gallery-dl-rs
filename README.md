# gallery-dl-rs

An early Rust implementation of a gallery downloader, inspired by
[gallery-dl](https://github.com/mikf/gallery-dl). The first target sites are
**Instagram, Facebook, and Twitter/X**.

**This is a working foundation, not a native Rust replacement for gallery-dl yet.**
The CLI, direct-link extraction, and HTTP download pipeline are Rust. Social-site
extraction defaults to a separately installed `gallery-dl` executable and reads
its JSON messages. Experimental native Instagram posts/profile feeds, Facebook
photos, and Twitter/X single-post extractors are available with `--extractor native`; authenticated live post
extraction has not yet been verified.

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

# Keep validated partial downloads and resume after an interruption
./target/release/gallery-dl-rs --resume -d downloads 'https://example.com/large.mp4'

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

# Experimental authenticated Twitter/X extraction in Rust
./target/release/gallery-dl-rs --extractor native -C x-cookies.txt -j 'https://x.com/USERNAME/status/POST_ID'

./target/release/gallery-dl-rs --help
```

The URLs above illustrate argument syntax; replace placeholders with real posts.
`--cookies` accepts a Netscape cookie file. `--config` accepts an explicit gallery-dl
configuration for site-specific settings. Default gallery-dl config files are
ignored. `--extractor-command /path/to/gallery-dl` selects another executable;
it is invoked directly, without a shell.

`--extractor compatibility` is the default. `--extractor native` experimentally
supports Instagram `/p/`, `/reel/`, and `/tv/` posts, Instagram `/USERNAME/` and
`/USERNAME/feed/` profiles, Facebook `/photo/?fbid=ID`, `/photo.php?fbid=ID`, and
`/USER/photos/ALBUM/ID` photo permalinks, and Twitter/X single-status URLs.
Other profiles, stories, threads, albums, and Facebook videos still need the
compatibility backend. There is no automatic fallback. Native mode rejects `--config`, which contains gallery-dl settings.
Use `--native-config` for settings applied by Rust with either extractor backend.

The native Instagram extractor reads Netscape cookies scoped to the API URL's domain, path,
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
native operation’s active time, including backoff and cooldown waits, with a 16 MiB response
limit per attempt. `--timeout` also bounds each native HTTP request (default 60
seconds), inside that overall deadline. Library callers retain zero retries with
`Instagram::new` and can opt in with `with_retries`; `with_request_timeout` sets a
separate request deadline.

Native Instagram responses include all carousel children in order and select the largest
available photo or direct video variant by pixel area. Missing children or video
variants fail extraction rather than silently downloading a partial post or video
thumbnail. JSON includes post/media IDs, shortcode, username, caption, timestamp,
and a one-based carousel index. Media requests carry user-agent/referer headers;
Instagram session cookies are never copied to media hosts.

Native Instagram profile traversal resolves the requested username to an ID and
fetches REST feed pages in order. Each post must belong to that ID. The extractor
retains at most one bounded response page and its parsed jobs; later pages are
requested only as downstream consumers drain the current page. Consumer pauses
are excluded from the cumulative extraction timeout. Missing/repeated pagination
cursors and inaccessible private profiles fail explicitly; results from earlier
pages remain available after a late failure. `Instagram::stream` supports these
collections; the collecting `extract` API remains single-post. Profile fixtures
are synthetic; live authenticated pagination remains unverified.

Native Facebook accepts only a Photo JSON object with the requested ID and its
own `image.uri`, never a global image or Open Graph preview. It selects the largest
identity-bound image and emits available metadata plus stable archive identities.
Cookies remain scoped to Facebook page requests; media receives user-agent and
referer only. Requests have bounded redirects, retries, cooldowns, response size,
and deadlines. Albums and videos are not implemented. An anonymous public page
check on 2026-10-18 returned no identity-bound photo; successful live extraction
and sanitized real response fixtures remain required. See the
[Facebook fixture notes](tests/fixtures/facebook/README.md).

Native Twitter/X requires current `x.com` Netscape `auth_token` and `ct0` cookies;
it does not log in or use a guest session. It requests the exact status through
GraphQL, selecting original-size photos and the highest-bitrate direct MP4 video
variants in mixed-media order. IDs, username, caption, UTC timestamp, and one-based
media indices feed metadata, filename templates, and the same stable archive keys
as compatibility extraction. The response must match the requested root tweet;
quoted posts and retweets are not silently substituted or expanded. Missing video
variants fail instead of returning thumbnails. Protected, missing, expired-session,
rate-limit, and challenge responses fail explicitly.

The native X client generates transaction headers from verification/SVG data and
the current signing asset, with a first-party `/i/jf/` bootstrap fallback for newer
module pages. It parses signing inputs without executing downloaded JavaScript.
Public bootstrap was checked successfully on 2026-10-18; this does **not** verify
authenticated extraction. The GraphQL operation and web signing protocol are
undocumented snapshots and can change. Cookies/CSRF are reselected on each API
request/redirect, response cookies update the in-memory jar, and API redirects
remain within the same origin and GraphQL path. Signing assets and media requests
carry no session cookies or API authorization. Requests use the same bounded
responses, retries, per-origin cooldowns, cancellation, per-request timeout, and
overall extraction deadline as the native pipeline. Parser/reference/local HTTP
tests and the [Twitter fixture notes](tests/fixtures/twitter/README.md) describe
what is verified and the remaining real-response/manual authenticated gates.

## Native configuration

`--native-config settings.json` loads an explicit, strict subset of
[gallery-dl's JSON configuration structure](https://gdl-org.github.io/docs/configuration.html).
It works with native and compatibility extraction;
`--config` remains a separate file passed only to the compatibility executable.
Neither configuration file is loaded automatically.

```json
{
  "extractor": {
    "base-directory": "downloads",
    "retries": 2,
    "timeout": 60,
    "instagram": {
      "cookies": "cookies.txt",
      "archive": "instagram.sqlite3",
      "filename": "{post_id}_{media_id}.{extension}"
    },
    "direct": {
      "filename": "{filename}"
    }
  }
}
```

Supported settings are `base-directory`, `filename`, `cookies`, `archive`,
`retries` (integer 0–10), and `timeout` (positive seconds, including fractions).
They can appear at the root, under `extractor`, or under `extractor.instagram`,
`extractor.twitter`, `extractor.facebook`, and `extractor.direct`. Precedence is
built-in defaults, extractor defaults, category settings, root settings, then
explicit CLI flags. In particular, an explicit `--retries 2` or `--timeout 60`
overrides a different configured value even though it equals the built-in default.

Configuration paths are literal and relative to the configuration file's parent
directory. CLI paths remain relative to the working directory. Cookies apply to
social extraction; retries/timeouts apply to Rust downloads and native extraction
(the compatibility executable retains its own network settings). Native extraction
also retains its overall `--extraction-timeout`. Category settings select each
media item's destination, filename, and archive. Inspection with `-g`/`-j` performs
extraction without creating configured destinations or opening archives.
The CLI also accepts fractional `--timeout` seconds; zero, non-finite values, and
timeouts outside the monotonic clock's range or precision are rejected.

Files are limited to 1 MiB. Unknown settings/categories, duplicate keys, nulls,
unsupported types, hooks, environment expansion, and conditional values fail
explicitly. Filenames use the native template subset described below; archive
files use the Rust archive format. Full gallery-dl configuration compatibility,
directory templates, automatic discovery, and postprocessors remain unimplemented.

## Current behavior

- By default Instagram, Facebook, and Twitter/X URLs use the compatibility backend, including
  its child-extractor resolution. Actual URL coverage and login requirements
  depend on the installed gallery-dl version and the account's access.
- Direct media links use a native extractor. Arbitrary HTML galleries are not
  implemented in this milestone.
- Download mode pulls one input at a time and incrementally parses compatibility
  messages into at most `--jobs` active downloads. A full pipeline pauses further
  stdout reads, applying backpressure to the process. Downloads can finish while
  extraction waits for more output. `-g` emits deduplicated URLs as individual
  media arrive, including before an input finishes; `-j` retains one ordered array.
  A later malformed message, truncated result, failed process exit, or timeout
  returns a nonzero status while keeping completed media and processing later
  inputs. Each encoded message is limited to 16 MiB with bounded nesting, and
  stderr drains concurrently while retaining only its final 8 KiB for diagnostics.
  Compatibility `--extraction-timeout` counts cumulative active reading/parsing
  and process-wait time; pauses caused by full download capacity do not count.
  Native extraction retains its overall per-input deadline.
- Media bodies stream to disk with bounded download concurrency. Each completed
  file is published from a temporary file without overwriting an existing file.
- `--resume` retains partial direct HTTP media in a hidden `.gallery-dl-rs`
  directory under the category directory, across retries, failures, and Ctrl-C.
  Subsequent attempts use `Range` and `If-Range` only with a saved strong ETag,
  or a Last-Modified value at least 60 seconds before the response Date when no
  ETag was provided. Weak/missing validators restart the full file. Full responses
  replace partial bytes; range responses must match the saved representation,
  effective URL, offset, total, and byte count. A validated 416 can finish an
  already complete partial; other 416 responses restart with a full request.
  Changed validators or redirect targets also restart without mixing bytes.
  Resume requests require identity content encoding. Checkpoint names hash the
  full request identity and destination; signed URL or header changes therefore
  start separate partials. JSON checkpoints contain only a validator, size, and
  effective URL digest; URLs, cookies, and request headers are not stored.
  File locks exclude simultaneous users of the same checkpoint and are released
  on exit. Empty `.lock` files remain to preserve locking across processes;
  completed partials/checkpoints are removed after no-overwrite publication.
  Remove an unused checkpoint directory only while no downloads are running.
  Without `--resume`, failed or cancelled partials are removed as before.
  This flag conflicts with `-g` and `-j`.
- `--write-metadata` saves `<media filename>.json` beside completed media, including
  files skipped because they already exist. Sidecars have `schema_version: 1`,
  `category`, the saved `filename`, and optional `metadata` (post/media IDs,
  shortcode, username, caption, timestamp, and carousel index). Post metadata is
  provided by native Instagram/Facebook/Twitter and allowlisted compatibility Instagram,
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
  final attempt. Native Instagram, Facebook, and Twitter reuse the same scheduler implementation, sharing cooldowns
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
  temporary downloads (or retains checkpoints with `--resume`), and stops the directly owned compatibility process. Files
  already published are kept. Cancellation exits with status 130.

## Limitations

Facebook extraction is **not native Rust**, and live authenticated social downloads
have not been verified in this repository. Native Instagram and Twitter/X use
undocumented endpoints that may change; fixture tests establish parser and
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

Rust incrementally decodes the compatibility backend's resolved JSON array,
preserving its child-extractor handling. The external backend still collects
results internally before dumping that array; very large profiles can exhaust
its memory or hit `--extraction-timeout` before any output arrives. Native
single-post extraction still buffers one complete post. The Rust download
pipeline retains one message buffer plus active jobs, while deduplication keeps
a fixed-size SHA-256 digest per unique job across all inputs. `-j` additionally
buffers all unique media for its JSON array. Native collection pagination and
end-to-end streaming remain planned. Library callers can use `Backend::stream`;
`Backend::extract` deliberately collects a complete vector. Signed URL changes produce new filenames; use the
download archive to skip previously saved social media by stable IDs. Existing files are assumed complete,
without revalidation.

## Development

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

`src/extract.rs` owns URL dispatch and the compatibility protocol.
`src/backend.rs` owns bounded compatibility process streaming and active timeouts;
`src/messages.rs` incrementally decodes individual resolved messages.
`src/download.rs` owns request headers, downloads, retries, and file publication.
`src/archive.rs` owns SQLite completed-media history and archive-aware downloads.
`src/naming.rs` owns native filename template parsing and safe basename rendering.
`src/config.rs` owns native configuration parsing, validation, and precedence.
`src/resume.rs` owns locked partial files and durable checkpoints;
`src/range.rs` validates resumable HTTP representations and ranges.
`src/transport.rs` owns redirect validation, per-origin cooldowns for downloads
and native extraction, and Retry-After parsing.
`src/main.rs` owns CLI orchestration and process exit status.
`src/instagram.rs` owns the experimental native Instagram extractor;
`src/twitter.rs` owns experimental native Twitter/X and transaction bootstrap;
`src/cookies.rs` owns scoped Netscape cookie selection. See the
[fixture and manual-check notes](tests/fixtures/instagram/README.md) before
promoting native extraction to the default.

See [ROADMAP.md](ROADMAP.md) for the native extractor work. This repository's
existing [GPL-3.0 license](LICENSE) is retained. The gallery-dl backend is a separate
program; this project does not bundle its Python source.
