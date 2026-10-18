# Experimental native X collection protocols

All records in `src/twitter_collections.rs`, `src/twitter_timeline.rs`, and
`src/twitter_profile_tests.rs` are synthetic derivatives of the photo and signing
fixtures in this directory. No personal browser profile, real private response,
credentials, or signed live media URL was read or saved. Authenticated live
traversal and sanitized real response fixtures remain unverified.

Primary upstream source inspected 2026-10-18:
[gallery-dl's Twitter extractor](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/twitter.py),
including Likes, Bookmarks, ListLatestTweetsTimeline, HomeTimeline,
HomeLatestTimeline, and `_pagination_tweets`.

| Feed | Operation snapshot | Instruction envelope beneath `data` |
| --- | --- | --- |
| Likes | `TGEKkJG_meudeaFcqaxM-Q/Likes` | `user.result.timeline.timeline.instructions` (also existing timeline_v2 variant) |
| Bookmarks | `pLtjrO4ubNh996M_Cubwsg/Bookmarks` | `bookmark_timeline_v2.timeline.instructions` |
| Lists | `06JtmwM8k_1cthpFZITVVA/ListLatestTweetsTimeline` | `list.tweets_timeline.timeline.instructions` |
| Latest home | `SFxmNKWfN9ySJcXG_tjX8g/HomeLatestTimeline` | `home.home_timeline_urt.instructions` |
| Ranked home | `DXmgQYmIft1oLP6vMkJixw/HomeTimeline` | `home.home_timeline_urt.instructions` |

Every operation uses the common pagination features and configured `count`
(default 50, bounded 1–100). Likes sends userId, no promotions/client events or
Birdwatch notes, withVoice=true, and withArticlePlainText=false in fieldToggles.
Bookmarks sends includePromotedContent=false. Lists sends listId. Home sends
includePromotedContent=false and latestControlAvailable=true; ranked home also
sends withCommunity=true. Non-Likes feeds omit fieldToggles. Opaque continuation
cursors are query-encoded without changing their values.

Likes binds the requested profile result but permits different post authors.
Collections accept mixed authors and still require canonical post IDs and
consistent author aliases. List decimal rest_id/id_str/id fields, when present,
must agree with the requested list; a decodable `List:ID` node ID also validates.
Other opaque node strings are accepted without inferring identity. These cases
are parser policy tested with synthetic records, not verified schema captures.
Exact envelopes are required; unrelated nested posts cannot replace a missing
feed. API errors, malformed identities, and unsupported entries fail privately.

Bookmarks allow 128 consecutive empty continuations, ending on the 129th empty
response. Likes, lists, and home feeds stop on their first empty response. An
explicit Bottom stopOnEmptyResponse=false resets the counter; explicit end states
still win. Cursor histories and the active deadline prevent unbounded looping.

Tests cover lazy initial/paginated requests, source order, first-page pin opt-in,
post dedup with complete media siblings, expansion and archive identity, exact
request signatures, cookie/CSRF rotation, API credential isolation from media,
range/drop cancellation, unordered dates, and late private failures. One budget
includes all requests and linked-post expansion while excluding consumer pauses.
Bookmark-specific timestamps/metadata, membership/follow routes, communities,
resume cursors and further options remain unfinished. Scoped uniqueness and card
selection share the native traversal pipeline; see the main Twitter fixture notes.
