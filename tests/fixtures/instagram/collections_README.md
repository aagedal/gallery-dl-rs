# Native post collection protocol coverage

Fixtures in `src/instagram_collections_tests.rs` and inline parser/transport tests
are synthetic. Media URLs use example hosts, cursor/session values are invented,
and no browser profile or supplied cookie file was read. No successful live
authenticated collection extraction or real sanitized response has been verified.

The primary protocol reference, inspected 2026-10-18, is
[gallery-dl's Instagram extractor](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/instagram.py):
`InstagramPostsExtractor`, `InstagramReelsExtractor`, `InstagramTaggedExtractor`,
`InstagramSavedExtractor`, `InstagramCollectionExtractor`, and the REST API methods
`user_clips`, `user_tagged`, `user_saved`, `user_collection`, `_pagination`, and
`_pagination_post`.

| Source | Request | Items | Pagination |
| --- | --- | --- | --- |
| Explicit posts | GET `feed/user/ID/`, count 30 | `items[]` | root `more_available` / `next_max_id` |
| Reels | POST `clips/user/`, target_user_id, page_size 50, include_feed_video=true | `items[].media` | `paging_info.more_available` / `paging_info.max_id` |
| Tagged | GET `usertags/ID/feed/`, count 20 | `items[]` | root `more_available` / `next_max_id` |
| Saved | GET `feed/saved/posts/`, count 50 | `items[].media` | root `more_available` / `next_max_id` |
| Saved collection | GET `feed/collection/ID/posts/`, count 50 | `items[].media` | root `more_available` / `next_max_id` |

All API paths are under `/api/v1/`. Username-based posts/reels/tagged routes first
resolve `users/web_profile_info/?username=...`; explicit `id:ID` routes skip it.
Saved routes refer to the session's current account, independent of the username
or collection-name label in the URL. The reels API decides private access instead
of rejecting a potentially accessible private account from its public flag.

One page is validated atomically and drained before another request. Earlier
pages survive a later failure. Reels accept the requested owner or an explicitly
listed coauthor; tagged and saved results preserve their actual post creator.
Posts, media, and owner aliases must agree, including composite media/owner IDs.
Archive identities match single-post extraction and never depend on signed URLs.

Tests cover exact endpoints/methods, form/query cursor escaping, mixed owners,
archive equivalence, collaborative reels, repeated/missing cursors, malformed
wrappers/media, empty pages, range/drop cancellation, source-order date filtering,
backpressure, and active-time budgets. POST transport tests cover retries from the
original request, 307/308 body preservation, 301/302/303 conversion to body-free
GET, cookie/CSRF rotation, and private errors for unsafe redirects/auth failures.

Live access, sanitized real API envelopes, saved collection discovery, extended
collection metadata/options, alternate layouts, and segmented video remain.
