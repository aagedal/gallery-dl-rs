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

`--extractor compatibility` is the default. In compatibility mode, URLs outside
every category listed here are still passed to gallery-dl, so any site it
supports works; those files keep gallery-dl's category and complete keyword
dictionary for filters, templates and postprocessors, and default to
`CATEGORY/{filename}.{extension}` paths (per-extractor upstream path formats
are not reported by gallery-dl, and no download-archive identity is derived). `--extractor native` experimentally
supports Instagram `/p/`, `/reel/`, and `/tv/` posts, Instagram `/USERNAME/` and
`/USERNAME/feed/` profiles, Instagram reels/tagged/saved collections and
stories/highlights (described below), Facebook `/photo/?fbid=ID`, `/photo.php?fbid=ID`, and
`/USER/photos/ALBUM/ID` photo permalinks, experimental Facebook sets via
`/media/set/?set=TOKEN` or `/photo/?set=TOKEN`, Facebook `/watch/?v=ID` and
`/USER/videos/ID` videos, and Twitter/X single-status plus `/USER/media`, `/USER/tweets`, `/USER/with_replies`,
and `/USER/highlights` URLs, root profiles, search, hashtags, likes, bookmarks,
lists, home feeds, and community feeds.
X conversation extraction is also available through scoped settings. Other
unsupported profiles and album listings still need the compatibility backend.
Native extraction does not fall back to that backend automatically. Native mode
rejects `--config`, which contains gallery-dl settings.
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
and deadlines. Page requests send upstream's navigation headers
(`Sec-Fetch-Mode: navigate`); album pages answer HTTP 400 without them. See the
[Facebook fixture notes](tests/fixtures/facebook/README.md).

Profile routes mirror upstream: `facebook.com/NAME` and `NAME/photos[_by]`
read the profile's photos, `NAME/avatar` its profile picture, and
`NAME/photos_albums[/TITLE]` each album (or the one titled `TITLE`) on the
profile's first album page. `profile.php?id=ID` (with `sk=photos` or
`sk=photos_albums`) and `people/NAME/ID/...` are accepted too. Profile pages
only name the set, albums or avatar photo; photos are then read like album and
photo URLs. Anonymous photo pages carry no next-photo navigation, so without
cookies an album or profile follows the first grid page listed on its album or
`photos_by` page; when the album continues beyond it, extraction ends with an
error asking for `--cookies` after the visible photos. Anonymous profile photos,
albums and avatars were checked live on 2026-10-23.

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

Experimental native Imgur extraction (`--extractor native`, category `imgur`)
accepts single images (`i.imgur.com/ID.ext`, `imgur.com/ID`), albums
(`imgur.com/a/ID`, `/a/title-ID`) and gallery posts (`/gallery/ID`,
`/gallery/slug-ID`, `/t/TAG/ID`). It reads Imgur's public `post/v1` API with
the anonymous web client ID used upstream; no cookies are needed. Galleries use
the albums endpoint and fall back to the media endpoint on 404. File URLs are
rebuilt as `https://i.imgur.com/{id}.{ext}` (`jpeg` becomes `jpg`; animated GIFs
use the MP4 rendition by default). Set `extractor.imgur.mp4` to `false` in the
native JSON configuration to preserve animated GIFs; static GIFs and native
MP4 videos are unaffected. This boolean also supports extractor and global
defaults with the usual configuration precedence. Each post is one
group: `post_id` is the album/gallery ID (or image ID), `media_id`/`id` the image
ID, `index` is 1-based, `timestamp` comes from `created_at`, `username` from the
account and `caption` from the image or post description/title. Default
filenames are `{id}.{ext}`; archive identity is the Imgur media ID alone. User
profiles, favorites, subreddit listings, searches and tag listings are rejected
as unsupported. Compatibility mode sends Imgur URLs to the backend.

Experimental native Bluesky extraction (`--extractor native`, category
`bluesky`) accepts single posts (`bsky.app/profile/NAME/post/RKEY`) and author
feeds (`bsky.app/profile/NAME`, `/media`, `/posts`, `/replies`, `/video`), custom feeds
(`/profile/NAME/feed/RKEY`) and lists (`/profile/NAME/lists/RKEY`), where NAME
is a handle or DID. It reads the public, unauthenticated AppView
(`public.api.bsky.app`): handles resolve via `com.atproto.identity.resolveHandle`, posts via
`getPostThread` and feeds via `getAuthorFeed` (`posts_with_media` for the bare
profile and `/media`, `posts_and_author_threads` for `/posts`, `posts_with_replies`
for `/replies`, `posts_with_video` for `/video`; cursor pagination
capped at 1,000 pages of 100). Custom feeds use `getFeed` with an
`app.bsky.feed.generator` record URI; lists use `getListFeed` with an
`app.bsky.graph.list` record URI. Their owners resolve once, while returned
posts retain their individual authors; the author-feed `reposts` restriction
does not filter mixed-author feed/list results. All feed/list pagination is
capped at 1,000 pages, follows opaque cursors (up to 1 KiB), and stops on
repeated cursors; invalid or oversized cursors are explicit errors. Images and videos (also inside
`recordWithMedia`) are emitted as full-size blob jobs using the media author's
discovered PDS and `/xrpc/com.atproto.sync.getBlob?did=DID&cid=CID`, built from
validated identifiers only. The root follows the actual author for mixed feeds,
liked posts and quotes; it also applies to avatars/banners. DID discovery caches
up to 1,024 successes or fallback roots per extractor, sharing results with like
repository reads. Failed discovery falls back to `https://bsky.social`.
Offline synchronous parsing helpers retain that fallback root; native `events`
and `stream` perform discovery. Blob host changes do not change DID/CID archive
identity. `extractor.bluesky.reposts` defaults to false and
selects the underlying author; own pinned posts retain their normal identity.
`extractor.bluesky.videos` defaults to true; false omits video blobs.
`extractor.bluesky.quoted` defaults to false; true follows quoted-post media
using hydrated quote records when available, fetching missing records only as
the stream reaches them. Quote URI, CID and author identities are checked,
blocked/detached/deleted quote views are skipped, and chains stop on cycles or
fail beyond 32 quotes. Date filters preserve later newer posts after older pins,
reposts, or quotes. Bare profiles use the posts feed when `quoted` or
`reposts` is true; explicit `/media` retains its media filter. Media-free posts
are skipped. Bare profiles queue one native child job for that default feed;
`include` can select other views as described below. Explicit feed URLs are root
jobs. `/avatar` and `/banner` (also `/background`) resolve the profile's blob CID
and emit original blob jobs, with upstream's synthetic JPEG/1000×1000 metadata.
`/info` exposes normalized handle/description in fileless post events; full
profile metadata dumping remains unfinished. `/likes` reads public repository
like records as described below. `post_id` is the underlying record key, `media_id`/`id` the blob
CID, `username` the underlying handle, `caption` the post text, `timestamp`
from `createdAt`; files carry aspect-ratio width/height, alt text and MIME type.
Default filenames are `{post_id}_{index}.{extension}`; archive identity is
(DID, CID), including quoted and reposted media. Compatibility mode sends
Bluesky URLs to the backend.

Native Bluesky likes resolve the actor's DID, discover its personal data server
from `plc.directory` or a `did:web` document, then read
`com.atproto.repo.listRecords` for `app.bsky.feed.like`. Discovery requires a
matching document ID and a credential-free HTTPS PDS endpoint; failures fall
back to `https://bsky.social`, matching upstream's default. Subject post URIs,
CIDs and authors are checked, and posts hydrate through AppView only as consumed.
Liked posts retain their original author/blob archive identity; the author-feed
`reposts` restriction does not remove other authors. Malformed, deleted and
inaccessible record subjects are skipped; repository-page errors fail the current
job. Source record cursors are opaque and bounded, empty pages can advance, and
repeated cursors or 1,000 pages stop traversal. Finite post/file ranges avoid
later hydration and pages while leaving sibling include views available.

`extractor.bluesky.endpoint: "getActorLikes"` selects the alternative AppView
feed endpoint; other bounded strings, `false` or `null` select repository records.
The alternative stops on an empty feed even with a cursor. No native Bluesky
login is implemented; restricted endpoints/content still report access errors.
Category-scoped `api-server` overrides the AppView base URL (HTTP(S), optional
path, no credentials/query/fragment), appending `/xrpc/`; it does not redirect
DID discovery or repository reads. An empty string, `false` or `null` restores
the public AppView. Both settings forward to compatibility mode. Blob routing
and repository reads use their authors' DID service roots independently of this
override; no API credentials are copied to blob jobs.

Experimental native Reddit extraction (`--extractor native`, category `reddit`)
accepts submissions (`/r/SUB/comments/ID`, `/comments/ID`, `redd.it/ID`),
subreddit listings (hot/new/top/rising/controversial, `?t=` kept), and user
submissions (`/user/NAME`, `/u/NAME`) via the public `.json` endpoints, paging
with `after` up to 100 pages. It downloads `i.redd.it` images, galleries in
`gallery_data` order, `v.redd.it` fallback MP4s (video only: Reddit serves audio
as separate DASH), and external links to direct media files; self posts,
crossposts, deleted posts, and other external sites are skipped. Plain `i.redd.it`
file links stay direct downloads in both modes. OAuth, comment/selftext link
scanning, searches, multireddits, and upstream's reddit options are not implemented.

Experimental native booru extraction (`--extractor native`) covers categories
`danbooru` (danbooru.donmai.us), `e621` (e621.net, e926.net), `gelbooru` and
`safebooru` (safebooru.org): single posts (`/posts/ID`,
`index.php?page=post&s=view&id=ID`), tag searches (`/posts?tags=...`,
`index.php?page=post&s=list&tags=...`) and Danbooru/e621 pools (`/pools/ID` or
`/pool/show/ID`). Gelbooru/Safebooru pools use
`index.php?page=pool&s=show&id=ID`. Danbooru favorite groups use
`/favorite_groups/ID` or
`/favorite_group/show/ID`. E621/e926 favorites use `/favorites?user_id=OWNER`;
the owner is an arbitrary query string, with an empty default. Gelbooru/Safebooru favorites
use `index.php?page=favorites&s=view&id=ID`.
Danbooru/e621 use before/after ID cursors or numbered pages according to search
tags, requesting 200/320 posts per page respectively. Ascending searches retain
the first API page and reverse subsequent pages before selecting the next cursor.
Searches stop below the source page-length threshold (180/300 by default), on
cancellation, or on an API error. They have no fixed 100-page cutoff. The
`threshold` configuration option accepts an integer, clamps values below one to
one, and restores the default for other JSON types, including null and booleans.
Gelbooru-style tag searches use `pid` (100 per page) and remain capped at 100 pages.
Danbooru pools and favorite groups paginate `ordpool:`/`ordfavgroup:` in API
order; e621 pools buffer numbered `pool:` pages and restore member order. Both
retain original member positions as `num`, including unavailable-member gaps.
Collection responses are exposed as `pool` or `favgroup`, with a normalized name
and without `post_ids`. Danbooru's category-scoped `order-posts` also accepts
`pool`, `pool_desc`, `id`, and `id_asc` (and source aliases). Nested collection
hook edits reach later posts; replacing or deleting the root collection keyword
detaches the current post. Metadata postprocessor archives use the source default
keys `p_{pool[id]}_{id}` and `fg_{favgroup[id]}_{id}`. Collection requests preserve
leading zeroes and the source URL digit capture. Posts without a file URL
(deleted, banned or restricted) are skipped. Files are accepted only from the
site's own HTTPS
CDN. `post_id` is the post ID, `media_id` the MD5, `content`/caption the
space-separated tag string, `username` the uploader name when the API provides
one (Gelbooru `owner`). Archive identity is (category, post ID). API-reported file size is exposed as
`filesize`; `rating` and signed `score` are available to post filters and filename
templates (e621 uses `score.total`). Per-category tags are not yet exposed.

Gelbooru pools read the title HTML, then list member IDs in 45-item HTML pages
and fetch each member through JSON DAPI. Safebooru reads IDs after the pool title
and fetches XML DAPI records without HTML pagination. Both preserve member order
and duplicate IDs, fetch lazily, and expose scalar `pool` and HTML-decoded
`pool_name` keywords. These keywords reset for each post; hook edits cannot alter
later requests. They have no synthetic `num`. Their metadata archive key is
`p_{pool}_{id}`. Safebooru XML IDs and tags retain their source string types.

E621/e926 favorites stream `/favorites.json` using numbered pages (initial page absent,
then 2, 3, ...), 320 posts per request, and the source page-length threshold.
They do not buffer a pool or interpret tag-order parameters. Query keys and
values are decoded once; the first decoded key wins, blank values are retained,
and entries without `=` are ignored. Supplied `page`/`limit` are replaced by the
pagination defaults; other parameters remain API-only. Only a query immediately
after `/favorites` is captured: the source prefix also matches suffix paths,
whose later query is ignored. No authenticated-owner lookup is performed.
The `user_id` keyword resets from the constructor for each post; Post/File hook
edits reach the current file but cannot change later posts or API requests.
Default directories are `{category}/Favorites/{user_id}` (empty owner segments
are omitted); filenames use the ordinary e621 format. Metadata postprocessor
archives use live `f_{user_id}_{id}` keys. Retained class descriptors also select
the correct default metadata archive for media-only library downloads.

Safebooru favorites begin with HTML at `pid=0`, collect numeric ID prefixes
following whitespace and `id="p`, and fetch each member through XML DAPI. A full
50-ID HTML page advances the offset by 50; empty or multi-record API responses
do not change that threshold. Traversal preserves duplicates and stops on
cancellation or error, with no fixed page cutoff. `favorite_id` is the scalar
integer owner, while HTML requests retain the lexical URL ID. The owner keyword
resets for each post; current Post/File edits reach its file and metadata archive.
Defaults are `{category}/favorites/{favorite_id}`, the ordinary DAPI filename,
and archive key `f_{favorite_id}_{id}`. DAPI source classes do not synthesize
`num`; hook-created values remain live within their post.

Gelbooru favorites use JSON throughout. A two-entry request determines API order
and count, then numbered 100-entry pages hydrate individual posts lazily.
Forward traversal continues until an empty entry page, even after a short page;
reverse traversal starts at the final count-derived page and reverses each page.
Category `order-posts` strings beginning with lowercase `r` or `a` flip the
detected direction. The setting retains raw JSON types and source truthiness;
compatibility forwarding preserves the configured value. Gelbooru uses the same
scalar `favorite_id`, paths, and archive format as Safebooru. Each post also gets
`date_favorited`, distinct from its creation date. It supports datetime templates
such as `{date_favorited:%Y-%m-%d}`; invalid timestamps retain the source's falsy
`[Invalid DateTime]` value. Hook-written strings remain strings, and metadata
JSON emits date strings without internal type markers.

