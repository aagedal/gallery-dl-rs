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
  unverified. Resume cursors, arbitrary root dispatch, and other collection
  routes remain unfinished.
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
  and other routes/options remain unfinished.
  **Implemented experimental community feeds:** explicit `/i/communities/ID`
  media timelines and the session's followed-community feed at `/i/communities`
  or `/USER/communities` share lazy traversal, limits, media options, expansion,
  source-order dates, cancellation, and signing. Exact operation envelopes and
  present community identity aliases are validated. Scoped `text-tweets`
  emits file-less post events and selects the CommunityTweetsTimeline variant. Post-owned community
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

**Implemented shared input budgets:** every CLI extraction now has one cumulative
active deadline spanning initialization, all pages/children, token lookups,
retries, and events later discarded by filters. Downloader/consumer pauses do
not consume it. Exhaustion drops the input and active HTTP request futures;
owned compatibility processes terminate. Firefox imports check cancellation
before starting and interrupt active SQLite scans, including nonmatching-row
scans beyond output limits. Virtual-clock tests cover accounting and filtered
errors; local HTTP tests cover later-page cancellation/connection closure, and
browser tests cover queued, active and idle-interrupt-race cleanup.

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
bounded arithmetic rather than expanding ranges. Selection runs per extractor job before
global deduplication and archive checks, applies to inspection and downloading,
and drops upstream immediately at the last matching index. Empty selections do
not start extraction or open archives. Cooperative skips preserve cancellation.
Parser, CLI, process-lifecycle, native-pagination, sidecar, and archive tests cover
selection behavior. Native Kemono/Coomer child jobs now reset counters;
site-specific offset skipping, additional native queue boundaries and flattened
compatibility child counters remain.

**Implemented explicit post selection:** native extractors and the compatibility
decoder expose post/media/end events, while media-only library APIs remain
available. `--post-range` and repeated `--post-filter`, plus scoped string/list
configuration, select whole posts before file selection. Post dates precede
post expressions and ranges; file predicates and ranges follow. Post expressions
exclude file-specific fields and include declared count when available. Counts
never delimit a post. Empty real posts occupy positions, repeated IDs do not
merge boundaries, and selected siblings retain source indices and identities.
Instagram carousels and whole reels, X tweet/quote/thread batches, and Facebook
photo or video/audio groups have explicit boundaries. Missing reels and skipped
X text-only tweets create posts only with `text-tweets`. Empty ranges start no extraction; finite
native ranges drop at the selected end. Compatibility must read the next
directory or validated EOF, potentially doing upstream work for the next post.
Parser, runtime, CLI, cancellation, selected-only archive/sidecar, configuration,
and error-ordering tests cover these paths. Child scopes and selection are
described below; complete upstream metadata, broader extractor queue boundaries
and live authenticated verification remain.

**Implemented native child jobs and selection:** explicit BeginChild/EndChild
boundaries with shared cancellation controls now wrap Kemono/Coomer collection
creators/posts/servers and Discord server channels. Bare X, Bluesky and Pixiv
profiles now dispatch one default timeline/feed/artworks child, with explicit
feed URLs remaining roots. A shared lazy child adapter exposes queue metadata
before constructing the extractor, closes matching ends after errors, and drops
active work on cooperative cancellation while unwinding visible descendants.
The default profile queue exposes only category/user labels, matching upstream's
private extractor marker; rejecting it avoids identity/session requests and
Pixiv token validation/refresh. One selection engine resets
post/file/child counters per job, applies child/file predicates before their
ranges, preserves existing post date/filter ordering, and restores parent state
after nested children. Finite ranges cancel only the current job and avoid later
pages while keeping its parent's later children available. Child metadata retains
bounded public scalar fields and actual one-level Discord server/channel maps;
unselected invalid identities need no requests. Child range/filter config and
CLI options forward to compatibility mode, whose own queue selection applies;
resolved compatibility media still lack child boundary markers for Rust counters.
Tests cover nested state restoration, framing failures, depth/identity guards,
manual cancellation, empty posts, source indices, selected-only requests,
metadata privacy, shared server metadata and actual nested HTTP traversal.
Profile tests also cover request-free rejection, deferred token errors, default
feed selection, direct-view isolation and unchanged archive identities.
Additional native queue producers, per-child/subcategory settings, parent metadata
propagation and authenticated verification remain unfinished.

**Implemented ordered profile include dispatch:** scoped native `include`
supports exact-string `all`, comma strings, literal lists and explicit empty
overrides, forwarded as typed compatibility options. Bare X, Bluesky and Pixiv
profiles queue their configured views in upstream catalog order or requested
order, preserving aliases and repeated child positions. Post/file limits cancel
each selected child independently; explicit view URLs remain roots. Recorded
upstream Dispatch reference vectors cover string/list distinctions and unknown
entries. Native mode currently omits unknown include names without upstream's
warning. All X profile views have native paths; Bluesky adds replies/video,
avatar/banner blobs and normalized fileless info; Pixiv adds original
avatar/background images and extension fallbacks. Native Pixiv novels now use
their own category client for user/bookmark child views; Sketch resolves the
artist account lazily after child selection and loads its public wall. Profile info retains only the
normalized public subset; full metadata-only JSON dumping remains unfinished.
X/Pixiv typed profile image identity isolates owner/kind/version archives from
ordinary posts and agrees with compatibility rows. Inspection, filters and
templates expose those public identity fields. HTTP fixtures verify actual
request order, no-request rejection, independent counters, failed-child recovery,
default preservation and native/compatibility archive stability. Successful live
public Bluesky avatar inspection on 2026-10-20 emitted one original blob job
under `include`/child/file limits, with no errors or media download. Live
authenticated X/Pixiv profile asset/include traversal remains unverified.

**Implemented native Bluesky likes:** explicit likes URLs and profile include
views now read public like records from the owner's discovered PDS, lazily
hydrating referenced posts through AppView. DID documents bind to the owner;
validated HTTPS service endpoints fall back to bsky.social on discovery failure.
Record URI, subject CID and hydrated author/record identities are checked.
Malformed/deleted/inaccessible subjects do not poison later likes; repository
errors fail the current job. Independent bounded opaque cursors follow empty
record pages, stop cycles and cap traversal; cooperative skip checkpoints and
moving record arrays preserve cancellation without duplicating page payloads.
The optional `getActorLikes` endpoint handles mixed-author feeds and stops on
empty responses. Scoped api-server overrides only AppView, while endpoint values
remain separate from Kemono's namesake setting; both forward to compatibility.
Empty server overrides restore defaults, and errors never echo private values.
HTTP/CLI fixtures cover discovery/fallback, order, identity, no credential
forwarding, malformed/deleted references, independent ranges and sibling views.
Valid own media now emit before quote validation, so a malformed quote cannot
erase those files or poison later likes. On 2026-10-20 bounded public native
likes inspection emitted one media job with no errors; an upstream resolved-JSON
inspection selected the same original post, author DID and blob CID. Neither
inspection downloaded media. Native login/restricted likes remain unfinished.

**Implemented author-specific Bluesky blob routing:** asynchronous native post,
feed, liked-post, quote and profile-image streams now use each media author's
validated DID service endpoint for getBlob jobs, including service base paths.
The shared bounded cache stores up to 1,024 successful/fallback roots with
in-flight lookup sharing, releases locks before network work and allows canceled
initialization to retry. Repository owner discovery shares this cache; unrelated
liked/quoted authors retain their own PDS and DID/CID archive identity. Empty
media and canceled later jobs start no blob discovery. Offline parse helpers
retain the bsky.social fallback because they perform no network requests.
Fixtures cover different author servers, profile assets, shared lookup reuse,
fallbacks, cache bounds, cancellation and credential isolation. CLI proxy tests
fail discovery locally to verify fallback without public traffic. On 2026-10-20
native and upstream public avatar inspections produced equivalent getBlob URLs
on the same non-default PDS. A separate bounded native download saved one
256,555-byte JPEG in a fresh temporary directory with no partial artifacts;
JPEG signatures, end marker and image metadata were verified, and the saved
bytes' SHA-256 digest matched the blob CID's multihash.
Authenticated/restricted blobs and broader PDS captures remain unverified.

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
helpers, datetime/regex and arbitrary upstream fields remain; bounded native
child filters are described above.

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
site-specific skip optimizations and pinned/out-of-order policies remain to audit.
Nested job date boundaries and live booru keyword types are implemented below.

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

**Implemented experimental native Imgur:** single images, albums and gallery
posts via the public `post/v1` API, with shared retries, timeouts, network
options and Retry-After cooldowns. The upstream boolean `mp4` option defaults
to true; false preserves animated GIFs while leaving native video unchanged.
User profiles (`/user/NAME`), favorites and favorite folders, subreddits
(`/r/NAME`), tags (`/t/NAME`) and searches queue their posts as album and
image children through the v3 API (2026-10-28). `/user/me` and
`/user/me/hidden` use the `accesstoken` cookie from `--cookies` (not verified
without an account).

**Implemented experimental native boorus:** Danbooru, e621/e926, Gelbooru and
Safebooru posts, tag searches and pools, plus Danbooru favorite
groups, e621/e926/Gelbooru/Safebooru favorites, and Danbooru/e621/e926 popular
views and artist discovery. Danbooru/e621 support
API-only username/password Basic authentication and Gelbooru/Safebooru support API-only
user-id/api-key credentials, with same-origin redirects and no credentials in
media metadata. API-reported file size, rating and signed score are exposed.
Remaining: other favorites, notes,
`tags`/`metadata` options, per-category tags and authenticated live verification.

