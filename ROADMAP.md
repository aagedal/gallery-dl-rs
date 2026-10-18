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
  **Implemented experimental stories/highlights:** user/ID stories, selected story
  media, direct/shared highlights, user highlight lists, and the authenticated
  story tray stream through REST reels endpoints. Identity/owner validation,
  one-reel buffering, lazy ordered requests, source item indices, stable per-item
  archive IDs, and cumulative active budgets are covered by synthetic parser and
  local HTTP tests. Recognized story/highlight URLs use non-terminating date-after
  filtering for both backends, preserving later newer items in source order.
  Real response fixtures and successful authenticated extraction remain required.
  Full upstream reel metadata, highlight titles/expiry, music/sticker audio,
  split/order configuration, and segmented story video remain.
  **Implemented experimental post collections:** explicit posts, reels, tagged,
  saved posts, and saved collection routes stream bounded REST pages. Reels use
  form POST and nested paging state; other routes use GET and root cursors.
  ID selectors skip username lookup. Profile/reel owner or coauthor checks,
  creator-preserving saved/tagged metadata, stable archive IDs, page atomicity,
  cursor loops, source-order date filtering, lazy range/cancellation, and active
  budgets have synthetic/local coverage. POST retries, cookie/CSRF rotation, and
  body-preserving versus body-free redirects are tested. Live authenticated
  verification, real response fixtures, broader API variants, saved collection
  discovery, and extended collection metadata/options remain.
