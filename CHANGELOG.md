# Changelog

All notable changes to gallery-dl-rs are listed here. The project follows
[Semantic Versioning](https://semver.org/); README.md's "Stability" section
lists what 1.x keeps compatible.

## [1.0.0] - Unreleased

The first stable release: a native Rust port of
[gallery-dl](https://github.com/mikf/gallery-dl) that needs neither Python nor
a gallery-dl installation.

### Highlights

- Native extractors for nearly every URL form in upstream gallery-dl's test
  suite, including Instagram, Facebook, Twitter/X, Pixiv, DeviantArt, Bluesky,
  Mastodon, Kemono/Coomer, Reddit, Tumblr, Pinterest, Flickr, Imgur, the
  boorus, and ports of upstream's remaining extractor modules.
- Files carry upstream's keyword dictionaries and default paths, so upstream
  filename and directory templates, filters and postprocessors work unchanged.
- Upstream configuration files, `cache.sqlite3` OAuth tokens and SQLite
  download archives are read and written in upstream's formats.
- Exit codes use upstream's bit flags; `-j` prints upstream's message lists.
- Browser cookie import (Chromium-based browsers on macOS, Firefox profiles),
  resumable downloads, and video handoff to yt-dlp or yt-dlp-rs.

### Changed

- **`--extractor native` is now the default.** Pass `--extractor
  compatibility` to delegate extraction to an installed gallery-dl as before.
  An unsupported URL now exits with upstream's code 64.
- **Licensed GPL-2.0-only**, matching gallery-dl, whose extractors the native
  code ports (previously GPL-3.0-only). Every linked dependency is
  GPL-2.0-compatible, which CI checks with `cargo deny`.
- HTTPS uses the operating system's TLS stack on macOS and Windows, and rustls
  with the RustCrypto provider elsewhere, verifying against the system's root
  certificates (Linux needs a CA bundle such as `ca-certificates`). On macOS
  and Windows, `ciphers` and `tls12: false` warn and are ignored; `truststore`
  no longer changes anything.
- LZMA entries written by the `zip` postprocessor use liblzma.

### Fixed

- e621 and e926 read the post API format those sites switched to in November
  2026 (bare post arrays, `files`/`stats`/`has` objects, flat tags), which
  gallery-dl 1.32.21 cannot read. Keywords keep upstream's legacy layout, and
  e926 media on `static1.e926.net` is no longer dropped.
- TheHentaiWorld reads the site's minified markup instead of returning its
  login link as the image.
- fappic.com images, and urlgalleries galleries that link them, are found
  again.

### Known limitations

- Logged-in Instagram, Facebook and Twitter/X extraction has not been verified
  against live accounts.
- `-g` and `-j` resolve queued URLs completely, like upstream's `-G` and `-J`;
  upstream's `-g` prints the queued URLs of the first level instead.
- See ROADMAP.md for remaining per-site differences.