Danbooru popular views use `/popular` or `/explore/posts/popular`; e621/e926
use `/explore/posts/popular`. These paginate numerically using the configured
threshold. API requests retain the original query parameters. Source keywords
`date` and `scale` default to the local calendar date and `day`; `week` normalizes
to Monday and `month` removes the date's final three characters. The date remains
a string: `{date!u}` works, while datetime formatting requires an explicit
conversion such as `{date!D:%Y}`. Defaults use
`{category}/popular/{scale}/{date}`, the ordinary filename, and metadata archive
`P_{scale[0]}_{date}_{id}` (an empty scale formats as `None` in the archive).
Post hook changes remain local to each post; creation timestamps and native
history retain the original post identity.

Danbooru/e621/e926 artist routes (`/artists/ID`, `/artists?QUERY`, and
`/artists/?QUERY`) discover independent tag-search child jobs. Artist searches
retain query parameters, normalize the initial page as a Python integer, and
continue at twenty artists per page. Names are validated and quoted before
child selection; selected children make tag requests lazily. Child filters and
ranges run before URL deduplication. Metadata inheritance defaults to the full
artist JSON under `_parent`, including parent child-hook edits. Set
`parent-metadata` to a nonempty string to choose another key, a falsy JSON value
to disable inheritance, or a truthy non-string value to flatten those fields.
The primary key wins over legacy `metadata-parent` by presence, including null.
Root edits to the shallow copy stay in the child; shared nested-map edits reach
`child-after`. Inherited fields are reapplied before each post and file, so flat
IDs, dates, names, tags, and filename fields can override API keywords. Native
history retains the original post identity.

Native booru `artist`, `artist-search`, and `tag` configuration objects select job hooks
by their actual class, even when inherited keywords change category labels.
They support inheritance, `init`, postprocessors/options, base directory,
filename/directory templates, child filters/ranges, post filters/ranges, and date
bounds. Custom parent keys can be used in templates, including templates declared in another scope. Tag
children use ordinary tag paths and source metadata archives; artist roots emit
no files and initialize parent hooks only with `init: true`.
Compatibility jobs retain category-level native hook and output settings.

Booru template keywords retain producer spelling independently of the native
logical basename. Danbooru derives `filename` and `extension` from the raw URL;
e621 uses `file.md5` and `file.ext` verbatim. Gelbooru/Safebooru preserve explicit
API filename/extension fields or derive them from the URL when extension is
absent. Root `md5` is retained only when the source supplies it. URL names are
percent-decoded with replacement for invalid UTF-8; empty extensions and `jpeg`
remain distinct from native logical suffixes at extraction. Download preparation
applies the source default extension mapping once before Prepare/File hooks
(lowercase JPEG aliases become `jpg`, and `html` becomes `htm`). Post hooks can
replace these keywords before that boundary; later File-hook replacements are
not mapped again. Safebooru prefers its API `file_url`; supported CDN
URLs without a recognized logical extension use `.bin` for the native basename.
Default paths now follow the source extractor class. Post downloads use the
category directory; tag searches add `search_tags`. Danbooru/e621 filenames are
`{category}_{id}_{filename}.{extension}`; Gelbooru/Safebooru use
`{category}_{id}_{md5}.{extension}`. Gelbooru/Safebooru pools use
`{category}/pool/{pool}` with the ordinary DAPI filename. Danbooru/e621 pools use
`{category}/pool/{pool[id]} {pool[name]}` and
`{num:>04}_{id}_{filename}.{extension}`. Danbooru favorite groups use
`{category}/Favorite Groups/{favgroup[id]} {favgroup[name]}` with the pool filename.
E926 source keywords and paths retain `e926` while its native settings remain
under `e621`. Spaces and Unicode are preserved, path separators are replaced,
control characters are removed, and empty directory segments are omitted.
Explicit filename and directory overrides apply independently; unmatched
conditional templates use the class default. Directories freeze before Post
hooks, filenames are built after Prepare hooks, and ordinary File-hook edits do
not rename the published file. Hash filename rebuilds and metadata
`extension-format` retain the selected formatter and directory. These defaults
also apply to compatibility streams with a supported booru input URL; standalone
`parse_messages` calls lack route provenance and keep their existing fallback.

Tag searches expose `search_tags` to metadata, filters, and templates, preserving
case, whitespace, empty values, and source query decoding. Danbooru selects the
last `tags=` capture; the other sites select the first. Gelbooru/Safebooru send
an empty API search for literal `all` while retaining `search_tags: "all"`.
Source-matched search URLs support default metadata archive keys
`t_{search_tags}_{id}`. Broader native URL aliases retain extraction support;
use an explicit `archive-format` when Python has no matching extractor.

Danbooru/e621 accept category-scoped `username` and `password` (the account's API
key) for HTTP Basic authentication. Gelbooru/Safebooru accept category-scoped
`user-id` and `api-key`; both must be nonempty to authenticate. Credentials are sent only to the
API origin, never attached to pool HTML, media requests or public JSON. Redirects
to another origin are refused. Authentication can expose posts hidden from public
API requests; posts without usable file URLs are still skipped. Plain CDN file
links stay direct downloads.

Experimental native Mastodon extraction (`--extractor native`) covers
mastodon.social, pawoo.net and baraag.net with upstream categories
`mastodon.social`, `pawoo` and `baraag`; the host is also exposed as `instance`.
Supported: statuses (`/@USER/ID`, `/web/statuses/ID`), user timelines
(`/@USER`, `/@USER/media`, `/users/USER`, including `@user@remote` and numeric
`id:ID` accounts), `/bookmarks`, `/favourites`, `/lists/ID`, and `/tags/TAG`.
Hashtags accept Unicode letters/numbers and underscores. Pages contain up to 40
statuses and traverse lazily, capped at 100 pages. Collection cursors come from
all `Link` headers, including bookmark cursors distinct from status IDs. Date
selection over collections preserves later newer posts in source order.

Shared options belong under `extractor.mastodon`; instance settings under
`extractor["mastodon.social"]`, `extractor.pawoo`, and `extractor.baraag` override
them. Set `access-token` only in an instance scope; the token is bound to that
instance's HTTPS API origin and never attached to media or other instances.
An empty string or false disables the native token. OAuth login/cache discovery
is not implemented. Explicit credentials and selection options also forward to
the compatibility backend for their input instance.

`reblogs` and `cards` default to false, `replies` to true, and `text-posts` to false.
Enabling reblogs includes original attachments under the outer status identity;
attachment archive IDs remain the same as direct extraction. `replies: false`
filters replies locally and in account API requests. `text-posts: true` emits
metadata-only post boundaries, so post ranges and filters include statuses
without files. `cards: true` appends the outer status's card preview image after
attachments and requests non-media-only account pages. Native `cards` accepts
booleans only. Numeric preview-thumbnail path IDs share upstream's `cardID`
archive identity; unknown thumbnail paths download without a stable archive key.
Otherwise media-less statuses are skipped (a media-less single
status is an error). Native `replies` accepts booleans only. Image, GIFV, video
and audio attachments are served from HTTPS CDN hosts without API credentials.
`post_id`/`id` is the status ID, `media_id` the attachment ID, `username` the account
`acct`, and caption the status HTML with tags stripped. Default filenames remain
`{post_id}_{media_id}.{extension}`. Default category directories now use the
upstream instance labels; existing files are not moved. Stable archive identity
remains (instance, attachment ID), including for older `mastodon` archive entries.

Experimental native Kemono/Coomer extraction (`--extractor native`) covers
kemono.cr/.su/.party (`kemono` category) and coomer.st/.su/.party (`coomer`).
Supported: posts (`/SERVICE/user/USER/post/ID`), creator listings
(`/SERVICE/user/USER`) and global post listings (`/posts`) via the `/api/v1` API
with `Accept: text/css`. Listings accept search (`q`), tag (`tag`) and initial
offset (`o`) query parameters; offsets round down to multiples of 50, with
50 posts per page and a 200-page cap. Global feeds preserve each post's creator
and service. Scoped `expand: true` fetches full post details lazily;
`endpoint: "posts+"` or `"legacy+"` does the same for creator listings only
(`"posts"`/`"legacy"` retain ordinary listings).
`max-posts` limits source posts, including file-less posts (zero removes the
limit). These settings forward to the compatibility backend when explicit.
Post limits and consumer cancellation prevent later detail/page requests.
Dates filter without stopping at an older post, since publication dates need
not follow listing order. Default selection is attachments, primary file, then
inline images for Kemono; Coomer selects the primary file first. `files` accepts
an ordered comma-separated string or list of `file`, `attachments` and `inline`;
`[]` selects no files. Inline sources are limited to safe `/inline/` or hashed
paths, optionally prefixed by the canonical kemono.cr/coomer.st origin.

`original` defaults to false, matching upstream: image files emit JPEG thumbnail
jobs on `https://img.HOST/thumbnail/data/PATH`, while other files are skipped.
`original: true` emits original files at `https://HOST/data/PATH`, with the
original name's extension when present. Named originals backed by a `.txt` URL
reject the exact missing-file placeholder before publication, including resumed
and HTTP 416 completions. Public post Referer and identity-encoding headers may
follow CDN redirects; credential headers disable that exception.

Repeated hashes are skipped by default. `duplicates` accepts a boolean, or a
comma-separated string/list of `file`, `attachment` and `inline` to retain
repeated files of those types. Hashless inline paths remain separate source
files. Source numbering/counts include nonimages skipped by thumbnail mode, so
file numbers can have gaps. At most 10,000 candidates are selected per post.

`post_id` is the post ID, `media_id` the file hash when available, `user` and
`creator_id` the immutable creator ID, `username` the optional creator display
name, `service` the service, caption the title, date from `published`/`added`, and
file description the original name. File `type` identifies `file`, `attachment`,
`inline` or `archive`. Compatibility output normalizes these fields; `service`,
`user`, `creator_id` and `creator_name` are available in filename templates. Media names
are `{post_id}_{index:02}.{extension}`; default destinations also carry the shared
request digest. New media headers change that digest and resume checkpoint keys.
Archive identity now uses service,
creator, post and source file number so requested duplicates and hashless inline
files remain distinct. Previous hash-based archive entries and output paths are
not migrated.
Plain `/data/` file links stay direct downloads.

`metadata` defaults to true for Kemono/Coomer and fetches each creator's profile
when their first post is consumed. A bounded cache retains up to 1,024 successful
or failed lookups, keyed by host, service and creator ID. `metadata: false`
suppresses these requests. Missing, invalid or unavailable profiles leave media
extraction available with the creator ID and service; `username` is then absent.
Profiles must match the requested identity. Typed `metadata.creator` includes
validated names, public ID, indexed/updated dates, relation ID, chat availability
and post/DM/share/chat counts when present. Filters and templates expose these
fields through `creator` and `user_profile`, plus `creator_id` and `creator_name`.
Profile renames preserve archive identity. Templates that previously used
`username` for the numeric ID should use `user` or `creator_id` instead.

API responses support gzip with separate 32 MiB encoded and decoded limits,
complete member/checksum validation and cancellable background decoding.
Unsupported content encodings fail explicitly. Media requests retain identity
encoding and their existing range/resume behavior.

`revisions` defaults to false; true includes the current post and historical
versions, while `"unique"` removes adjacent versions with equal content hashes.
`order-revisions` accepts `"desc"` (default, newest first), `"asc"` or `"reverse"`.
Explicit `/post/ID/revision` and `/post/ID/revisions` URLs select historical
versions only; append `/REVISION_ID` to select one. Explicit URLs bypass the
configured revision mode and its uniqueness filter. Embedded revision data is
used when available; otherwise one revision API request is made for each source
post as it is consumed. `max-posts` counts source posts and retains all revisions
of each selected post. Up to 1,000 historical versions are accepted per post.

Revision metadata exposes ID, content hash, source revision index and count in
`metadata.revision`; filters and templates also accept `revision_id`,
`revision_hash`, `revision_index` and `revision_count`. The current version uses
ID zero. Hashes follow upstream's compact sorted Unicode JSON and SHA-1 rules,
excluding revision IDs, import dates, navigation fields and original file names.
Nonfinite numeric overflows fail explicitly. Historical filenames gain `_rID`
before the extension, and their archive keys include the revision ID; current
post keys are unchanged. Compatibility output retains the same typed metadata.

Native Discord extraction supports `/discord/server/SERVER/CHANNEL`,
`/discord/server/SERVER/channel/CHANNEL`, `#CHANNEL`/`#channel/CHANNEL` channel
selectors, and `/discord/server/SERVER` for all channels in server order.
Server, channel and message IDs validate against the API response before output.
Channel messages paginate lazily in batches of 150, up to 200 pages per channel;
`order-posts` accepts `"asc"` (default), `"desc"` or `"reverse"`. Reverse order
uses the channel's post count and reverses each page. `max-posts` applies per
channel, including messages with no files. Dropping the stream stops later
channel/page requests. A failed channel reports an error while whole-server
traversal continues with later channels. Attachments precede inline Discord CDN links, with
repeated files retained; `files` and `duplicates` apply to creator/global posts.
The existing `original` setting selects site attachment thumbnails or original
files. Inline image links use validated direct Discord CDN URLs and their actual
extensions even in thumbnail mode; nonimages require `original: true`.

Discord metadata exposes immutable `server_id`/`channel_id`, optional server and
channel names, topic, NSFW/type and parent ID in `metadata.discord`. Filters and
templates accept `discord` and these fields directly. Message content supplies
`caption`/`content`; dates come from `published`. Archive keys use server,
message and source file position, preserving duplicate files and name changes.
This follows the gallery-dl 1.32.17 thumbnail baseline used by existing native
Kemono extraction; newer upstream master has removed thumbnail selection.
Further author/embed/revision/archive metadata and authenticated verification
remain unfinished.

Native collection discovery supports `/artists`, `/favorites`, and
`/account/favorites`. Artist discovery filters creator names with `q` and
services with `service` (both case-insensitive), then sorts by `sort_by`
(default `favorited`); `order=asc` selects ascending order, otherwise descending.
Favorites require session cookies: `favorites: "artist"` (default) traverses each
favorite creator or Discord server; `favorites: "post"` fetches individual posts.
A nonempty `type=artist` or `type=post` query overrides the configured type.
Favorites accept `sort` (default `updated` for artists, `faved_seq` for posts)
and `order` (descending by default; values other than `desc` select ascending).
Use query parameters on the bare favorites routes: upstream-compatible trailing
labels such as `/favorites/artists` do not select a type and ignore their query.
Repeated recognized query parameters use the first value; bare keys without
`=` are ignored.

