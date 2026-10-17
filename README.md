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
HTTPS requirement, and expiry. It does not log in, refresh cookies, or import
browser sessions. It rejects redirects and reports authentication/session failures,
missing posts, and rate limits (including numeric `Retry-After` advice). It makes
one request per input, bounded by `--extraction-timeout` and a 16 MiB response
limit. `--retries` applies to downloads, not native extraction.

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
- Media bodies stream to disk with bounded download concurrency. Each completed
  file is published from a temporary file without overwriting an existing file.
- Filenames use a SHA-256 digest of the media URL and job/request context plus a
  sanitized filename, under `destination/category/`. Exact duplicate jobs within
  a run are removed, and existing paths are skipped for direct downloads. Bare
  direct links keep their original URL-only digest; media with headers now use a
  different digest and may be downloaded again after upgrading.
- Transport errors, HTTP 429 rate limits, and HTTP 5xx responses have bounded
  retries controlled by `--retries`. Retry-After accepts seconds or an HTTP date;
  missing/invalid values use exponential backoff. Concurrent downloads share
  cooldowns by origin (scheme, host, and port), including cooldowns reported on a
  download's final attempt. Requests already in flight can finish. `--timeout`
  bounds each media attempt and each cooldown wait; longer cooldowns fail with
  retry-later advice. Social extraction has a separate timeout and its own backend
  retries; native Instagram still reports rate limits without retrying.
- Failed inputs do not prevent other inputs from being processed. Any failed
  extraction or download results in a nonzero exit code. Status goes to stderr;
  `-g` and `-j` output goes to stdout.

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
creation, even if a destination file exists. No companion dependency or video
adapter is enabled yet. The parser follows the
[upstream handoff protocol](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/downloader/ytdl.py).

HLS/DASH downloads, video merging, fallback URL handling, browser cookie import,
archive databases, profile
pagination in Rust, and gallery-dl filename/config compatibility are not implemented.
Media redirects are rejected. The Rust downloader forwards per-item
`_http_headers`, but does not import the backend's session cookie jar or TLS/browser
impersonation. Media requiring those features may fail even when extraction succeeds.

Extraction results are currently buffered before downloads begin. Very large
profiles can exhaust memory or hit `--extraction-timeout`; start with individual
posts. Signed URL changes produce new filenames, so skipping existing files is
not a permanent archive. Existing files are assumed complete, without revalidation.

## Development

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

`src/extract.rs` owns URL dispatch and the compatibility protocol.
`src/download.rs` owns request headers, downloads, retries, and file publication.
`src/transport.rs` owns shared download cooldowns and Retry-After parsing.
`src/main.rs` owns CLI orchestration and process exit status.
`src/instagram.rs` owns the experimental native Instagram extractor;
`src/cookies.rs` owns scoped Netscape cookie selection. See the
[fixture and manual-check notes](tests/fixtures/instagram/README.md) before
promoting native extraction to the default.

See [ROADMAP.md](ROADMAP.md) for the native extractor work. This repository's
existing [GPL-3.0 license](LICENSE) is retained. The gallery-dl backend is a separate
program; this project does not bundle its Python source.
