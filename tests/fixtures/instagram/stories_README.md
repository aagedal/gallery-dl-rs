# Native stories/highlights protocol coverage

Story fixtures are synthetic JSON records constructed in `src/instagram_stories_tests.rs`
and the parser's unit tests. They contain example.test media URLs and no real account,
cookie, or signed response data. No successful authenticated live story/highlight
request has been verified and no real response envelope has been captured.

Primary upstream references inspected 2026-10-18:

- [InstagramStoriesExtractor and InstagramHighlightsExtractor](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/instagram.py): user stories, explicit IDs, selected media, direct and base64-shared highlights, highlight lists, and the current user's story tray.
- The same module's REST `reels_media`, `reels_tray`, `highlights_tray`, and `user_by_name` methods define the request and response fields used here.
- Its REST item parser establishes `items`, owner `user`, item `pk`/`code`, image/video variants, and item `taken_at` fields. Missing item shortcodes can be derived from numeric media IDs.

Native requests use `/api/v1/users/web_profile_info/?username=...`,
`/api/v1/feed/reels_media/?reel_ids=...`,
`/api/v1/highlights/USER_ID/highlights_tray/`, and `/api/v1/feed/reels_tray/`.
Unlike upstream's five-highlight request batches, native traversal requests one
reel at a time to stop at downstream range/cancellation boundaries. It preserves
tray and item source order and does not invent newest-first ordering.

HTTP tests cover requested username/reel/owner identity, photo/video order, exact
media selection, malformed unselected siblings, empty and missing stories, private
profile access decided by the API, late failures, safe encoded query parameters,
backpressure, range cancellation, request cancellation, and consumer pauses.
Parser tests cover URL forms, shared tokens, duplicate/conflicting selectors,
numeric bounds, shortcode consistency, repeated media/owner aliases, and private
errors. Date tests verify ascending/mixed-order story filtering through the CLI
and the library, with strict boundaries and cooperative cancellation.

Native metadata treats each story item as its own `post_id`/`media_id`, retaining
the same archive identity through stories and highlights. Explicit post events
group the whole reel with independent reel ID, owner, and available timestamp;
file identities remain unchanged. Runtime tests cover selected story items,
returned empty reels, absent reels, and post-range request cutoffs. An absent
reel emits no boundary, while a returned reel with no items occupies a position.
Profile and collection runtime tests also cover carousel boundaries, repeated
IDs, and dropping at the selected post end. These are synthetic local checks.
Upstream's complete
reel-level metadata model is not yet ported. Highlight titles/expiry, optional
music/sticker audio, split/order options, segmented video, real schema variants,
and authenticated live verification remain.
