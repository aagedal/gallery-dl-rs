# Twitter/X experimental native extractor fixtures

The GraphQL payloads, legacy HTML, signing script fragments, usernames, URLs,
and posts are **synthetic**. The modern module fixture is a minimal sanitized
public HTML shape with a replaced bundle hash, as described below. These are
not complete captures and do not demonstrate that an authenticated X session
works. Never put auth_token, ct0, signed private CDN URLs,
raw private posts, or complete session payloads in this repository.

Protocol research (2026-10-18) used primary upstream sources:

- https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/twitter.py
- https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/utils/twitter_transaction_id.py

The original operation snapshot is TweetResultByRestId, authenticated with x.com cookies,
web-client Bearer authorization, CSRF, and a generated client transaction ID.
Homepage verification metadata/loading SVG and the referenced abs.twimg.com
ondemand script supply the transaction inputs. Script requests carry no cookies,
CSRF, or Authorization. Single-post media order and the requested root tweet ID
are checked. Explicit retweet/quote expansion is covered by synthetic derivatives
in `tests/twitter_expansion.rs` and local HTTP cases in `src/twitter_profile_tests.rs`.
These check original/repost identities, lazy missing-target requests, alias/root
validation, bounds/cycles, scoped credentials, ranges, dates, and cancellation.
Posts/replies/highlights tabs have synthetic/local coverage described in
[the profile timeline notes](media_README.md). Root-profile search fallback and standalone search/hashtags also have synthetic
coverage there. Likes, bookmarks, lists, and home feeds have separate
[collection protocol notes](collections_README.md). TweetDetail and conversation modes have separate
[detail protocol notes](detail_README.md). Playlists and other
collections remain unsupported natively. Compatibility remains the default.

`tests/twitter_media.rs` and local HTTP cases in `src/twitter_profile_tests.rs`
cover scoped `videos`/`previews` selection using synthetic mixed-media derivatives.
These verify direct video, skipped video, poster-only and video-plus-poster
ordering, consecutive file/archive positions, animated GIFs, disabled malformed
fields, atomic/private enabled-field failures, quote/retweet/thread propagation,
deduplication, and range/drop cancellation. Poster URLs preserve non-size query
parameters and use the configured primary image size. `videos: "ytdl"` emits the existing typed
video-page job for the validated post; no response-provided delegation URL is
trusted. Without an explicit direct HTTP fallback, the Rust downloader still
rejects these jobs pending the companion adapter. Parser/HTTP success is not evidence of a live authenticated download.

`tests/twitter_cards.rs` and local HTTP cases in `src/twitter_profile_tests.rs`
add synthetic card derivatives. Primary upstream sources above supply summary
prefix/size priority, card-name normalization, blacklist matching, unified media
and component order, and typed Ytdl fallback behavior. Both binding formats,
card-only posts, inherited video/preview choices, original/repost card ownership,
quote/thread traversal, numbering, archives, and cancellation are checked.
Unified media use a custom ordered JSON visitor so map sorting cannot reorder
files. Duplicate keys/references and malformed enabled fields fail privately and
atomically, including repeated components that could amplify output. This differs
from upstream's warning/partial-output behavior. Richer metadata, delegated
downloads, and live authenticated card verification remain
unimplemented or unverified. No card fixture contains a captured private session.

Unavailable-media cases are also synthetic. Upstream's `unavailable` option and
`ext_media_availability.status` field define the default skip and explicit-attempt
behavior. Parser/runtime tests cover photos, videos, GIFs, previews, unified cards,
quotes, threads, all-unavailable posts, remaining-file numbering, and private
malformed-field failures. Pages containing only unavailable media still advance
profile, search, and bookmark cursors. An opt-in cannot bypass protected-post identity/session
checks or permit unsafe media URLs. No authenticated marked-media attempt has
been verified.

Article fixtures in `src/twitter_articles.rs`, `tests/twitter_articles.rs`, and
local runtime cases are synthetic. Primary sources above and
[upstream article utilities](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/utils/twitter_article.py)
(inspected 2026-10-18) define default metadata/cover/media selection, flag aliases,
file order, and document structure. The native renderer escapes content and
attributes, validates links, handles UTF-16 ranges, and bounds output; malformed
optional data recovers with previously completed files/metadata intact. Tests
cover lists, entities, formatting, source ownership, shared metadata storage,
archives, quote/thread cutoff, and pagination after optional failures. Inline
text tests cover exact UTF-8 downloads, filtering, atomic publication, and sidecar
HTML exclusion. No authenticated article response or download has been captured.
Additional content variants and recovery warnings remain.

Image-size cases in `tests/twitter_images.rs` and local runtime fixtures are
synthetic. Default orig/4096x4096/large/medium/small selection and custom ordered
sizes preserve non-size query parameters, file order, and archive identities
across ordinary media, video posters, cards, articles, quotes, and threads.
Summary URLs without a size parameter and raw component/conversation-preview
URLs remain unchanged without size chains; article HTML uses only the primary
size. Private fallback context now changes native image request
hashes/default filenames and resume keys; existing paths/checkpoints are not
migrated, while social archive keys remain stable.

`tests/fallbacks.rs` and CLI HTTP fixtures verify shared `_fallback` execution:
ordered direct HTTP candidates, retries, scoped/default-enabled settings,
validation, cancellation, publication, and resume isolation. Explicit cross-origin
fallbacks clear all per-item headers, while cross-origin redirects remain
rejected. Size-filtered outcomes and local filesystem failures do not try later
candidates. Fallback publication keeps the original destination, metadata, and
archive identity, including the original extension without MIME adjustment.
Failed candidate partials are discarded when switching URLs; the last failed
candidate checkpoint is retained when resume is enabled. Private fallback URLs
are excluded from inspection/sidecars.
These local checks do not prove authenticated live X image fallback behavior or
implement delegated/segmented downloads.

`transaction-home.html` and `transaction-script.js` are deliberately small
synthetic shapes. The fixed transaction vector uses key bytes 0..31, protocol
seconds 1000, mask 164, and the specified API path. Its animation and token were
computed independently by the existing trusted upstream Python algorithm (only
as a development reference, never a runtime dependency). `animation-vectors.json`
adds 21 reference cases covering interpolation and endpoint/extrapolation curves.
The Rust tests compare exact token/animation strings.

Before claiming native live support, supply sanitized real public photo, video,
and mixed-post GraphQL captures and bootstrap HTML/JS fixtures with provenance,
then run a manual authenticated check with a disposable/public test post and
locally supplied current x.com `auth_token` and `ct0` Netscape cookies. Verify
original photo/video URLs, media order, metadata, cancellation, expiry/protected
post diagnostics, and actual downloads. Keep credentials local. Repeat when X
changes operation IDs, features, transaction assets, or request requirements.
No authenticated network check has been performed for this implementation.

The public unauthenticated bootstrap was checked on 2026-10-18: `x.com/`
served a newer module entry shell; `x.com/i/jf/` served the legacy verification,
loading SVG, and webpack map. The referenced current `ondemand.s` asset supplied
four indices and the Rust bootstrap generated a transaction successfully. This
verifies public bootstrap only, **not authenticated extraction**. The retained
module fixture is a minimal sanitized HTML shape with a replaced bundle hash;
full downloaded public pages/assets are kept outside the repository.

Experimental native profile tabs have separate
[synthetic timeline protocol notes](media_README.md). Local HTTP tests exercise
lazy traversal and account/transaction identity. Successful authenticated media
timeline extraction and sanitized real response fixtures remain unverified.
