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

`followed.json` records `PixivFollowedExtractor.metadata/works` and the actual
App API feed pagination helper with synthetic data. It captures the initial
OAuth owner, mixed artists, `restrict=all`, ignored URL query options and owner
metadata that stays fixed after the API user object is replaced. Private-looking
owner fields are synthetic sentinels used to verify native metadata allowlisting.
No API requests were made.
