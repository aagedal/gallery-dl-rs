Synthetic ugoira fixture: two one-pixel PNG frames in a stored ZIP, with fixed timestamps. No network captures or credentials. Timing is supplied by the test metadata.

`following.json` records gallery-dl 1.32.17's `PixivFavoriteExtractor._items_following`
and `Job.update_kwdict` output with synthetic API previews. It covers modern and
legacy routes, public/private visibility, queued profile URLs, forced extractor
classes, and child-filter metadata. No network requests or credentials were used.

`ranking.json` records `PixivRankingExtractor.metadata()` from the same upstream
version. It covers mode aliases, duplicate/bare query parameters, content filters,
and date text normalization, including calendar-invalid and Unicode-decimal dates.
The dynamic default date is omitted. No API requests were made.