- Facebook: photo permalinks and albums first, then video posts. Establish tested
  URL coverage and distinguish login pages from successful extraction.
  **Implemented experimentally:** single-photo permalink extraction from matching
  Photo objects in embedded Relay JSON, largest direct image selection, metadata,
  stable archive identity, scoped rotating cookies, and bounded transport.
  Synthetic parser and local HTTP fixtures cover identity validation and errors.
  Anonymous public check on 2026-10-18 returned no identity-bound photo; it did
  not verify successful extraction. Real response fixtures, successful live
  extraction and album response verification remain required.
  **Implemented experimental videos:** watch/user-video URLs, exact Video identity,
  legacy/current inline DASH and progressive delivery, bounded XML, largest full
  MP4 plus optional M4A, separate stable archive IDs, and existing rotating-cookie
  transport. Synthetic fixtures and local tests cover quality, metadata, aliases,
  malformed audio, private errors, retries, and cookie isolation. Anonymous native
  CLI extraction on 2026-10-18 returned one MP4 for upstream's first public video
  and MP4/M4A for its second. One initial response-read failure succeeded with a
  longer timeout; separate raw capture attempts returned shells. Sanitized real
  fixtures, authenticated verification, and complete live downloads remain.
  Dynamic/segmented/encrypted/external-only manifests and merging remain unsupported.
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
  Compatibility remains the default. Scoped retweet modes and quote expansion
  now share an identity-checked,
  bounded traversal engine across single posts and profile timelines. Missing
  embedded targets fetch lazily; range/drop cancellation avoids later requests.
  Original and quoted media preserve direct-post archive identities; repost mode
  retains the repost identity. Parser, scoped configuration, compatibility
  forwarding, and local HTTP tests cover these paths. Live authenticated
  expansion remains unverified. Article defaults and optional document rendering are described below.
  **Implemented experimental profile tabs:** `/USER/media`, `/USER/tweets`,
  `/USER/with_replies`, `/USER/highlights`, and numeric ID forms stream their
  operation-specific pages. Username lookup uses identity-bound `UserByScreenName`;
  numeric IDs skip lookup unless pins require `UserByRestId` metadata. Structured
  entries, grid/conversation modules, pin instructions, inferred pin IDs, appended
  modules, and cursor/end instructions have synthetic/local coverage. Media-tab
  owners remain strict; other tabs allow validated conversation authors. Scoped
  `pinned` and `replies` settings follow upstream defaults, with independent quote
  selection and standalone/profile self-reply behavior. Per-input post dedup keeps
  media siblings while suppressing overlapping pages/pins/quotes. Backpressure,
  source-order date/range selection, active budgets, cancellation, credential
  scoping, and compatibility forwarding are tested. Authenticated live traversal,
  real response fixtures and dynamic operation refresh remain unfinished.
  **Implemented experimental search and root timelines:** ordinary/numeric profile
  roots and `/timeline` share an initial strategy-selected feed, a search for
  older links, and an unfiltered search only if the link search has no post
  results. Numeric roots resolve a canonical username before constructing the
  query. Standalone search/hashtag routes use the same strict search envelope,
  structured entries, and identity validation. Scoped strategy, result product,
  page size, empty-response tolerance, and max-ID/cursor/date pagination forward
  to compatibility mode when explicit. Raw post boundaries survive reply/text
  filtering; timestamp-aware date pagination and API keep-going hints preserve
  continuation. One cumulative active budget, deduplication set, lazy request
  scope, and range/date pipeline span all stages. Synthetic/local tests cover
  these behaviors; live authenticated search and real response fixtures remain
  unverified. Resume cursors, arbitrary root dispatch, text-only output, and
  other collection routes remain unfinished.
  **Implemented experimental account collections:** Likes profile tabs, bookmarks,
  numeric list timelines, and latest/ranked home feeds share the native pipeline.
  Exact response envelopes and present list ID aliases validate before output;
  mixed authors retain underlying post/archive identities. Scoped `limit` sets
  profile/collection page size independently of search-limit and forwards only
  explicit values to compatibility mode. First-page pins are opt-in, including a
  corrected search-pin default. Synthetic parser/local HTTP/CLI tests cover
  operation variables, owner/alias validation, cookie rotation, signed paths,
  bounded empty pages, cursor loops, expansion/dedup, dates/ranges, late errors,
  consumer pauses, and cancellation. Authenticated live collection traversal,
  real fixtures, bookmark-specific metadata, membership/follow traversal,
  text-only output, and other routes/options remain unfinished.
  **Implemented experimental community feeds:** explicit `/i/communities/ID`
  media timelines and the session's followed-community feed at `/i/communities`
  or `/USER/communities` share lazy traversal, limits, media options, expansion,
  source-order dates, cancellation, and signing. Exact operation envelopes and
  present community identity aliases are validated. `text-tweets` and the
  CommunityTweetsTimeline text variant remain unsupported. Post-owned community
  metadata uses a public allowlist, with validated IDs and optional typed fields;
  malformed optional metadata cannot supply a community archive identity. Native
  and transformed compatibility community fields support `{community_id}` and
  `{community_name}` filename/directory substitutions. Sidecars omit the banner
  URL while retaining descriptive fields. Community feed archive keys include
  community/post/file identity; ordinary statuses retain their prior archive
  identity even with descriptive community metadata. Missing/invalid IDs remain
  downloadable without stable archive entries. Compatibility subcategory values
  select community archive scope. Synthetic parser/runtime/CLI tests cover these
  distinctions, cross-feed deduplication, and shared media selection. Live
  authenticated traversal and real sanitized response captures remain pending.
  **Implemented experimental TweetDetail and conversations:** authenticated status
  Auto/Detail now select the exact focal post from structured TweetDetail pages;
  explicit restid retains the original route. Conversations false/true/accessible
  select the focal post, accessible context regardless of focal availability, or
  context gated on focal accessibility. Pre-focal context buffering is capped at
  16 MiB; owned tombstones are skipped without revealing their text. Shared quote
  targets reuse response data, missing targets fetch lazily, and original-retweet
  author resolution caches target responses. Self-reply context now follows the
  focal author for both status endpoints. Parser, selection, scoped config,
  compatibility, date, and HTTP tests cover missing/late focal posts, context
  order, identity, bounds, archives, cookies/signing, deadlines, cancellation,
  and late errors. Real response captures, authenticated live verification,
  broader thread variants and extended metadata remain.


  **Implemented experimental thread expansion and uniqueness:** scoped `expand`
  traverses each conversation ID once per input using lazy child TweetDetail
  pages. Child posts do not recursively expand; failed detail requests fall back
  to the original post even after earlier child output. Original-retweet lookup
  uses the target conversation; missing IDs skip while explicit false bypasses
  expansion. Child traversal shares parent budgets, reply context, cancellation,
  and source-order date selection. Scoped `unique: false` retains repeated post
  batches and their siblings, including across CLI inputs; once-per-conversation
  expansion stays independent. Native runtime, strict configuration,
  compatibility forwarding, and CLI tests cover these options. Authenticated live
  verification and sanitized real thread fixtures remain required.
  **Implemented experimental video/preview selection:** scoped `videos` and
  `previews` select direct MP4 files, poster images, both, or neither for videos
  and animated GIFs. File numbering follows selection, with previews immediately
  after their video. Shared parsing applies across statuses, retweets, quotes,
  and expanded thread/collection streams; disabled fields are not parsed, and
  enabled malformed media still fails atomically. Parser, local HTTP, strict
  configuration, and compatibility-forwarding tests cover selection, private
  errors, identities, archive positions, deduplication, and cancellation. The
  `ytdl` video setting emits typed fixed-first-party video-page jobs for inspection;
  downloading those jobs still requires the unimplemented companion adapter.
  Authenticated live verification and real response captures remain required.


  **Implemented experimental card media:** scoped cards false/true/ytdl and exact
  card-name/domain/name-domain blacklists apply across statuses, quotes, retweets,
  and expanded thread/collection streams. Summary image priority, both binding
  formats, ordered unified media, component previews, shared video/poster choices,
  and combined file/archive numbering have parser/local HTTP coverage. Repost
  cards use the outer post; original mode uses the original card. Unhandled cards
  can emit typed fixed-first-party video-page jobs. Disabled/blacklisted payloads
  are ignored, while enabled malformed cards fail atomically and privately;
  upstream instead warns and can retain partial output. Duplicate component
  references are rejected to prevent output amplification. Strict scoped config,
  JSON-safe compatibility forwarding, cancellation, and identities are tested.
  Richer metadata, real authenticated captures, and companion-adapter downloads
  remain unfinished.


  **Implemented unavailable-media selection:** native X now skips individually
  marked unavailable media by default, matching upstream, instead of failing the
  whole post. Scoped `unavailable: true` retains those media for normal validated
  file/preview selection; protected posts and transport failures remain errors.
  Shared selection covers ordinary media and unified cards across quotes,
  retweets, conversations, and threads. Parser/local HTTP/configuration/CLI tests
  verify skipped malformed fields, consecutive numbering, private errors,
  explicit opt-in, compatibility forwarding, and cancellation. Live authenticated
  attempts of marked media remain unverified.


  **Implemented experimental articles:** default article selection now emits
  metadata, cover, and ordered embedded images. Scoped boolean/comma/list flags
  add HTML inspection metadata and standalone document jobs, or independently
  disable components. Escaped HTML rendering covers the upstream text/list/atomic
  block and media/divider/emoji/LaTeX entity forms, validated links/mentions, and
  UTF-16 inline ranges, with bounded output. Documents precede article images
  after ordinary/card files, preserving shared numbering/archive identities.
  Outer repost versus original article ownership, quote/thread traversal, lazy
  cutoff, and optional-error recovery have local coverage. Completed files and
  metadata survive later article failures; null/malformed optional envelopes do
  not abort normal media or later pagination. Large metadata strings use shared
  storage to avoid per-file amplification. Strict configuration and canonical
  compatibility forwarding are tested. Real authenticated captures, broader
  content variants, and recovered-error warnings remain.

  **Implemented image sizes and shared direct HTTP fallbacks:** native X image
  requests default to orig/4096x4096/large/medium/small. Scoped string/list `size`
  replaces the ordered list for photos, posters, unified-card and article images;
  summary-card URLs are normalized only when they already carry a size parameter.
  Component/conversation-preview URLs remain raw without size chains; article
  HTML uses only the primary size. Remaining candidates are private
  `_fallback` URLs. Strict bounded configuration and Twitter-only compatibility
  size forwarding have parser, runtime, and CLI coverage. Native image request
  hashes/default paths and resume keys change because default fallback context is
  now present; no old paths/checkpoints are migrated. Stable social archive keys
  and file positions remain unchanged, including when fallback execution is off.

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

