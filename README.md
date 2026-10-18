# gallery-dl-rs

An early Rust implementation of a gallery downloader, inspired by
[gallery-dl](https://github.com/mikf/gallery-dl). The first target sites are
**Instagram, Facebook, and Twitter/X**.

**This is a working foundation, not a native Rust replacement for gallery-dl yet.**
The CLI, direct-link extraction, and HTTP download pipeline are Rust. Social-site
extraction defaults to a separately installed `gallery-dl` executable and reads
its JSON messages. Experimental native Instagram posts/collections/stories/highlights, Facebook
photos/albums/videos, and Twitter/X posts/profile timelines are available with `--extractor native`; authenticated live post
extraction has not yet been verified.

## Build and run

Requires a current stable Rust toolchain. The default social extractor additionally
requires `gallery-dl` on `PATH` (for example, install it with `pipx install gallery-dl`).
See [upstream installation instructions](https://github.com/mikf/gallery-dl#installation).

```sh
cargo build --release --locked

# List media URLs from a post without downloading them
./target/release/gallery-dl-rs -g 'https://www.instagram.com/p/SHORTCODE/'

# Extract using your session cookies, then download using Rust
./target/release/gallery-dl-rs -C cookies.txt 'https://www.instagram.com/p/SHORTCODE/'
./target/release/gallery-dl-rs -C cookies.txt 'https://www.facebook.com/photo/?fbid=PHOTO_ID'
./target/release/gallery-dl-rs -C cookies.txt 'https://x.com/USERNAME/status/POST_ID'

# Native direct media downloads need no Python or gallery-dl installation
./target/release/gallery-dl-rs --jobs 4 -d downloads 'https://example.com/photo.jpg'

# Keep validated partial downloads and resume after an interruption
./target/release/gallery-dl-rs --resume -d downloads 'https://example.com/large.mp4'

# Save available post metadata beside downloaded media
./target/release/gallery-dl-rs --write-metadata -C cookies.txt --extractor native 'https://www.instagram.com/p/SHORTCODE/'

# Keep completed-media history across runs, including refreshed social media URLs
./target/release/gallery-dl-rs --download-archive history.sqlite3 -C cookies.txt 'https://x.com/USERNAME/status/POST_ID'

# Choose native filenames from available post metadata
./target/release/gallery-dl-rs -f '{post_id}_{media_id}_{index:03}.{extension}' -C cookies.txt --extractor native 'https://www.instagram.com/p/SHORTCODE/'

# Inspect extracted media jobs as JSON; private request options are omitted
./target/release/gallery-dl-rs -j 'https://x.com/USERNAME/status/POST_ID'

# Experimental Instagram extraction in Rust, without gallery-dl
./target/release/gallery-dl-rs --extractor native -C cookies.txt -j 'https://www.instagram.com/p/SHORTCODE/'

# Experimental authenticated Twitter/X extraction in Rust
./target/release/gallery-dl-rs --extractor native -C x-cookies.txt -j 'https://x.com/USERNAME/status/POST_ID'

./target/release/gallery-dl-rs --help
```

The URLs above illustrate argument syntax; replace placeholders with real posts.
`--cookies` accepts a Netscape cookie file. `--config` accepts an explicit gallery-dl
configuration for site-specific settings. Default gallery-dl config files are
ignored. `--extractor-command /path/to/gallery-dl` selects another executable;
it is invoked directly, without a shell.

`--extractor compatibility` is the default. `--extractor native` experimentally
supports Instagram `/p/`, `/reel/`, and `/tv/` posts, Instagram `/USERNAME/` and
`/USERNAME/feed/` profiles, Instagram reels/tagged/saved collections and
stories/highlights (described below), Facebook `/photo/?fbid=ID`, `/photo.php?fbid=ID`, and
`/USER/photos/ALBUM/ID` photo permalinks, experimental Facebook sets via
`/media/set/?set=TOKEN` or `/photo/?set=TOKEN`, Facebook `/watch/?v=ID` and
`/USER/videos/ID` videos, and Twitter/X single-status plus `/USER/media`, `/USER/tweets`, `/USER/with_replies`,
and `/USER/highlights` URLs, root profiles, search, hashtags, likes, bookmarks,
lists, and home feeds.
Other profiles, threads, and album listings still need the
compatibility backend. There is no automatic fallback. Native mode rejects `--config`, which contains gallery-dl settings.
Use `--native-config` for settings applied by Rust with either extractor backend.

The native Instagram extractor reads Netscape cookies scoped to the API URL's domain, path,
HTTPS requirement, and expiry. Response `Set-Cookie` updates, including session
and CSRF rotation and deletions, are retained in memory across redirects, retries,
and later posts on the same extractor instance. Updates validate domain scope
against a compiled public-suffix list, honor path/expiry rules, and protect secure
cookies from insecure replacement. Server cookies are limited to 4 KiB each;
new identities are accepted while the jar has fewer than 256 entries (larger
input files are still loaded). The input file is never rewritten. This does not
log in or renew an expired session. Explicit Firefox imports are described below.
It follows up to five redirects within the same origin and
`/api/` paths, reselecting cookies and the CSRF token for each URL. Redirects to
login/challenge pages fail explicitly, as do authentication/session failures and
missing posts. `--retries` controls additional attempts for native HTTP 429,
HTTP 5xx, and send/read transport failures as well as downloads. Authentication,
missing-post, rejected-redirect, and parser errors are not retried; this includes
rate-limit payloads returned with HTTP 200. `--extraction-timeout` bounds the entire
native operation’s active time, including backoff and cooldown waits, with a 16 MiB response
limit per attempt. `--timeout` also bounds each native HTTP request (default 60
seconds), inside that overall deadline. Library callers retain zero retries with
`Instagram::new` and can opt in with `with_retries`; `with_request_timeout` sets a
separate request deadline.

Native Instagram responses include all carousel children in order and select the largest
available photo or direct video variant by pixel area. Missing children or video
variants fail extraction rather than silently downloading a partial post or video
thumbnail. JSON includes post/media IDs, shortcode, username, caption, timestamp,
and a one-based carousel index. Media requests carry user-agent/referer headers;
Instagram session cookies are never copied to media hosts.

Native Instagram profile traversal resolves the requested username to an ID and
fetches REST feed pages in order. Each post must belong to that ID. The extractor
retains at most one bounded response page and its parsed jobs; later pages are
requested only as downstream consumers drain the current page. Consumer pauses
are excluded from the cumulative extraction timeout. Missing/repeated pagination
cursors and inaccessible private profiles fail explicitly; results from earlier
pages remain available after a late failure. `Instagram::stream` supports these
collections; the collecting `extract` API remains single-post. Profile fixtures
are synthetic; live authenticated pagination remains unverified.

Native Instagram also streams `/USERNAME/posts/`, `/USERNAME/reels/`,
`/USERNAME/tagged/`, `/USERNAME/saved/` (or `/saved/all-posts/`), and
`/USERNAME/saved/COLLECTION_NAME/COLLECTION_ID/`. Posts, reels, and tagged routes
also accept `id:USER_ID` in place of the username to skip profile lookup. Saved
routes always use the currently authenticated account's saved items; their URL
username and collection-name label do not select another account or change the
numeric collection endpoint.

Reels use form POST pagination; the other routes use GET pagination. Each page
is validated before output, preserving post/carousel order, actual creator
metadata, and the same archive identities as single-post extraction. Profile
posts require the requested owner; reels also accept an explicitly listed
coauthor. Tagged and saved posts can belong to other creators. Missing wrappers,
conflicting identities, missing/repeated cursors, and malformed media fail
explicitly; earlier pages survive a later failure. Later pages are requested
only as consumers drain the current page, with shared active extraction budgets.
For these named routes, the API decides private-account access.

POST requests reuse scoped cookies, CSRF updates, proxy/user-agent settings,
retry cooldowns, response bounds, and cancellation. Same-origin API redirects
preserve POST bodies for 307/308; 301/302/303 switch to GET without the form body.
A retry restarts the original request. These routes have synthetic parser and
local transport tests; authenticated live verification and sanitized real
response fixtures remain required. See the [collection fixture notes](tests/fixtures/instagram/collections_README.md).

Experimental Instagram stories/highlights use `Instagram::stream` and the native
CLI. Supported URLs include `/stories/USERNAME/`, `/stories/USERNAME/MEDIA_ID/`,
`/stories/id:USER_ID/`, `/stories/highlights/HIGHLIGHT_ID/`, `/USERNAME/highlights/`,
and `/stories/me/` (the signed-in account's story tray). Highlight share links
`/s/BASE64_HIGHLIGHT_TOKEN/` and their `story_media_id` query selector are accepted;
the decoded token must identify a numeric `highlight:` reel. Explicit ID forms
skip username lookup. Session cookies still come from the configured Netscape
file or selected Firefox profile; the downloader does not log in.

Username lookup, highlight/story trays, reel identity, and owner/media identifiers
are checked before output. Reels are requested one at a time in source order,
without prefetch while the current reel is being consumed. Each bounded reel is
validated atomically; a later reel failure preserves earlier results. An empty
reel list is a valid empty collection; a selected missing story is an error.
Selecting a story by media ID does not require unrelated items to have usable
media variants. Private profile flags do not establish denial: the reels API
decides whether the session may access those stories.

Each item uses its own media ID, shortcode (derived from that ID when absent),
timestamp, and source position within its reel. Its stable archive identity is
the same when viewed through a direct story or a highlight; reel/owner IDs do not
replace the story media ID in `post_id`. Full upstream reel metadata, highlight
titles, expiry fields, music/sticker audio, split/order configuration, and segmented
video remain unimplemented. Synthetic parser/local HTTP tests cover the supported
protocol; authenticated live story/highlight extraction and sanitized real
response fixtures remain unverified. See the [story fixture notes](tests/fixtures/instagram/stories_README.md).

Native Facebook accepts only a Photo JSON object with the requested ID and its
own `image.uri`, never a global image or Open Graph preview. It selects the largest
identity-bound image and emits available metadata plus stable archive identities.
Cookies remain scoped to Facebook page requests; media receives user-agent and
referer only. Requests have bounded redirects, retries, cooldowns, response size,
and deadlines. Profile album listings are not implemented. An anonymous public page
check on 2026-10-18 returned no identity-bound photo; successful live extraction
and sanitized real response fixtures remain required. See the
[Facebook fixture notes](tests/fixtures/facebook/README.md).

Experimental Facebook album traversal uses `Facebook::stream` and the native CLI.
It resolves the first photo only from an ordered collection tied to the requested
set token, then follows each requested Photo's own set-bound navigation. A
`/photo/?fbid=ID&set=TOKEN&setextract` URL starts directly at that photo. Plain
photo permalinks keep their single-photo behavior; the collecting `extract`
library method handles individual photos/videos. Album output follows viewer navigation
and assigns a one-based album index while preserving photo/archive identity.

Only the current bounded HTML response and parsed photo are retained, plus a set
of visited IDs. No next-photo request occurs during downstream backpressure.
Cumulative extraction time excludes consumer pauses. Explicit null navigation or
a return to the starting photo completes traversal; other repeated IDs, ambiguous
identity, missing navigation, and unsupported seed envelopes fail explicitly.
Earlier photos survive late failures. Range/date cutoffs drop traversal before
later requests. A starting-photo URL follows navigation until an explicit end or
wrap to that starting photo; it does not use upstream's numeric jump heuristic.

Public album/photo checks on 2026-10-18 returned logged-out shells with no usable
Photo, token, or navigation data. Accepted seed/navigation nesting is covered by
synthetic protocol fixtures and local HTTP tests and remains unverified against
an authenticated response. Empty albums and additional live envelope variants
are not yet recognized. Compatibility remains the default. See the
[album fixture notes](tests/fixtures/facebook/album_README.md).

Experimental Facebook video extraction accepts `/watch/?v=ID` and
`/USER/videos/ID` URLs through both `Facebook::extract` and `Facebook::stream`.
Delivery fields must belong to the exact requested Video (or a legacy record
owning that `video_id`); previews and unrelated recommendations are excluded.
Inline DASH manifests select the largest complete-resource MP4 representation
and highest-bandwidth optional M4A audio. Video and audio remain separate files,
matching upstream's default output, with distinct `id.extension` archive identities.
Legacy and modern progressive MP4 delivery fields provide HD/SD selection when
there is no inline DASH manifest. Existing authenticated transport and configured
user-agent settings apply to video requests and both emitted files.

XML parsing is bounded to 2 MiB and 65,536 nodes, with DTD processing disabled.
Static, single-period manifests with complete-resource BaseURLs are supported;
SegmentBase initialization/index byte ranges refer to the same complete file.
Segment templates/lists, external initialization, encrypted media, external-only
manifests, and ambiguous or incomplete delivery fail explicitly. General segmented
downloads and audio/video merging remain unimplemented.

Anonymous native CLI checks on 2026-10-18 extracted the upstream public video
examples as one MP4 and as MP4 plus M4A. One request initially failed while reading
the response and succeeded with a longer timeout. Separate attempts to capture
the raw response returned logged-out shells, so sanitized real delivery fixtures
remain required. These checks verify extraction, not complete media downloads or
authenticated access. See the [video fixture notes](tests/fixtures/facebook/video_README.md).

Native Twitter/X requires current `x.com` `auth_token` and `ct0` cookies from
a Netscape file or explicit Firefox profile import;
it does not log in or use a guest session. It requests the exact status through
GraphQL, selecting original-size photos and the highest-bitrate direct MP4 video
variants in mixed-media order. IDs, username, caption, UTC timestamp, and one-based
media indices feed metadata, filename templates, and the same stable archive keys
as compatibility extraction. The response must match the requested root tweet;
retweets are skipped and quoted posts are not expanded by default. Missing video
variants fail instead of returning thumbnails. Protected, missing, expired-session,
rate-limit, and challenge responses fail explicitly.

Experimental X profile timelines use `Twitter::stream` and the native CLI at
`/USERNAME/media`, `/USERNAME/tweets`, `/USERNAME/with_replies`, and
`/USERNAME/highlights`, with equivalent `/id:USER_ID/TAB` forms on X/Twitter hosts.
Username lookup uses `UserByScreenName`. Numeric IDs skip lookup by default;
`pinned: true` requests `UserByRestId` metadata to identify the pinned post.
The response must identify the requested account. Media-tab posts must belong to
it; posts/replies/highlights may include conversation authors, whose IDs must
still agree with their own aliases. Structured entries, conversation/grid
modules, appended module items, replacement cursors, and explicit termination
preserve source order. Each post shares single-post media and archive handling.

The stream retains one bounded page, pending jobs, and sets of fixed-size cursor
and emitted-post identity digests. All media siblings from a post's first
occurrence are kept; overlapping pages, pins, and quote expansion do not emit the
same post again within that input. Later pages wait until current jobs drain;
dropping the stream or reaching a file range stops later requests. Active
extraction time includes profile lookup, transaction bootstrap, requests, and
retries, excluding consumer pauses. Empty pages with an advancing Bottom cursor
continue. A well-formed entries response without a Bottom cursor, or explicit
Bottom termination, completes traversal. Missing instructions, conflicting or
repeated cursors, unavailable posts, inconsistent identities, and unsupported
entries fail explicitly; emitted media survive a later failure. Known promotions,
recommendations, and valid text-only posts are skipped.

Scoped Twitter settings include `"pinned": false` (default) or `true`, and
`"replies": true` (default), `false`, or `"self"`. Pins are opt-in and precede
regular first-page posts, whether supplied as a pin instruction or identified in
user metadata. With `replies: false`, reply posts are skipped. With `"self"`, a
profile feed retains replies authored by that profile, while a standalone status
retains replies addressed to its own author. Quote posts are filtered
independently. These settings also forward to compatibility extraction only when
explicitly configured. Profile tabs use unordered date filtering so pins and
conversation context cannot hide later qualifying media.

Root profiles (`/USER`), `/USER/timeline`, `/id:USER_ID`, `/i/user/USER_ID`, and
`/intent/user?user_id=USER_ID` run the native timeline traversal. They resolve a
canonical username, including `UserByRestId` for numeric roots. The initial feed
uses media by default or tweets when retweets are enabled. Scoped `"strategy"`
can select `"auto"`, `"media"`, `"tweets"`, or `"with_replies"`. After that feed
drains, search continues from its last raw post ID with `from:USER max_id:ID
filter:links`; retweet modes add the matching include operators. If this link
search has no post results, traversal retries the same boundary without the
link filter. Empty initial feeds finish without a search. One deadline,
deduplication set, and cancellation scope cover all stages; reply/text filtering
does not discard the continuation boundary.

Native `/search?q=QUERY` and `/hashtag/TAG` URLs use the same lazy search engine.
Query parameters decode once, hashtag path plus signs stay literal, duplicate
queries fail explicitly, and query text is limited to 8 KiB. Scoped settings are:

- `search-results`: `Latest` (default), `Top`, `Media`, `People`, or `Lists`;
  upstream aliases `live`, `user`, and `list` are accepted. User/list result
  entries are skipped by this media pipeline.
- `search-pagination`: `max_id` (default), `cursor`, or `date`, with upstream
  `maxid`/`id` and `until`/`datetime`/`dt` aliases. Max-ID mode rewrites the query
  to the last raw post ID minus one; date mode rewrites `until:YYYY-MM-DD` from
  the post's snowflake timestamp and uses the opaque cursor when that full
  timestamp repeats. Empty advancing pages retain cursor pagination.
- `search-limit`: integer 1–100, default 20.
- `search-stop`: integer 0–128 or `auto` (default 3), the number of empty
  continuations tolerated before stopping. An explicit API keep-going hint
  resets that counter. Request-history and non-advancing-bound checks reject
  loops, and the active extraction deadline still applies.

A single valid `from:USER` query resolves that profile when `replies: "self"`
needs its owner context. Search/profile date filtering preserves source order.
All new settings forward to compatibility extraction when explicitly configured.
Native likes use `/USER/likes` or `/id:USER_ID/likes`. The response must identify
the requested profile, while liked posts may have different authors. The current
session must have access to that likes feed. Bookmarks use `/i/bookmarks`, list
posts use `/i/lists/LIST_ID`, and home feeds use `/home` or `/home/following` for
the latest feed and `/home/foryou` (`for-you`, `for_you`, and encoded `for you` also
accepted) for the
ranked feed. These routes use their own exact timeline envelopes, accept mixed
authors with consistent identity fields, and share post expansion, original
archive identities, first-page pin opt-in, duplicate suppression, and active-time
budgets. Present decimal list identifiers must match the requested list; opaque
GraphQL node IDs are not treated as decimal aliases.

Scoped `"limit"` sets the page size for profile and collection requests (integer
1–100, default 50), independently of `search-limit`, and forwards to compatibility
mode only when explicit. Bookmarks tolerate 128 consecutive empty continuations;
likes, lists, and home feeds stop at their first empty response unless the API
explicitly says to continue. All still stop on an explicit terminal response,
reject cursor loops, and defer later requests until current jobs drain. Collection
date filtering preserves source order because bookmark, like, list, and ranked
feed order need not match original post dates. No collection request credentials
are copied to media requests.

Cards/articles, communities, list membership/follow relations, arbitrary
root-profile dispatch selection, cursor-resume configuration, configurable
duplicate handling, text-only output, and other collection routes remain
unfinished. New collection behavior has synthetic/local coverage; successful
live authenticated traversal remains unverified. See the
[collection protocol notes](tests/fixtures/twitter/collections_README.md).

Scoped native configuration supports `"retweets": false` (default), `true`, or
`"original"`, and `"quoted": false` (default) or `true`. For example:

```json
{"extractor": {"twitter": {"retweets": "original", "quoted": true}}}
```

With `retweets: true`, original media/caption use the repost's ID, username, and
timestamp, with `retweet_id` recording the original post. With `"original"`, media
keep the original post's identity and `reposted_by_id` records the repost ID.
Quoted media follow their parent in source order, keep their direct-post archive
identity, and record `quoted_by_id`. These fields are omitted when absent. Archive
keys retain the Rust schema; this does not implement upstream's full metadata or
archive-template language.

Only explicit retweet/quote links are followed. Present aliases must agree and
target IDs must match. Missing embedded targets are fetched lazily through the
fixed status API; range limits and dropped streams prevent later requests.
Traversal rejects cycles, more than eight quote links, more than sixteen total
links, and more than four visibility wrappers. Single-post parsing is atomic;
streaming preserves media already emitted if a later linked post fails. API and
media requests retain their existing credential scopes and active-time budget.
Quote expansion uses unordered date filtering because linked posts need not be
chronological. Settings also forward to the compatibility backend when explicit;
absent settings preserve its configuration.

Timeline API IDs/features are upstream snapshots. Synthetic parser and local HTTP
tests cover traversal, signing paths, cookie rotation, user-agent propagation,
date/range selection, deadlines, and cancellation. Real response fixtures and
successful authenticated timeline traversal remain unverified. The collecting
`Twitter::extract` API remains single-post. See the [media timeline fixture notes](tests/fixtures/twitter/media_README.md).

The native X client generates transaction headers from verification/SVG data and
the current signing asset, with a first-party `/i/jf/` bootstrap fallback for newer
module pages. It parses signing inputs without executing downloaded JavaScript.
Public bootstrap was checked successfully on 2026-10-18; this does **not** verify
authenticated extraction. The GraphQL operation and web signing protocol are
undocumented snapshots and can change. Cookies/CSRF are reselected on each API
request/redirect, response cookies update the in-memory jar, and API redirects
remain within the same origin and GraphQL path. Signing assets and media requests
carry no session cookies or API authorization. Requests use the same bounded
responses, retries, per-origin cooldowns, cancellation, per-request timeout, and
overall extraction deadline as the native pipeline. Parser/reference/local HTTP
tests and the [Twitter fixture notes](tests/fixtures/twitter/README.md) describe
what is verified and the remaining real-response/manual authenticated gates.

## Browser sessions

Native mode can import an explicitly selected Firefox profile:

```sh
./target/release/gallery-dl-rs --extractor native \
  --cookies-from-browser 'firefox:/absolute/path/to/firefox-profile' \
  -j 'https://www.instagram.com/p/SHORTCODE/'
```

Log in to the site in that Firefox profile first (use `x.com` for X). Supply the
profile directory containing `cookies.sqlite`, or that database file itself.
Relative paths use the working directory; `~` expansion, automatic profile
search, container selectors, and native imports from other browsers are not
implemented. Native import selects only the requested site's default,
unpartitioned cookies; it does not combine container or partitioned logins.

The database is opened read-only, using a consistent transaction that includes
committed WAL records. Credentials stay in memory and retain domain, path,
secure, and expiry scope. Native imports are lazy and reused by the site's client;
response-cookie updates remain in memory. The importer bounds cookie counts and
sizes, rejects malformed records, and reports failures without profile paths or
cookie values. It creates no cookie export and does not write browser cookies.
Synthetic database/WAL tests verify import behavior; a successful live native
Instagram/X extraction remains unverified.

Compatibility mode forwards the browser specification directly to the installed
gallery-dl executable, which manages its own supported browsers and profile
syntax. For example, `--cookies-from-browser chrome` is available there when
supported by that executable. Rust does not read a browser database on this path.
The explicit browser flag conflicts with `-C/--cookies` and takes precedence over
all configured cookie sources. Explicit `-C/--cookies` also replaces a configured
browser source.

Native configuration accepts the upstream [`cookies` browser-array format](https://gdl-org.github.io/docs/configuration.html#extractor-cookies)
for explicit Firefox profiles, allowing separate sessions per site:

```json
{
  "extractor": {
    "instagram": { "cookies": ["firefox", "profiles/instagram"] },
    "twitter": { "cookies": ["firefox", "profiles/x"] }
  }
}
```

Profile paths in configuration are literal and relative to that file's directory.
They select a directory or `cookies.sqlite` file, without profile-name discovery.
An optional third entry must be null; an optional fourth entry may be null or
`"none"`, selecting the default non-container context. Other browsers, keyrings,
containers, and a fifth domain selector are not yet accepted in native configuration.
A cookie file string and a browser array replace one another according to normal
scope precedence. Sources are opened only when their site's native client is
needed; direct links and empty selections do not read a profile. Compatibility
mode forwards each configured Firefox source through `--cookies-from-browser`
without opening the database in Rust. Use the CLI browser flag for other browsers
supported by that backend.

## Network settings

`--proxy http://127.0.0.1:8080` sets an HTTP(S) proxy for native API/bootstrap and
media requests, and `-a` / `--user-agent 'My downloader/1'` overrides their HTTP
User-Agent. Both settings also reach the compatibility executable as individual
arguments before its URL separator. Native configuration supports `proxy` and
`user-agent` in the normal root/extractor/category scopes; explicit CLI values win.
Proxy URLs can include proxy credentials. SOCKS proxies, proxy path/query/fragment
components, and per-scheme bypass rules are not implemented.

Without a proxy setting, reqwest retains its environment/system proxy behavior.
An explicit empty value (`--proxy ''` or `"proxy": ""`) disables that discovery.
Configured proxies replace automatic discovery. TLS verification and the existing
manual redirect rules remain in effect. User agents must be nonempty ASCII
headers of at most 1024 bytes; invalid proxy/header settings fail without quoting
private values. Network configuration Debug output is redacted.

An explicit user agent replaces extracted User-Agent headers regardless of case,
and becomes part of the effective media request before deduplication, destination
hashes, direct-link archive IDs, and resume identities. Changing it can therefore
produce a different default filename or direct-link archive entry. Stable social
archive IDs retain their existing meaning. Proxy credentials never become media
headers, inspection data, sidecars, or resume records. Proxy routing itself does
not change a job's stable identity.

Library callers can use `NetworkOptions`, native `with_network`, and downloader
`with_network`; `DownloadClient::effective_media` exposes the normalized job for
matching destination calculations. A category's download clients reuse their
connection pool and share the origin cooldown scheduler. The setting changes an
HTTP header; TLS/browser impersonation remains unimplemented.

## URL input files

Use `-i urls.txt` / `--input-file urls.txt` to read a UTF-8 URL list; repeat the
option for more files. Positional URLs run first, followed by files in option
order. Paths are relative to the working directory. `-i -` reads stdin, for example:

```sh
cat urls.txt | ./target/release/gallery-dl-rs -i -
./target/release/gallery-dl-rs -g -i first.txt -i second.txt
```

Each nonempty line supplies one URL. Leading/trailing whitespace, CRLF, and a
first-line UTF-8 BOM are accepted. Lines beginning with `#` are ignored. A `#`
preceded by whitespace starts an inline comment; URL fragments such as `#part`
remain intact. Source files are never changed. Empty files succeed, and stdin can
be listed only once.

Inputs stream line by line; extraction and downloads can proceed before stdin
reaches EOF. Only the current line (up to 64 KiB before the newline) and an 8 KiB
read buffer are retained by the file reader. Backpressure pauses later input.
Unread files are opened lazily, native clients initialize only when their site
is reached, and archives open on their first selected media download. Same-path
archive connections and site clients are reused across files. The existing
fixed-size media deduplication digests still grow with unique emitted jobs;
JSON inspection also retains its result array.

Missing/unreadable files report an error and continue to later files. Malformed
UTF-8, oversized lines, and inline configuration directives report a line error
and continue after that line; affected runs return a failure status even if later
URLs succeed. Input-file diagnostics identify the file ordinal and line number
without echoing contents or paths. Inline `-KEY=VALUE` / `-G` options, configured
`input-files`, and success-based comment/delete modes remain unimplemented.

## Media selection

`--range '2-5,8,10:20:3'` selects media positions separately for each input URL.
Positions start at one. Hyphen endpoints are inclusive (`2-5` selects 2 through
5); slices exclude their stop (`10:20:3` selects 10, 13, 16, 19). Comma groups
form a union and output retains extraction order. Omitted endpoints default to
one or the signed 64-bit maximum; `5-`, `5:`, and `5::` start at item five.
Signed slice steps are supported as mathematical index sets; negative arguments
are not relative to collection length and do not reverse the output order.

The same selection applies to downloads, `-g`, and `-j`. Native configuration
accepts `"file-range": "2-5"` or `"file-range": ["2-5", "8"]` in the normal
scopes; an explicit CLI range overrides it. Empty configuration strings/lists
restore all files; `0` or an empty mathematical range selects none. Specifications
are limited to 4096 bytes, 256 groups, and signed 64-bit integer arguments;
malformed values fail explicitly.

Selection counts emitted media before global deduplication and archive checks;
duplicates and archived media therefore occupy positions. Extraction errors are
reported without occupying a media position. At the last requested position the
upstream stream is dropped, cancelling pending work and the owned compatibility
process. No later pages or rows are consumed. Earlier selected downloads finish
normally. An empty selection starts no extraction or media requests and opens no
archive. The selector scans earlier media rather than using site-specific skip
optimizations; native extractors still parse a whole response page or post.

Compatibility positions count the resolved media stream for an entire input,
including flattened child extractors; counters do not yet reset at nested child
boundaries. The separate compatibility configuration can apply its own selection
before Rust receives media. Post/child ranges and metadata expressions remain
unimplemented.

## Date selection

`--date-before 2026-10-01 --date-after 2026-09-01` selects posts strictly between
those instants. `--date-before` excludes posts at or after its bound and continues
looking. `--date-after` stops the current input at the first post at or before its
bound, drops the extractor, and proceeds to the next input. This assumes newest
first ordering; an older pinned or out-of-order post can end traversal early.
Already selected downloads finish normally.

Recognized Instagram story, highlight, story-tray, reels, tagged, and saved-collection URLs, plus X profile timelines, use source-order date
filtering with either extractor backend: `--date-after` skips older items and
continues scanning. This avoids dropping newer items later in an ascending or
mixed-order source. Library users can select the same behavior with
`DateFilter::apply_unordered`. Date selection still runs before file ranges.

Values accept signed integer Unix seconds, `YYYY-MM-DD` (midnight UTC), or
`YYYY-MM-DDTHH:MM[:SS]` with an optional dot-separated fraction after seconds.
A space can replace `T`. Naive date-times use UTC; `Z`, `+HH:MM`, `-HH:MM`, and
compact `+HHMM`/`-HHMM` offsets normalize to UTC. Fractions are discarded to whole
seconds, including before 1970. Inputs are limited to 128 ASCII bytes; calendar,
clock, offset, and integer bounds are validated. Relative dates, week dates, and
the full upstream ISO syntax are not supported. An effective `date-after` at or
later than `date-before` fails explicitly. Epoch zero is a real bound.

Native configuration supports `date-before` and `date-after` as strings or
integer timestamps in the normal scopes, followed by CLI overrides. Date
selection applies to downloads and inspection before file ranges, deduplication,
and archive checks. Excluded media receives no files, sidecars, or archive entry.
Media with missing or unparseable timestamps passes through unchanged, including
direct links. Compatibility formatted dates are normalized into public numeric
metadata when recognized; invalid formats remain unknown. Counters and cutoffs
apply to each input's flattened stream; site-specific date skipping and nested
child boundaries remain unimplemented.

## File-size limits

`--filesize-min 500k --filesize-max 2.5M` filters direct HTTP media by its complete
size, including resumed downloads. Bare numbers are bytes; suffixes `b`, `k`, `m`,
`g`, `t`, and `p` are case-insensitive powers of 1024. Fractional amounts round to
the nearest even byte. Both bounds are inclusive, and `0` disables that bound.
Negative, non-finite, overflowing, or contradictory effective limits fail before
downloading. Native configuration accepts `filesize-min` / `filesize-max` as
numbers or strings in the normal root/extractor/category scopes; explicit CLI
values, including zero, take precedence.

Known response sizes are checked before reading the body. For unknown lengths,
the maximum is checked before writing each chunk and the minimum after complete
EOF. Resume checks use the validated full representation size, including a
completed partial confirmed by HTTP 416; stale partials cannot reject a changed,
allowed representation. Rejected resume state loses its checkpoint and stored
bytes; an empty partial and lock file may remain.

Filtering succeeds without publishing media, metadata, or an archive entry, so
a later run with different bounds can download it. Filtering itself is not
retried. Existing files and archive hits retain their ordinary skip behavior;
limits do not delete or recheck them. `-g` and `-j` inspect extraction results
without requesting media or applying size filters. Empty category/output folders
may remain after a rejected response.

## Native configuration

`--native-config settings.json` loads an explicit, strict subset of
[gallery-dl's JSON configuration structure](https://gdl-org.github.io/docs/configuration.html).
It works with native and compatibility extraction;
`--config` remains a separate file passed only to the compatibility executable.
Neither configuration file is loaded automatically.

```json
{
  "extractor": {
    "base-directory": "downloads",
    "retries": 2,
    "timeout": 60,
    "instagram": {
      "cookies": "cookies.txt",
      "archive": "instagram.sqlite3",
      "directory": ["{category}", "{username}", "{post_id}"],
      "filename": "{post_id}_{media_id}.{extension}"
    },
    "direct": {
      "filename": "{filename}"
    }
  }
}
```

Supported settings are `base-directory`, `directory`, `filename`, `file-range`, `cookies`, `archive`,
`proxy`, `user-agent`, `date-before`, `date-after`, `filesize-min`, `filesize-max`,
`retweets`, `quoted`, `pinned`, `replies`, `strategy`, `search-pagination`,
`search-results`, `limit`, `search-limit`, `search-stop`, `retries` (integer 0–10), and `timeout` (positive seconds, including fractions).
They can appear at the root, under `extractor`, or under `extractor.instagram`,
`extractor.twitter`, `extractor.facebook`, and `extractor.direct`. Precedence is
built-in defaults, extractor defaults, category settings, root settings, then
explicit CLI flags. In particular, an explicit `--retries 2` or `--timeout 60`
overrides a different configured value even though it equals the built-in default.

Configuration paths are literal and relative to the configuration file's parent
directory. CLI paths remain relative to the working directory. Cookies apply to
social extraction; retries/timeouts apply to Rust downloads and native extraction
(the compatibility executable retains its own network settings). Native extraction
also retains its overall `--extraction-timeout`. Category settings select each
media item's destination, filename, and archive. Inspection with `-g`/`-j` performs
extraction without creating configured destinations or opening archives.
The CLI also accepts fractional `--timeout` seconds; zero, non-finite values, and
timeouts outside the monotonic clock's range or precision are rejected.

Files are limited to 1 MiB. Unknown settings/categories, duplicate keys, null settings
(except the optional browser-array placeholders described above),
unsupported types, hooks, environment expansion, and conditional values fail
explicitly. Filenames use the native template subset described below; archive
files use the Rust archive format. Full gallery-dl configuration compatibility,
conditional directory templates, automatic discovery, and postprocessors remain unimplemented.

## Current behavior

- By default Instagram, Facebook, and Twitter/X URLs use the compatibility backend, including
  its child-extractor resolution. Actual URL coverage and login requirements
  depend on the installed gallery-dl version and the account's access.
- Direct media links use a native extractor. Arbitrary HTML galleries are not
  implemented in this milestone.
- Download mode pulls one input at a time and incrementally parses compatibility
  messages into at most `--jobs` active downloads. A full pipeline pauses further
  stdout reads, applying backpressure to the process. Downloads can finish while
  extraction waits for more output. `-g` emits deduplicated URLs as individual
  media arrive, including before an input finishes; `-j` retains one ordered array.
  A later malformed message, truncated result, failed process exit, or timeout
  returns a nonzero status while keeping completed media and processing later
  inputs. Each encoded message is limited to 16 MiB with bounded nesting, and
  stderr drains concurrently while retaining only its final 8 KiB for diagnostics.
  Compatibility `--extraction-timeout` counts cumulative active reading/parsing
  and process-wait time; pauses caused by full download capacity do not count.
  Native extraction retains its overall per-input deadline.
- Media bodies stream to disk with bounded download concurrency. Each completed
  file is published from a temporary file without overwriting an existing file.
- `--resume` retains partial direct HTTP media in a hidden `.gallery-dl-rs`
  directory under the category directory, across retries, failures, and Ctrl-C.
  Subsequent attempts use `Range` and `If-Range` only with a saved strong ETag,
  or a Last-Modified value at least 60 seconds before the response Date when no
  ETag was provided. Weak/missing validators restart the full file. Full responses
  replace partial bytes; range responses must match the saved representation,
  effective URL, offset, total, and byte count. A validated 416 can finish an
  already complete partial; other 416 responses restart with a full request.
  Changed validators or redirect targets also restart without mixing bytes.
  Resume requests require identity content encoding. Checkpoint names hash the
  full request identity and destination; signed URL or header changes therefore
  start separate partials. JSON checkpoints contain only a validator, size, and
  effective URL digest; URLs, cookies, and request headers are not stored.
  File locks exclude simultaneous users of the same checkpoint and are released
  on exit. Empty `.lock` files remain to preserve locking across processes;
  completed partials/checkpoints are removed after no-overwrite publication.
  Remove an unused checkpoint directory only while no downloads are running.
  Without `--resume`, failed or cancelled partials are removed as before.
  This flag conflicts with `-g` and `-j`.
- `--write-metadata` saves `<media filename>.json` beside completed media, including
  files skipped because they already exist. Sidecars have `schema_version: 1`,
  `category`, the saved `filename`, and optional `metadata` (post/media IDs,
  shortcode, username, caption, timestamp, and carousel/album index). Post metadata is
  provided by native Instagram/Facebook/Twitter and allowlisted compatibility Instagram,
  Twitter/X, and Facebook fields when available. Compatibility timestamps are
  normalized from numeric epochs or recognized ISO calendar date-times; unsupported
  backend date formats are omitted. Instagram
  uses site post/media IDs, Twitter uses tweet ID and file number, and Facebook
  uses `id.extension` as media ID to distinguish audio and video. Non-Instagram
  shortcodes are empty; unavailable fields stay empty/null. URLs, request headers,
  cookies, and private adapter options are excluded.
  Existing sidecars are never overwritten. Sidecar publication is atomic but is
  separate from media publication: a sidecar failure returns a nonzero status and
  keeps the media; rerunning backfills missing sidecars without downloading again.
  Cancellation between the two publications can also leave a missing sidecar.
  This flag conflicts with `-g` and `-j`; inspection JSON retains its existing format.
- `--download-archive FILE` (alias `--archive`) keeps completed-media history in
  SQLite. Social identities use stable, site-specific post/media identifiers,
  independent of signed URLs or headers. Missing or ambiguous social IDs are
  downloaded normally without an archive entry. Direct links use a hash of the
  full URL and request context, so changed direct URLs remain distinct. Entries
  store only namespaced hashes and completion times, without URLs or credentials.
  Archive mode serializes downloads of the same stable identity within the run:
  completed items are skipped, while a failed URL leaves later alternatives usable. Files skipped
  because they already exist are recorded after any requested metadata repair;
  failed downloads or sidecars are never recorded. A database write failure keeps
  completed media and returns a nonzero exit status; rerunning can record it.
  Archive hits skip without checking the filesystem, even if files were removed
  or `--write-metadata` was enabled later. Omit the archive to backfill sidecars.
  Extraction still runs to discover media identities. Separate simultaneous runs
  can both download an archive miss; inserts are idempotent. The parent directory
  must exist, and this archive format is separate from Python gallery-dl archives.
  This flag conflicts with `-g` and `-j`.
- Filenames use a SHA-256 digest of the media URL and job/request context plus a
  sanitized filename, under `destination/category/`. Exact duplicate jobs within
  a run are removed, and existing paths are skipped for direct downloads. Bare
  direct links keep their original URL-only digest; media with headers now use a
  different digest and may be downloaded again after upgrading.
- `-f` / `--filename` replaces the default digest-prefixed basename with a native
  template under `destination/category/`. Available fields are `{filename}`
  (including extension), `{name}` (without the final extension), `{extension}`
  (without the dot), `{category}`, `{post_id}`, `{media_id}`, `{shortcode}`,
  `{username}`, `{index}`, and `{timestamp}`. Only index supports zero-padding,
  such as `{index:03}` or `{index:03d}`. `{{` / `}}` escape braces; all output then
  uses safe ASCII filename characters, so literal braces become underscores.
  Unknown fields/formats, missing values, and literal directory paths fail
  explicitly. Substituted values are sanitized; names are limited to 240 bytes
  while preserving the final extension. Include enough IDs to make names unique:
  files at colliding paths are assumed complete and skipped without overwriting.
  Changing the template does not clear archive history. This is a limited native
  syntax; gallery-dl expressions, conversions, and conditionals are not supported.
  This flag conflicts with `-g` and `-j`.
- `--directory-template '{category}' --directory-template '{username}'` replaces
  the default category folder with ordered component templates under the output
  root. Configuration uses `"directory": ["{category}", "{username}"]`; an explicit
  empty list saves directly in the base directory. Fields and sanitization match
  filename templates. Components cannot contain literal paths, control characters,
  or drive prefixes; metadata separators are sanitized. Limits are 32 components,
  8 KiB per encoded component template, 240 bytes per rendered component, and
  3072 bytes for the rendered relative directory. Missing fields fail before
  requests or directory creation. `-D` / `--directory PATH` chooses an exact
  directory, overriding configured base/directory values; it conflicts with `-d`
  and `--directory-template`. Filename selection, sidecars, resume checkpoints,
  no-overwrite handling, and archives all use the rendered destination. Archive
  hits retain their existing meaning even after changing the output layout.
- Transport errors, HTTP 429 rate limits, and HTTP 5xx responses have bounded
  retries controlled by `--retries`. Retry-After accepts seconds or an HTTP date;
  missing/invalid values use exponential backoff. Concurrent downloads share
  cooldowns by origin (scheme, host, and port), including cooldowns reported on a
  final attempt. Native Instagram, Facebook, and Twitter reuse the same scheduler implementation, sharing cooldowns
  across calls on an extractor instance and recording them even on a final
  attempt. Requests already in flight can finish. `--timeout` bounds each media
  attempt, including its redirect chain and cooldown waits; longer cooldowns fail
  with retry-later advice.
  Native extraction uses its overall `--extraction-timeout` deadline. The
  compatibility backend retains its own retries and separate extraction timeout.
- Failed inputs do not prevent other inputs from being processed. Any failed
  extraction or download results in a nonzero exit code. Status goes to stderr;
  `-g` and `-j` output goes to stdout.
- Ctrl-C cancels extraction, active downloads, and retry/cooldown waits, removes
  temporary downloads (or retains checkpoints with `--resume`), and stops the directly owned compatibility process. Files
  already published are kept. Cancellation exits with status 130.

## Limitations

Facebook extraction is **not native Rust**, and live authenticated social downloads
have not been verified in this repository. Native Instagram and Twitter/X use
undocumented endpoints that may change; fixture tests establish parser and
transport behavior, not current live coverage. Tests exercise all three routing paths
with a fixture backend and HTTP downloads with a local server. Facebook coverage
in particular must be checked against the installed backend before relying on it.

Only direct HTTP(S) media is downloaded. Extraction retains typed direct HTTP,
video-page, and HLS/DASH jobs in input order. `-g` preserves the original `ytdl:`
source; `-j` includes the job kind and manifest protocol/base URL. Inline manifest
data, headers, cookies, and other private handoff options are omitted from JSON.
The JSON output is for inspection, not a lossless job export. Different inline
manifests with the same source can therefore look identical in inspection output.

Manifest links (`.m3u8`/`.mpd`, case insensitive) can be inspected directly.
Compatibility extraction retains `_ytdl_*` and `_fallback` options for a future
adapter, including inline manifests and manifest-specific headers. Handoffs
require an HTTP(S) target/base URL, including synthetic `ytdl:https://…` URLs;
synthetic non-HTTP identifiers and unknown manifest types fail extraction.
Video-page and manifest downloads fail explicitly before any request or file
creation, even if a destination file exists. A stable archive hit can skip an
already completed logical media item before download-type validation.
No companion dependency or video
adapter is enabled yet. The parser follows the
[upstream handoff protocol](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/downloader/ytdl.py).

HLS/DASH downloads, video merging, fallback URL handling, broader native browser cookie import,
broader native collection pagination, and full gallery-dl filename/config compatibility are
not implemented.
Media downloads follow up to five redirects within the same origin (scheme,
host, and effective port), retaining the original destination name and request
headers. Cross-origin redirects, HTTPS downgrades, URL credentials, malformed
redirects, and loops fail without requesting the target. The Rust downloader
forwards per-item `_http_headers`, but does not import the backend's session cookie
jar or TLS/browser impersonation. Media requiring those features may fail even
when extraction succeeds.

Rust incrementally decodes the compatibility backend's resolved JSON array,
preserving its child-extractor handling. The external backend still collects
results internally before dumping that array; very large profiles can exhaust
its memory or hit `--extraction-timeout` before any output arrives. Native
single-post extraction still buffers one complete post. The Rust download
pipeline retains one message buffer plus active jobs, while deduplication keeps
a fixed-size SHA-256 digest per unique job across all inputs. `-j` additionally
buffers all unique media for its JSON array. Native collection pagination and
end-to-end streaming remain planned. Library callers can use `Backend::stream`;
`Backend::extract` deliberately collects a complete vector. Signed URL changes produce new filenames; use the
download archive to skip previously saved social media by stable IDs. Existing files are assumed complete,
without revalidation.

## Development

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

`src/extract.rs` owns URL dispatch and the compatibility protocol.
`src/backend.rs` owns bounded compatibility process streaming and active timeouts;
`src/messages.rs` incrementally decodes individual resolved messages.
`src/download.rs` owns request headers, downloads, retries, and file publication.
`src/archive.rs` owns SQLite completed-media history and archive-aware downloads.
`src/naming.rs` owns native filename/directory template parsing and safe component rendering.
`src/config.rs` owns native configuration parsing, validation, and precedence.
`src/resume.rs` owns locked partial files and durable checkpoints;
`src/range.rs` validates resumable HTTP representations and ranges.
`src/selection.rs` parses file-index ranges and filters streams with early cancellation.
`src/input.rs` reads bounded URL lines lazily from files and stdin.
`src/browser.rs` imports explicitly selected Firefox cookies with site/context scoping.
`src/size.rs` parses binary byte amounts and applies inclusive size limits.
`src/date.rs` normalizes date bounds and stops timestamp-ordered collections.
`src/network.rs` validates private proxy/header settings and builds HTTP clients.
`src/transport.rs` owns redirect validation, per-origin cooldowns for downloads
and native extraction, and Retry-After parsing.
`src/main.rs` owns CLI orchestration and process exit status.
`src/instagram.rs` owns the experimental native Instagram extractor;
`src/twitter.rs` owns experimental native Twitter/X and transaction bootstrap;
`src/twitter_expansion.rs` validates explicit retweet/quote links and media identity;
`src/twitter_search.rs` handles search routing, options, and pagination progress;
`src/twitter_collections.rs` validates collection routes and response envelopes;
`src/cookies.rs` owns scoped Netscape cookie selection. See the
[fixture and manual-check notes](tests/fixtures/instagram/README.md) before
promoting native extraction to the default.

See [ROADMAP.md](ROADMAP.md) for the native extractor work. This repository's
existing [GPL-3.0 license](LICENSE) is retained. The gallery-dl backend is a separate
program; this project does not bundle its Python source.