Catalog responses are limited to 32 MiB and 500,000 entries, with bounded
background parsing and cancellable sorting. Numeric sort keys retain wide integer
precision; equal nonempty keys retain source order, while empty/zero keys follow
upstream's reversed source ordering. Child identities validate only when consumed,
and media retain the same metadata/archive identity as direct post/creator URLs.
Child requests are lazy, failures report an error and leave later children
available, and cancellation drops the active child. `max-posts` applies per
creator or Discord channel. Native child jobs reset post/file/child counters;
`--child-range` and `--child-filter` select creators and channels before requests.

Kemono/Coomer API requests now accept scoped Netscape or imported Firefox
cookies, recomputed for each request URL and rotated from responses. Cookies
respect domain, path, expiry and Secure attributes, and stay out of media jobs
and inspection output. API redirects remain confined to the original origin;
public site media may follow CDN redirects with validated post/channel Referers.
Username/password login is not implemented.

Experimental native image host extraction (`--extractor native`), one category
per site: `pixeldrain` files (`/u/ID`, via `/api/file/ID/info`, downloaded from
`/api/file/ID?download`) and lists (`/l/ID`, `/api/list/ID`), including
zero-based `#item=N` selection with original list numbering (out-of-range
selection produces no files). Set `extractor.pixeldrain.api-key` to use HTTP
Basic authentication for API requests and file downloads; credentials stay in
private request headers and are omitted from inspection JSON. `catbox` albums
(`/c/ID`, file links, `<h1>` title and creation date from the page); `imgbox` galleries
(`/g/ID`, thumbnails mapped to `imagesN.imgbox.com/.../ID_o.EXT`) and image pages
(`/ID`); `postimg` (postimg.cc/postimages.org) image pages (download link, else
`og:image`) and galleries (thumbnail links, each image page fetched; capped at
1000). Media must be HTTPS on the site's CDN. `post_id` is the file/list/album/
gallery ID, `media_id` the file ID, caption the title (or file name), file
`media_type` the MIME type and description the original name. Filenames are
`{media_id}.{extension}` for single files and `{post_id}_{index:02}.{extension}`
otherwise; archive identity is (category, file ID). Plain CDN file links
(files.catbox.moe, i.postimg.cc, images2.imgbox.com) stay direct downloads.

Experimental native Tumblr extraction (`--extractor native`, `tumblr`
category) covers blog listings (`BLOG.tumblr.com`, `/posts`,
`www.tumblr.com/BLOG`), single posts (`/post/ID[/slug]`,
`www.tumblr.com/BLOG/ID`) and tags (`/tagged/TAG`) via API v2
(`/v2/blog/BLOG.tumblr.com/posts`, legacy format, reblogs included, offset
paging capped at 200 pages). It uses upstream's public API key unless
`extractor.tumblr.api-key` is set (accepted only in the `tumblr` category).
Media: photo `original_size` or a larger `alt_sizes` rendition, `video_url`, direct
`audio_url`, and inline `<img>` images; all must be HTTPS on
`*.media.tumblr.com` (audio also `a.tumblr.com`). `post_id` is the post ID,
`media_id` a hash of the media path, `username` the blog name, caption the
summary (or caption with HTML stripped), with timestamp and width/height.
Default filenames are `{post_id}_{index}.{extension}`; archive identity is
(blog, post, media ID). Capped photo renditions (`s2048x3072` at width 2048 or
height 3072) and modern inline image sizes request the original through
Tumblr's `s99999x99999` HTML token page. Legacy inline image sizes become
`_1280`; GIF downloads also offer a GIFV fallback. Token requests use the
configured network settings and timeouts without API credentials. Failed
token lookups retain the API rendition as a download fallback; upstream's
delayed token refresh retries are not implemented. Archive identities remain
stable across refreshed tokens. Selection options include `reblogs` (boolean or
`"same-blog"`, requiring the reblog root to belong to the current blog), `posts`
(`"all"`, a comma-separated string or list of legacy post types, or false/empty
selection to disable listings), `inline`
(boolean or `"reblog"` for the comment, `"original"` for the original tree with
comment fallback), and `original` (boolean, default true). Single-post URLs
ignore `posts`, matching upstream. Disabling `original` avoids token lookups;
legacy inline `_1280` rewriting remains enabled. Plain media links stay direct downloads.

