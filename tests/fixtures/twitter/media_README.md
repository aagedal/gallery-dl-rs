# Experimental profile media timeline protocol

All timeline and user records in `src/twitter_timeline.rs` and
`src/twitter_profile_tests.rs` are synthetic, reusing the synthetic photo payload
and transaction fragments in this directory. No real browser profile, supplied
cookie file, private post, or signed live media URL was read or saved. Successful
authenticated live traversal and sanitized real response fixtures remain required.

Primary source inspected 2026-10-18:
[gallery-dl's Twitter extractor](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/twitter.py),
including `TwitterMediaExtractor`, `user_media`, `user_by_screen_name`,
`_pagination_tweets`, and the feature snapshots.

Current upstream operation snapshots are `ck5KkZ8t5cOmoLssopN99Q/UserByScreenName`
and `jCRhbOzdgOHp6u9H4g2tEg/UserMedia`, under `https://x.com/i/api/graphql/`.
Username lookup sends `screen_name` and `withGrokTranslatedBio=false`. Media
requests send `userId`, count 50, no promoted/client-event/Birdwatch content,
`withVoice=true`, and an optional opaque cursor. Feature/toggle JSON and cursors
are encoded as query values. These IDs are snapshots, not stable API contracts.

Native URLs accept `/USER/media` and `/id:NUMERIC_ID/media` on first-party
X/Twitter hosts. Numeric IDs bypass lookup. The requested user must match the
lookup and timeline root; tweet IDs and owner IDs must agree with their aliases.
Known structured timeline items/grid modules/module appends/pins are parsed in
instruction order, never by a recursive search that could substitute quoted
posts or recommendation payloads. Known promoted/recommendation entries and
valid text-only tweets are skipped. Retweets are skipped by default; explicit
retweet and quote settings use the shared bounded expansion engine after outer
owner validation. Unsupported/unavailable entries fail explicitly. Only actual
direct media are selected.

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

Other profile tabs, root-profile dispatch, cards/articles,
unknown timeline variants, dynamic operation-ID refresh, and real authenticated
verification remain unfinished. Compatibility remains the default.