**Implemented experimental native Mastodon:** statuses, user timelines,
bookmarks, favourites, list timelines and Unicode hashtags on mastodon.social,
pawoo.net and baraag.net. Instance scopes (`mastodon.social`/`pawoo`/`baraag`)
inherit shared `mastodon` settings; tokens bind explicitly to an instance API
origin and stay out of media requests. Instance output categories now match
upstream; existing attachment archive identities remain stable, while default
category directories change without migrating files. Numeric account IDs skip
lookup. Collection paging follows server bookmark/status cursors from all Link
fields, detects cycles, and stops lazily; collection dates are non-terminating.
`reblogs`/`replies`/`text-posts` preserve outer repost metadata, attachment IDs,
and empty post boundaries. Optional card images append outer-post previews with
upstream thumbnail IDs, conservative archive eligibility and card-only traversal.
Scoped config, compatibility forwarding, header
privacy, split Link fields, date ordering and cancellation have local coverage.
Remaining: OAuth/cache discovery, arbitrary/additional instances, followed views,
account-search fallback/moved handling, and fuller metadata/default
naming. Authenticated live verification and real captures remain required.

**Implemented experimental native Kemono/Coomer:** posts, creator post listings
and global `/posts` feeds with deduplicated files. Creator/global listings accept
query/tag/initial offset parameters and paginate lazily in batches of 50.
Global posts validate their own service/creator identities; present conflicting
detail identities are rejected. Scoped `expand` fetches full details on demand,
and `max-posts` counts raw posts including file-less ones, cutting off later
detail/page requests. Empty post boundaries retain metadata for post ranges and
filters. Publication dates use non-terminating filtering. Configuration and
compatibility forwarding, metadata/archive equivalence, parser identity checks,
and local HTTP pagination/cancellation tests cover these paths. Reduced sanitized
public listing/detail captures are included. On 2026-10-20 native Kemono global
inspection and a separate one-post detail expansion each emitted two media jobs;
no files were downloaded. Subsequent gzip-aware inspection emitted a Kemono
thumbnail job and a Coomer MP4 original job with creator display names; no media
was downloaded in those inspections. Authenticated verification, wider service
captures and successful original transfers remain required.
Remaining: popular views, comments/DMs, archive/link
extraction from post text, username/password login, additional endpoint behavior
and full upstream metadata/default naming.

**Implemented Kemono/Coomer creator metadata:** default-enabled, lazy profile
lookups attach typed immutable IDs and optional display names, public IDs, dates,
counts and other validated profile fields. A host/service/creator cache retains
up to 1,024 successes or failures; `metadata: false` suppresses lookups. Invalid
or unavailable profiles preserve extraction with identity-only metadata.
Profiles bind to the requested identity; display names never substitute for IDs
in archives. Native and compatibility output, filters and templates share these
fields, and renames preserve archive keys. Public API responses observed on
2026-10-20 used gzip even when identity encoding was requested. Bounded gzip
support now validates all members/checksums off the async executor, with input
and output cancellation checkpoints. Local tests cover profile caching,
concurrent lookup/cancellation, identity rejection, scoped configuration,
compatibility parity, gzip limits/checksums and cancellation across empty gzip
members. A sanitized public profile schema fixture accompanies these tests.

**Implemented Kemono/Coomer Discord:** explicit channel paths/hash selectors
and whole-server traversal validate enclosing server/channel/message identities.
Forward and reverse 150-message pagination, retained duplicate attachments and
inline CDN links, source indices/counts, empty boundaries and per-channel raw
post limits share native range/date/filter/cancellation behavior. Typed Discord
metadata reaches compatibility output, filters and templates; positional server
archives do not depend on display names. Public Referers permit media CDN
redirects without exposing API cookies. Scoped cookies rotate on API responses,
respect URL attributes and never enter emitted jobs; API redirects remain
same-origin. Creator `endpoint` aliases now select ordinary or lazily expanded
listings, independently of Discord ordering. Sanitized public server/message
captures accompany synthetic and local HTTP tests. A native one-post public
channel inspection on 2026-10-20 emitted one original media job with typed Discord
metadata and no errors; no media was downloaded. The existing 1.32.17
thumbnail baseline is retained; direct inline CDN images preserve their real
extensions rather than forming upstream's malformed thumbnail path. Newer
upstream master removes thumbnail selection, so version alignment remains open.
Discord author/embed/revision/archive metadata, authenticated verification,
full downloads and broader service coverage remain unfinished.

**Implemented Kemono/Coomer favorites and artists:** session-backed artist/post
favorites and public creator discovery share lazy creator/post/Discord traversal.
Scoped `favorites` selects the default type; supported URL query overrides,
name/service filtering and sorting follow upstream behavior, including exact wide
integer comparisons, stable truthy ties and reversed false-value ties. Child
identities validate when consumed, preserving cutoffs before later malformed
entries; failed children report private errors and leave later children available.
Catalogs retain the 32 MiB response bound and accept up to 500,000 entries, with
background parsing and cancellable filtering/sorting. Sanitized public creator
schema captures and Python-generated ordering vectors accompany local tests.
On 2026-10-20 public catalog checks returned 108,063 Kemono and 220,005 Coomer
creators within those bounds; anonymous favorites returned HTTP 401. A filtered
native Kemono artist inspection emitted two original media jobs; a separate
Coomer artist inspection emitted one. Both retained creator identities and
reported no errors; no media was downloaded.
Authenticated favorites traversal and downloads remain unverified; login,
per-child settings and fuller queue metadata remain open.

**Implemented Kemono/Coomer file selection:** ordered `files`, per-kind
`duplicates`, and `original` settings now match upstream category order and
thumbnail defaults. Safe inline sources, backslash normalization, original-name
extensions and file types are covered. Source counts/indices precede thumbnail
skips. Positional archives preserve requested duplicates and hashless inline
archives; old hash-based entries are not migrated. Public media headers preserve
CDN redirects while credentials disable that exception. Exact missing-file
placeholders on renamed `.txt` originals fail before direct, resumed, range-
assembled or HTTP 416 publication; poisoned partials are reset. Candidate paths
are bounded and descriptive names shared across repeated file groups.
Scoped configuration, compatibility forwarding, source identity/counts,
inline validation, memory sharing, safe redirects, and download cleanup tests
cover these paths. A public native thumbnail check on 2026-10-20 downloaded one
27,699-byte JPEG from `img.kemono.cr` with the default selection; separate
inspection confirmed thumbnail-host jobs. This verifies one anonymous image
transfer, not complete CDN/original/service coverage.
A separate original-file attempt with a 15-second download timeout and 2 MiB
size limit timed out without publication; successful original transfers remain
unverified.

**Implemented Kemono/Coomer revisions:** boolean/`unique` revision extraction,
`asc`/`reverse`/`desc` ordering, explicit history and selected-revision URLs,
embedded revision data and API fallback, and encoded historical file metadata.
Unique mode removes adjacent equal content hashes before indexing/order changes;
explicit history bypasses that filter and excludes the current post. Historical
IDs/owners validate before output. Typed post-owned revision metadata survives
empty/nonimage boundaries and reaches filters/templates/compatibility output.
Historical archives and filename suffixes retain versions even when file URLs
are identical; current ID zero preserves its normal archive key. Source limits
drain the selected parent's revisions and prevent later parent requests.
Python SHA-1 reference vectors cover Unicode, float notation, signed zero and
large integers; nonfinite overflow is rejected. Requests, bounds, cancellation,
ordering, uniqueness, identity errors and disabled-mode behavior have local
coverage. A public revision-enabled native inspection on 2026-10-20 emitted one
current-version thumbnail job, whose hash matched the Python calculation on a
separate API response. Successful live historical-version extraction remains
unverified.

**Implemented experimental native Pixeldrain/Catbox/imgbox/Postimg:** files,
lists (including zero-based Pixeldrain `#item=N` selection), albums, galleries
and image pages. Catbox album dates accept upstream's `Created Month DD YYYY`
and the previously accepted `Uploaded DD Month YYYY` format; invalid optional dates do not
change media selection. Pixeldrain API keys authenticate API requests and file downloads.
Remaining: Pixeldrain filesystem (`/d/`) links and other domains; Catbox file
`/c/` descriptions; imgbox per-image metadata (width/height); Postimg
JS-paginated gallery pages beyond the server-rendered listing.

**Implemented experimental native Tumblr:** blog listings, single posts and
tagged listings via API v2 with an overridable `api-key`, original-image token
resolution for capped photos and modern inline images, legacy inline `_1280`
rewriting, GIFV download fallbacks, and `reblogs` (boolean/`same-blog`), `posts`
(legacy type selection), `inline` (boolean/`reblog`/`original`) and `original`
settings. Single-post URLs bypass the `posts` filter, matching upstream.
Failed token lookups fall back to the
API rendition; upstream's delayed token refresh retries remain unimplemented.
Remaining: OAuth and private/dashboard blogs, likes/search/day views, NPF posts,
external audio/video, `external`/`fallback-delay`/`fallback-retries` options,
and custom-domain blogs.

