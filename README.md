# gallery-dl-rs

An early Rust implementation of a gallery downloader, inspired by
[gallery-dl](https://github.com/mikf/gallery-dl). The first target sites are
**Instagram, Facebook, and Twitter/X**.

**This is a working foundation, not a native Rust replacement for gallery-dl yet.**
The CLI, direct-link extraction, and HTTP download pipeline are Rust. Social-site
extraction currently runs a separately installed `gallery-dl` executable and reads
its JSON messages. Native social extractors are the next phase.

## Build and run

Requires a current stable Rust toolchain. Social URLs additionally require
`gallery-dl` on `PATH` (for example, install it with `pipx install gallery-dl`).
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

# Inspect extracted media as JSON; request headers are omitted
./target/release/gallery-dl-rs -j 'https://x.com/USERNAME/status/POST_ID'

./target/release/gallery-dl-rs --help
```

The URLs above illustrate argument syntax; replace placeholders with real posts.
`--cookies` accepts a Netscape cookie file. `--config` accepts an explicit gallery-dl
configuration for site-specific settings. Default gallery-dl config files are
ignored. `--extractor-command /path/to/gallery-dl` selects another executable;
it is invoked directly, without a shell.

## Current behavior

- Instagram, Facebook, and Twitter/X URLs use the compatibility backend, including
  its child-extractor resolution. Actual URL coverage and login requirements
  depend on the installed gallery-dl version and the account's access.
- Direct media links use a native extractor. Arbitrary HTML galleries are not
  implemented in this milestone.
- Media bodies stream to disk with bounded download concurrency. Each completed
  file is published from a temporary file without overwriting an existing file.
- Filenames use a SHA-256 digest of the complete media URL plus a sanitized
  filename, under `destination/category/`. Duplicate media URLs within a run are
  removed, and existing paths are skipped.
- Transport errors and HTTP 5xx responses have bounded retries. Each attempt has
  a timeout; social extraction has a separate timeout and its own backend retries.
- Failed inputs do not prevent other inputs from being processed. Any failed
  extraction or download results in a nonzero exit code. Status goes to stderr;
  `-g` and `-j` output goes to stdout.

## Limitations

Social extraction is **not native Rust**, and live authenticated social downloads
have not been verified in this repository. Tests exercise all three routing paths
with a fixture backend and HTTP downloads with a local server. Facebook coverage
in particular must be checked against the installed backend before relying on it.

Only direct HTTP(S) media is downloaded. HLS/DASH manifests, `ytdl:` URLs, video
merging, fallback URL handling, browser cookie import, archive databases, profile
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
`src/main.rs` owns CLI orchestration and process exit status.

See [ROADMAP.md](ROADMAP.md) for the native extractor work. This repository's
existing [GPL-3.0 license](LICENSE) is retained. The gallery-dl backend is a separate
program; this project does not bundle its Python source.
