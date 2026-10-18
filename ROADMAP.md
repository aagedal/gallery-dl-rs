# Native Rust roadmap

The compatibility backend is a bootstrap step. The intended end state is a native
Rust gallery downloader, starting with Instagram, Facebook, and Twitter/X.

## 1. Native single-post extractors

Implement one site at a time, with independently testable response parsers and a
shared extraction interface that emits media, post metadata, and child URLs.

- Instagram: photo posts and full carousels first, then direct reel/video media.
  Support logged-in sessions, redirects to login, expired sessions, and unavailable
  posts explicitly.
  **Implemented experimentally:** opt-in REST single-post extraction for photos,
  full mixed carousels, and direct video variants; scoped cookie loading, metadata,
  bounded requests, and explicit error handling. Synthetic parser fixtures and
  local HTTP transport checks are included. **Still required:** sanitized real
  response fixtures and a documented successful live authenticated manual check.
  Compatibility remains the default until those checks are complete.
- Facebook: photo permalinks and albums first, then video posts. Establish tested
  URL coverage and distinguish login pages from successful extraction.
  **Implemented experimentally:** single-photo permalink extraction from matching
  Photo objects in embedded Relay JSON, largest direct image selection, metadata,
  stable archive identity, scoped rotating cookies, and bounded transport.
  Synthetic parser and local HTTP fixtures cover identity validation and errors.
  Anonymous public check on 2026-10-18 returned no identity-bound photo; it did
  not verify successful extraction. Real response fixtures, successful live
  extraction, albums, and videos remain required.
- Twitter/X: single-post photos and direct video variants, including mixed-media
  posts and explicit unavailable/protected-post errors.
  **Implemented experimentally:** authenticated single-status GraphQL extraction,
  original photos/highest-bitrate direct MP4 variants, requested-root validation,
  ordered mixed media, metadata/archive identities, scoped cookies/CSRF rotation,
  bounded retries/deadlines/redirects, and native web transaction generation.
  Current public signing bootstrap was checked on 2026-10-18 and a modern module
  shell required a first-party `/i/jf/` fallback. Reference vectors, parser fixtures,
  and local HTTP tests cover protocol behavior. **Still required:** sanitized real
  GraphQL photo/video/mixed responses and a successful live authenticated download.
  Compatibility remains the default. Profiles, threads, retweet/quote expansion,
  and cards/articles remain outside native coverage.

Before calling a site supported natively, require sanitized response fixtures for
single-media and multi-media posts, authentication failures, missing posts, and
rate limits, plus a documented manual check against a real accessible post.
Do not commit credentials or signed media URLs from private sessions.

## 2. Shared authenticated transport

Add domain/path-scoped Netscape cookies, user-agent/referer propagation, safe
redirect handling, Retry-After-aware rate limits, and cancellation. Replace each
compatibility extractor independently as native coverage is verified. Keep the
backend explicitly selectable until the three sites have sufficient parity.

Native Instagram now uses domain/path/secure/expiry-scoped input cookies and
propagates user-agent/referer without copying cookies to media hosts. Native API
and media requests now follow up to five same-origin redirects (same scheme,
host, and effective port), reject loops/unsafe targets, and keep the entire chain
within the operation deadline. Instagram rebuilds cookies and CSRF headers for
each URL and rejects redirects outside `/api/`, including login/challenge pages.
Media redirects retain the original job identity and destination, reject redirected
manifest responses, and preserve request headers only within that origin. Local
tests cover scoped credentials, retry restart, redirect limits/loops, unsafe
locations, deadline bounds, and publication behavior. Cross-origin redirects
remain unsupported. `--retries` controls additional attempts for direct media
downloads and native Instagram HTTP 429, HTTP 5xx, and send/read transport
failures; the compatibility backend retains its own retries. Retry-After accepts
seconds or an HTTP date, with exponential backoff for missing/invalid values.
Native extraction reuses the download transport's per-origin cooldown scheduler
across calls on an Instagram instance, recording cooldowns even on a final
attempt. `Instagram::new` retains zero retries unless `with_retries` is used.
Authentication, missing-post, rejected-redirect, and parser errors are not retried,
including rate-limit payloads returned with HTTP 200. Each download attempt,
including redirects and cooldown waits, is bounded by the download timeout;
excessive delays fail with retry-later advice without sending an early retry. The
native per-input extraction timeout bounds the entire operation, including requests,
backoff, and cooldown waits.
Local HTTP tests and virtual clock scheduler tests cover retry bounds, shared
cooldowns, origin isolation, deadline extensions, and excessive delays.
CLI Ctrl-C cancellation now drops extraction/download futures, cleans
temporary downloads, and stops the directly owned compatibility process, with
exit status 130. Unix CLI tests verify partial-file cleanup and child termination;
native requests, redirects, and cooldown waits use the same cancellation scope.
Native API response cookies now update the in-memory jar before redirect/status
handling, so session and CSRF changes and deletions apply to redirects, retries,
and subsequent posts on the same instance. Domain/public-suffix validation,
default paths, expiry and Max-Age precedence, secure-cookie protection, cookie
prefix rules, and bounded server-controlled storage are covered by unit tests.
Local HTTP tests cover rotation through redirects/retries, deletion on an
authentication failure, credential isolation from media jobs, and preservation of
the input cookie file. Updates are not persisted and cannot renew an expired
session. A companion adapter will also need cancellation at its library boundary.