**Implemented experimental native Pixiv:** artworks and user illustration/manga
works via the App API with a `refresh-token`, including lazy ugoira frame ZIPs
and `ugoira: "original"` individual frames with ordered ZIP member names/delays
in metadata (`ugoira: false` skips them). Original URL derivation, JPG/PNG/GIF
probes, empty frame lists, file-only frame indices and exact option truthiness
are covered by upstream vectors. Probes omit OAuth credentials and permit bounded
same-origin redirects; cross-origin probe redirects remain unsupported. Artwork
archive keys now include extension across all artwork contexts to separate ZIPs
from individual frames; older keys without extensions do not suppress downloads
under these corrected identities. Conversion and live authenticated original-frame
verification remain unfinished.
Public/private illustration bookmarks support user/legacy URLs, path/query tag
filters, and own bookmarks resolved from OAuth, with lazy pagination scoped to
the selected visibility, owner and tag. Private access still requires an account
authorized by Pixiv; live authenticated bookmark verification remains unverified.
Illustration bookmark jobs resolve a named owner before listing or snapshot the
initial OAuth owner. Public `user_bookmark` identity is shared across posts and
files, while artist metadata stays separate. Archives now include bookmark owner,
artwork, page and extension; tag, visibility, cursor and display fields do not
change identity. Prior ordinary artwork entries do not suppress the corrected
bookmark archive namespace. Extended owner fields remain unfinished.
The separate `pixiv-novel` category supports single novels, user works, series,
public/private bookmarks, exact UTF-8 text, optional original covers and
`full-series` expansion. Profile novel views use novel-scoped credentials and
network/output settings. HTTP fixtures cover lazy selection, scoped pagination,
raw text publication and recovery; authenticated live novel extraction remains
unverified. Native `embeds` expands uploaded images in response order and queues
linked artworks after text and covers. Source numbering and persistent public
image metadata reach files and child predicates, with bounded snapshots and
traversal. Artwork children use independently configured illustration clients;
selection precedes requests and selected targets deduplicate across the novel job.
Novel archive identities now include extension. Novel `embeds`, `covers` and
`full-series` switches support scoped JSON truthiness and explicit null clearing.
Source oracle and HTTP tests cover
ordering, metadata mutations, selection, client scope and failure boundaries.
Text/cover failures currently report errors where upstream warns. Per-child category predicate overrides remain unfinished.
Illustration and novel bookmark `cursor` settings support strings, integers and
explicit null clearing in independent category scopes. The starting cursor is
sent only on the first request; pagination can replace or remove it without
changing fixed owner/tag/visibility scope. Recorded source/API vectors and HTTP
fixtures cover owner snapshots, empty animations, cursor encoding and novel
archive exclusions. Compatibility forwarding retains the cursor's JSON type;
automatic resume hints and separate child-category compatibility overrides remain
unfinished.
Native illustration/novel `metadata` now loads complete artist user details
before comments/bookmark enrichment, with source ordering, listing-owner preloads,
empty animation posts and fatal pre-post failure handling. Basic artist objects
are retained when disabled; avatar/background jobs reuse their required detail
lookup. Ordered user/profile/publicity/workspace values and native API extras
reach inspection, sidecars, predicates and templates without changing archives.
Successful same-artist lookups share bounded cached snapshots, including owner
and profile discovery; attached illustration/novel clients share matching API
roots. Cache/snapshot limits and identity/canonical-key guards bound untrusted
responses; failed or cancelled lookups are not cached. Metadata configuration
now follows upstream JSON truthiness in Pixiv and Kemono/Coomer, preserving their
different runtime defaults. Source vectors and HTTP tests cover ordering, cache
reuse, option scope, selection, shared snapshots and cancellation. Live authenticated
verification and complete arbitrary compatibility-field normalization remain
unfinished.
Native illustration `captions` fills missing captions through the web AJAX API
after other enrichment, preserving source gates, literal jump-link URL decoding
and HTML5 entities. Full `caption_html` is exposed separately from normalized
text, including unchanged novel captions. Web requests use scoped Netscape or
Firefox cookies, rotate response cookies in memory, and isolate App API
credentials. Same-origin redirects, request/retry deadlines and encoded/decoded
32 MiB limits bound traversal; corrupt gzip retries while size excess does not.
Source-compatible 4xx JSON semantics, soft request/API failures and fatal malformed
caption bodies are covered by source vectors and HTTP tests, alongside ranges,
empty posts and cancellation. Live authenticated AJAX remains
unverified. Shared Firefox import now accepts every native
cookie consumer's domain, fixing Kemono/Coomer/FurAffinity allowlist omissions.
Native Pixiv sanity recovery now uses browser AJAX metadata for limited artwork,
including original page discovery and limited ugoira ZIP/frame metadata. Artist
artwork/illustration/manga routes share one untyped App API feed; web profile IDs
are merged lazily in source order before tag filtering and selection. Recovery
flags remain visible on empty posts, filters, sidecars and templates. The `sanity`
option defaults to true and supports sparse JSON truthiness configuration.
Source-oracle fixtures and local HTTP tests verify traversal and recovery; live
authenticated recovery remains unverified.
Native illustration `comments` and `metadata-bookmark` enrich selected works
lazily after file discovery. Comment objects retain nested source fields and
ordered pagination; enabled empty/error cases emit an empty list, discarding
partial failed traversals. Registered bookmark tags retain order and duplicates;
bookmark-detail failures abort before the affected post. Both fields reach
inspection, sidecars, predicates and templates without changing archives.
Configuration honors JSON truthiness and explicit-null inheritance clearing.
Fixed comment endpoint/scope, cycle/page/aggregate bounds and cancellation protect
pagination. Native novels support the same switches in an independent category,
including profile queues. Novel comments use their own endpoint and ID scope,
ignore `_ajax`, and propagate failures before the post or text/cover requests.
Enrichment is shared by Post/text/cover output; later text failures keep the post
and allow subsequent novels. Source oracle cases and HTTP tests cover category
routing, ordering, failure boundaries and selection. Compatibility novel text and
cover defaults now use source `{id}_p{N}.{extension}` filenames. Live authenticated
enrichment verification remains unfinished.
Native following lists queue selected users through normal profile `include`
dispatch, with bounded preview fields for child filters, lazy scoped pagination,
cycle detection and a 200-page cap. Modern and legacy URLs support public/private
visibility; ownerless legacy routes resolve the OAuth user. Fixture/oracle tests
cover queue metadata, selection and recovery. Live authenticated following remains
unverified.
Native rankings support all upstream web mode aliases, UTC-yesterday/default
dates, exact content filtering before rank assignment, lazy scoped pagination
and ranking-specific mode/date archives. Ranking metadata is available in
inspection, sidecars, predicates and templates; post/file selection retains
original ranks and unordered date selection scans later entries. Recorded
upstream metadata vectors and HTTP fixtures cover query semantics and request
behavior. Live authenticated ranking extraction remains unverified.
Empty or failed animation entries retain post-range positions; metadata failures
still report native errors where upstream warns.
Native tag/search URLs now support upstream term precedence/decoding, order and
target aliases, optional date bounds, lazy scoped pagination and date-window
rollover at API offset 5,000. Search metadata retains original bounds and is
available in inspection, sidecars, predicates and templates. Term-scoped archives
exclude sort/target/dates and remain separate from ranking/artwork entries.
Date selection scans unordered results; empty animation posts keep their range
positions. Recorded upstream query vectors and HTTP fixtures cover traversal;
live authenticated search remains unverified.
Native illustration series support singular-user series URLs, scoped lazy
pagination, first-response id/title/caption/total metadata, descending signed
`num_series`, source filename prefixes and ordinary artwork archive identity.
Recorded source pagination vectors cover empty first pages, immutable detail,
mixed artists and numbering gaps; HTTP fixtures cover selection and recovery.
Extended series-detail metadata and live authenticated verification remain
unfinished.
The followed-artists illustration feed now supports `bookmark_new_illust.php`,
scoped lazy pagination and an immutable public OAuth owner snapshot. Feed archive
identity includes owner/artwork/page and excludes display fields. Source vectors
and HTTP fixtures cover mixed artists, token refresh, private-field omission,
selection and recovery. Extended OAuth user metadata and live authenticated
verification remain unfinished.
Native Pixivision articles load their canonical public page before OAuth,
then lazily resolve artwork details in source order. Exact anchor matching,
adjacent raw-link deduplication and full HTML5 title decoding follow recorded
upstream vectors. Article metadata is available in inspection, sidecars,
predicates and templates; article-scoped archives exclude title changes.
HTTP fixtures cover authentication order, empty articles, selection, failures
and animation positions. Live authenticated extraction and upstream default
article directory layout remain unfinished.
Native illustration and novel `max-posts` limits count source works per job,
preserve the final work's files, and stop later enrichment and pagination.
Queued profile views receive independent budgets; rankings filter content and
searches suppress replayed rows before counting. Zero clears inherited limits,
and compatibility mode forwards the root category setting upstream. Separate
compatibility overrides for queued child categories remain incomplete.
Native illustration and novel tags support Japanese names, translated names
with stable fallback/deduplication, and complete original tag objects. Metadata
is shared across post/file output and available in predicates and templates;
original dictionary order survives rendering. Thirty vectors recorded from
both upstream generators cover mode fallback, duplicates, empty/Unicode values
and original extra fields. Bounded malformed tag arrays are omitted where
upstream can abort; tag options need no additional API calls.
Native Sketch walls support public pagination without OAuth, lazy original
photos, empty posts and independent owner/photo archives. Public post metadata
is retained for inspection, predicates and templates; source API headers,
filenames, identity, ignored Pixiv-only options and malformed-photo cutoffs are
covered by recorded upstream vectors. Profile include views discover the wall
account only after child selection. Anonymous first-photo output from the public
`@pxv_sketch` wall matched upstream IDs, owner, dimensions, filename and URL on
2026-10-20. Live pagination/profile discovery remains unverified. Configured
cookies, default directory layout and source `filename` template semantics remain
unfinished.
Native ugoira postprocessing now supports ZIP and original-frame inputs,
FFmpeg concat/image2, MKVToolNix remuxing, archive repacking, timing strategies,
two-pass encoding, scoped lists/options and named definitions. Conversion outputs
are atomic and failures preserve inputs without recording success. Other
postprocessor types, extractor-subcategory selectors, converter diagnostic
forwarding and full hook/event parity remain unfinished.
Remaining: bookmark resume-cursor diagnostics, additional metadata fields and
Referer-routed plain `i.pximg.net` links.