Experimental native Pixiv extraction (`--extractor native`, `pixiv` category)
requires `extractor.pixiv.refresh-token` (accepted in the `pixiv` or `pixiv-novel`
category, never echoed); without it selected Pixiv jobs fail before any network access,
except for public Pixiv Sketch feeds and the Pixivision article request described below.
The token is exchanged via upstream's iOS App OAuth refresh grant and the
access token cached in memory (refreshed once on `invalid_grant`). Supported:
`www.pixiv.net/[en/]artworks/ID`, `member_illust.php?illust_id=ID` (App API
`illust/detail`) and `users/ID` (one artworks child),
`users/ID/artworks`, `users/ID/illustrations`, `users/ID/manga` and legacy
`member_illust.php?id=ID` (one shared `user/illusts` feed with `next_url` paging
capped at 200 pages). Public and private illustration bookmarks support
`users/ID/bookmarks/artworks[/TAG]` and `bookmark.php[?id=ID]` through
`user/bookmarks/illust` (at most 200 pages). `rest=hide` selects private
bookmarks; all other values select public bookmarks. Without `id`,
the legacy bookmark URL uses the authenticated user's ID from OAuth.
A `tag` query overrides the path tag, following upstream's URL decoding.
Pagination preserves the selected user, visibility, and tag.
Named illustration bookmark jobs load `user/detail` before the first listing;
ownerless jobs capture the initial OAuth owner without an extra profile request.
`user_bookmark.id`, `.name` and `.account` remain fixed across token refreshes,
separately from the original artist metadata. Archives include bookmark owner,
illustration, page and extension, excluding tag, visibility, cursor and display
fields. Existing ordinary artwork archive entries no longer suppress these
owner-scoped bookmark entries. Additional upstream owner fields remain unfinished.
`/users/ID/avatar` and `/background`
read `user/detail`, remove upstream's thumbnail/crop suffixes and emit original
profile images. JPEG backgrounds without query suffixes carry upstream's PNG/GIF
fallbacks. Dated profile filenames use UTC dates; archives use kind, owner and
asset date, including `none` for undated default images. Media are original `i.pximg.net` files
(`meta_single_page`/`meta_pages`) downloaded with
`Referer: https://app-api.pixiv.net/` (same-origin redirects only). Ugoira
animations fetch `ugoira/metadata` lazily per work and default to the frame ZIP
(upstream's `600x600` ZIP URL is upgraded to `1920x1080`). Set `ugoira: "original"`
to download individual original frames instead. Known original URLs are derived
directly; other forms probe JPG, PNG and GIF in order, falling back to GIF if all
probes fail. Probes carry the App Referer without OAuth credentials and follow
bounded same-origin redirects; cross-origin probe redirects are unsupported.
Ordered `frames` metadata retains each ZIP member filename and millisecond
`delay` in both modes. Filters and templates also expose `_ugoira_frame_data`,
`ugoira_original`/`_ugoira_original`, and the file-only zero-based
`ugoira_frame_index`/`_ugoira_frame_index`. Empty frame lists emit an empty post
in original mode and one file in ZIP mode. Original mode permits up to 100,000
frames with a 32 MiB aggregate URL limit. `ugoira: false` skips animation metadata;
limited works can still request AJAX artwork details first. Only the exact string
`"original"` selects individual frames,
while other values follow upstream truthiness for ZIP versus skip.
Bookmark date selection preserves later newer artworks in source order.
Native `postprocessors` supports `ugoira` conversion from ZIPs or individual
original frames. For example, add `"postprocessors": [{"name": "ugoira",
"extension": "webm", "mode": "concat"}]` to the Pixiv extractor configuration.
Modes `auto`, `concat`, `image2`, `mkvmerge` and `archive` support upstream timing,
repeat-last-frame, custom/uniform framerates, FFmpeg arguments and two-pass
encoding, MKVToolNix arguments, metadata and mtime options. Install FFmpeg for
video conversion and MKVToolNix for `mkvmerge`; `archive` needs neither.
`keep-files`, `skip`, filters, category lists, named global `postprocessor`
definitions and scoped `postprocessor-options` are supported. Scoped lists run
from the most specific extractor configuration to the root list; root common
options take precedence. Category lists accept arrays or comma-separated strings;
selectors for specific extractor subcategories remain unsupported. Converter
output is consumed with a size limit; forwarding diagnostics to the terminal
remains unfinished.
Converted files are published atomically, and interrupted ZIP downloads retain
resume checkpoints when resuming is enabled. Failed conversion reports an error, preserves source
files and does not add a success archive entry; this deliberately differs from
upstream paths that can archive failed conversions. Original-frame conversion
requires the complete frame set and a newly downloaded final frame; selecting
only part of an animation retains those frames. Native metadata, mtime and hash
processors are described below; other postprocessor types remain unfinished.
`post_id`/`id` is the illustration
ID, `media_id` `ID_pN`, `username` the account, caption the title plus HTML-stripped
caption, with `create_date` timestamp and page-0 App API width/height (AJAX recovery
retains dimensions on every page). Default filenames
are `{id}_p{N}.{extension}`. Artwork archives include extension in ordinary,
series, ranking, search, followed-artist, article and bookmark scopes, keeping
ZIPs distinct from their first image frame. Older entries without an extension
will not suppress downloads under the corrected keys; existing archives are not
rewritten. Sketch, profile-image and novel archive identities are unchanged.
Plain `i.pximg.net` links stay direct downloads and
usually fail without the Referer.

Native Pixiv illustrations and novels support `metadata: true` to load artist
user details before comments and bookmark-tag enrichment. Illustrations discover
files first; novels fetch details before emitting the post. User listings preload
the owner profile before the feed. Detail failures abort before the affected post
and stop later works in that job. Avatar/background jobs reuse their required
profile lookup and expose the complete detail when enabled. Without enrichment,
ordinary works still retain their complete basic `user` object.

Public metadata includes `user`, `profile`, `profile_publicity` and `workspace`
objects with their original nested values and key order. Native API response
extras are retained too; templates can access them through `{metadata[KEY]}`.
Compatibility normalization retains the four standard detail roots. Snapshots
are shared across post/file output and expose profile values to predicates,
sidecars and templates without changing archives. Successful lookups are cached
by artist ID, including profile discovery and bookmark-owner lookups; attached
illustration/novel clients share the cache for the same API root. The cache keeps
up to 256 entries and 32 MiB, with 32 MiB/one-million-node/depth-128 snapshot
bounds. Failed requests are not cached, and cancellation publishes no incomplete
response. Mismatched identities and collisions with native canonical/transport
fields are rejected where upstream may overwrite work fields. `metadata` uses
JSON truthiness, with explicit null disabling inherited settings; Pixiv defaults
to disabled, separately from Kemono/Coomer's enabled default. Live authenticated
user-detail extraction remains unverified.

Native Pixiv illustrations support `captions: true` to fill missing captions
through the web AJAX API after user details, comments and bookmark tags. Existing
captions and `_mypixiv`/`_ajax` works skip the lookup. The source's literal
`jump.php` anchor decoding and HTML5 entity handling are preserved. `caption_html`
retains complete App API or sanitized AJAX HTML for inspection, sidecars, filters
and templates; the existing `caption` remains normalized text. Novel jobs retain
their existing caption HTML and ignore the `captions` switch, matching upstream.

Pixiv web requests accept Netscape cookies or an explicitly selected Firefox
profile, including `PHPSESSID`. Cookies stay URL-scoped and rotate in memory across
retries and redirects without modifying their source. Web requests use browser
headers and configured network settings, separate from App API authorization.
Redirects stay within one origin, with a ten-hop limit. Both encoded and decoded
bodies are limited to 32 MiB; cancellation stops pending reads and decoding.
HTTP 4xx bodies other than 429 follow source JSON semantics. Failed requests,
API errors and invalid JSON leave the original caption unchanged; corrupt gzip
and transient requests retry within configured limits. Oversized payloads do
not retry. Truthy successful bodies with malformed caption fields abort before
that post. Source vectors and local HTTP tests cover these paths; authenticated
live AJAX extraction remains unverified.

Native Pixiv `sanity` recovery defaults to enabled and follows JSON truthiness.
Artist artwork, illustration and manga URLs share the upstream untyped App API
feed. A web profile lookup supplies omitted IDs, which are merged in descending
order around App API works and recovered lazily within selection limits. Path
and legacy query tags filter the merged feed before recovery, as upstream does.
Recognized sanity-limit images use AJAX artwork metadata and original page URLs;
missing originals can be discovered with ordered JPG, PNG and GIF HEAD probes.
Limited ugoira uses web animation metadata for ZIP or original-frame output.
`sanity: false` disables recovery and the profile lookup. Restricted works still
produce post boundaries, with `ajax`/`_ajax` and `mypixiv`/`_mypixiv` flags available
to filters and templates. Recovery bounds profile IDs and tags to 100,000 each,
artwork pages to 10,000, and transformed tags/generated page URLs to 32 MiB.
Source fixtures and local HTTP tests verify recovery; live authenticated recovery
remains unverified.

Native Pixiv illustrations also support `comments` and `metadata-bookmark`, both
disabled by default and using upstream JSON truthiness (explicit null disables
an inherited value). `comments` retains complete API comment objects in source
order, including nested user/stamp fields. Works with no reported comments
emit an empty list without another request; exhausted comment fetches discard
partial results and emit an empty list. `metadata-bookmark` fetches only works
marked as bookmarked and exposes registered names as `tags_bookmark`, preserving
order and duplicates. A bookmark-detail failure aborts that job before emitting
the affected post. Both fields are shared by post/file inspection, sidecars,
predicates and filename templates, and do not affect archive identity.
Enrichment runs after file discovery and stays lazy across works and selection
limits. Comment pagination uses a fixed API endpoint and illustration scope,
with cycle, page and aggregate-size bounds. Illustration jobs include Pixivision.
Novels support the same settings independently under `pixiv-novel`, including
queued profile novel views. Novel enrichment runs before the post and text/cover
requests; comment or bookmark-detail failures abort the novel job before emitting
that post. Unlike illustrations, `_ajax` does not suppress novel comments.
Successful enrichment is shared by the post, text and optional cover; a later
text-fetch failure retains the post and permits later novels. Both comment
endpoints use fixed work-ID scopes and the same pagination bounds. Compatibility
mode forwards each category's resolved switches. Compatibility novel default
filenames also use upstream's `{id}_p{N}.{extension}` for text and covers. Live
authenticated enrichment remains unverified.

Native Pixiv illustrations and novels honor `max-posts` in their respective
`pixiv` and `pixiv-novel` scopes; zero removes an inherited limit. The limit
counts source works before post filters and media extraction, including skipped
or failed works, and retains every page, text file or cover of the final work.
Ranking content filtering and search overlap suppression happen before counting.
Each queued profile view starts a fresh budget; following user queues themselves
are not limited. Reaching the budget avoids later detail, animation, text and
pagination requests. Compatibility mode forwards the selected root category's
setting upstream; separate overrides for queued child categories remain incomplete.

Native Pixiv following lists support `/users/ID/following` and illustration
bookmark URLs with `type=user`, including legacy `/bookmark.php` URLs. `rest=hide`
selects private following; ownerless legacy URLs resolve the authenticated user.
The list queues each followed user's profile, whose `include` settings select
artworks or other views normally. Child filters see `category == 'pixiv'`,
`subcategory == 'following'`, and the preview's `id`, `name` and `account`;
nested profile views have `subcategory == 'user'`. Filtering precedes child
ranges and duplicate-user checks, so rejecting a preview starts no child
requests. Pagination preserves owner and visibility, detects cycles, and stops
at 200 pages. Stopping a selected child leaves later followed users available;
dropping the input stops later users and pages. Authenticated live following
extraction remains unverified.

Illustration and novel bookmark jobs support a category-scoped `cursor` starting
bookmark ID, as a string or integer of at most 1,024 bytes. `null` clears an
inherited cursor and omits the query parameter; `""` and zero are sent literally.
Only the first request uses the configured cursor. Later API cursors can change
or disappear while owner, tag and visibility remain fixed. URL query cursors
are ignored. The separate `pixiv-novel` client retains its own cursor when
dispatched from a Pixiv profile; novel bookmark archives remain ordinary novel
archives and do not fetch bookmark-owner metadata. Compatibility mode forwards
the selected root category's cursor with its JSON type preserved. Automatic
resume-cursor diagnostics and separate compatibility child overrides remain
unfinished.

Native Pixiv illustrations and novels expose `tags` in inspection, sidecars,
post/file predicates and templates. Configure `tags` independently in `pixiv`
and `pixiv-novel`: `"japanese"` (default) keeps names in source order, including
duplicates; `"translated"` uses each translation or falls back to its name,
then removes duplicates in first-occurrence order; `"original"` retains complete
tag objects, including extra fields. Any other setting falls back to Japanese,
matching upstream. Strings preserve whitespace and Unicode. Examples include
`'cat' in tags` for names and `{tags[0][name]}` for original objects. Tag settings
perform no extra API requests and do not change archives. Compatibility data
retains upstream's already transformed tags. Missing, malformed or oversized
arrays are omitted; upstream may instead abort on malformed tag fields.
Original tag numbers must fit native expression limits (signed 128-bit integers
or finite floating-point values); unsupported arrays are omitted.

Native Pixiv's followed-artists feed supports `/bookmark_new_illust.php` in
the `pixiv` category, using `extractor.pixiv.refresh-token`. URL query options
are ignored; the App API request always uses `restrict=all`. The initial OAuth
user is captured before the first feed request and remains fixed in
`user_follow.id`, `user_follow.name` and `user_follow.account` across token
refreshes. Artwork artist metadata stays separate. Only those public owner
fields are retained; additional upstream user metadata remains unfinished.
Feed archives include the authenticated owner ID and artwork/page identity,
separately from ordinary artworks, searches and rankings. Display-name changes
do not change archive keys. Pagination pins the endpoint and restriction,
detects cycles and stops at 200 pages. Empty/failed animation entries retain
post positions, with native metadata failures reported as errors where upstream
warns. Live authenticated feed extraction remains unverified.

Native Pixiv rankings support `/ranking.php?mode=daily&date=YYYYMMDD` in the
`pixiv` category. The default mode is `daily`; omitted or malformed dates use
yesterday in UTC. Supported mode names are `daily`, `daily_r18`, `daily_ai`,
`daily_r18_ai`, `weekly`, `weekly_r18`, `monthly`, `male`, `male_r18`, `female`,
`female_r18`, `original`, `rookie` and `r18g` (case-insensitive).
`content` filters exact work types before assigning ranks; omit it or leave it
empty for all types. The literal `content=all` filters for a work type named
`all`, matching upstream. Inspection, filters and templates expose
`ranking.mode` (the App API mode), `ranking.date`, `ranking.rank` and
`ranking.type`, for example `{ranking[rank]:03}_{id}.{extension}`. Ranks span
pages and retain their positions after post/file selection. Ranking archives
include mode and date, remain separate from ordinary artworks, and ignore rank
and content filter. Pagination preserves mode/date, detects cycles and stops at
200 pages. Date filtering scans the whole ranking because artwork dates are not
ordered. Live authenticated ranking extraction remains unverified.
Animation metadata failures retain an empty post position and allow later works
to continue; native mode currently reports these as errors where upstream warns.

Native Pixiv illustration search supports `/[en/]tags/WORD[/VIEW]`, `/search`
and `/search.php` in the `pixiv` category. A path tag takes precedence; search
pages use a nonempty `q`, falling back to `word`. Terms are decoded once, with
literal plus signs preserved in path tags. `order` accepts `date_d` (default),
`date`, `popular_d`, `popular_male_d` or `popular_female_d`. `s_mode` accepts
`tag`/`s_tag`, `tag_full`/`s_tag_full` (default), or `tc`/`s_tc`. Optional
`scd`/`ecd` bounds are passed as API start/end dates. Tag view suffixes, including
`novels`, still select illustration search, matching upstream.
Date-sorted searches restart their date window at offset 5,000 and suppress
overlapping results as upstream does. Pagination preserves the selected term,
sort, target and current bounds, detects cycles and stops at 200 pages total.
Inspection, predicates, sidecars and templates expose `search.word`,
`search.sort`, `search.target`, `search.date_start` and `search.date_end`;
metadata retains the original bounds after pagination advances its window.
Search archives are scoped by term, separately from artworks and rankings;
sort, target and date bounds do not change their identity. Date selection scans
search results without assuming newest-first order. Empty/failed animation
posts retain their selection positions; metadata failures currently report
errors where upstream warns. Live authenticated search remains unverified.

Native Pixiv illustration series support `/user/OWNER/series/ID` (singular
`user`). The API selects the series ID; the URL's owner does not filter artists.
The first API response supplies normalized `series.id`, `series.title`,
`series.caption` (raw text), and `series.total`, even if that page has no works.
Later response details do not replace it. `num_series` counts down from the
reported total before user/media selection, including zero and negative numbers
if the reported count is inconsistent. Default filenames use
`{num_series:>03}_{id}_pN.EXT`; custom templates can use the same public fields.
Series files share ordinary artwork archive identity, so ordering and series
membership do not create a new archive entry. Pagination pins the series ID,
detects cycles and stops at 200 pages. Date selection scans series order without
assuming publication-date order. Empty/failed animation entries retain post
positions; native metadata failures remain errors where upstream warns.
Additional series-detail fields and live authenticated verification remain
unfinished.

Native Pixivision articles support `pixivision.net/[en/]a/ID` in the `pixiv`
category. They first load the canonical public `/en/a/ID` page anonymously;
an article with no artwork links succeeds without a refresh token. Linked
artworks then load lazily through authenticated App API detail requests.
Titles preserve whitespace and use full HTML5 entity decoding. Exact English
artwork anchors are scanned in source order, deduplicating only adjacent
identical links before removing query strings, matching upstream.
Inspection, sidecars, predicates and templates expose `pixivision.id` and
`pixivision.title`, with flat aliases `pixivision_id` and `pixivision_title`.
Archives include article ID and artwork/page identity; changing the title does
not change archive keys. Files use ordinary artwork filenames. Artwork detail
failures abort the article; failed animation metadata retains an empty post
position and reports a native error where upstream warns. Date selection scans
later artworks without assuming publication order. Live authenticated article
extraction remains unverified; upstream's default article directory layout
remains unfinished.

Native Pixiv Sketch supports `sketch.pixiv.net/@USERNAME` without OAuth. It
loads public wall pages lazily and emits each post before parsing its original
photos, including empty posts. Images retain the wall Referer and source
dimensions; source media URLs can use any HTTP(S) host. Default filenames are
`POST_ID PHOTO_ID.EXT`. Sketch archives use the actual post owner's ID and photo
ID, separately from ordinary Pixiv works; moving a photo between posts or
renaming its owner does not change archive identity. Inspection, sidecars,
filters and templates retain bounded public post fields under `sketch`, with
flat aliases such as `user`, `text` and `created_at`. On files, `id` is the
photo ID; `post_id` remains the containing post. A failed photo closes its post
and stops the wall. Date selection scans later entries without assuming order.
Pagination stays on the selected wall, detects cycles and stops at 200 pages.
Sketch ignores Pixiv-specific `max-posts`, `tags`, `cursor` and `ugoira` settings, matching
upstream's separate extractor. Global post/file filters and ranges still apply.
An included profile Sketch view resolves the Pixiv account only after its child
job is accepted. Anonymous first-photo extraction was verified against upstream
on the public `@pxv_sketch` wall. Live pagination and authenticated profile-to-wall
discovery remain unverified; configured cookies and the default Sketch directory
layout remain unfinished. Generic native
`filename` template semantics still use the logical output filename.

Native Pixiv novels use the separate `pixiv-novel` category and
`extractor.pixiv-novel.refresh-token`. Supported URLs are `/novel/show.php?id=ID`,
`/n/ID`, `/users/ID/novels`, `/novel/series/ID`, and
`/users/ID/bookmarks/novels[/TAG]` (including private bookmarks with `rest=hide`).
Bookmark tags come from the path; the `tag` query is ignored, matching upstream.
Listings page lazily with a 200-page limit and retain their owner, series,
visibility and tag scopes. Selected works fetch the App API webview text and
save exact UTF-8 as `ID_p0.txt`, preserving newlines and embedded-image markers.
`covers: true` adds the original JPG cover with a PNG fallback;
`full-series: true` expands a single novel into its series. `embeds: true` expands
uploaded images and linked illustrations after text and cover output. All three
settings default false and follow JSON truthiness, with explicit null disabling
an inherited setting. They resolve through root, extractor and category scopes;
only novel jobs consume them. An uploaded-image marker triggers one web AJAX lookup, which emits all returned images in response
order. Linked illustration markers ignore page selectors and queue each artwork
once per novel, after uploaded images. Child predicates and ranges run before
artwork requests; repeated selected artwork targets are fetched once per job.
Artwork children use the `pixiv` category's own credentials, network, enrichment
and ugoira settings. Profile `include` novel children retain novel-scoped settings.

Novel source fields and persistent uploaded-image metadata reach sidecars,
filters and templates, including source `num` (zero for text), `suffix`, title,
and custom fields through `metadata`. Native `index` remains one-based for files.
Public snapshots are bounded to 32 MiB, one million nodes and depth 128; marker,
image and child traversal have count and aggregate URL bounds. Output numbering
is limited to five digits. Archives include novel ID, output number and extension,
isolated from illustration IDs. Previous entries without an extension do not
suppress the corrected identity and may be downloaded again.

Text/cover failures close the current post and allow later novels to continue;
native mode reports these as errors where upstream warns. Embedded-image lookup
failures skip uploads and allow linked artwork; malformed individual images abort
the novel job after closing its post. Child failures leave later children and
novels available. Authenticated live novel extraction remains unverified; source
oracle and local HTTP fixtures cover ordering, mutations, selection, independent
clients, archives, cancellation and failure boundaries.

Experimental native DeviantArt extraction (`--extractor native`, `deviantart`
category) uses the OAuth2 API with a client-credentials token (upstream's public
client unless `extractor.deviantart.client-id`/`client-secret` are set; accepted
only in the `deviantart` category, never echoed), cached for the run and
refreshed once on HTTP 401. Supported: `www.deviantart.com/USER/art/SLUG-ID`
(UUID from the page's `DeviantArt://deviation/UUID` app link, then
`deviation/UUID`), `USER`, `USER/gallery[/all]` (`gallery/all`) and
`USER/gallery/FOLDER/NAME` (folder UUID matched by name via `gallery/folders`);
legacy `USER.deviantart.com` hosts are normalized. Listings use offset paging
capped at 200 pages. Media are `content.src`, or the `deviation/download`
original for downloadable deviations; HTTPS on `*.wixmp.com`/`*.deviantart.net`
only. Deviations without image content (literature, journals) are skipped.
`post_id`/`media_id`/`id` is the deviation UUID, `username` the author, caption
the title, with `published_time` and width/height. Default filenames are
`{post_id}.{extension}`; archive identity is the UUID.

Experimental native Flickr extraction (`--extractor native`, `flickr` category)
uses the REST API with upstream's default public key unless
`extractor.flickr.api-key` is set (accepted only for `tumblr`/`flickr`/`wallhaven`/`weasyl`/`pixeldrain`/`gelbooru`/`safebooru`, never
echoed). Supported: `www.flickr.com/photos/USER/ID` (`photos.getInfo` +
`photos.getSizes`: "Original" or the largest photo; videos pick the best
stream, "Video Original" first), `photos/USER/albums/ID` (`photosets.getPhotos`),
`photos/USER` (`urls.lookupUser` + `people.getPhotos`) and `flic.kr/p/CODE`
(base58 photo ID). Listings page 500 per page, capped at 200 pages, using
`url_o/k/h/l` extras; videos cost one `getSizes` call each. Media must be HTTPS
on `*.staticflickr.com` (videos also `*.flickr.com`). API `stat: fail` replies
map to errors (code 1 not found, 100 invalid key). `post_id`/`media_id`/`id` is
the photo ID, `username` the owner, caption the title, timestamp the upload
date, with width/height and photo/video type. Default filenames are
`{post_id}.{extension}`; archive identity is the photo ID. Plain
`staticflickr.com` links stay direct downloads.

Experimental native Wallhaven extraction (`--extractor native`, `wallhaven`
category) uses the public API (`/api/v1`). Supported: `wallhaven.cc/w/ID` and
`whvn.cc/ID` (`w/ID`), `wallhaven.cc/search?...` (same query forwarded to
`search`), `user/USER/uploads` (search `q=@USER`) and
`user/USER/favorites/ID` (`collections/USER/ID`); listings follow
`meta.last_page`, capped at 200 pages. An optional `extractor.wallhaven.api-key`
is sent as `X-API-Key` (never echoed); NSFW and private collections require it
(HTTP 401 maps to an api-key hint). Media are `data.path`, HTTPS on
`w.wallhaven.cc` only. `post_id`/`media_id`/`id` is the wallpaper ID,
`username` the uploader and caption the space-joined tags (single wallpapers
only; listing items carry neither), timestamp `created_at`, with
width/height. Default filenames are `wallhaven-{id}.{extension}`; archive
identity is the wallpaper ID. Plain `w.wallhaven.cc` links stay direct.

Experimental native ArtStation extraction (`--extractor native`, `artstation`
category) uses the site JSON: `www.artstation.com/artwork/HASH` and
`USER.artstation.com/projects/HASH` (`projects/HASH.json`), and portfolios
`www.artstation.com/USER[/albums/all]` or `USER.artstation.com`
(`users/USER/projects.json?page=N` until `total_count`, capped at 200 pages,
then each project's JSON). ArtStation sits behind Cloudflare: requests send a
browser User-Agent, Accept and Referer, but may still be blocked (HTTP 403
reports this). Only `image` assets are downloaded, using `image_url` as-is
(no `4k` rewrite), HTTPS on `cdn*.artstation.com` only; covers, video/player
embeds (YouTube etc.) and 3D/other assets are skipped. `post_id`/`id` is the
project hash, `media_id` the asset ID, `username` the artist, caption the
title, timestamp `published_at`, index 1-based per project, with width/height.
Default filenames are `{post_id}_{media_id}.{extension}`; archive identity is
the asset ID. Plain CDN links stay direct.

Experimental native Pinterest extraction (`--extractor native`, `pinterest`
category) uses the web resource API (`resource/NAME/get/?source_url=..&data=..`
with upstream's XHR headers, including a pinned `X-APP-VERSION` that Pinterest
may eventually reject). Supported on `pinterest.com` and an explicit list of
country domains/subdomains: `/pin/ID/` (`PinResource`), `/USER/BOARD/`
(`BoardResource` then `BoardFeedResource`), `/USER/pins/` (`UserPinsResource`)
and `pin.it/CODE` (resolved via the `url_shortener` redirect). Feeds follow the
bookmark until `-end-`, capped at 200 pages. Media are story-pin image/video
blocks, else the largest MP4 in `videos.video_list` (HLS-only videos are
skipped), else `images.orig`; HTTPS on `i.pinimg.com`/`v*.pinimg.com` only.
Promoted pins and pins without media are skipped. `post_id`/`id` is the pin
ID, `media_id` the pin ID (`ID_N` for multi-block story pins), `username` the
pinner, caption title/grid title/description, timestamp `created_at`, index
1-based, with width/height. Default filenames are `{post_id}_{index}.{extension}`;
archive identity is pin ID plus index. Plain `pinimg.com` links stay direct.

Experimental native VSCO extraction (`--extractor native`, `vsco` category)
reads the page-embedded `window.__PRELOADED_STATE__` JSON, which is
undocumented and fragile. `vsco.co/USER/media/ID` uses `medias.byId`;
`vsco.co/USER` and `/USER/gallery` take the site ID and anonymous `tkn` bearer
token from that state and page `api/3.0/medias/profile` by cursor (capped at
200 pages). Images use `responsiveUrl` with an `https://` prefix; videos are
kept only as progressive MP4 (HLS is skipped). HTTPS on `*.vsco.co` only.
`post_id`/`media_id` is the media ID, `username` the `permaSubdomain`, caption
the description, timestamp `uploadDate`, with width/height. Default filenames
are `{media_id}.{extension}`; archive identity is the media ID.

Experimental native Newgrounds extraction (`--extractor native`, `newgrounds`
category) scrapes art pages `www.newgrounds.com/art/view/USER/SLUG` (main
image from `<div class="image">` or `og:image`, extra images from `imageData`
/`art-images`, title from `og:title`, date from `itemprop="datePublished"`).
`USER.newgrounds.com/art` pages `art/page/N` with `X-Requested-With:
XMLHttpRequest` (capped at 200 pages) and fetches every listed post. Audio,
movies and games are unsupported; there is no login, so adult-rated posts are
skipped. HTTPS on `*.ngfiles.com` only. `post_id` is the image's numeric ID,
`media_id` `POSTID_INDEX`, `username` the URL user, caption the title. Default
filenames are `{post_id}_{index}.{extension}`; archive identity is `media_id`.

Experimental native Fur Affinity extraction (`--extractor native`,
`furaffinity` category) scrapes `www.furaffinity.net/view/ID/` and `/full/ID/`
(the `Download` link, title, `/user/` artist, `popup_date` and rating).
`/gallery/USER/` and `/scraps/USER/` page `/gallery/USER/N/` `<figure
id="sid-ID">` items until a page adds no new IDs (capped at 200 pages) and
fetch every submission. Session cookies `a`/`b` from `--cookies` or
`--cookies-from-browser` are sent to the site only; without them mature
submissions are skipped. HTTPS on `d.furaffinity.net`/`*.facdn.net` only.
`post_id`/`media_id` is the submission ID, `username` the artist, caption the
title. Default filenames are `{post_id}.{extension}`; archive identity is the
submission ID.

Experimental native Weasyl extraction (`--extractor native`, `weasyl`
category) uses the API: `weasyl.com/~USER/submissions/ID/...` and
`/submission/ID` read `api/submissions/ID/view`; `/~USER` and `/~USER/gallery`
page `api/users/USER/gallery?nextid=N` until `nextid` is null (capped at 200
pages). An optional `extractor.weasyl.api-key` is sent as `X-Weasyl-API-Key`
(never echoed). Submissions without `media.submission` are skipped; HTTPS on
`*.weasyl.com` only. `post_id`/`media_id` is `submitid`, `username`
`owner_login`, caption the title, timestamp `posted_at`. Default filenames are
`{post_id}.{extension}`; archive identity is the submission ID.

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
a Netscape file or explicit Firefox profile import; it does not log in or use a
guest session. Authenticated single-status extraction now defaults to
`TweetDetail`, matching upstream's `"tweet-endpoint": "auto"`. Scoped
`"tweet-endpoint": "detail"` selects it explicitly; `"restid"` retains the
`TweetResultByRestId` route. Conversation mode always uses detail, independently
of endpoint selection. API operation IDs/features remain experimental snapshots.

Normal status selection requires the exact requested post ID among structured
detail entries. Other conversation posts cannot replace a missing focal post.
Ordered photo sizes, highest-bitrate direct MP4 variants, mixed-media order,
metadata, and stable archive keys use the existing shared parser. Retweets are
skipped and quoted posts are not expanded by default. Same-response explicit
quote targets are reused; missing targets are fetched lazily through the fixed
post API. Invalid selected media fails instead of returning thumbnails, and API/session
failures remain private explicit errors.

Scoped `"conversations": false` (default) selects only the requested post and its
enabled quote expansion. `true` emits accessible conversation context and replies,
including preceding posts, even if the focal post never becomes accessible.
`"accessible"` emits the conversation only after finding that focal post; a
terminal response without it produces no media. Both modes buffer preceding
context until finding the focal post or reaching the end so reply filtering can
use its author. The pre-focal context buffer is limited to 16 MiB and fails
privately if exceeded. Recognized owned tombstones/unavailable items are skipped;
malformed accessible identities and unsupported envelopes still fail explicitly.

Detail traversal retains post order, per-input deduplication, first-page pin
opt-in, active-time budgets, cursor progress checks, and range/drop cancellation.
Normal selection stops once the requested post and its enabled quotes drain;
conversation modes continue through later detail pages. Prior output survives
late failures. Source-order date filtering applies to conversations. Scoped
endpoint/conversation settings also forward to compatibility mode when explicit.
All detail behavior has synthetic/parser/local HTTP coverage; authenticated live
verification and sanitized real captures remain required. See the
[conversation protocol notes](tests/fixtures/twitter/detail_README.md).

Scoped X `"expand": true` replaces each source post with a lazy TweetDetail
traversal for its conversation ID. Each conversation expands once per input,
including when duplicate output is enabled. Child posts do not recursively start
new thread traversals. Detail API failures fall back to the original post,
including after earlier child media has been emitted. Missing, null, or empty
conversation IDs are skipped; an explicit false ID keeps the original post.
Expansion shares the parent active-time budget, reply-author context, and
range/drop cancellation. Source-order date filtering applies while expanding.
The default is `"expand": false`.

Scoped X `"unique": true` (default) suppresses repeated post batches while keeping
all media siblings. `false` retains repeated batches and disables the CLI's
cross-input media deduplication for X. Existing-file and archive checks still
apply when downloading. Both options forward to compatibility mode when explicit.
Synthetic and local HTTP tests cover these settings; authenticated live thread
expansion remains unverified.

Native X photos use the ordered sizes `orig`, `4096x4096`, `large`, `medium`,
and `small` by default. Scoped `size` replaces that list, preserving order; it
accepts a comma-separated string or an array of 1–32 names, each 1–32 ASCII
letters/digits, `_`, or `-`. Names are trimmed; empty names and control characters
are rejected. For example:

```json
{"extractor": {"twitter": {"size": ["orig", "large", "medium"], "fallback": true}}}
```

The first size is the primary URL; later sizes become private fallback URLs.
Non-size query parameters are preserved. The same setting applies to video
posters, unified-card images, and article images. Summary-card images use it
when their supplied URL already has a `name` parameter; otherwise that URL is
preserved. Component and conversation-preview image URLs also remain unchanged
and do not receive size chains. Article HTML references only the first size and
does not perform browser-side fallback. Explicit `size` settings forward only for X in
compatibility mode. Library callers can use `Twitter::with_image_options` and
`parse_response_with_image_options`.

Adding the default private fallback chain changes request identities for native
X images, including default digest-prefixed paths and resume checkpoint names.
Existing paths/checkpoints are not migrated automatically. Social archive keys
and post/file numbering are unchanged. `fallback: false` disables attempts but
leaves the extracted fallback context in the request identity.

Scoped X `"videos": true` (default) selects direct MP4 video and animated-GIF
files. `false` skips them. `"previews": true` additionally selects their poster
images; combine it with `"videos": false` for posters only. Previews default to
false. A requested preview follows its video in source order, and files are
numbered consecutively after selection. Changing these settings can therefore
change filename and archive positions. Disabled video variants and previews are
not parsed; an invalid enabled video still fails instead of silently returning
its poster. Statuses, quotes, retweets, conversations, and collection streams use
the same selection. Explicit settings forward only for X in compatibility mode.

Scoped X `"text-tweets": false` (default) skips accepted posts with no selected
files. `true` (or `--text-tweets`; `--no-text-tweets` overrides configuration)
emits those posts as file-less post events carrying their IDs, caption, author,
timestamp, community, and attribution metadata, matching upstream's metadata-only
output. They can satisfy post filters (with `count == 0`) and occupy post-range
positions, but create no media, archive entries, or sidecars, like upstream's
default file-event `--write-metadata`; post-event metadata output is not
implemented. It also selects
`CommunityTweetsTimeline` instead of `CommunityMediaTimeline` for community
feeds. Explicit values forward to compatibility mode for X.

Scoped X `"unavailable": false` (default) skips individual media whose API
availability status is `Unavailable`, before parsing their media fields. Other
files continue in source order and are numbered consecutively. `true` keeps those
media eligible for normal video/preview selection and URL validation. It does not
make a protected post accessible or guarantee that its media server will serve
the file. The setting also applies inside unified cards, quotes, retweets, and
expanded threads; explicit values forward to compatibility mode for X. Changing
this selection can change file/archive positions.

`"videos": "ytdl"` emits a typed video-page job targeting the validated first-party
post, optionally followed by its preview. Downloads run it through yt-dlp (see
"Video pages and HLS/DASH" below). Direct MP4 downloads do not need yt-dlp. Library callers
can use `Twitter::with_media_options` and `parse_response_with_media_options`.

Scoped X `"cards": true` extracts summary-card images and unified-card media.
Cards default to false. Summary cards choose the first usable image by upstream
prefix/size priority; an existing image size parameter uses the configured first
size. Unified cards retain embedded media order, then component and
conversation-preview order.
Their videos and posters follow the same `videos`/`previews` settings. Card files
follow ordinary post media and share consecutive file/archive positions.
Repost mode uses the repost's card; original mode uses the original post's card.

`"cards-blacklist": ["summary", "youtube.com", "player:twitch.tv"]` skips exact
card names, vanity domains, or `name:domain` pairs. An empty list clears inherited
entries. Both options support normal configuration scopes and forward only for X
when explicit. `"cards": "ytdl"` handles recognized summary/unified cards directly,
then emits a typed first-party video-page job for an unhandled card, which is
downloaded with yt-dlp.

Enabled malformed cards fail the current post atomically with private diagnostics;
this is stricter than upstream's warning and partial-output behavior. Repeated
binding-array keys, unified-media keys, and component references are rejected.
Disabled/blacklisted payloads are skipped. Richer card metadata and real
authenticated card captures remain unfinished. Library callers can use `Twitter::with_card_options` and `parse_response_with_card_options`.

X articles default to `"articles": true`, selecting article metadata, cover,
and embedded images after ordinary post/card media. Metadata includes the article
ID, title, publication time, and update time. `false` disables article parsing.
A comma-separated string or list selects flags: `meta`/`metadata`, `cover`,
`media`, `html`, and `doc`/`document`. For example,
`"articles": ["meta", "doc", "cover", "media"]` saves a standalone HTML document
before the article images. `html` adds the rendered fragment to inspection
metadata; it does not add a file. Empty strings/lists disable all flags.

Article files share post order and consecutive file/archive positions. Repost
mode uses the outer post's article; original mode uses the original article.
Quotes and expanded threads retain their configured article flags. Malformed
optional articles stop article processing while preserving metadata/files already
selected and allowing later posts/pages to continue. The renderer supports text,
atomic blocks, lists, media/dividers, LaTeX annotations, mentions, URLs, and inline
styles; unsupported or malformed content can omit later article output. Text and
attributes are escaped, links require HTTP(S), and rendered output is bounded to
16 MiB. Documents reference remote images; they do not bundle local image files.

Compatibility `text:` messages and native article documents now use typed
inline-text jobs. Downloads save exact UTF-8 bytes atomically, apply size limits,
and use normal archive/sidecar checks without an HTTP request or resume checkpoint.
Inspection URLs percent-encode inline content; the job retains its exact original
bytes. Article HTML is omitted from metadata sidecars so referenced media URLs
are not copied there; article ID/title/dates remain. Native and compatibility
settings forward only for X when explicit. Live authenticated article verification,
broader content variants, and upstream-style warning output for recovered article
errors remain unfinished. Library callers can use
`Twitter::with_article_options` and `parse_response_with_article_options`.

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
same post again within that input by default. Later pages wait until current jobs drain;
dropping the stream or reaching a file range stops later requests. Active
extraction time includes profile lookup, transaction bootstrap, requests, and
retries, excluding consumer pauses. Empty pages with an advancing Bottom cursor
continue. A well-formed entries response without a Bottom cursor, or explicit
Bottom termination, completes traversal. Missing instructions, conflicting or
repeated cursors, unavailable posts, inconsistent identities, and unsupported
entries fail explicitly; emitted media survive a later failure. Known promotions,
recommendations, and valid text-only posts are skipped unless `text-tweets` is enabled.

Scoped Twitter settings include `"pinned": false` (default) or `true`, and
`"replies": true` (default), `false`, or `"self"`. Pins are opt-in and precede
regular first-page posts, whether supplied as a pin instruction or identified in
user metadata. With `replies: false`, reply posts are skipped. With `"self"`, a
profile feed retains replies authored by that profile. A status/conversation
request uses the selected focal author's context, including the original author
when original-retweet mode is selected. If conversation mode allows an inaccessible
focal and none is found, the unbound fallback retains replies to their own author. Quote posts are filtered
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
Bare user profiles queue this timeline as one native child job, matching
upstream's default dispatcher. An explicit `/USER/timeline` or another tab URL
is a root job; it is not filtered by child predicates.
Native `/USER/photo` and `/USER/header_photo` extract the original avatar and
banner from the validated user response; default avatars and absent banners
emit no files. These accept canonical HTTPS `pbs.twimg.com` profile paths,
with owner/version archive identities separate from tweets. Banner filenames
remain extensionless when the URL declares no format. `/USER/info` emits a
fileless post containing normalized username, description and account creation
date; full raw profile metadata dumping remains unfinished.

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

Native community media uses `/i/communities/COMMUNITY_ID`. The followed-community
feed uses `/i/communities` or `/USER/communities`; the latter is an alias for the
current session's feed, not another user's followed communities. Requests use
`CommunityMediaTimeline` and `CommunitiesMainPageTimeline`, respectively. They
share lazy pagination, media selection, quote/thread expansion, limits, and
source-order dates with other collection feeds. With `text-tweets`, explicit
community feeds request `CommunityTweetsTimeline` and require its envelope.
Community response identity aliases, when present, must match the requested ID;
an unrelated nested timeline cannot replace the required envelope.

Posts can include allowlisted `metadata.community`: canonical decimal `id`,
`name`, `description`, epoch-second `date`, `nsfw`, `role`, `member_count`, string
`rules`, `admin`, `creator`, and a validated HTTP(S) `banner` URL. Optional invalid
fields are omitted or null; missing, invalid, or conflicting community IDs omit
the community object. Community identity comes from each post's explicit
relationship, never from an assumed feed ID. Compatibility extraction accepts
the corresponding transformed public fields. Inspection can include the banner;
metadata sidecars strip it and retain descriptive community fields.

Community feeds use a separate stable archive namespace containing community ID,
post ID, and file number. The same file in two communities or an ordinary post
remains distinct; repeated instances in the same community collapse. Ordinary
status/profile archives keep their existing identity even when community metadata
is present. Feed items without a valid community ID still download but have no
stable archive entry. Compatibility rows select this scope through upstream
`subcategory: "community"` or `"communities"`. Custom filename and directory
templates can use `{community_id}` and `{community_name}`; missing values fail
like other required template fields. Default output directory rules are unchanged.
Authenticated live community traversal and captures remain unverified.

Scoped `"limit"` sets the page size for profile and collection requests (integer
1–100, default 50), independently of `search-limit`, and forwards to compatibility
mode only when explicit. Bookmarks tolerate 128 consecutive empty continuations;
likes, lists, and home feeds stop at their first empty response unless the API
explicitly says to continue. All still stop on an explicit terminal response,
reject cursor loops, and defer later requests until current jobs drain. Collection
date filtering preserves source order because bookmark, like, list, and ranked
feed order need not match original post dates. No collection request credentials
are copied to media requests.

List membership/follow relations, subcategory-specific
profile settings, cursor-resume configuration,
and other collection routes remain
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
`Twitter::extract` API collects the selected status or enabled conversation;
`Twitter::stream` also handles profiles and collections. See the [media timeline fixture notes](tests/fixtures/twitter/media_README.md).

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

Native imports support Instagram, Facebook, X, Pixiv web requests, Kemono/Coomer
and FurAffinity. Log in to the site in that Firefox profile first (use `x.com`
for X). Supply the
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

`--range '2-5,8,10:20:3'` selects media positions separately for each extractor
job. Native Kemono/Coomer collections and Discord servers reset counters for
each child creator, post or channel. Bare X, Bluesky and Pixiv profiles queue
their default feed as one child; explicit feed URLs begin with one root job.
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
before Rust receives media. Rust forwards child ranges and child filters to
the compatibility process, which applies them to its queued jobs.

## Post selection

`--post-range '2-5'` selects whole posts, and repeated `--post-filter EXPR`
clauses require all expressions to match a post. Both work with native and
compatibility extraction, downloads, and inspection. For example:

```sh
gallery-dl-rs --post-filter "count >= 2" --post-range '1-3' URL
```

Scoped `post-range` accepts the same string/list syntax and limits as
`file-range`; scoped `post-filter` accepts the same string/list expressions and
limits as `file-filter`. Empty strings/lists clear inherited settings. Explicit
CLI values replace configured values. Counters start at one for each root job
and reset for native child jobs with explicit boundaries.

With post selection enabled, dates apply to post timestamps, followed by post
filters, post ranges, file filters, file ranges, global deduplication, and archive
checks. Rejected posts do not occupy post-range positions. All files in a selected
post remain eligible for file selection; their original indices and archive
identities stay unchanged. Rejected posts create no media, sidecars, or archive
entries. Missing timestamps pass date selection. Without post selection, dates
retain their existing per-file behavior.

Post expressions use the file-filter expression language and normalized public
post fields, including available IDs, caption, username, timestamp, community,
article, and declared `count`. File-specific fields such as `url`, `extension`,
dimensions, `metadata.file`, `media_id`, and `index` are normally absent. Fields
already supplied on the source Directory event remain available, including
Pixiv's initial `num: 0` and ArtStation's asset position. Missing keys
and evaluation errors reject the current post; invalid syntax fails before
extraction. A declared count is descriptive and never truncates a post.

Explicit extractor events define posts rather than changes in IDs. An Instagram
carousel is one post, as is a whole story/highlight reel. A returned empty reel
occupies a position; an absent/expired reel does not. Each accepted X tweet batch
is a post; quotes and thread tweets have separate boundaries. X text-only tweets
occupy positions only with `text-tweets`; otherwise they are skipped. Each Facebook album photo is a
post, while a single video's video/audio files share one post. Direct URLs each
form one post. Empty compatibility directory events also occupy positions.

Empty ranges start no extraction. Finite native post ranges stop at the selected
post's end, without requesting later posts. Compatibility output has no explicit
end marker: Rust must read the next directory event or validated EOF to finish a
post, which can require upstream work for the next post. Compatibility counters
cover flattened child extractors for the whole input because resolved JSON
output does not identify its child boundaries.

Library callers can consume `ExtractionEvent::Post`, `Media`, and `EndPost`
through native `events` methods or `Backend::events_with_options`. Native child
jobs additionally emit `BeginChild(ChildScope)` and matching `EndChild` events.
`JobSelection` applies the combined predicates and counters; legacy
`PostSelection::apply` also honors native child boundaries. Existing media-only
stream methods remain available; `event::media_only` ignores framing events.

## Child selection

`--child-range '1-3'` selects child jobs within each parent; repeat
`--child-filter EXPR` to require all expressions. Native configuration accepts
`child-range` strings/lists and `child-filter` string/list expressions with the
same scopes and limits as post selection. Explicit CLI values override config;
empty strings/lists clear inherited selections. A mathematical empty child range
stops the parent at the first child that passes its filter; preceding root posts
remain available. Empty file/post ranges
still start no extraction.

Child filters run before child counters and ranges. Rejected children do not
occupy positions and start no child requests. After the last selected child,
only its parent job stops; later siblings of that parent remain available.
File/post limits similarly cancel the current native child before later pages,
then continue sibling children. Nested counters reset independently, including
favorites that select Discord servers whose channels are further child jobs.
Error events add no extra positions; a selected child still occupies its
position if extraction fails. Later siblings remain available unless a range
has finished their parent.
The cumulative input extraction budget is shared across every child.

Collection child expressions expose bounded public queue fields such as `id`,
`name`, `account`, `service`, `user`, `indexed`, `updated`, `favorited`, `faved_seq`, plus
`category` and the parent `subcategory`. Values retain their raw scalar types;
creator IDs in observed catalogs are strings. Discord channel children expose
upstream's `server` and `channel` objects with public scalar fields, for example:

```sh
gallery-dl-rs --extractor native --child-filter "service == 'patreon'" --child-range '1-3' 'https://kemono.cr/artists'
gallery-dl-rs --extractor native --child-filter "channel['type'] == 0" --child-range '1-2' 'https://kemono.cr/discord/server/SERVER_ID'
```

Fields also appear under `child` and `metadata` map aliases. Arbitrary transport
URLs, headers, cookies, and private fields are omitted; semantic names and topics
retain their text. Strings are limited to 64 KiB, numeric representations to
128 bytes, and nested objects to the known server/channel shape. Missing keys
and evaluation errors reject the child. Invalid selected identities fail before
any request; invalid unselected identities can be skipped. Direct Discord channel
URLs are root jobs, so child predicates do not filter their own messages.

Bare X, Bluesky and Pixiv profiles also expose one child job for their default
timeline/feed/artworks view, or one child per configured `include` view.
These dispatch queues contain only the private
extractor marker upstream, so public child fields are empty apart from
`category` and `subcategory` (`user`). Rejecting that child avoids owner
resolution, session requests and Pixiv token validation/refresh. Explicit feed
URLs remain root jobs. One child wraps the whole feed, rather than each post,
illustration kind or fallback search stage. A reusable `event::child_job` adapter
provides lazy construction, matching ends and cooperative cancellation of
visible nested scopes. Media-only callers retain the same default files and archive keys.

Scoped `include` selects profile children in the requested order. For example:

```json
{"extractor": {
  "twitter": {"include": ["media", "likes"]},
  "bluesky": {"include": "avatar,posts"},
  "pixiv": {"include": ["avatar", "artworks"]}
}}
```

| Category | Default | Catalog order for `"all"` |
| --- | --- | --- |
| twitter | timeline | info, avatar, background, timeline, tweets, media, with-replies, highlights, likes |
| bluesky | media (posts with quotes/reposts enabled) | info, avatar, background, posts, replies, media, video, likes |
| pixiv | artworks | avatar, background, artworks, favorite, bookmark, user, sketch |

Only the exact string `"all"` expands the catalog; `["all"]` selects no known
view. Comma-separated strings remove ASCII spaces; list entries remain literal.
Empty strings/lists, `false` and `null` disable dispatch rather than restore the
default. At most 128 entries of 128 bytes are accepted. Unknown names are omitted
(native mode does not yet emit upstream's warning). Repeated views retain their
child positions; downstream deduplication/archive selection still applies.
X accepts `replies` as an alias for `with-replies`. Pixiv's illustration bookmarks
are `favorite`; `bookmark` and `user` select novels, also available as
`novel-bookmark` and `novel-user`. Native Pixiv novel views are supported;
Sketch views resolve the artist's account through the App API, then load its public wall.
`"all"` includes those entries; child ranges/filters can reject them without
requests. Include settings forward to compatibility mode as typed options.
Explicit view URLs ignore `include`. Post/file ranges reset for each selected
view, while the CLI extraction budget covers the whole input.

X/Pixiv profile image metadata exposes `profile_asset` (`kind`, string `owner_id`
and `version`) in inspection, filters and templates. Native and compatibility
profile archive keys agree; ordinary post identities stay unchanged.
Additional extractor queue boundaries, subcategory-specific child settings,
full queue metadata and authenticated verification remain unfinished.

## Metadata file filters

`--filter EXPR` selects files using a native Python-style expression. It applies
with both extractors and in `-g` / `-j` inspection. Repeat the flag to require all
clauses. For example:

```sh
gallery-dl-rs --filter "width >= 1000 and extension in ('jpg', 'png')" URL
gallery-dl-rs --filter "metadata.get('file', {}).get('width', 0) >= 1000" URL
```

Scoped `file-filter` (or the upstream `image-filter` alias) accepts a string or
list of strings. Clauses in a list are ANDed; an empty string/list clears an
inherited filter. Within one scope a nonempty `file-filter` wins over its alias.
Normal root/category/default precedence applies; explicit CLI clauses replace
configured clauses. Rust applies the filter itself; it does not forward another
filter to the compatibility executable.

Dates and any post selection run first, then metadata file filters, file ranges, global
deduplication, and archive checks. Rejected files do not consume `--range`
positions. Original per-post `index`/`num` metadata and archive identities stay
unchanged. Reaching the final selected range position drops upstream immediately;
rejection loops yield for cancellation. Rejected files publish no media, sidecar,
or archive entry. Extraction errors still report normally. As with upstream's
default expression mode, missing keys, invalid operations, and evaluation-limit
failures reject only the current file. Invalid syntax or unsupported calls fail
before extraction without echoing the expression.

Available names include `category`, `filename`/`name` (stem), `extension`,
`url`/`_url`, post/media IDs, `username`, `caption`/`content`, `timestamp`/`date`
(epoch seconds), `index` and `num`, file properties, `community`, `article`, `profile_asset`, and the
nested public `metadata` map. Available dimensions have `image_width` /
`image_height` aliases; `type` aliases `media_type`. X also supplies `tweet_id`,
`quote_id`, `retweet_id`, and available `source_user` attribution; Facebook
supplies `id`. Instagram `description`
means its post caption; X `description` means file alt text. Canonical decimal
IDs are exact integers in this expression context. Missing optional properties
are absent, so map `.get()` calls can provide defaults. Only normalized public
fields are available; arbitrary backend keys, headers, cookies, and private
handoffs are excluded. The extension groups `exts_image`, `exts_video`, and
`exts_archive` support membership tests.

The supported expression language includes finite numbers, strings and escapes,
booleans/`None`, lists, tuples, string-keyed dictionaries, indexing (including
negative sequence indexes), arithmetic, chained comparisons, membership,
`and`/`or`/`not`, and conditional expressions. Builtins are `len`, `bool`, `str`,
`int`, `float`, `abs`, `round`, `min`, `max`, `any`, `all`, and gallery-dl's
`contains`. Supported methods are dictionary `get` and string `lower`, `upper`,
`startswith`, `endswith`, `strip`, `lstrip`, `rstrip`, `replace`, `count`, `find`,
`index`, `split`, and `join`. Function/method arguments are positional.

This is a native expression subset, not a Python runtime. Comprehensions,
lambdas, slicing, regex/datetime/module helpers, arbitrary attributes/calls,
dictionary views, full Python integer precision, and the full upstream metadata
schema remain unfinished. Integer arithmetic is checked at 128 bits. Each clause
is limited to 16 KiB source, 4096 tokens/nodes, and depth 64; a filter has at most
32 clauses and 64 KiB total source. Evaluation bounds steps, scanned work,
sequence lengths, and generated values. Oversized metadata values can therefore
reject a file even when an equivalent unrestricted Python expression would pass.
The checked-in CPython reference cases and synthetic streaming/CLI tests cover
the implemented behavior; they do not establish complete Python compatibility.

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

Bounds follow the source's conversion order: strings try ISO dates first, then
integer Unix seconds. Calendar and ISO week dates accept basic or extended
spelling; times accept hours, minutes, seconds and fractions. `24:00` rolls to the
next day. Any single character can separate the date and time. Naive values use
UTC; timezone offsets can include hours, minutes, seconds and fractional seconds.
UTC normalization precedes discarding microseconds, including before 1970.

For example, `20240102` means January 2, 2024, while `" 20240102 "` falls back to
Unix seconds. Calendar ISO strings do not trim surrounding whitespace. Integer
fallback accepts Unicode decimal digits and single underscores between digits.
Invalid values disable their bound; this also lets an explicit CLI value clear an
inherited bound. If `date-after` is later than `date-before`, only the after bound
applies. Equal bounds exclude every dated post. CLI `0` is a real epoch bound.

Native configuration supports these bounds in the normal scopes and native booru
artist/tag class scopes, followed by CLI overrides. Invalid or falsy configuration
bounds disable that comparison, including numeric zero; use `"0"` for the epoch.
Truthy booleans and fractional numbers convert to integer Unix seconds. Explicit
null or invalid bounds clear inherited settings.

Post filters run before date checks, followed by post ranges. Dates are checked
once per Directory event before post hooks; accepted posts keep all their files.
Booru Popular's raw string date and flattened artist dates retain their source
types: falsy values pass, datetime values compare, and truthy non-datetimes raise
a private error. A failed or stopped artist child leaves later artist children
available. Native tag children use their own class predicates. Excluded media
receives no files, sidecars, or archive entry. Compatibility formatted dates are
normalized when recognized; invalid formats remain unknown. The standalone
`DateFilter` and `date::parse_date` library APIs retain their existing strict
timestamp-based contracts. `date::source_date_bound` exposes source conversion.

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
When no explicit native configuration is given, discovery checks
`$GALLERY_DL_RS_CONFIG` first. On Unix it then checks
`$XDG_CONFIG_HOME/gallery-dl-rs/config.json` (or
`~/.config/gallery-dl-rs/config.json`) and `~/.gallery-dl-rs.conf`; on Windows,
`%APPDATA%/gallery-dl-rs/config.json` and `%USERPROFILE%/gallery-dl-rs.conf`.
The first existing file is loaded. `--native-config-ignore` disables discovery.
Upstream gallery-dl config files are not discovered because the native schema
is a strict subset. Compatibility config is loaded only when explicitly selected.

`-o KEY=VALUE` (`--option`) sets one value on top of the loaded native
configuration, like upstream: `KEY` is a dotted path (`extractor.4chan.retries`)
and `VALUE` is parsed as JSON, falling back to plain text. Options apply in
order after the file, and are validated like file settings. Options under
`extractor.` are also forwarded to the compatibility backend as `--option`.

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
`file-filter`, `image-filter`, `post-range`, `post-filter`, `child-range`, `child-filter`, `include`,
`retweets`, `quoted`, `pinned`, `replies`, `tweet-endpoint`, `conversations`,
`expand`, `unique`, `videos`, `previews`, `mp4`, `reblogs`, `reposts`, `ugoira`, `tags`, `posts`, `inline`, `original`, `files`, `duplicates`, `text-posts`,
`unavailable`, `text-tweets`, `articles`, `cards`,
`cards-blacklist`, `size`, `fallback`, `max-posts`, `cursor`, `revisions`, `order-revisions`, `strategy`, `search-pagination`,
`search-results`, `limit`, `search-limit`, `search-stop`, `metadata`, `comments`,
`metadata-bookmark`, `captions`, `sanity`, `embeds`, `covers`, `full-series`, `endpoint`,
`order-posts`, `favorites` (Kemono/Coomer options), category-scoped Bluesky
`api-server` and likes `endpoint`, `retries` (integer 0–10), and `timeout`
(positive seconds, including fractions).
They can appear at the root, under `extractor`, or under a supported category
such as `extractor.instagram`, `extractor.imgur`, or `extractor.mastodon`.
Site-specific media options affect their respective extractors. Booru
`username`/`password` and Gelbooru/Safebooru `user-id` are category-scoped credentials;
API keys also forward to the selected compatibility extractor. API credentials
are accepted only in the documented category scopes. Mastodon instance scopes
inherit `extractor.mastodon` before applying their own overrides. Precedence is
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
unsupported types, executable hooks, and unsupported conditional values fail
explicitly. Native processor events are described below. Processor directory
options support home/environment expansion without shell execution; the other
native configuration paths remain literal. Filenames use the native template subset described below; archive
files use the Rust archive format.

Like upstream, `filename` may be an object mapping filter expressions to
templates and `directory` an object mapping them to component lists, e.g.
`{"filename": {"media_type == 'video'": "{post_id}.{extension}", "": "{filename}"}}`.
Conditions are tried in file order with the `--filter` evaluator; the first match
wins, then the `""` entry, then the default name or category folder. Condition
errors (such as missing keys) fail that file. A plain value or CLI flag in a
narrower scope replaces an inherited object entirely.

Without `--native-config`, the first existing file among `$GALLERY_DL_RS_CONFIG`,
`$XDG_CONFIG_HOME/gallery-dl-rs/config.json` (or `~/.config/...`), and
`~/.gallery-dl-rs.conf` is loaded (`%APPDATA%\gallery-dl-rs\config.json` and
`%USERPROFILE%\gallery-dl-rs.conf` on Windows). `--native-config-ignore` skips
discovery. Upstream gallery-dl configuration files are never read natively. Full gallery-dl
configuration compatibility and additional postprocessor types remain
unimplemented.

## Native postprocessors

Native configuration accepts `ugoira`, `metadata`, `mtime` and `hash` processors.
Lists accumulate from category settings through extractor defaults to root
settings. Root `postprocessor-options` takes precedence over narrower common
options; named definitions live in the root `postprocessor` object. The `name`
shorthands `metadata/tags` and `metadata/json@skip` select a mode and event.

For example, this computes a digest before publishing each file and writes a
sidecar containing that digest:

```json
{
  "extractor": {
    "filename": "{sha256}.{extension}",
    "postprocessors": [
      {"name": "hash", "hashes": "sha256", "filename": true},
      {"name": "metadata", "mode": "json", "include": ["id", "sha256", "date"]}
    ]
  }
}
```

`hash` defaults to MD5 and SHA1. It also supports SHA224/256/384/512,
SHA512/224 and /256, SHA3 and BLAKE2. `hashes` accepts a comma-separated string
(`"sha256:digest"` stores the result under `digest`), an object mapping keyword
names to algorithms, or pairs of names and algorithms. `chunk-size` controls
bounded reads. `filename: true` rebuilds the configured filename after hashing;
standard digest names are available in native filename templates. Hashes can be
used by subsequent processor filters and metadata templates.

`metadata` supports JSON (default), JSONL append records, tags, custom content,
printing, and sequential `modify`/`delete` keyword operations. JSON defaults to
four-space indentation and supports `include`, `exclude`, `private`, `ascii`,
`sort`, `indent` and `separators`. Tags use one tag per line. `content-format`
(or `format`) accepts a template or a list of lines for custom content. Sidecars
support `base-directory`, `directory`, `filename`, `extension`,
`extension-format`, `metadata-path`, `skip`, `empty`, `mtime`, `newline`, and
text overwrite/append/exclusive open modes. UTF-8 and ASCII encodings are
supported. Ordinary sidecars are atomic; append records are serialized across
concurrent downloads. Metadata processors export the native keyword context,
including the media URL; `--write-metadata` retains its separate versioned schema.
For `modify`, a single replacement field retains its value type after supported
conversions and operators: `{value!i}` produces an integer, `{num:A+10/}`
performs numeric arithmetic, and `{items:S/}` produces a sorted list. An
explicit format such as `{num:03}`
produces text; mixed literal/field templates stringify their field results.
Invalid field assignments leave the previous keyword value in place. Numeric
results can feed later producer increments and processor filters.

Metadata processors support SQLite `archive`, `archive-format`, `archive-prefix`,
`archive-table`, `archive-pragma`, and `archive-reuse`. These databases use Python
gallery-dl's `archive(entry)` schema, separate from the native download archive.
For example, `{"name":"metadata","archive":"metadata.sqlite3",
"archive-format":"{id}_{num}"}` records successfully completed sidecars.
Default keys use the source extractor's category and archive format where the
native producer preserves the required fields and job boundaries. Unsupported
defaults produce a warning and disable that processor's archive; an explicit
`archive-format` selects keys from the available native keywords.

Sessions initialize before `init` hooks and close after each job's final hooks.
Archive hits bypass destination rendering and `metadata-path` updates. Existing
sidecars skipped with `skip` add no row; completed empty output does. Rows commit
immediately and survive later job failures. Modify/delete/stdout modes initialize
and close archives but bypass checks and additions. String archive paths expand
home/environment variables and resolve relative to the process working directory;
path lists and table templates use the initial job keywords. Reused connections
retain their first pragma settings. Dispatch-hook exceptions stop the affected
job while allowing its parent to continue. Exceptions escaping child
finalization abort the parent's queue callback.

`mtime` uses `key: "date"` by default, or a `value` template. File mode updates
the completed file's modification time. `target: "directory"` (also `dir`)
updates its containing directory after the post completes. Missing or zero values
retain the normal timestamp. File mtime hooks record a timestamp for download
finalization; `after` and `skip` hooks update metadata without changing the saved
media file's time. Metadata sidecars with `mtime: true` use preceding
mtime processor values or the HTTP Last-Modified timestamp.

Processors default to the `file` event, except directory mtime, which defaults
to `post-after`. Explicit events may be a comma-separated string or list:
`init`, `prepare`, `prepare-after`, `file`, `after`, `skip`, `error`, `post`,
`post-after`, `child`, `child-after`, `finalize-success`, `finalize-error`, and
`finalize`. Post boundaries wait for concurrent files before running their closing
hooks. Selected empty posts retain their hooks; rejected posts and children do
not run them. Abort and terminate drain finalization hooks. URL/JSON inspection
runs no postprocessors. Ugoira retains its fixed source hooks.

The `init` configuration option runs initialization before extraction when truthy;
`false`, `null`, and the exact string `"lazy"` retain initialization on the first
selected post. Eager jobs run initialization and finalization even when they
produce no results. Job metadata destinations use the current post directory and
retain the previous filename across empty posts; before the first file, the
default sidecar basename is `metadata`. Hash filename rebuilding updates the path
seen by subsequent job hooks without moving an already completed file.

Native producers declare whether file keywords share the post dictionary, copy
post keywords, or arrive independently. Ordinary Pixiv artwork, boorus, Flickr,
Fur Affinity, ArtStation, Bluesky, Kemono/Coomer, Mastodon and Pinterest declare
shared dictionaries. Pixiv novels also share their root dictionary; cover, upload,
and linked-artwork counter increments build on preceding hook mutations. Upload
fields are assigned before incrementing, and cover fallback cleanup runs when
the producer resumes. Linked-artwork child hooks retain changes for the next
queue, while eager child initialization uses the artwork configuration. Uploaded
API counters retain native bounded nonnegative-integer validation. Shared files
run in source order when generic processors are enabled, preserving post-hook deletions and earlier file-hook mutations;
other file contexts can still run concurrently. Twitter/Tumblr copy post fields
into prepared files; Instagram lets prepared file fields take precedence.
ArtStation emits a post boundary per supported asset, retaining source asset
positions and the project's declared asset count.

Booru search keywords follow their producer ownership: Danbooru/e621 retain
post-hook changes to `search_tags`, while Gelbooru/Safebooru reassign it before
each file, matching the source's extractor keyword updates. Danbooru/e621 also
observe the last post's live `id` after file work drains to select the next
cursor. Post/file-hook changes and deletions affect that cursor; `post-after`
changes occur after its keyword snapshot.

`num` preserves the producer's counter separately from the native file `index`.
Ordinary Pixiv artwork and original ugoira frames start `num` at zero while
`index` remains one-based. Compatibility input retains the original counter
value and type. Both filters and filename/directory templates use the source
counter, including zero; archive identities retain their existing native index.

The native formatter remains a bounded, non-executable subset of upstream's
formatter. PostgreSQL processor archives, further extractor default-key coverage,
other text encodings/open modes,
extractor-subcategory selectors, keyword-driven changes to native archive
identities and pre-download selection, additional producer ownership policies,
nested object aliases, additional counter policies, mutation-driven request URLs
and text fetches, remaining temporary-path behavior, and additional processor
types remain unfinished. These processors
operate on metadata supported by the native extractor, which may omit fields
available from upstream.

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
  Native extraction uses the same cumulative per-input budget across initialization,
  pages, token/quote lookups, retries and discarded events. Consumer pauses are
  excluded. Exhaustion drops active request futures and owned compatibility
  processes; Firefox cookie import scans are interrupted as well.
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
- Like upstream, newly downloaded files take their modification time from HTTP
  `Last-Modified`; `--no-mtime` disables this. `--mtime [NAME]` (default `date`)
  instead uses a numeric metadata timestamp, falling back to `Last-Modified` when
  absent. Times are applied before atomic publication; existing files are untouched.
- `--exec CMD` runs a shell command (`sh -c`, or `cmd /C` on Windows) after
  each newly downloaded file, replacing `{}` with the shell-quoted path;
  `--exec-after CMD` runs once at the end. Both are repeatable; failures count
  as failed operations. Upstream's metadata fields in commands are not supported.
- `-r RATE`/`--limit-rate RATE` (e.g. `500k`, `2.5M`) caps each download's body
  rate. Throttling pauses extend that attempt's `--timeout` deadline, so slow
  limits do not cause timeouts.
- `--sleep SECONDS` or `--sleep MIN-MAX` waits (uniformly within the range)
  before each network download, after archive and existing-file checks, outside
  the per-attempt timeout.
- `-T N`/`--terminate N` stops all inputs successfully after N consecutive files
  were skipped as existing or archived (counted in completion order). `-A N`/`--abort N`
  stops only the current input: its extraction ends and queued, unstarted
  downloads are dropped, while later inputs continue.
- `--write-metadata` saves `<media filename>.json` beside completed media, including
  files skipped because they already exist. Sidecars have `schema_version: 1`,
  `category`, the saved `filename`, and optional `metadata` (post/media IDs,
  shortcode, username, caption, timestamp, carousel/album index, and optional
  per-file properties). Post metadata is
  provided by native Instagram/Facebook/Twitter and allowlisted compatibility Instagram,
  Twitter/X, and Facebook fields when available. Compatibility timestamps are
  normalized from numeric epochs or recognized ISO calendar date-times; unsupported
  backend date formats are omitted. Instagram
  uses site post/media IDs, Twitter uses tweet ID and file number, and Facebook
  uses `id.extension` as media ID to distinguish audio and video. Non-Instagram
  shortcodes are empty; unavailable fields stay empty/null. URLs, request headers,
  cookies, and private adapter options are excluded.
  When supplied by the extractor, `metadata.file` contains positive `width` /
  `height`, original dimensions, duration in seconds, bitrate, media type,
  description/alt text, recognized sensitive flags, and media source ID/username.
  Properties belong to the selected file: carousel siblings and separate audio
  tracks do not borrow another file's dimensions. Invalid optional values are
  omitted. These are response-declared properties; Rust does not probe downloaded
  bytes, and a smaller fallback image can differ from the original declared size.
  X/card/article dimensions are retained only when explicitly supplied. Instagram
  uses the selected variant's dimensions and per-item original dimensions;
  Facebook DASH uses the selected representation's dimensions and bandwidth.
  Compatibility properties use an allowlist, including `image_width` /
  `image_height` aliases. Adding descriptive file properties does not change
  request identities, default paths, file numbering, or stable archive keys.
  These properties are also available to native file-filter expressions.
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
- Booru inputs use the source class paths described above. Other filenames use a SHA-256 digest of the media URL and job/request context plus a
  sanitized filename, under `destination/category/`. Exact duplicate jobs within
  a run are removed, and existing paths are skipped for direct downloads. Bare
  direct links keep their original URL-only digest; media with headers now use a
  different digest and may be downloaded again after upgrading.
- CLI deduplication includes validated social post/file identity as well as request
  context. Distinct Instagram, X, and Facebook files sharing a URL are retained;
  repeated copies of the same file and request are suppressed across inputs.
  Rows without usable social IDs retain request-based deduplication. This also
  applies without a download archive; X `unique: false` bypasses deduplication.
- `-f` / `--filename` replaces the default basename with a native
  template, retaining the selected directory. Available fields are `{filename}`
  (including extension outside boorus), `{name}` (without the final extension), `{extension}`
  (without the dot), `{category}`, `{post_id}`, `{media_id}`, `{shortcode}`,
  `{username}`, `{index}`, `{num}`, `{timestamp}`, `{date}` (a UTC datetime),
  `{caption}`/`{content}`, `{description}`, `{community_id}`, `{community_name}`,
  and the nested `{community}`, `{article}`, `{source_user}`, and `{metadata}`
  maps. File properties also support `{width}`, `{height}`, `{width_original}`,
  `{height_original}`, `{duration}`, `{bitrate}`, `{media_type}`/`{type}`,
  `{sensitive_flags}`, and `{source_id}`/`{source_username}`; X adds
  `{tweet_id}`, `{retweet_id}`, and `{quote_id}`, and Facebook adds `{id}`.
  Outside boorus, `{filename}` includes the extension. `{url}` is
  unavailable so signed transport URLs cannot leak into paths.
  Fields use gallery-dl's replacement-field syntax: `.key`/`[key]`, `[index]`,
  and `[start:stop]` accessors; `{a|b|'literal'}` alternatives; the `!l`, `!u`,
  `!c`, `!C`, `!t`, `!s`, `!S`, `!r`, `!j`, `!d`, `!D`, `!T`, `!i`, `!f`, `!g`,
  `!U`, `!H`, and `!W` conversions; chained `?before/after/`, `L<max>/<text>/`,
  `J<sep>/`, `R<old>/<new>/`, `A<+|-|*><n>/`, `C<conversions>/`, `S<a|r>/`, and
  `O<+HH:MM>/` specifiers; then a Python format specification (fill, alignment,
  sign, `#`, zero padding, width, grouping, precision, and `s d x X o b f F e E %`
  types) or, for dates, a `strftime` pattern such as `{date:%Y-%m-%d}`.
  Missing or empty values fail explicitly unless an alternative supplies one or
  a `?` specifier makes the field optional. Widths are limited to 240 and
  intermediate values to 64 KiB; nothing evaluates arbitrary code.
  `{{` / `}}` escape braces; all output then
  uses safe ASCII filename characters, so literal braces become underscores.
  Unknown fields/conversions/specifiers and literal directory paths fail when
  the template is parsed. Substituted values are sanitized; names are limited to 240 bytes
  while preserving the final extension. Include enough IDs to make names unique:
  files at colliding paths are assumed complete and skipped without overwriting.
  Changing the template does not clear archive history. Upstream's `D`, `X`,
  `M`, and `I` specifiers, byte slices, `{_now}`/`{_env}` globals, and Python
  expression (`\f`) templates are not supported.
  This flag conflicts with `-g` and `-j`.
- `--directory-template '{category}' --directory-template '{username}'` replaces
  the default category folder with ordered component templates under the output
  root. Configuration uses `"directory": ["{category}", "{username}"]`; an explicit
  empty list saves directly in the base directory. Fields and sanitization match
  filename templates. Components cannot contain literal paths, control characters,
  or drive prefixes; metadata separators are sanitized. Limits are 32 components,
  8 KiB per encoded component template, and 3072 bytes for the rendered relative
  directory. Legacy ASCII names are limited to 240 bytes per component; booru
  source names preserve Unicode and use filesystem component limits. Missing
  booru fields format as source `None`; other missing fields fail before
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

### Social platforms

Native coverage of the largest social platforms (`--extractor native`;
`telegram`, `snapchat`, `kuaishou`, `xiaohongshu`, `threads` and `ytdl` have no
upstream extractor and
are always native):

| Platform | Extractor | Anonymous access (checked live) |
|---|---|---|
| Instagram | native (experimental) | public posts and reels, and the 12 posts a public profile page lists; the rest of a profile needs `--cookies` |
| X/Twitter | native (experimental) | single posts as a guest; timelines and searches need `--cookies` |
| Facebook | native photos, albums, videos and profiles | first grid page of public albums and profiles; the rest needs `--cookies` |
| Reddit | native REST listings, submissions, users | Reddit sends logged-out JSON clients on some networks to its login page; pass `--cookies` |
| Pinterest, Bluesky | native | yes |
| Tumblr | native | yes; when the shared default API key hits its daily limit, the web client's public token is used (its NPF posts are converted) |
| TikTok | native posts and user listings | yes |
| Telegram | public channels and posts via `t.me/s/` | yes |
| Weibo | native statuses, albums, videos | statuses and albums; timelines need cookies |
| VK | native photos, albums, walls, tags | yes |
| Bilibili | native opus articles and user/favorite lists | yes, via the dynamic API under risk control |
| Discord | native channels and servers | needs the `token` site option |
| YouTube, Twitch, Douyin (yt-dlp needs fresh browser cookies), LinkedIn, Snapchat Spotlight posts, Bilibili/VK video | `ytdl` site through yt-dlp | as yt-dlp; cookies are forwarded |
| Snapchat | native public profiles (`@USER`, `add/USER`): story, highlights and Spotlight; single Spotlight URLs through yt-dlp | yes |
| Xiaohongshu | native image and video notes (`explore/ID`, `discovery/item/ID`) | only through shared links carrying their `xsec_token`, else cookies; fixture-tested only, as this network is sent to login |
| Threads | native posts (`@USER/post/CODE`) through the logged-out `BarcelonaPostPageTargetQuery`; its rotating query ID is rediscovered from the page's bundles when stale | yes |
| Kuaishou | native short videos (`visionVideoDetail` GraphQL) | no: anonymous clients get a captcha; pass a browser session's cookies |

WhatsApp, WeChat, Messenger and QQ have no public media pages. The Kuaishou
and Xiaohongshu extractors are tested against fixtures only; this network gets
a captcha or a login page from them.

## Limitations

Native Instagram, Facebook, and Twitter/X remain experimental, and live
authenticated social downloads have not been verified in this repository.
Without session cookies they fall back to logged-out access, checked live on
2026-10-23: Instagram posts are read from the public post page's embedded
post object; X posts use a guest token with `TweetResultByRestId` (upstream's
`tweet-endpoint: auto` choice for guests), and other X queries report that a
session is required; Facebook profiles, albums and avatars are read as
described above. A partial X session (only one of `auth_token` and `ct0`) is
an error rather than a guest.
Their undocumented endpoints may change; fixture tests establish parser and
transport behavior. Anonymous Facebook video extraction has been checked, but
complete live downloads and authenticated coverage remain unverified.

Direct HTTP(S) media and inline-text jobs are downloaded. Extraction retains
typed direct HTTP, inline-text, video-page, and HLS/DASH jobs in input order. `-g` preserves the original `ytdl:`
source; `-j` includes the job kind and manifest protocol/base URL. Inline job bodies, headers, cookies, and other private handoff options are omitted
from their job fields in JSON. Inline-text inspection URLs carry encoded text.
The JSON output is for inspection, not a lossless job export. Different inline
manifests with the same source can therefore look identical in inspection output.

Manifest links (`.m3u8`/`.mpd`, case insensitive) can be inspected directly.
Compatibility extraction retains `_ytdl_*` options for a future adapter, including
inline manifests and manifest-specific headers. `_fallback` URL arrays are used
by the shared Rust downloader for direct HTTP fallbacks. Handoffs
require an HTTP(S) target/base URL, including synthetic `ytdl:https://…` URLs;
synthetic non-HTTP identifiers and unknown manifest types fail extraction.
The parser follows the
[upstream handoff protocol](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/downloader/ytdl.py).

### Video pages and HLS/DASH

Video-page jobs and HLS/DASH manifest URLs are downloaded by running an external
[yt-dlp](https://github.com/yt-dlp/yt-dlp) (`yt-dlp` on `PATH`, or
`--ytdl-command PATH`), like gallery-dl's `ytdl` downloader. It is invoked
directly, without a shell, with `--ignore-config`; request headers, `--proxy`,
`--retries` and `--limit-rate` are forwarded, and destinations with a video or
audio container extension are merged/remuxed into that container. Output is
written to a private temporary directory beside the destination and published
without replacing an existing file; size limits apply to the finished file.
Diagnostics never include URLs or yt-dlp output. If yt-dlp is missing or fails,
direct HTTP `_fallback` URLs are tried. `--no-ytdl` disables yt-dlp; video-page
and manifest jobs then fail before any request or file creation unless a direct
fallback exists. Inline manifest data (`_ytdl_manifest_data`) has no fetchable
location for relative segments and is not passed to yt-dlp. A stable archive hit
can skip an already completed logical media item before download-type validation.

`--cookies FILE` and `--cookies-from-browser SPEC` are also forwarded to yt-dlp,
both when extracting `ytdl` sites and when downloading. A cookie file is passed
as a private temporary copy, because yt-dlp rewrites its cookie file on exit; a
browser specification is passed through unchanged for yt-dlp to read itself.

Broader native browser cookie import,
broader native collection pagination, and full gallery-dl filename/config compatibility are
not implemented.

Direct HTTP fallback attempts are enabled by default. Scoped `"fallback": false`
disables them for Rust downloads; explicit values also forward to compatibility
extraction for all social categories. After a remote/request failure and its
configured retries, the downloader tries the next distinct valid HTTP(S) URL in
`_fallback` order. Lists are limited to 128 entries and URLs to 16 KiB; invalid,
repeated, credentialed,
and manifest targets are skipped. A delegated/manifest job can also use a direct
fallback when yt-dlp is unavailable or fails. Local filesystem errors and
size-filtered results do not advance the fallback chain.

All attempts publish to the original job's destination and retain its metadata
and archive identity, including the original filename extension; fallback MIME
types do not adjust that extension. Failed candidate partials are discarded
before changing URLs; resume state is bound to each candidate's request identity.
With `--resume`, the final failed candidate's checkpoint is retained for a later
run. Explicit
cross-origin fallbacks clear every per-item header, including authorization,
referer, user-agent, and custom headers. Configured HTTP-client defaults still
apply. This does not enable cross-origin redirects or import browser/session
cookies into media requests. Fallback URLs remain private in inspection output
and metadata sidecars. Signed-URL/header changes and changed fallback context
can create separate request paths/checkpoints while stable social archive keys
remain unchanged.

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
status extraction buffers each media batch, and conversation modes can buffer
pre-focal context up to their documented limit. The Rust download
pipeline retains one message buffer plus active jobs, while deduplication keeps
a fixed-size SHA-256 digest per unique job across all inputs, except for X with
`unique: false`. `-j` additionally buffers all selected media for its JSON array. Native collection streams now pull supported pages and jobs lazily; further
sites and collection types remain planned. Library callers can use `Backend::stream`;
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
`src/twitter_detail.rs` selects focal posts and buffers pre-focal conversation context;
`src/twitter_detail_parse.rs` parses owned conversation entries and tombstones;
`src/cookies.rs` owns scoped Netscape cookie selection. See the
[fixture and manual-check notes](tests/fixtures/instagram/README.md) before
promoting native extraction to the default.

See [ROADMAP.md](ROADMAP.md) for the native extractor work. This repository's
existing [GPL-3.0 license](LICENSE) is retained. The gallery-dl backend is a separate
program; this project does not bundle its Python source.