## 3. Large collections and reliable archives

Stream extraction into a bounded work queue; add pagination, stable site/post/media
identifiers, a SQLite download archive, media metadata sidecars, and resumable
downloads with range validation. URLs alone are not stable archive identifiers.

**Implemented:** opt-in `--write-metadata` sidecars use a versioned allowlist of
saved filename/category and available native post metadata, excluding URLs and
private request/adapter options. They publish atomically without overwriting,
backfill missing sidecars for skipped media, and preserve completed media on
metadata errors. Local tests cover schema/content, private option exclusion,
concurrent publication, repair, failed media, and CLI behavior.

**Implemented pipeline step:** download mode now pulls one input at a time into
a pipeline capped by `--jobs`. A full pipeline pauses later extraction, and active
downloads continue during pending extraction. It retains at most the current
input’s pending media and active jobs, plus fixed-size identity digests for global
deduplication. URL listing emits results incrementally in input order; JSON
inspection still buffers one array. CLI regressions cover publication during
blocked extraction, backpressure at capacity, deduplication/order, failure
continuation, and simultaneous extraction/download cancellation.

**Implemented message-streaming step:** compatibility results now flow through
an incremental resolved-array parser rather than buffering a whole Rust vector.
Only the current encoded message (up to 16 MiB, bounded nesting), stdout buffer,
and active media jobs are retained. A full download pipeline pauses reads;
diagnostics drain concurrently into an 8 KiB tail. Completed media survive late
JSON/process errors, later inputs continue, and URL listing emits each item as
received. Compatibility timeout budgets count cumulative active parsing/reading
and process waits, excluding downstream backpressure. The collecting library API
remains, alongside `Backend::stream`. Tests cover chunk boundaries, parser bounds,
early output, producer backpressure, diagnostics, failed exits/trailers, timeouts,
and lazy startup/drop cancellation. Upstream's resolved-array job still collects
internally before writing; native collection traversal is required
for end-to-end streaming without that external memory cost. Native single-post
extractors still buffer one post. Range resume is described below.

**Implemented experimental native pagination:** Instagram profiles resolve a
username to an ID, then request REST feed pages with opaque URL-encoded cursors.
Only one bounded response page and its jobs are retained, with no next-page
prefetch while current jobs remain. Owner identity, cursor presence, and cursor
cycles are checked; cursor history stores fixed-size digests. The cumulative
active timeout includes requests/retries/cooldowns and excludes downstream
pauses. Earlier pages survive late failures; dropping the stream cancels pending
requests. Local fixtures exercise order, backpressure, cursor encoding/errors,
private/foreign profiles, and deadlines. Authenticated live profile pagination
and real response fixtures remain unverified; other native collections remain.

**Implemented archive step:** `--download-archive` tracks completed media in a
versioned SQLite database. Stable namespaced identities for native and
compatibility Instagram, Twitter/X, and Facebook survive signed URL/header
rotation and distinguish carousel files and Facebook audio/video. Compatibility
post metadata now preserves allowlisted site fields. Unknown social identities
are left unarchived; direct links explicitly use full-request identity hashes.
Archive checks run before download requests, and entries publish only after
successful media and optional sidecar publication (including existing files).
Failed database updates keep media and can be repaired on rerun. Archive hits
intentionally ignore missing local files and later sidecar requests. Concurrent
processes can download the same initial miss; writes are idempotent and do not
reserve jobs. The format is separate from Python gallery-dl archives. Tests cover
rotation, identity separation, privacy, persistence, concurrent writes, foreign
databases, failure recovery, and CLI behavior. Full extraction still occurs before
archive filtering; broader native collection pagination remains.

**Implemented resume step:** opt-in `--resume` retains direct HTTP partial files
under a hidden category-local directory. Strong ETags or conservative strong
Last-Modified dates authorize conditional byte-range requests; validators,
effective target identity, offsets, totals, and body lengths are checked before
publication. Ignored ranges and changed representations restart safely, partial
ranges continue under the same attempt deadline, and matching 416 responses can
publish completed bytes. Weak/missing validators never authorize appending.
Checkpoint names bind full request identity and output name without storing URLs
or request credentials. Persistent file locks and ordered blocking writes protect
against concurrent writers and cancellation during disk I/O. Cancellation retains
resume state only when opted in; default temporary-file cleanup is unchanged.
Archive entries and metadata sidecars are produced only after complete publication.