**Implemented experimental native DeviantArt:** deviations, user galleries and
folders via the OAuth2 API (client credentials, overridable `client-id`/
`client-secret`). Remaining: user OAuth (refresh tokens) and private/premium
content, literature/journal text, favorites, collections, scraps, stash, tags,
search, popular, avatars/backgrounds, upstream's `original`/`quality` rewriting
and metadata options, and upstream's `{title}_{index}` filenames.

**Implemented experimental native Wallhaven:** wallpapers, searches, user
uploads and collections via the API (optional `api-key`). Remaining: per-item
`w/ID` metadata (tags/uploader) for listings, collection lists, upstream's
`{category}_{id}_{resolution}` filenames and `metadata` option.

**Implemented experimental native Pinterest:** pins, boards, user pins and
`pin.it` links via the resource API. Remaining: login/cookies and CSRF token,
board sections, secret boards, search, related pins, created/saved/all-pins
listings, embedded/external (ytdl) and HLS-only videos, upstream's `sections`,
`videos` and `domain` options.

**Implemented experimental native VSCO:** single media and user galleries via
the page preloaded state and `medias/profile` API. Remaining: HLS videos
(upstream ytdl), collections, journals, spaces, avatars, and upstream's
`videos` option.

**Implemented experimental native Newgrounds:** art posts and user art
listings via page scraping and XHR listing pages. Remaining: login/cookies
(adult posts), audio, movies/games, favorites, following, search, user
overview, upstream's `flash`, `format` and `include` options, and tags/rating/
score metadata.

**Implemented experimental native Fur Affinity:** submissions, galleries and
scraps via page scraping with optional `a`/`b` session cookies. Remaining:
favorites, search, user profile/journals, followed users, upstream's
`descriptions`, `external`, `include` and `layout` options, and tags/
description metadata.

**Implemented experimental native Weasyl:** submissions and user galleries via
the API with an optional `api-key`. Remaining: folders, journals, favorites,
characters, multi-file/non-visual submissions, and upstream's `metadata`
option.

**Implemented experimental native ArtStation:** projects and user portfolios
via site JSON with browser-like headers. Remaining: Cloudflare challenges,
video/embed assets (upstream ytdl), `4k` image upgrade with fallback, likes,
albums by ID, collections, challenges, search/artwork listings, following.

**Implemented experimental native Flickr:** photos, videos, albums, user
photostreams and `flic.kr` short links via the REST API (overridable `api-key`).
Remaining: OAuth/private content, favorites, galleries, groups, tags/search,
album lists, `contexts`/`exif`/`metadata` options, upstream's `size-max`/
`videos` options, and album metadata on listed items.

**Implemented experimental native Reddit:** submissions, galleries, subreddit
and user listings over public JSON; video-only v.redd.it MP4s. Remaining: OAuth,
DASH audio, comment scanning, external-site recursion, and reddit options.

**Implemented experimental native Bluesky:** single posts, author media/posts
feeds, public custom feeds and list feeds via the public AppView, with full-size
`getBlob` images and videos. Custom feeds/lists resolve their record owner once,
retain mixed post authors/reposts, follow opaque cursors lazily, and stop cursor
cycles. `reposts`, `quoted`, and boolean `videos` match upstream defaults;
quoted records retain their own media identity, validate reference/view
identities, use hydrated data and fetch missing records lazily with cycle and
depth bounds. Bare profiles switch to the posts feed when quotes/reposts are
requested; own pinned posts are retained. Ordered include, replies/video and
profile-image and public likes views are described above. Not yet: searches,
authentication, thread `depth`,
metadata facets/user expansion, and newer multi-item media embeds.

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

**Implemented replacement-field step:** filename and directory templates now
parse gallery-dl's replacement-field grammar: nested key/index/slice accessors,
`|` alternatives with quoted literals, upstream conversions, chained `?`, `L`,
`J`, `R`, `A`, `C`, `S`, and `O` specifiers, Python format specifications, and
`strftime` date formatting over `{date}`. Field values come from the same
metadata context as `--filter`, minus transport URLs. Output was cross-checked
against upstream's formatter; unsupported specifiers, globals, and `\f`
expression templates still fail explicitly. Missing values fail unless an
alternative or `?` specifier handles them.

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
Full configuration compatibility and additional postprocessors remain.

**Implemented generic postprocessor step:** native metadata, hash and mtime
processors share scoped lists, definitions, filters and lifecycle hooks. Metadata
supports JSON/JSONL, tags, custom/print and keyword modify/delete modes, bounded
content templates, custom sidecar paths and serialized append records. Hashes
include MD5, SHA1/SHA2/SHA3 and BLAKE2, with digest-based filename rebuilding before
atomic publication. Mtime supports metadata/template values and file/directory
targets. Selection preserves post/child boundaries so empty selected posts,
concurrent downloads, abort/terminate, errors and finalization retain their hooks;
inspection has no processor side effects. Source fixtures cover rendering,
algorithm choices, timestamps and lifecycle traces. Job paths now distinguish the
current directory from the retained filename and completed file path, including
empty posts and ordered hash/metadata hooks. Scoped `init` supports eager jobs,
including no-result roots and children, with source truthiness fixtures.
Explicit producer policies now preserve shared root mutations/deletions for
ordinary Pixiv artwork, boorus, Flickr, Fur Affinity, ArtStation, Bluesky,
Kemono/Coomer, Mastodon and Pinterest. Shared file contexts wait for earlier
leases, including cancelled/filtered files; independent and copied contexts
retain concurrent execution. Twitter/Tumblr and Instagram declare their different
copy precedence. ArtStation has per-asset boundaries and source asset positions
and counts. The keyword engine replays all 204 snapshots in nine genuine source
job traces; six additional consumed Pixiv emitter cases document overwrites,
incrementing counters and nested aliases.
Source counters are now stored independently of the native file index: Pixiv
artwork and original ugoira frames use zero-based `num`, compatibility input
preserves the original counter value/type, and filters plus filename/directory
rendering retain zero. Source Directory counters remain available to post filters.
Canonical native indices, serialized metadata, and archive keys remain stable.
Recorded consumed-generator cases verify ordinary/Pixivision counters against
both native and compatibility inputs.
Pixiv novel producer writes now run against live shared root keywords: cover and
upload counters advance after preceding hooks; uploaded fields can override the
counter before incrementing; cover fallback deletion runs on producer resume.
Linked artwork queues wait for pending file hooks and retain child/child-after
mutations across queues. Eager child initialization selects the artwork category.
Raw native indices and archive identities remain separate from these live writes.
Child-after failures now retain partial keyword mutations through post-after and
error finalization, including premature child stream completion. A recorded
DownloadJob/MetadataPP ASCII-write failure verifies the retained keywords.
Metadata modify formatting now preserves typed results after supported
conversions/operators, stringifies mixed templates, and retains values on field
errors. A generated 321-case source matrix covers numeric arithmetic, lists, sorting,
format specs, dates and errors; native dates retain JSON representations rather
than Python runtime datetime/sentinel objects. Novel runtime fixtures exercise
conversion/arithmetic results through later file and child counter increments.
Keyword-driven pre-download selection and request/text-fetch changes remain open.
Remaining: PostgreSQL processor archives and further default-key coverage,
additional encodings/open modes and formatter dialects, Unicode
decimal/underscore-float parsing, nested-container sorting, runtime Python
datetime/sentinel object identity, arbitrary native filename fields,
keyword-driven archive identity and pre-download selection changes, further
cross-processor hook parity, further producer policies and framing, nested alias
identity, additional counter policies and mutation-driven request URLs/text
fetches, remaining temporary-path behavior, and
other processor types.

**Implemented processor archive step (gallery-dl 1.32.17):** SQLite metadata
archives are separate from the native download archive schema.
Writing modes JSON/tags/custom/JSONL use source `archive(entry)` tables,
producer category/extractor archive-format descriptors, and per-job sessions
created before init hooks. Defaults use category + `_MD_` + extractor format;
explicit table/prefix/format values have distinct default and empty semantics.
Path-list/table templates resolve against initial Directory keywords. Checks run
before destination/path/skip/render side effects and preserve `_archive_metadata`
for additions; existing-file skips add nothing, while empty completed output adds.
Modify/delete/stdout initialize and close sessions but bypass checks/additions;
hash/mtime/ugoira have no such generic archive. Connection reuse/pragma semantics,
immediate commits through later failures, and unconditional close after active
work drains are implemented. Source fixtures cover 26 archive cases, 21 complete
processor cases across 22 jobs, 124 extractor routes, and three producer key
traces. Tests cover instance isolation, cancellation/concurrency, and Python
SQLite interoperability. Native sidecar publication remains atomic on failure.
Default-key support is gated where native fields or source job framing are not
yet preserved; explicit formats remain available. PostgreSQL remains a missing
backend. See upstream
`postprocessor/common.py:24`, `postprocessor/metadata.py:111`, `archive.py:19`,
and `job.py:817` for initialization and lifecycle order.

