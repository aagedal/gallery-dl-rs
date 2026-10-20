Synthetic ugoira fixture: two one-pixel PNG frames in a stored ZIP, with fixed timestamps. No network captures or credentials. Timing is supplied by the test metadata.

`following.json` records gallery-dl 1.32.17's `PixivFavoriteExtractor._items_following`
and `Job.update_kwdict` output with synthetic API previews. It covers modern and
legacy routes, public/private visibility, queued profile URLs, forced extractor
classes, and child-filter metadata. No network requests or credentials were used.

`ranking.json` records `PixivRankingExtractor.metadata()` from the same upstream
version. It covers mode aliases, duplicate/bare query parameters, content filters,
and date text normalization, including calendar-invalid and Unicode-decimal dates.
The dynamic default date is omitted. No API requests were made.

`search.json` records `PixivSearchExtractor.metadata()` with synthetic tag/search
URLs. It covers path/query term precedence, decoding, empty terms, exact sort and
target aliases, bare/duplicate parameters and unvalidated date text. No API
requests were made.

`search-pagination.json` records the actual upstream `PixivAppAPI._pagination_search`
generator with synthetic API pages. Date-ascending, date-descending and popular
sort cases capture offset-5,000 requests, UTC date-window changes and overlap
suppression, including the skipped crossing row. No API requests were made.

`series.json` records `PixivSeriesExtractor.works` with the real App API series
pagination helper and synthetic responses. It covers an empty first page,
immutable first-page detail, numbering before user/media filtering, mixed artists,
and zero/negative numbering when the reported total differs from the page count.
No API requests were made.

`pixivision.json` records `PixivPixivisionExtractor.metadata/works` with synthetic
HTML, plus Python `html.unescape` vectors for all HTML5 named entities and numeric
edge cases. It captures canonical article URL/headers, exact literal anchors,
adjacent raw-link deduplication before query removal, and title decoding.
No article or API requests were made.

`followed.json` records `PixivFollowedExtractor.metadata/works` and the actual
App API feed pagination helper with synthetic data. It captures the initial
OAuth owner, mixed artists, `restrict=all`, ignored URL query options and owner
metadata that stays fixed after the API user object is replaced. Private-looking
owner fields are synthetic sentinels used to verify native metadata allowlisting.
No API requests were made.

`tags.json` records the actual `PixivExtractor.items` and
`PixivNovelExtractor.items` generators from installed gallery-dl 1.32.17 at
`/private/tmp/gdlvenv/lib/python3.15/site-packages/gallery_dl/extractor/pixiv.py`.
That source file's SHA-256 is
`add614ad99cf2739f934b5d4e30155fab361ebb9f3ed99f9b89f08468aef507f`.
The oracle directly called both generators on synthetic one-work extractors:
`works`/`novels` returned the supplied tag array, illustration `_extract_files`
returned one fixed synthetic image, and novel `novel_webview` returned fixed text.
Optional metadata, comments, bookmark metadata, covers, and embeds were disabled.
Every post/media tag value was deep-copied at its emission boundary to avoid the
upstream generator's shared mutable metadata. No requests or authentication ran.

Its 30 cases cover absent/default and exact/unknown tag settings, nonstring option
fallback, Japanese duplicate retention, translated fallback and stable deduplication,
Unicode/whitespace/empty names, empty arrays, and complete original dictionaries
with nested fields and insertion order. `mode_set: false` means the option was
absent, whereas `mode: null` with `mode_set: true` supplies explicit null. The
`malformed` cases record missing-key exception classes only; they document source
behavior without prescribing the native parser's malformed-response policy.
All expected illustration and novel results agree. Every identity and string is
synthetic; no captured user data or credentials are included.

`sketch.json` records the actual `PixivSketchExtractor.items` and `posts`
generators from gallery-dl 1.32.17, using the installed source path and SHA-256
recorded for `tags.json` above. Only `request_json` was replaced: it records the
requested URL and explicit headers and returns deep-copied synthetic API pages.
Each emitted post/media dictionary was deep-copied before further source mutation;
datetimes are represented as naive UTC ISO strings with an equivalent Unix
`timestamp` alongside. Expected filenames and archive strings use the actual
extractor's format strings with each emitted media dictionary.

The cases cover two-page public traversal, exact API headers and media Referer,
multiple photos, an empty post, mixed owner contexts, opaque string/integer IDs,
repeated media IDs across posts, timezone normalization, ignored input query
options, and lowercased URL extensions without converting `jpeg` to `jpg`.
Supplying Pixiv `max-posts`, `tags`, and `ugoira` options does not change this
separate extractor's results. A malformed second photo records the source
`KeyError` after the first media and confirms later posts/pages remain unfetched;
this documents source failure timing, not the native stream's balanced-end policy.
All domains, identities, text, dates, and media data are synthetic. No extractor
initialization, authentication, session creation, or network requests ran.

