# Experimental native X collection protocols

All records in `src/twitter_collections.rs`, `src/twitter_timeline.rs`, and
`src/twitter_profile_tests.rs` are synthetic derivatives of the photo and signing
fixtures in this directory. No personal browser profile, real private response,
credentials, or signed live media URL was read or saved. Authenticated live
traversal and sanitized real response fixtures remain unverified.

Primary upstream source inspected 2026-10-18:
[gallery-dl's Twitter extractor](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/twitter.py),
including Likes, Bookmarks, ListLatestTweetsTimeline, HomeTimeline,
HomeLatestTimeline, CommunityMediaTimeline, CommunitiesMainPageTimeline,
`_transform_community`, and `_pagination_tweets`.

| Feed | Operation snapshot | Instruction envelope beneath `data` |
| --- | --- | --- |
| Likes | `TGEKkJG_meudeaFcqaxM-Q/Likes` | `user.result.timeline.timeline.instructions` (also existing timeline_v2 variant) |
| Bookmarks | `pLtjrO4ubNh996M_Cubwsg/Bookmarks` | `bookmark_timeline_v2.timeline.instructions` |
| Lists | `06JtmwM8k_1cthpFZITVVA/ListLatestTweetsTimeline` | `list.tweets_timeline.timeline.instructions` |
| Latest home | `SFxmNKWfN9ySJcXG_tjX8g/HomeLatestTimeline` | `home.home_timeline_urt.instructions` |
| Ranked home | `DXmgQYmIft1oLP6vMkJixw/HomeTimeline` | `home.home_timeline_urt.instructions` |
| Community media | `ZniZ7AAK_VVu1xtSx1V-gQ/CommunityMediaTimeline` | `communityResults.result.community_media_timeline.timeline.instructions` |
| Followed communities | `p048a9n3hTPppQyK7FQTFw/CommunitiesMainPageTimeline` | `viewer.communities_timeline.timeline.instructions` |

Community media routes are `/i/communities/ID`; `/i/communities` and
`/USER/communities` select the current session's followed-community feed, not a
lookup of another user's membership. Both operations send `withCommunity=true`;
the media operation also sends canonical `communityId`. Upstream `text-tweets`
and CommunityTweetsTimeline remain unimplemented. Present decimal and decoded
`Community:ID` response aliases must match the requested community. Post metadata
is never inferred from the feed ID: its source is the tweet's own
`author_community_relationship.community_results.result`.

Community metadata exposes canonical ID, name/description, epoch-second date,
nsfw/role/member count, string rules, admin/creator names, and a validated banner
URL. Compatibility rows use transformed public fields. Unknown fields and raw
relationship/user payloads are excluded; invalid/conflicting IDs omit metadata.
Sidecars exclude banners. Templates accept `{community_id}`/`{community_name}`.
A separate community archive scope applies only to community feeds (compatibility
subcategory `community`/`communities`), leaving ordinary post archives unchanged.
Scoped keys require valid community/post/file IDs; missing IDs do not prevent
download but provide no stable archive key. CLI fixtures verify distinct scoped
files versus ordinary files and repeated-file suppression across inputs.

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
Bookmark-specific timestamps/metadata, membership/follow routes, community text
output, resume cursors and further options remain unfinished. Scoped uniqueness and card
selection share the native traversal pipeline; see the main Twitter fixture notes.