**Implemented booru search archive context:** native tag searches retain
`search_tags` separately from request tags, expose it to filters/templates, and
enable default `t_{search_tags}_{id}` metadata archive keys for source-matched
routes. Constructor decoding, duplicate-query rules, empty searches, whitespace,
and Gelbooru/Safebooru `all` request sentinels follow source fixtures. Shared
Danbooru/e621 keywords retain post-hook mutations; DAPI extractors reapply their
search keyword before each file. The source oracle includes 110 URL cases
(23 unmatched) and four live mutation traces. Broader native
aliases remain usable with explicit archive formats. Pool metadata/default keys
remain unfinished.

**Implemented booru tag pagination:** Danbooru/e621 follow source prefix
selection and tag precedence, including ascending ID cursors and numbered pages
for non-ID ordering and ID/collection filters. Cursors use the last raw post,
with later ascending responses reversed before emission and cursor selection.
Source page sizes (200/320), threshold defaults (180/300), integer clamping and
explicit default resets are supported. Threshold configuration follows native
scope precedence and is forwarded to compatibility extraction. Tag searches no
longer stop at 100 pages or an unchanged ID; cancellation drops pending requests.
Malformed terminal cursors retain preceding files before reporting an error.
Source fixtures cover request parameters, emitted order, threshold boundaries,
repeated pages, long traversal and cancellation. DAPI traversal bounds and
range-driven source page skipping remain unfinished.
Shared keyword feedback captures the last post's live ID after selected file
leases drain, before post-after hooks. Cursor generation resumes from that value,
so supported post/file-hook changes and deletions affect the next request.

**Implemented booru pools (source oracle, 1.32.17):** preserve the full response
pool object without `post_ids`, normalize its name once, and retain original
member positions as `num`. Feed queries use the requested ID; archive keys use
the response `pool[id]`. Danbooru streams numbered `ordpool:` pages in API order
and supports pool/id ordering options; reverse modes buffer and validate first.
E621 buffers numbered `pool:` pages, retains the final response per ID, then
restores member order with unavailable-member gaps. Repeated e621 members retain
the source's partial-output failure. No native ID-chunk truncation remains.
A job-scoped object binding carries nested pool hook mutations across posts and
post-after boundaries; root replacement/deletion, including equal replacements,
detaches only that post. Mutation observers stay outside public JSON. Source
fixtures and native traversal/live download-hook replay cover the contract;
default pool archive keys are enabled. Source publication paths are implemented
below, separately from the native logical basename. Generic root order settings
retain the existing asc/desc/reverse grammar; extra pool aliases require the
Danbooru category scope.

**Implemented Danbooru favorite groups:** `/favorite_groups/ID` and
`/favorite_group/show/ID` share collection traversal, ordering, member positions,
and nested object ownership with pools. Metadata comes from
`/favorite_groups/ID.json`, retains its response ID, and is projected as
`favgroup`; metadata archives use `fg_{favgroup[id]}_{id}`. Pool and favorite-group
routes preserve literal decimal IDs (including zero and leading zeroes), support
legacy aliases, and capture only the source regex digit prefix before suffixes.
Source replay covers requests, metadata, directories/format keys, live hooks,
error checkpoints and cancellation. Collection compatibility capture is scoped
to its booru categories; Pixiv user-detail and novel reserved-root guards reject
`pool`/`favgroup` extras to prevent duplicate serialized JSON keys. Source
publication paths are implemented below. Alternate source hosts and raw Unicode
URL digit captures remain unimplemented; URL parsing percent-encodes those raw
digits, so their original spelling is unavailable from the typed URL.

**Implemented booru filename keywords:** preserve raw producer `filename`,
`extension`, and root `md5` independently of the native logical path in a private,
nonserialized keyword snapshot. Danbooru applies `text.nameext_from_url` semantics
before URL normalization (lossy percent decoding, final-dot split, sixteen
Unicode-character extension limit, lowercase, no fragment stripping). E621 uses
raw `file.md5`/`file.ext`; Gelbooru/Safebooru preserve API keys when extension is
present. Safebooru prefers the actual API file URL and retains the validated
image/directory fallback; unsupported logical suffixes use `.bin` without losing
source keywords. Empty/missing/null/typed values stay distinct. Filter contexts,
direct filename/directory templates, and real shared Post/File hooks use these
values. The real source PathFormat checkpoint is replayed too: default extension
alias mapping occurs once after Post inheritance and before Prepare/File hooks;
raw extraction values remain untouched. The same preparation runs without
configured postprocessors, for direct downloads and archive checks; explicit
directories retain their pre-normalization values. File-hook replacements are
not mapped again. E926 compatibility metadata now follows the booru identity
path. Full filename replay replaces the earlier prefix-only assertions; helper and actual
job oracles cover all four families, and compatibility tests cover scoping,
serialization, raw field presence and stable initial directories.

**Implemented booru source paths:** native post/tag/pool/favorite-group routes
retain immutable source class templates privately alongside the raw keyword
snapshot. Source category `e926` is preserved independently of native e621
settings and identity. Filename/directory overrides resolve independently, and
conditional templates without a matching branch fall back to the source class
default; an explicit empty directory remains empty. Directories freeze before
Init/Post and after the preceding PostAfter, including implicit class defaults.
Source path rendering preserves spaces/Unicode, replaces separators, removes
controls, omits empty directory segments, and formats missing/null fields as
`None`. Parent-directory basenames remain rejected. The selected filename
formatter reaches Prepare/File hash rebuilds and metadata `extension-format`;
the original mapped extension is cached separately from later live keyword/path
changes, through PostAfter. Source-mode filenames are built after Prepare and
remain fixed under ordinary File keyword edits. Direct and archived downloads
without processors use the same preparation. Supported compatibility input URLs
retain their class descriptor too; standalone `parse_messages` calls have no
route provenance and retain legacy fallback behavior. Genuine source publication
fixtures compare full media/sidecar paths, bytes, five hook snapshots, independent
and conditional overrides, empty directories, typed MD5 fields, hash rebuilds,
and multi-post directory checkpoints across all fourteen supported route classes.

**Remaining booru paths/options:** custom path restriction/replacement/conversion,
strip/substitution settings, extension-map overrides, unsupported routes and
alternate source hosts still need their source contracts. Gelbooru video URL
rewriting outside pools and broader DAPI response normalization remain unfinished.

**Implemented E621/e926 favorites:** E621 favorites stream
`/favorites.json` with numbered pages (no pool buffering). The constructor
matches `/favorites` as a prefix; only an immediately following query is
captured. Decode query keys/values once, skip bare pairs, and retain the first
value for each decoded key, including blanks. URL `page`/`limit` are overwritten
by native pagination defaults; other query parameters remain API-only and do
not create `search_tags`. Retain `user_id` as an arbitrary string, including the
empty default, and reapply it per Post only. Current Post/File mutations survive
within the post; the next post and API request restore the constructor owner.
ID mutations/deletion do not affect numbered cursors. The default metadata
archive key is `f_{user_id}_{id}` using live file keywords; the source directory
is category/Favorites/user_id. Danbooru has no matching `/favorites` extractor.
Native traversal is lazy, has no 100-page cutoff, and does not buffer, reorder,
or deduplicate API pages. User IDs remain exact strings (including blanks) and
compatibility owner keywords preserve raw types without leaking private schema
fields into inspection JSON. Retained source class descriptors now include
archive formats, allowing `for_media` to recover proven booru post/tag/pool/
favorite-group/favorite defaults without inferring classes from mutable owner or
collection keywords. Genuine source replay covers constructor parsing, API
parameters, pages, cancellation, later-page failures, five hook checkpoints,
complete published paths/bytes, live metadata archive keys, and unchanged native
SQLite identity. Compatibility tests cover typed owner presence, category scope,
E926 keyword category, unknown-route rejection, and media-only default archives.

**Implemented DAPI pools (source oracle, 1.32.17):** Gelbooru matches the ordered
`page=pool&s=show&id=ID` query on `/` or `/index.php`; Safebooru requires
`/index.php`. Preserve lexical digit captures, then expose scalar integer `pool`
and HTML-decoded `pool_name`. Missing/empty titles fail before post requests.
Gelbooru fetches the title HTML, fetches the listing again with `pid=0`, then
requests JSON DAPI records for each HTML ID in order; continue at 45 HTML IDs
with offsets increasing by 45, preserving duplicates and counting IDs rather
than API results. Safebooru retains thumbnail IDs from its initial HTML and
fetches XML DAPI records lazily, without HTML pagination. Reapply the two pool
keywords per post, without Danbooru shared-object semantics or synthetic `num`.
Defaults are `{category}/pool/{pool}`, the ordinary DAPI filename, and metadata
archive key `p_{pool}_{id}`. Replay title failures/decoding, repeated HTML reads,
45-ID boundaries, duplicate/zero/multiple API results, cancellation before later
ID requests, and per-post keyword restoration. Native source replay compares
actual publication paths/bytes, all six hook events including duplicate skips,
metadata archive keys, and unchanged native SQLite post identity. Pool HTML
never receives API credentials, including across redirects; both DAPI sites
accept API-only credentials. XML decoding uses the bounded existing parser and
rejects DTDs; API error bodies are excluded from diagnostics. XML attribute IDs
and stripped tags preserve source string types in filters, templates and hooks.
Both sites validate required tags/date after skipping records without file URLs.