**Implemented direct HTTP fallback execution:** the shared downloader consumes
bounded private `_fallback` arrays in order after remote/request failures and
configured retries. Invalid/repeated/manifest candidates are skipped. Scoped
`fallback` defaults to true; explicit false prevents further candidates, and
explicit values forward for every social compatibility category. Unsupported
video-page/manifest jobs can use a direct fallback while segmented downloads
remain unsupported. Local filesystem failures and size filtering do not trigger
fallback. Original destination/metadata/archive identity and atomic publication
are preserved, including the original filename extension without MIME adjustment.
Failed candidate partials are discarded before switching URLs; resume keys remain
request-bound, and the final failed candidate checkpoint survives with resume
enabled. Every per-item header is cleared across explicit cross-origin
fallbacks, while HTTP-client defaults remain; cross-origin redirects remain
rejected. Local HTTP and CLI regressions cover order, retries, headers, validation,
resume/publication, and configuration precedence. Authenticated live X fallback
verification remains pending.

**Implemented explicit browser import:** native `--cookies-from-browser
firefox:PROFILE_PATH` reads a selected Firefox directory or cookie database through
a read-only SQLite snapshot, including committed WAL rows. Only the requested
site's default, unpartitioned context is admitted into the existing scoped jar;
cookie counts/sizes and malformed fields are checked with private diagnostics.
Native clients can also be constructed from a scoped in-memory jar. Compatibility
mode forwards browser specifications to the installed backend, avoiding Rust
cookie export files. Synthetic database and CLI fixtures cover scope, WAL,
source preservation, precedence, lazy reads, and errors. Live authenticated
verification, profile discovery, containers, and other native browser formats
remain. Scoped native configuration now accepts explicit Firefox `cookies`
arrays with configuration-relative profile paths. Browser arrays and cookie-file
strings replace each other through normal scope and CLI precedence; per-category
clients import lazily, while compatibility forwards the selected browser source.
Synthetic profile/configuration tests cover isolation, precedence, lazy reads,
private errors, and source preservation. Keyring/container/domain selectors and
cookie rotation lists remain unsupported.