`bookmark_owner.json` records actual `PixivFavoriteExtractor.items`, `metadata`,
and `works`, plus `PixivNovelBookmarkExtractor.items` and `novels`, from gallery-dl
1.32.17. The installed source path and SHA-256 are recorded for `tags.json` above
and repeated in this fixture. The real illustration `_extract_files` method ran
on synthetic works; optional enrichment, sanity requests, ugoira loading, novel
covers, and embeds were disabled. A fake API provides owner details, explicit
metadata login, novel text, and page responses while delegating both bookmark
API methods and `_pagination` to the actual installed implementation. Recorded
`login` calls denote explicit metadata login, not the authentication performed
inside real HTTP requests. No HTTP or OAuth requests ran.

Cases cover named public/private bookmark owners, mixed artwork authors,
multipage media, an empty disabled animation, an ownerless initial OAuth-owner
snapshot that survives replacement of the API's current user, and private novel
bookmarks without illustration-owner metadata or archive scope. Initial cursor
values and mutable successor cursor parameters are captured from the real API
helper. Separate cursor wire vectors use `requests.Request.prepare` locally to
record omitted null, included empty/zero strings, and source behavior for values
outside the documented string type. Expected filename/archive strings use the
actual extractor's formats. Dates are normalized to naive UTC ISO text and lazy
fallback URL iterators are materialized at emission.

Owner dictionaries deliberately include synthetic private-looking fields because
the source retains the complete owner object. Native tests should compare the
implemented public owner subset rather than infer full-owner metadata parity.
All identities, URLs, strings, dates, and private-looking markers are synthetic;
no captured user data or credentials are included.

`ugoira_original.json` records the actual `PixivWorkExtractor.items`/`works` and
`PixivExtractor._extract_files`/`_extract_ugoira` implementations from gallery-dl
1.32.17. The installed source path and SHA-256 match the values recorded above
and are repeated in the fixture. Only the API's illustration/ugoira responses
and the extractor's HEAD request were stubbed. The two source `_init` ugoira
option expressions were evaluated without creating a session. Optional metadata
and sanity requests were disabled. Every emitted dictionary was deep-copied;
datetimes are serialized as naive UTC ISO text and expected filenames/archives
use the real extractor format strings.

Its 19 cases cover direct frame URL construction, preserved query suffixes,
last-marker partitioning inside a query, JPEG/PNG/GIF HEAD selection, all HTTP
failures retaining GIF URLs, already-exhausted network errors wrapped in the
actual `HttpError` class, and empty frame arrays in original and ZIP modes.
They also record absent/default, false/null/empty, unknown, and case-sensitive
option behavior. A direct uppercase `.JPEG` source URL verifies that output
filenames and archive extensions remain `jpeg`, rather than becoming `jpg`.
Public frame arrays, private ugoira flags/frame indices, post
counts, URL-derived dates, and probe order are retained exactly from the source.
The synthetic illustration declares width 640 and height 480; source snapshots
retain those dimensions on every emitted frame.
The source request retry loop itself is not simulated by the completed-error
stub. Recorded HEAD kwargs are explicit source arguments only; an initialized
source session can additionally inherit authenticated headers. Native public
probes intentionally exclude OAuth credentials and App API headers.
All identities, dates, URLs, text, and metadata are synthetic. No HTTP, OAuth,
session initialization, ZIP downloads, or frame downloads ran.

`enrichment.json` records 37 cases from the actual `PixivExtractor.items`
implementation in gallery-dl 1.32.17, using a `PixivWorkExtractor` instance.
The installed source path and SHA-256 match the values above and are repeated
in the fixture. Only `works`, `metadata`, `_extract_files`, and the two API
methods `illust_comments` and `illust_bookmark_detail` were stubbed; the two
source `_init` option assignments were evaluated without initializing a
session. This isolates illustration enrichment while retaining real source
config truthiness, gates, sequencing, tag transformation, and emitted metadata.
Each emitted dictionary was deep-copied before the next yield; dates use naive
UTC ISO text, and expected filenames/archives use the real extractor formats.

Cases record absent and JSON truthy/falsy option values; zero, missing, and
truthy comment counts; `_ajax` set during file extraction; comment generators
that fail before or after yielding values; ordered duplicate registered tags;
unregistered tags without names; absent versus empty enrichment fields; and
zero or multiple media files. The call log demonstrates file extraction before
comments, comments before bookmark detail, and both enrichments before tag
transformation and post emission. Comment exceptions discard the entire
partially collected list. Malformed source responses and request failures
record their actual exception class without imposing the same failure policy
on native optional metadata. Japanese, translated, and original tag modes
remain independent of bookmark tags.

This oracle covers illustration `items` only. The source novel implementation
has different comment behavior: it lacks this illustration `_ajax` gate and
comment exception handler. All identities, comment contents, URLs, dates, and
metadata are synthetic. No HTTP, OAuth, sessions, or media downloads ran.

`novel_enrichment.json` records 36 cases from the actual
`PixivNovelExtractor.items` implementation in gallery-dl 1.32.17, using a
`PixivNovelNovelExtractor` instance. The source path and SHA-256 match the
values above and are repeated in the fixture. Only `novels` and the API's
`novel_comments`, `novel_bookmark_detail`, and `novel_webview` methods were
stubbed. The source `_init` enrichment option assignments were evaluated
without session initialization. Metadata snapshots are deep-copied at each
source yield, dates use naive UTC ISO text, and filenames/archives use the
source formats. Covers are generated by the real source code.

