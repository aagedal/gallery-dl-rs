# Instagram fixture and manual verification notes

These JSON responses are synthetic, minimal examples of the web REST media-info
schema, based on the protocol used by the
[upstream extractor](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/instagram.py).
They are not captured session responses. Media URLs use `cdn.example`; IDs,
captions, timestamps, and users are invented. No credentials or signed private
media URLs are included. Photo, video, mixed carousel, login, expired/challenged
session, missing-post, and rate-limit cases are covered. Local HTTP unit tests
also exercise request headers, cookies, status handling, redirects, and timeout.

Live verification is pending: no account session or accessible test post was
provided. Do not claim native site support based solely on these tests.

To verify manually, build with `cargo build --locked`, then run locally with your
own Netscape cookie export (keep it outside the repository):

```sh
./target/debug/gallery-dl-rs --extractor native -C /path/to/cookies.txt -j 'https://www.instagram.com/p/REAL_SHORTCODE/'
./target/debug/gallery-dl-rs --extractor native -C /path/to/cookies.txt -d /tmp/instagram-check 'https://www.instagram.com/p/REAL_SHORTCODE/'
```

Check an accessible single photo, multi-photo carousel, mixed carousel, and reel.
Confirm child count/order against the post, inspect downloaded files, and verify
that each video is playable. Check expired cookies, an inaccessible/missing post,
and a login redirect. Do not deliberately trigger rate limits; test those locally.
Record date, URL shape, result, media count/types, and account-access conditions
without storing session values or signed media URLs. Create sanitized real response
fixtures before considering native mode verified. Confirm Facebook/Twitter still
work through `--extractor compatibility` independently.

Profile tests in `src/instagram_profile_tests.rs` synthesize `web_profile_info`
identity responses and `feed/user/{id}/` pages based on the same upstream REST
protocol. They cover opaque cursor encoding, ordered pages, current-page
backpressure, late cursor failures, owner validation, private profiles, and active
timeouts. They are not live captures. Before claiming profile support, verify
an accessible profile spanning multiple pages and compare post counts/order,
including a carousel/video, an empty profile, and an authorized private profile.

Stories, highlight lists/shared highlights, and the signed-in story tray now have
separate [synthetic protocol and verification notes](stories_README.md). Their
authenticated live verification remains pending as well.

Explicit posts/reels/tagged/saved collections have separate
[synthetic request and pagination notes](collections_README.md), including POST
transport and source-order date filtering. Their live verification also remains pending.