**Implemented network settings:** native configuration and CLI `proxy` /
`user-agent` options apply across native APIs/bootstrap requests and Rust media
jobs, and are forwarded to compatibility extraction. Explicit HTTP(S) proxies
replace environment discovery; an empty value disables it. User-agent overrides
normalize all extracted header casing before request identity, deduplication,
destination/resume hashing, and direct-link archive identity. Stable social IDs
remain unchanged. Clients preserve manual redirects and TLS verification, reuse
category pools, and share origin cooldowns. Local proxy/transport tests cover all
three sites, profile pages, effective media headers/paths, credential isolation,
private diagnostics, scope/CLI precedence, environment disabling, and resume
separation after user-agent changes. SOCKS, proxy bypass rules, and TLS/browser
impersonation remain.

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

**Implemented inline text:** compatibility `text:` rows and native article
HTML documents retain exact bounded UTF-8 payloads in typed jobs. Inspection URLs
encode content; identities include original bytes. Atomic publication, size
selection, archives, sidecars, existing-file behavior, and resume bypass have
regressions. Article sidecars exclude HTML/embedded URLs while retaining the
allowlisted descriptive fields. No video companion adapter is needed for text.

**Implemented pipeline step:** download mode now pulls one input at a time into
a pipeline capped by `--jobs`. A full pipeline pauses later extraction, and active
downloads continue during pending extraction. It retains at most the current
input’s pending media and active jobs, plus fixed-size identity digests for global
deduplication. URL listing emits results incrementally in input order; JSON
inspection still buffers one array. CLI regressions cover publication during
blocked extraction, backpressure at capacity, deduplication/order, failure
continuation, and simultaneous extraction/download cancellation.

Global CLI deduplication now combines validated social post/file archive keys
with request context, retaining distinct siblings/posts that share a media URL.
Repeated copies of the same file/request still collapse across inputs; changed
URLs or headers remain distinct. Missing/invalid social IDs retain request-based
fallback, and X `unique: false` still bypasses suppression. CLI regressions cover
Instagram, X, and Facebook, including a demonstrated pre-fix loss of media siblings.
That deduplication fix does not itself alter naming or resume identities; new
request context, including image fallback chains, can still change their hashes.

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