## 4. Broader compatibility

Add native configuration and filename templates, then prioritize additional sites.
Integrate the companion **yt-dlp-rs** project for video extraction and segmented
downloads. Document compatibility differences as features land.

**Implemented filename step:** `-f` / `--filename` supports native placeholders
for filename/name/extension/category and available post IDs, username, shortcode,
index, and timestamp, with optional index zero-padding. Names remain under the
category directory, are sanitized to safe ASCII, and are bounded to 240 bytes
while preserving the final extension. Unsupported syntax and unavailable values
fail explicitly. Downloads, sidecars, archive recording, and no-overwrite behavior
share the same rendered destination. Default names remain unchanged. This is a
documented subset, not full gallery-dl formatting compatibility; directory
templates, expressions, and additional sites remain.

**Implemented configuration step:** `--native-config` accepts an explicit strict
subset of gallery-dl's JSON scopes: extractor defaults, category settings, and
root overrides, followed by explicit CLI flags. Supported settings include
base-directory, filename, cookies, archive, retries, and fractional-second timeout.
Paths are anchored to the configuration directory, and unknown/unsupported values
fail without echoing private input. The same settings drive Rust media downloads,
per-category compatibility cookie arguments, and native Instagram credentials,
retries, and per-request deadlines. The overall extraction deadline remains
separate. Inspection opens no configured archives and creates no output paths.
Tests cover schema, scope/CLI precedence, relative paths, credentials selection,
fractional timeouts, retry counts, and separate compatibility configuration.
Full configuration compatibility, discovery, directory templates, and
postprocessors remain.

Native Instagram, Facebook, and Twitter session initialization failures now remain local to
their inputs, allowing unrelated direct links and other inputs to proceed.

### Companion yt-dlp-rs integration

Prefer an optional Rust library adapter once yt-dlp-rs has a stable, consumable
release. Keep both projects independently usable; do not add a dependency on a
developer's local checkout. A separately selected executable adapter can provide
compatibility with Python yt-dlp if needed, but is not a prerequisite for the Rust
integration.

- Gallery-dl-rs owns gallery/post traversal, ordered media jobs, metadata, archive
  identity, destination naming, concurrency, and final file publication.
- Yt-dlp-rs owns delegated video extraction, format selection, HLS/DASH download,
  and optional FFmpeg merging/remuxing. Agree on output ownership: it writes into
  a caller-owned temporary directory and returns the actual completed path and
  extension; gallery-dl-rs publishes it without overwriting existing files.
- Introduce typed jobs for direct HTTP media, video-page extraction, and HLS/DASH
  manifests. Manifest jobs must carry the base URL and optionally inline manifest
  data. A `ytdl:` handoff can refer to a page or carry a synthetic URL accompanied
  by manifest metadata; stripping the prefix and downloading the URL is insufficient.
- Preserve relevant compatibility-backend metadata (`_ytdl_manifest`, inline
  manifest data, headers, and fallback URLs) when translating to typed jobs. Keep
  session cookies scoped to each request URL; never flatten them into a global
  header. Define timeouts/cancellation, progress events, format selection, typed
  errors, and the completed-file result at the adapter boundary.
- Start integration tests with local progressive media, then local HLS/DASH
  fixtures, inline manifests, authenticated segments, merge failures, cancellation,
  and cleanup/no-overwrite behavior. Keep unsupported handoffs explicit until the
  companion implements the required capability.

**Implemented preparation:** media now carries typed direct HTTP, video-page, or
HLS/DASH manifest jobs. Compatibility parsing retains the original `ytdl:` source,
an HTTP(S) manifest base URL, optional inline data, all `_ytdl_*` options, fallback
URLs, and request headers. Private options/inline data are excluded from inspection
JSON. Deduplication and destination hashes distinguish job/request context;
ordinary direct links without that context retain their existing paths. Local
tests cover parsing, malformed handoffs, private JSON fields, distinct inline jobs,
and rejection of unsupported jobs before requests or filesystem changes. Synthetic
non-HTTP identifiers are rejected because no usable base URL can be established.

**Remaining integration:** select a stable companion release and implement the
library adapter contract, scoped segment authentication, cancellation/progress,
format selection, completed-path publication, and the segmented-download/merge
fixture suite. Retained fallback URLs and opaque options are not executed yet.

The inspected local yt-dlp-rs prototype exposes `YoutubeDl`, `VideoInfo`,
`VideoFormat`, and an `Extractor` interface for direct media and limited HTML5
pages. It does not yet implement HLS/DASH, cookies, or FFmpeg postprocessing.
Those are prerequisites for the corresponding video handoffs, not functionality
that gallery-dl-rs can currently advertise. No companion dependency or runtime
delegation is enabled yet.