**Implemented Safebooru favorites (source oracle, 1.32.17):** match the ordered
`page=favorites&s=view&id=ID` query on `/index.php`. Expose scalar integer
`favorite_id` while retaining the lexical requested ID in HTML queries. Begin
HTML traversal at `pid=0` without a title request; the source regex matches
Python whitespace followed by `id="p` and a Unicode decimal digit prefix, without
requiring a closing quote. Count every match, including duplicates, and advance
by 50 when at least 50 matches occur, independently of XML response row counts.
Each member uses lazy XML DAPI preparation, preserving string IDs and stripped
tags and skipping missing file URLs before required-field validation. Reapply
`favorite_id` per Post without sharing later mutations across posts or requests.
Default paths are `{category}/favorites/{favorite_id}` with the ordinary DAPI
filename; metadata archives use live `f_{favorite_id}_{id}` keys. Native history
retains the original canonical post identity. Compatibility keywords preserve
raw owner types for Gelbooru/Safebooru without inferring a source class; proven
Gelbooru/Safebooru favorite routes select these defaults. Existing Pixiv Unicode
17 decimal rules now serve the HTML scanner too. Genuine source replay covers
41 cases and nine route probes, six hook checkpoints, full media/sidecar bytes,
actual source metadata archive SQL, and canonical native history identity.
DAPI source contexts no longer synthesize `num`; user-added values remain live
within the current post. The existing 73 pool cases also verify that absence.

**Implemented Gelbooru favorites (source oracle, 1.32.17):** the ordered
`page=favorites&s=view&id=ID` query matches `/` or `/index.php`, retaining lexical
IDs for API requests and scalar `favorite_id` for source keywords. A JSON probe
uses string limit `2`, compares the first two raw entry IDs with Python ordering,
and chooses forward or reverse traversal. Forward pages use numeric limit 100
and stop only on an empty favorite result; reverse traversal starts at
`divmod(count-1,100)[0]`, reverses each page and decrements through zero, including
the source's initial negative page for zero counts and float page spellings.
Regular single-object responses are supported forward but fail in reverse, where
the source requires a mutable list. Count types are validated only on paths that
use them. Category `order-posts` retains raw JSON and first-element truthiness
rules, including delayed errors after the probe. Global supported order strings
retain their original spelling rather than conflating `reverse` with `desc`.
Each favorite entry hydrates posts lazily, then assigns `date_favorited`; missing
`added` fails only when hydration returns a post. Timestamp conversion follows
source integer coercion, with a typed falsy NullDatetime sentinel for invalid
values. Typed dates survive hook copies and template/filter evaluation; primitive
hook replacements retain their types, and public metadata JSON removes internal
type markers. Default paths and archives match Safebooru favorites. Source replay
covers 76 cases, ten typed timestamp vectors and six route probes, including
pagination, cancellation, six hook events, full publication bytes, metadata
archive SQL and stable native history IDs. Compatibility/configuration tests
cover date types, retained source class provenance, sparse raw order settings,
precedence and exact backend argument forwarding. Arbitrary literal strftime
specifications on unknown copied-date aliases remain a generic formatter gap;
standard `%Y` date aliases are supported.

**Implemented Danbooru/e621/e926 popular views (source oracle, 1.32.17):**
Popular routes retain original API query parameters, first-value query decoding,
numbered pagination and source thresholds. Keyword dates use the local calendar
when omitted, Monday normalization for ISO calendar/week dates, and literal
Unicode slicing for month scale. Root date/scale retain primitive types through
filters, formatting, hook copies, deletion and reassignment; explicit datetime
conversion retains typed provenance. Default paths and live metadata archives
use `{category}/popular/{scale}/{date}` and `P_{scale[0]}_{date}_{id}`, including
empty-scale `None` formatting. Native creation timestamps and history identities
remain attached to the original posts. Proven compatibility routes keep their
class defaults and primitive scope even when hooks delete date/scale. Strict
response handling preserves already published rows before a later malformed
record fails. Genuine source replay covers 139 cases, twelve route probes and
five formatter vectors, including API parameters, pagination, cancellation,
six hook checkpoints, complete publication bytes and actual metadata archive SQL.

**Implemented Danbooru/e621/e926 artist routes (source oracle, 1.32.17):**
Artist detail IDs retain lexical spelling; searches retain first-decoded query
parameters, source integer page coercion and fixed twenty-row pagination.
Artists queue lazy tag-search jobs with complete bounded artist JSON, fixed
quoted URLs, selection before shared URL deduplication, and name validation
before selection. Children inherit hook-mutated artist metadata under `_parent`
as a shallow copy: root edits stay with the child, nested-map edits propagate to
the parent's later callback, and replacements detach those objects. The source
reapplies its original parent mapping at each post/file message. Tag children
own paths and `t_{search_tags}_{id}` archives; queue-only roots use their inherited
empty archive format and initialize hooks only when eager initialization is set.
Native history retains canonical post identities independently of source metadata
archives. Genuine source jobs cover 129 cases and 24 route probes, including
request order, cancellation, filters/ranges/deduplication, malformed partial
results, hook ownership, primitive date fields, publication bytes and SQLite keys.
**Implemented artist metadata inheritance options (source oracle, 1.32.17):**
`parent-metadata` and legacy `metadata-parent` retain raw JSON and resolve each
key across configuration layers before primary-key presence selects the result.
Falsy values disable inheritance, nonempty strings select exact nested keys,
and truthy non-strings flatten the artist fields. Shallow nested identities and
hook-created aliases survive Child, child Post/File, ChildAfter and final parent
hooks. Inherited fields reapply at every Directory and Url boundary, including
colliding IDs, dates, category labels, tags, names, filename/extension and custom
metadata keys. Original API identity remains separate for native history.
Raw date strings stay primitive; explicit conversions retain datetime identity.

Stable artist/artist-search/tag job configuration selects hooks and eager
initialization independently of mutable public class labels. These class scopes
support inheritance, job hooks/options, base directory, filename/directory,
child selection, post filters/ranges and date bounds; unsupported class extraction/selection options fail
explicitly. Declared custom parent names work in templates across scopes.
CLI artist classes keep independent cached inheritance settings, and tag children
use their own job templates and output settings. Compatibility forwards both
raw inheritance keys without collapsing absence or null. Compatibility jobs retain
category-level native hook and output settings. Canonical booru tag keywords now
retain source-normalized arrays. Genuine new source jobs cover
168 cases (including 48 observed collision-path Skip events), all eight hook
phases, complete publication bytes and actual metadata archive SQL. The original
129 artist cases remain frozen and pass unchanged.

Remaining: other class-scoped extraction/selection options, source download-archive
path arrays, dynamic Python class/proxy keyword objects, and array-element
mutation. Configurable parent inheritance for other native queue producers has
not yet been implemented.

**Implemented live booru date selection (source oracle, 1.32.17):** 141 genuine
DownloadJob cases plus 12 separately labeled predicate helper vectors preserve
Directory-only evaluation, post-filter/date/range order, source raw types,
exclusive boundaries, inverted-window behavior, and child-local errors/stops.
Falsy live dates bypass comparisons; truthy non-datetime values fail privately.
The selector borrows live artist keyword objects instead of cloning their trees.
Actual tag classes resolve their own date/post predicates independently of artist
root settings. Invalid/falsy configuration bounds disable comparison by presence,
including clearing a less specific bound; numeric truthy values convert to integer
Unix seconds. Native API history still records original post identities. Replay
checks exact consumed messages, hook phases, paths, publication bytes and both
SQLite archives. Source skip_date policies for other extractors and producer date precision remain
to audit; source bound conversion is extended below.

**Implemented source date-bound conversion (gallery-dl 1.32.17/Python 3.15):**
Configuration and CLI share ISO-first conversion followed by Python-style decimal
integer fallback. Basic/extended calendar and week dates, ambiguous week-date
separators, arbitrary Unicode separators, 24:00 rollover, timezone seconds and
fractions, long fractions, Unicode decimal digits and numeric underscores retain
source behavior. UTC normalization happens before microsecond truncation; invalid
or out-of-range conversions disable the bound by presence. Raw configuration
accepts unpaired-surrogate date separators without changing unrelated JSON data,
valid surrogate pairs or duplicate-key validation. The legacy strict date API is
unchanged. Genuine source cases cover 328 conversions (including two raw
surrogate inputs) and 66 complete DownloadJobs, verifying normalized dates, category/class resolution, clearing, predicates, publication
bytes and SQLite identities. Broader source JSON decoding, including the global
4,300-digit integer literal limit, remains separate configuration parity work.

**Implemented source template date conversion (gallery-dl 1.32.17/Python 3.15):**
`!D` shares ISO-first conversion while `!d` uses timestamp coercion, including
numeric zero and truncation toward zero. Invalid conversions retain the source's
falsy NullDatetime identity, formatting marker, scalar attributes, and epoch
conversion. Missing values and explicit null values preserve their different
default-formatter behavior. Metadata modify preserves datetime values copied to
arbitrary keys; assigning primitive values to date-named keys does not infer a
new datetime. Default multi-part formatting rejects object-valued fields, while
identity-mode formatting safely converts them to text; the `I` specifier retains
object identity. Source templates validate final format specifications against
the actual value, allowing literal datetime formats.

Booru keyword contexts now retain arbitrary API fields and nested primitive
types, with source filename/extension, tag groups, dates and immutable job class
labels taking precedence. Configuration and CLI templates accept dynamically
provided fields; native history identities retain canonical post IDs. Genuine
source evidence includes 1,230 direct cases, 32 modifications and nine complete
DownloadJobs with exact paths, bytes, typed callbacks and SQLite identities.
Native replay covers 1,224 direct cases and 28 modifications plus all nine jobs.
Four unpaired-surrogate string cases, two raw fractional-timezone renderings and
four preexisting subsecond datetime modifications remain explicit representation
boundaries in the frozen fixture. An additional 72 formatter evaluations and 12
MetadataPP runs preserve shifted NullDatetime payloads and enforce calendar
overflow at years one and 9999. Broader producer datetime precision and source
JSON decoding remain parity work.