**Implemented URL input files:** repeated `-i`/`--input-file` sources follow
positional URLs, with `-` for streaming stdin. The reader accepts whitespace,
comments, inline comments, CRLF, and a first-line BOM; it retains a bounded line
and buffer, applies backpressure, and yields while scanning/discarding lines.
Recoverable line/file errors report private diagnostics and allow later inputs.
Native clients initialize lazily by category, and shared same-path archives open
on first selected media, allowing file-only inputs without an eager URL scan.
Tests cover stdin before EOF, pending-read Ctrl-C, malformed/oversized lines,
source preservation, configuration, archives/metadata, and failure continuation.
Input-file configuration directives, configured input-files, and successful-item
comment/delete modes remain.

**Implemented experimental Facebook traversal:** native album/set roots and
explicit starting-photo `setextract` URLs now stream one photo response at a time
through existing authenticated transport. Seeds require a token-bound ordered
first edge; photo navigation requires the exact photo/set identity and explicit
next/end state. Null/first-photo wrap ends traversal; other cycles/incomplete
navigation fail, retaining earlier results. Backpressure, cumulative active
budgets, stable IDs, per-album indices, and range/date cancellation are covered by
synthetic parser and local transport tests. Anonymous upstream public album and
photo checks returned logged-out shells; accepted object nesting and successful
live authenticated traversal remain unverified. Empty albums, more response
variants, profile album listings, and upstream jump heuristics remain.

**Implemented file selection:** `--range` and scoped native `file-range` strings
or lists select one-based media positions using comma unions, inclusive hyphen
ranges, and exclusive-stop signed slices. Membership and final indices use
bounded arithmetic rather than expanding ranges. Selection runs per input before
global deduplication and archive checks, applies to inspection and downloading,
and drops upstream immediately at the last matching index. Empty selections do
not start extraction or open archives. Cooperative skips preserve cancellation.
Parser, CLI, process-lifecycle, native-pagination, sidecar, and archive tests cover
selection behavior. Site-specific offset skipping, counters at flattened child
boundaries, post/child ranges, and their metadata filters remain.

**Implemented native file metadata expressions:** repeated `--filter` and scoped
`file-filter` / `image-filter` string/list settings select normalized public fields
for both backends and inspection. A bounded native parser/evaluator supports
Python-style literals, list/tuple/dict operations, short-circuit logic, arithmetic,
comparisons, membership, conditionals, common builtins/string methods, and the
upstream `contains` helper. Missing keys/runtime errors reject only the file;
syntax/unsupported calls fail privately before extraction. Date selection precedes
filters; file ranges count filter survivors before dedup/archive. Cooperative
skips and finite-range drop preserve cancellation and prevent later page/row
requests. Filters do not mutate filenames, source indices, or stable identities.
Scoped replacement/clearing, aliases, selected-only sidecars/archives, streaming
errors, and cancellation have CLI coverage; recorded CPython reference vectors
exercise accepted expression semantics. Full Python syntax, integer precision,
helpers, datetime/regex, arbitrary upstream fields, and post/child filters remain.

**Implemented file-size selection:** CLI and scoped native `filesize-min` /
`filesize-max` settings support byte amounts and binary suffixes, inclusive
bounds, and explicit zero overrides. Full and resumable downloads check known
sizes before reading media, reject upper-bound overruns before writing chunks,
and validate unknown sizes at EOF. Resumed ranges use complete representation
totals, including validated 416 completions; changed resources are validated
before stale partials can reject them. Filtered media publishes no file, sidecar,
or archive entry, and rejected resume checkpoints/contents are cleared. Existing
files/archive hits keep their skip behavior; extraction-only inspection makes no
media requests. Local HTTP tests cover withheld/chunked bodies, unknown sizes,
resume transitions, cleanup, config/CLI precedence, and archive retryability.

