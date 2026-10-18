# Experimental native X detail and conversations

All detail records in `src/twitter_detail_parse.rs`, `src/twitter_detail.rs`, and
`src/twitter_profile_tests.rs` are synthetic derivatives of the existing photo,
transaction, and timeline fixtures. No live private response, cookie values,
browser profile, or signed media URL was read or saved. Sanitized real captures
and successful authenticated live traversal remain required.

Primary sources inspected 2026-10-18:
[upstream Twitter extractor](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/twitter.py)
and [configuration reference](https://github.com/mikf/gallery-dl/blob/master/docs/configuration.rst),
including `_tweets_detail`, `_tweets_conversation`, `_expand_tweets`, `tweet_detail`,
`tweet-endpoint`, `conversations`, `expand`, and `unique`.

The operation snapshot is `iFEr5AcP121Og4wx9Yqo3w/TweetDetail`. Variables include
focalTweetId, referrer=profile, with_rux_injections=false,
includePromotedContent=false, withCommunity=true,
withQuickPromoteEligibilityTweetFields=false, withBirdwatchNotes=true, and
withVoice=true. Pagination adds an opaque cursor. Common pagination features
apply. Field toggles enable withArticleRichContentState and disable
withArticlePlainText, withGrokAnalyze, and withDisallowedReplyControls.

The exact instruction envelope is
`data.threaded_conversation_with_injections_v2.instructions`. Only known entries,
modules, appended items, pins, and cursor/end instructions are traversed.
Recognized TimelineTombstone, TweetUnavailable, TweetTombstone, and owned
result.tombstone records are skipped without logging their private text.
Accessible posts require canonical root IDs and consistent author aliases;
visibility wrapping is bounded to four. Unrelated nesting cannot establish the
requested post or replace a missing detail envelope.

Auto defaults to Detail for this authenticated native client. Explicit restid
keeps TweetResultByRestId; conversations always overrides endpoint selection.
Default Off selects only the exact requested post, errors on terminal absence,
and stops after its enabled quote expansion drains. All returns accessible
conversation context even when the focal post is missing. Accessible buffers
until the focal appears and returns nothing if it never appears. Context before
focal selection is capped at 16 MiB; overflow is an explicit private error.

Focal author context is resolved before releasing buffered replies when needed.
Original-retweet mode follows the original author and reuses any fetched target.
When All permits a missing focal, no author context is invented: SelfOnly retains
only replies whose author equals their reply recipient. Quote references are
resolved from known response posts before making additional fixed-ID requests.
The first occurrence of duplicate posts supplies quote data; output dedup keeps
all initial media siblings and suppresses later copies by default.

Scoped expand (default false) runs a raw child detail traversal for each distinct
conversation ID. Child posts do not recursively expand. Original-retweet mode
uses the original target's conversation ID; missing/null/empty IDs skip, explicit
false bypasses expansion, and malformed IDs fail privately. A failed child API
request falls back to the original post, including after prior child output.
Children retain the parent's author context and share its active-time budget,
range selection, and cancellation. Scoped unique (default true) controls repeated
post batches independently of the once-per-conversation set. False retains all
media siblings and disables the CLI's X deduplication across inputs. Both status
endpoints, profile/search feeds, and account collections share these settings.

Parser/selection/HTTP/CLI tests cover focal selection among unrelated posts,
page-late and inaccessible focal posts, quote reuse, original-retweet owners,
archive equivalence, memory bounds, order, strict private errors, exact variables
and transaction paths, scoped rotated credentials, source-order dates, consumer
pauses, overall deadlines, cursor loops, range/drop cancellation, and late
failure preservation, lazy thread expansion, original-retweet thread lookup,
duplicate settings, and canonical child API paths. Broader detail variants, extended
metadata, and real authenticated verification remain unfinished.
