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
