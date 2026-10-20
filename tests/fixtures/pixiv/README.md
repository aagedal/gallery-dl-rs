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