Source `O` arguments now use duration arithmetic, including unsigned hours,
Unicode decimal integers, underscores, signed minutes, values beyond 24 hours,
and the source's zero-hour minute-sign rule. Arbitrary-size operands can cancel
before enforcing Python timedelta bounds. Empty and `local` arguments look up
the host timezone at the input date, preserving historical second-level offsets
and the float timestamp rounding used by Python. Numeric parse failures remain
distinct from calendar overflow during rendering. Frozen source formatter,
MetadataPP and DownloadJob replays cover these behaviors; timezone replays use
isolated subprocesses. The epoch-only JSON date wrapper still cannot distinguish
aware UTC dates from naive dates; source rejects aware dates for local offsets.

**Implemented typed booru source path results (gallery-dl 1.32.17):** Source
filename templates require text. Directory components accept text, expand flat
string lists and insertion-ordered map keys, and discard the missing-value
sentinel. Numeric, null, datetime and nested non-string results fail privately.
Bare conversions preserve their result types; explicit formatting converts them
to text. Empty source fields ignore both conversions and an empty context key.
The strict native naming APIs retain their required-field behavior. Dynamic
directories share the existing segment cleanup and 3,072-byte path bound, with a
4,096-item budget that also counts empty elements.

Prepare hooks run before final filename validation and can repair or invalidate
keyword values. Their metadata destinations use the frozen directory, cached
extension and unset filename, so default sidecars are named `metadata.*`.
A later filename error preserves completed sidecars and leaves canonical media
history empty. Evidence includes 74 complete source jobs, six additional
Prepare jobs, five direct conversion failures and four real PathFormat
open/finalize probes. Seven empty-basename cases retain an explicit native
policy difference: native rejects them before file hooks or publication; source
open/finalize fails later and may retain a partial file. Synthetic transfer
artifacts in these cases are recorded separately from actual file-open evidence.
First-file Prepare hashing uses the cleared input path rather than a provisional
destination; the source fallback to a prior cached realpath across files/posts
remains file-state parity work. That cache records the last built path, including
archived, skipped or failed attempts, and a filename-rebuilding callback changes
the input for subsequent callbacks in the same hook. Typed path handling remains
gated by proven booru source defaults; other native extractors still need source
path and class defaults.


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

**Implemented conditional templates:** object-valued `filename` and `directory`
settings map filter expressions to templates in configuration order (JSON key
order is preserved), with `""` as the default and the normal name or category
folder as the final fallback. Conditions share the `--filter` evaluator and fail
the file on evaluation errors. Plain values in narrower scopes or CLI flags
replace inherited conditional settings as a whole.

**Implemented modification times:** new files take HTTP `Last-Modified` times by
default (`--no-mtime` disables), and `--mtime [NAME]` mirrors upstream's mtime
postprocessor using a metadata date field. Times are set on the temporary or
resume file before publication, including resumed 416 completions via the saved
date validator and inline text from metadata.

Native Instagram, Facebook, and Twitter session initialization failures now remain local to
their inputs, allowing unrelated direct links and other inputs to proceed.

**Implemented postprocessor parity, 2026-10-28:** native configuration accepts
upstream's `exec` (string and list commands, `commands`, `async`, `output`,
`verbose`, `session`; non-zero exits warn), `classify`, `directory`, `zip`
(`extension`, `compression` store/zip, `mode` safe, `files`, `keep-files`) and
`rename` (`from`/`to`, `skip`, with the prepare-after recheck) processors.
`--exec`/`--exec-after` now configure `exec` processors like upstream, and the
`--zip`, `--cbz`, `--rename`, `--rename-to`, `--ugoira FMT`, `--ugoira-conv*`,
`--mtime-from-date`, `-O/--postprocessor-option` and `--no-postprocessors`
shortcuts are available. `--no-check-certificate`, `--source-address`, `-4` and
`-6` apply to every native HTTP client and are forwarded to the compatibility
backend. Repeated CLI options override earlier ones, as with argparse.
`--no-skip` and `skip: false` replace existing files (the previous file is
kept until the replacement completes) and bypass archive checks; `skip`
values `abort:N`, `terminate:N` and `exit:N` map to `-A`/`-T`. The `compare`
processor (`action` replace/enumerate, `shallow`) is supported.
`compression: bzip2` is supported (2026-10-28).
The `python` processor runs `function` (`MODULE:NAME`, module name or file
path) or `expression` (`mode: eval`) in an external `python3`
(`$GALLERY_DL_PYTHON`), exchanging the keywords as JSON; `archive` works.
`compression: lzma` writes LZMA (method 14) entries built natively and
copied into the archive, readable by Python's `zipfile` (2026-10-28).

**Implemented input, auth and pacing options, 2026-10-28:** input files accept
upstream's `-KEY=VALUE` (next URL) and `-G KEY=VALUE` (all following URLs)
option lines; such inputs re-resolve their settings. `-I/--input-file-comment`
and `-x/--input-file-delete` comment out or delete the lines of URLs that
completed without errors (applied when the run ends, not after each URL).
`-u/--username`, `-p/--password` and `--netrc` (machine = category) provide
lowest-priority credentials. `--write-unsupported FILE` records queued URLs no
extractor handles. Registry sites honor `sleep-request`, `sleep-retries` and
`sleep-429` (CLI flags or site options); `--sleep-skip` waits after skipped
files. `-v`, `-w`, `--no-part`, `--chunk-size`, `--no-colors`, `--no-input`
and `--config-ignore` are accepted for compatibility.

**Implemented path cleaning options, 2026-10-28:** `path-restrict` (auto,
unix, windows, windows+, ascii, ascii+, character sets or mappings),
`path-replace`, `path-remove` and `path-strip` are read process-wide from the
root or `extractor` scope or `-o`, with `--restrict-filenames VALUE` and
`--windows-filenames`. Separators and NUL are always replaced. Without them,
native filename templates kept the conservative ASCII cleaning (since
2026-10-28 `path-restrict` defaults to `auto`). `{filename}`
now excludes the extension for every extractor, as upstream.
**Remaining divergences:** direct links (`direct/<digest>_NAME` versus
upstream `directlink` with `{domain}`, `{path}`, `{query}` and `{fragment}`).
The default base directory is `./gallery-dl/` like upstream (since
2026-10-28; it was `downloads`).