Cases exercise absent and JSON truthy/falsy options, absent/false comment
counts, comments despite a truthy `_ajax`, registered bookmark tag order and
duplicates, absent versus empty enrichment fields, independent tag modes,
optional covers, empty text, and no novels. Comment failures before or after
partial iteration and bookmark failures abort before any post or text event;
two-novel cases verify that later novels are not consumed. By contrast, a
webview failure leaves the already emitted post, skips its text and covers,
and permits the next novel. A missing text field also leaves only the post.
Malformed source responses record exception classes without prescribing
native malformed-response policy. Post, text, and cover enrichment snapshots
retain the same values. All identities, comments, text, dates, URLs, and
metadata are synthetic; no HTTP, OAuth, sessions, or downloads ran.

`user_detail.json` records 30 cases from the actual illustration and novel
`items` implementations in gallery-dl 1.32.17, together with the actual
`PixivArtworksExtractor.metadata`, `PixivFavoriteExtractor.metadata`,
`PixivAppAPI.user_detail`/`_user_detail_impl`, and `Extractor.cache` helpers.
The source path and SHA-256 match the values above and are repeated in the
fixture. Synthetic work/file/comment/bookmark/webview providers and an API
`_call` stub replace network operations. The source metadata option assignment
is evaluated without session initialization. The process-global memory cache
is cleared between cases inside the isolated generator process.

Cases cover default and JSON truthy/falsy metadata modes, full user/profile/
profile_publicity/workspace dictionaries and unknown top-level fields, account
replacement, repeated or different artists, an empty file list, user-detail
failures, artist preload before the artwork feed, and named bookmark owner
lookups sharing the artist cache. Merge-dependent comment/bookmark gates and
canonical field collisions record the source's unrestricted `work.update`
behavior; native tests should distinguish deliberate canonical-field guards
from ordinary public metadata preservation. Novel cases include covers,
repeated artists, fatal detail failures, ignored `_ajax` comment gates, and
text failure after an enriched post.

A separate vector calls the real user-detail wrapper first with `fatal=False`
and then `fatal=True`, recording one `_call` and the same cached response
object. The source memory-cache key uses the user ID but omits the fatal flag,
and the default expiry is zero. This records source behavior, including reuse
of a synthetic nonfatal error response, without requiring unsafe error caching
in native code. Metadata is deep-copied at each emitted boundary; dates use
naive UTC ISO text and filename/archive formats are evaluated by the source.
All identifiers, strings, dates, URLs, and metadata are synthetic. No HTTP,
OAuth, sessions, or downloads ran.

`captions.json` records 34 actual source `items` cases and 16 direct
`_sanitize_ajax_caption` vectors from gallery-dl 1.32.17. The installed source
path and SHA-256 match the values above and are repeated in the fixture.
Illustration `items`, `_request_ajax`, and `_sanitize_ajax_caption` run
unmodified, with synthetic work/file/API providers and a `request_json` stub.
Source option assignments and the source jump-link regular expression are
used without App API or OAuth initialization. One actual novel `items` case
records that the captions option is ignored there.

Cases cover default and JSON truthy/falsy options, existing versus absent or
falsy captions, `_ajax`/`_mypixiv` gates, empty file lists, and full enrichment
ordering. User metadata, comments, and registered bookmark tags precede the
caption request; an existing caption supplied by user metadata suppresses it.
The real AJAX helper swallows request exceptions, API errors, and missing
body keys. Falsy bodies leave the existing caption untouched; truthy malformed
bodies can fail outside the helper and abort before post emission. Raw HTML
and sanitized caption outputs are retained in per-yield deep-copied metadata.

Sanitizer vectors preserve the exact lowercase double-quoted jump-anchor
rewrite, `urllib` percent decoding without plus conversion, double escapes,
malformed UTF-8/percent escapes, HTML5 named and numeric references, and raw
HTML. Falsy nonstring values return an empty string. The fixture's default web
headers come from actual `Extractor._init_session` with absent config; that
local session was closed without sending requests. `PixivAppAPI` snapshots
these headers before installing mobile App API headers. Native web requests
intentionally omit OAuth and App API authentication headers. Dates use naive
UTC ISO text. All identifiers, URLs, dates, HTML, and metadata are synthetic;
no HTTP, authentication, or media downloads ran.

Seven additional `http_status_vectors` run the complete source `_request_ajax`
→ `request_json` → `request` chain with in-memory `requests.Response` objects,
zero retries, and a stubbed `session.request`; no network is used. A valid
`error:false` JSON caption body survives statuses 200, 400, 403, and 404 because
source `fatal=False` accepts 4xx other than 429. Exhausted 429, 500, and 503
responses become `NullResponse`, so their original JSON body is not parsed.
Recorded final request headers also show `request_json` adding `Origin`,
`Sec-Fetch-Dest: empty`, `Sec-Fetch-Mode: cors`, and
`Sec-Fetch-Site: same-site` to the saved browser-header map.