**Implemented date selection:** native CLI/configuration `date-before` and
`date-after` accept integer Unix timestamps and validated ISO calendar date-times,
with UTC defaults/offset normalization. Bounds are strict; after-cutoff stops and
drops the current input at its first older/equal timestamp, following the newest
first assumption. Unknown timestamps pass through. Date filtering precedes file
ranges/deduplication/archive checks and applies to inspection too. Compatibility
metadata now normalizes recognized formatted dates without changing stable IDs.
Local tests cover calendar/offset/fraction boundaries, range order, private
configuration errors, native pagination and process cancellation, later inputs,
and selected-only metadata/archive effects. Relative/week-date syntax,
site-specific skip optimizations, pinned/out-of-order handling, and nested child
boundaries remain.

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

**Implemented descriptive file metadata:** native Instagram, X, and Facebook
selections retain available file dimensions, original dimensions, seconds-based
duration, bitrate, media type, description/alt text, sensitivity, and source
identity in a typed optional `metadata.file` object. Compatibility normalization
accepts only known fields and types. Selected variants/representations supply
properties; separate audio tracks and carousel siblings do not inherit video or
neighbor dimensions. Optional malformed metadata is omitted without changing
selection. Large text uses shared storage. Inspection, sidecars, and numeric/type
filename/directory substitutions expose these properties while request identity,
default paths, file order, and stable archives remain unchanged. Dimensions are
API declarations, not downloaded-file probes; fallback image sizes can differ.
Synthetic parser, selection, inheritance, naming, privacy, and identity tests
cover these paths. Real authenticated captures, complete upstream metadata, and
full Python expression compatibility remain unfinished.

**Implemented filename step:** `-f` / `--filename` supports native placeholders
for filename/name/extension/category and available post IDs, username, shortcode,
index, and timestamp, with optional index zero-padding. Names remain under the
category directory, are sanitized to safe ASCII, and are bounded to 240 bytes
while preserving the final extension. Unsupported syntax and unavailable values
fail explicitly. Downloads, sidecars, archive recording, and no-overwrite behavior
share the same rendered destination. Default names remain unchanged. This is a
documented subset, not full gallery-dl formatting compatibility; conditional templates, expressions, and additional sites remain.

**Implemented configuration step:** `--native-config` accepts an explicit strict
subset of gallery-dl's JSON scopes: extractor defaults, category settings, and
root overrides, followed by explicit CLI flags. Supported settings include
base-directory, directory, filename, file-range, cookies, archive, retries, and fractional-second timeout.
Paths are anchored to the configuration directory, and unknown/unsupported values
fail without echoing private input. The same settings drive Rust media downloads,
per-category compatibility cookie arguments, and native Instagram credentials,
retries, and per-request deadlines. The overall extraction deadline remains
separate. Inspection opens no configured archives and creates no output paths.
Tests cover schema, scope/CLI precedence, relative paths, credentials selection,
fractional timeouts, retry counts, and separate compatibility configuration.
Full configuration compatibility, discovery, conditional templates, and
postprocessors remain.

**Implemented directory step:** native configuration `directory` lists replace
category folders with ordered component templates; explicit `[]` selects the
base directory. Scopes and explicit CLI overrides share the filename precedence.
Repeated `--directory-template` flags build nested folders, and `-D`/`--directory`
selects an exact directory. Template substitutions are sanitized per component;
unsafe literal paths, missing fields, excessive components, and excessive lengths
fail explicitly. Sidecars, resume checkpoints, publication, and archives use the
same resulting path. Tests cover configuration/CLI precedence, empty lists,
unsafe metadata, limits, missing-field side effects, and resumed archived media
with sidecars in nested folders. Conditional templates and the full upstream
format language remain.

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
fixture suite. Direct HTTP fallback URLs now execute through the shared downloader;
opaque `_ytdl_*` options still await the companion adapter.

The inspected local yt-dlp-rs prototype exposes `YoutubeDl`, `VideoInfo`,
`VideoFormat`, and an `Extractor` interface for direct media and limited HTML5
pages. It does not yet implement HLS/DASH, cookies, or FFmpeg postprocessing.
Those are prerequisites for the corresponding video handoffs, not functionality
that gallery-dl-rs can currently advertise. No companion dependency or runtime
delegation is enabled yet.