**Implemented output and configuration commands, 2026-10-28:** `-N/--print
[EVENT:]FORMAT` and `--print-to-file [EVENT:]FORMAT FILE` configure metadata
processors like upstream and imply `download: false`, which (also via
`--no-download` or the `download` option) runs preparation and `after` hooks
without fetching files; `-s` remains the separate simulation mode.
`--config-status`, `--config-create` and `--config-open` manage the native
configuration file. `-U/--update`, `--update-check` and `--update-to` report
that self-updating is unsupported. `--xff` sends a random `X-Forwarded-For` address from CIDR blocks or
country codes (upstream's table), `--blacklist`/`--whitelist` filter child
extractors for external URLs by category, `--write-log` copies standard error
(Unix), and the `--cache-*` commands report that no cache is kept. `-X` and
`--compat` fail explicitly. `--tags-blacklist`/`--tags-whitelist` (and the
`tags-*`/`*-tags` options) filter posts by tags with Danbooru-style rules;
`/import` is unsupported. `-E/--extractor-info` prints each registry route's
category, subcategory and formats (all routes of the matching site, since
routes are chosen during extraction) with configured custom values. `--cookies-export FILE` writes the session cookies of Instagram, Twitter/X,
Facebook and every initialized registry site as a Netscape file.
`--write-pages` (and the `write-pages` option: `true`, `all`, `ALL`) dumps
registry responses to numbered files in the working directory, and
`--print-traffic` shows registry requests and response headers. Dedicated
extractors (Instagram, Twitter/X, Facebook, ...) do not report traffic.
Configuration files may be YAML (`.yaml`/`.yml`) or TOML (`.toml`), or be
declared with `--config-json`, `--config-yaml` or `--config-toml`;
`--config-type` selects the `--config-create` format. Every upstream 1.32.17
command-line option is now accepted; `-X` and `--compat` fail explicitly and
the update options report that self-updating is unsupported.

**Implemented upstream configuration compatibility, 2026-10-28:** native
configuration files now load typical upstream `config.json` files. Unknown
keys are ignored by dedicated extractors and passed to registry sites with
upstream's root → `extractor` → category inheritance; the `downloader`
section's `rate`, `retries`, `timeout`, `verify` and `mtime` apply; `output`
and `cache` are ignored. Credentials stay confined to supporting categories,
and `base-directory`, `archive` and cookie-file paths expand `~` and
environment variables. The `output` section's `logfile`, `unsupportedfile`, `errorfile` (paths or
`{"path": ...}`), `mode: null`/`false` and `skip: false` apply.
`archive` also accepts upstream's list-of-segments form with `{category}`,
and archive parent directories are created on demand.
Registry sites apply user `keywords`/`keywords-global` (with `keywords-eval`
format strings), `metadata-url` and `metadata-version`; `cookies-update` with
a path writes session cookies there (rewriting the input file with `true` is
not supported). `downloader.ytdl` `format`, `cmdline-args` (whitespace-split strings or
lists) and `config-file` apply to yt-dlp downloads; `forward-cookies: false`
stops cookie forwarding, and common `raw-options` (quiet, subtitles,
merge format, rate/retries, ...) are translated to yt-dlp flags.
`sleep`, `sleep-extractor`, `sleep-skip`, `source-address`, `geo-bypass` and
`netrc` are also read from the root/`extractor` configuration scope.
Upstream's `extension-map` applies to every extractor (default: `jpeg`,
`jpe`, `jfif`, `jif` and `jfi` become `jpg`).
`output.progress` (default on, or a format string) prints
`[CURRENT/TOTAL] URL` before each of several inputs.
Completed and skipped files are reported on standard output per
`output.mode` (`auto`, `pipe`, `terminal`, `color`, `null` or custom
`success`/`skip` formats; `output.colors`, `NO_COLOR`, `output.skip`)
instead of `downloaded`/`skipped` lines on standard error.
Downloads running longer than `downloader.progress` seconds (default 3)
show upstream's `PERCENT SIZE RATE` line on standard error in terminal
output modes (one download at a time owns the line).
Errors and warnings use upstream's `[NAME][LEVEL] MESSAGE` format
(`output.log`: a format string or `{"format": ...}` with `{name}`,
`{levelname}`, `{message}`, `{asctime}`).
yt-dlp warnings and errors are relayed as `[ytdl][LEVEL]` messages
(`logging`, deprecation warnings only with `deprecations: true`);
`logging: false` shows yt-dlp's own output.
`outtmpl` lets yt-dlp name the file (`default`: `%(title)s [%(id)s].%(ext)s`). Relative configuration paths resolve against the working
directory like upstream (since 2026-10-28; they used to be anchored to the
configuration file's directory).

**Implemented OAuth helpers, 2026-10-28:** `oauth:pixiv` (PKCE with a pasted
code), `oauth:deviantart`, `oauth:reddit`, `oauth:mastodon:INSTANCE`
(registering an application for unknown instances), `oauth:tumblr`,
`oauth:smugmug` and `oauth:flickr` (OAuth 1.0a) open the authorization page,
receive the callback on `localhost:6414` (`extractor.oauth.host`/`port`), and
print the token with the configuration key to set. Tokens are never cached.

**Implemented special inputs, 2026-10-28:** `recursive:URL`/`r:URL` (also
`file://` documents) queues every URL found, and `ytdl:URL` hands any HTTP(S)
page to the yt-dlp site (metadata from `yt-dlp -J --flat-playlist`).

**Implemented actions, archives and downloader options, 2026-10-28:**
`skip: "enumerate"` saves next to existing files as `NAME.N.EXT`; compare
`equal` (`abort`, `terminate`, `exit`, with `:N`) stops after consecutive
equal files; exec `success`/`error` run upstream actions. The `actions`
option runs actions for matching error and warning messages (download and
extraction failures, exec warnings) with `print`, `status`, `level`, `exec`,
`wait`, `keyword`, `raise`, `abort`, `terminate`, `restart` (as `abort`) and
`exit`; `status` and `exit` set the exit status. `flag` and signal actions
are not supported. `keywords-default` fills missing path keywords.
Existing Python gallery-dl archives (an `archive` table of `entry` keys)
are read and updated with upstream keys (category + `archive_fmt`); new
archives keep the native schema. `archive-event` (`file`, `skip`, `after`;
upstream archives default to `file`, native ones also record existing
files), `archive-mode: memory` (written after a run without errors) and
`skip-filter` are supported. Downloader `adjust-extensions` (file signature
checks, default on), `validate-html`, `retry-codes` and `part-directory`
apply. `-j/--dump-json` prints upstream's `[2, post]`/`[3, url, file]`
message list (`output.private`, `num-to-str`, `jsonl`, `ascii`); the native
media dump is the hidden `--dump-media-json`. `path-restrict` defaults to
`auto` like upstream (only `/`, or Windows' reserved characters, are
replaced). Exit codes follow upstream's
bit flags: 4 for download and extraction errors, 8 for bot challenges, 16 for
authentication, 32 for format errors, 64 for unsupported URLs and 128 for
local file errors (extraction errors are classified by their messages).
**Remaining divergences:** direct links keep `direct/<digest>_NAME` paths
(request-identity separation) rather than `directlink/{domain}_{path}_NAME`;
`metadata-extractor`, `parent-*`, `category-transfer`,
`file-unique`/`child-unique`, `path-extended`/`path-convert`,
`tls12: false` (TLS 1.3 only) and `truststore` (system certificates) apply
to every native client; `ciphers` restricts connections to the listed
suites (OpenSSL or IANA names that rustls implements: TLS 1.3 and ECDHE
AEAD suites), while the `firefox`/`chrome` presets (fingerprint imitation)
are ignored with a warning; `cookies-select` picks one
source per run (`rotate` starts with the first);
`proxy-env: false` disables environment proxies. `metadata-http` (root or
`extractor` scope) exposes response headers, the Content-Disposition
`filename`/`extension` and the Last-Modified `date` under the given keyword;
the destination is rendered again after the response so filename templates
can use them. (Upstream itself rejects
replacement fields in string `archive` paths.) `config-map` (default: upstream's renamed categories, e.g.
`kemonoparty` → `kemono`, `pixiv` → `pixiv-novel`) copies settings to new
category names; `category-map` and `--compat` rename `category` (and
`subcategory`) keywords, but configuration lookups keep the native names.

**Implemented upstream default paths for dedicated extractors, 2026-10-28:**
without `-f`/`-d` templates, Pixiv, Twitter/X, Instagram, Facebook, Bluesky,
Reddit, Tumblr, Mastodon, Kemono/Coomer, Pinterest, ArtStation, Newgrounds,
VSCO, Flickr, Imgur, DeviantArt, Fur Affinity, Weasyl, Wallhaven and the
image hosts use upstream's directory and filename formats, mapped onto the
native keywords (`post_id`, `media_id`, `username`, `caption`, `num`).
Files lacking a required field, and direct links, keep the digest names.
Exact keyword differences remain (e.g. DeviantArt's `{index}` is the UUID,
Instagram omits the sidecar prefix, Reddit uses the user directory).

**Implemented upstream URL coverage for dedicated extractors, 2026-10-28:**
a sweep of every `#url` in upstream's `test/results` (excluding `#fail` and
`#exception` cases) against `--extractor native -g` found 175 URLs the
native extractors rejected. Added: Imgur user/favorite/favorite-folder/
subreddit/tag/search listings and slug/thumbnail forms; Danbooru/e621
`/post/show/ID`, `/posts/random`, e621 `/p/BASE32`, e621.cc and
e621.anthro.fr, Gelbooru `redirect.php`; Twitter fx/vx/fixup/fixvx hosts;
Pixiv touch./phixiv hosts, `/i/ID`, `/u/ID`, `member.php`, `#id=` and
`pixiv.me`; Weasyl journals, journal, favorites, folders and `~` names;
Wallhaven collections; Pixeldrain `/d/` filesystem folders (recursive);
Flickr albums overview, favorites, galleries, groups and search; Fur
Affinity folders, user dispatch, search, watchlist, `/msg/submissions` and
mirror hosts; DeviantArt `/view/ID`, `fav.me`, `view.php`, favourites and
collections, tags, avatar, banner, watching and fxdeviantart; Facebook
`profile.php?id=ID/TAB`; `i.reddituploads.com`; remaining directlink
extensions. Newgrounds audio/movie pages, user audio/movies/games
listings, favorites, following and search were ported from upstream but
only tested against local fixtures (the site serves an "NG Guard" browser
check to this host). VSCO video (HLS via yt-dlp), avatar, collection and spaces
views were ported the same way (VSCO returns HTTP 403 here).
ArtStation albums, likes, collections, challenges and the artwork page were ported too
(Cloudflare blocks ArtStation here).
**Remaining:** ArtStation search
(CSRF-token POST API), DeviantArt scraps/search/statuses/
watch (Eclipse API needing a CSRF session, or login); journals and literature are saved as `.htm`
from `deviation/content` (simpler than upstream's journal template; not yet
verified live because DeviantArt rate-limited this host), 500px liked (login;
the native port uses the newer api-neo GraphQL schema, whose liked query is
unknown without an account), Facebook `/info` (profile metadata only).
MangaDex feed/follows log in with `username`/`password` (and optional
`client-id`/`client-secret`), unverified without an account.

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

**Companion audit, 2026-10-20:** the old main checkout was stale. Published
[yt-dlp-rs main at `10223c019ef6ec5552627b90251d8e967404e440`](https://github.com/aagedal/yt-dlp-rs/tree/10223c019ef6ec5552627b90251d8e967404e440)
already implements HLS VOD, static single-period DASH, AES-128 HLS, scoped
cookies, FFmpeg merging, progress/cooperative transfer cancellation, and
completed-path results. Its public APIs and fixture coverage were inspected;
that revision has not been independently compiled as a gallery dependency.
No tags or GitHub releases existed at audit time. The immutable commit is a
reproducible preview candidate, not a stable released adapter contract.

Remaining public API requirements: inline manifest data plus its base URL;
configurable request/connect timeouts (published defaults are hard-coded);
cancellation during extraction; scope-preserving session cookie sharing; and
request-specific sensitive headers that do not follow unrelated segment/key
origins. Manifest parsers are private, and the companion's cookie jar is a
different concrete type. Global default headers cannot replace the required
scoped request boundary. Do not discard inline manifests or flatten cookies
to make a preview adapter appear complete. No companion dependency or runtime
delegation is enabled yet; gallery-owned integration/publication tests remain.
