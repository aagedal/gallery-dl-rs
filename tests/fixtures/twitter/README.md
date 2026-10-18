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

The operation snapshot is TweetResultByRestId, authenticated with x.com cookies,
web-client Bearer authorization, CSRF, and a generated client transaction ID.
Homepage verification metadata/loading SVG and the referenced abs.twimg.com
ondemand script supply the transaction inputs. Script requests carry no cookies,
CSRF, or Authorization. Single-post media order and the requested root tweet ID
are checked; quote/retweet expansion, other profile tabs, threads, cards/articles, playlists,
and collections remain unsupported natively. Compatibility remains the default.

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

Experimental native profile media now has separate
[synthetic timeline protocol notes](media_README.md). Local HTTP tests exercise
lazy traversal and account/transaction identity. Successful authenticated media
timeline extraction and sanitized real response fixtures remain unverified.
