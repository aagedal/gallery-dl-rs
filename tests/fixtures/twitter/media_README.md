# Experimental profile timeline protocol

All timeline and user records in `src/twitter_timeline.rs` and
`src/twitter_profile_tests.rs` are synthetic, reusing the synthetic photo payload
and transaction fragments in this directory. No real browser profile, supplied
cookie file, private post, or signed live media URL was read or saved. Successful
authenticated live traversal and sanitized real response fixtures remain required.

Primary source inspected 2026-10-18:
[gallery-dl's Twitter extractor](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/twitter.py),
including the media/tweets/replies/highlights extractors, their API methods,
`user_by_screen_name`, `user_by_rest_id`,
`_pagination_tweets`, and the feature snapshots.

Current upstream operation snapshots are `ck5KkZ8t5cOmoLssopN99Q/UserByScreenName`
and `jCRhbOzdgOHp6u9H4g2tEg/UserMedia`, under `https://x.com/i/api/graphql/`.
Username lookup sends `screen_name` and `withGrokTranslatedBio=false`. Media
requests send `userId`, count 50, no promoted/client-event/Birdwatch content,
`withVoice=true`, and an optional opaque cursor. Feature/toggle JSON and cursors
are encoded as query values. These IDs are snapshots, not stable API contracts.

Additional tab operations are `E8Wq-_jFSaU7hxVcuOPR9g/UserTweets`,
`-O3QOHrVn1aOm_cF5wyTCQ/UserTweetsAndReplies`, and
`gmHw9geMTncZ7jeLLUUNOw/UserHighlightsTweets`. All use `userId`, count 50,
`includePromotedContent=false`, `withVoice=true`, and the common pagination
features/toggle snapshot. Tweets adds
`withQuickPromoteEligibilityTweetFields=false`; replies adds `withCommunity=true`.

Native URLs accept `/USER/{media,tweets,with_replies,highlights}` and matching
`/id:NUMERIC_ID/TAB` forms on first-party X/Twitter hosts. Numeric IDs bypass lookup
unless `pinned:true` needs user metadata through
`8r5oa_2vD0WkhIAOkY4TTA/UserByRestId` with `userId` and user features.
The requested user must match the lookup and timeline root. Tweet IDs and owner
IDs must agree with their own aliases. Media requires the requested owner;
other tabs allow conversation authors present in the bound timeline. Known
structured timeline items/grid/conversation modules and module appends are parsed
in instruction order without global recursion. Known promoted/recommendation
entries and valid text-only posts are skipped. Retweets are skipped by default;
explicit retweet/quote settings share bounded expansion. Only direct media are
selected, and unsupported/unavailable entries fail explicitly.

Pins default to disabled; enabled pin instructions or user
`legacy.pinned_tweet_ids_str` move a matching first-page post to the front.
Metadata from lookup is retained when the timeline omits it. Reply modes include
all (default), skip, and self, comparing author against the profile owner when
present or the reply recipient for standalone posts. Quote selection remains
independent of parent filtering. Per-input post identity digests suppress repeat
posts after their first media batch, retaining all initial media siblings.

An explicit Bottom termination or well-formed entry response without a Bottom
cursor ends traversal. An empty page with a new Bottom cursor continues. Explicit
termination takes precedence over a repeated final cursor; other repeated or
conflicting cursors are errors. Page structure, owner IDs, and cursors validate
before output. Media/link parsing proceeds lazily per post; prior emitted media
survive a later media or linked-post failure, including within the same page.
Fixed-size cursor digests detect cycles without retaining cursor strings from
all pages.

Tests verify operation variables, opaque cursor escaping, account ownership,
module ordering, explicit/end states, empty continuation, archive equivalence,
late errors, range/date behavior, no prefetch, active deadlines, cancellation,
configured user agents, cookie/CSRF rotation, and API credential isolation from
media. Transaction tests regenerate each request's token using its actual API
path, decoded timestamp, and mask rather than checking only header presence.

Other collection routes, configurable uniqueness, arbitrary root profile
dispatch, resume cursors, text-only output, cards/articles,
unknown timeline variants, dynamic operation-ID refresh, and real authenticated
verification remain unfinished. Compatibility remains the default.

Search and root timeline continuation use `4fpceYZ6-YQCx_JSl_Cn_A/SearchTimeline`
with `rawQuery`, `count`, `querySource=typed_query`, product, and
`withGrokTranslatedBio=false`. The exact search envelope is
`data.search_by_raw_query.search_timeline.timeline.instructions`. No user-result
root is required, but each emitted post and its author aliases still validate.
Search parameters use percent encoding without double decoding. Hashtag path
plus signs remain literal. Root/profile/search routing is covered by parser,
CLI missing-authentication, and direct-input continuation tests.

Max-ID pagination rewrites the last raw post ID minus one and clears the cursor;
empty pages fall back to the opaque cursor. Date pagination uses the snowflake's
full timestamp for progress and rewrites its calendar day as `until:`. Repeated
full timestamps retain cursor navigation, while distinct timestamps can reset
an identical day query. Request hashes include this timestamp progress to permit
that intentional retry and still reject loops. `search-stop=3` permits three
empty continuations and ends at a fourth empty response; an explicit Bottom
`stopOnEmptyResponse=false` resets the count. Invalid hint aliases fail privately.

Root profiles resolve canonical names, choose media/tweets/replies by strategy,
and search older link posts after the initial feed. Only a link-search stage with
no raw post results widens to an unfiltered search. Range/drop/deadline tests
verify no prefetch, preservation of earlier output on failures, shared active
time excluding consumer pauses, cross-stage dedup, and unchanged archive keys.
All responses remain synthetic, including direct/quoted/retweet search results.

Likes, bookmarks, list posts, and home feeds now have separate synthetic/local
coverage described in [the collection protocol notes](collections_README.md).
Search pin instructions now respect the default disabled setting and explicit
first-page opt-in, including across root fallback stages.
