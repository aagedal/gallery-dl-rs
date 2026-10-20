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
feeds (`bsky.app/profile/NAME`, `/media`, `/posts`), custom feeds
(`/profile/NAME/feed/RKEY`) and lists (`/profile/NAME/lists/RKEY`), where NAME
is a handle or DID. It reads the public, unauthenticated AppView
(`public.api.bsky.app`): handles resolve via `com.atproto.identity.resolveHandle`, posts via
`getPostThread` and feeds via `getAuthorFeed` (`posts_with_media` for the bare
profile and `/media`, `posts_and_author_threads` for `/posts`; cursor pagination
capped at 1,000 pages of 100). Custom feeds use `getFeed` with an
`app.bsky.feed.generator` record URI; lists use `getListFeed` with an
`app.bsky.graph.list` record URI. Their owners resolve once, while returned
posts retain their individual authors; the author-feed `reposts` restriction
does not filter mixed-author feed/list results. All feed/list pagination is
capped at 1,000 pages, follows opaque cursors (up to 1 KiB), and stops on
repeated cursors; invalid or oversized cursors are explicit errors. Images and videos (also inside
`recordWithMedia`) are downloaded as full-size blobs from
`https://bsky.social/xrpc/com.atproto.sync.getBlob?did=DID&cid=CID`, built from
validated identifiers only. `extractor.bluesky.reposts` defaults to false and
selects the underlying author; own pinned posts retain their normal identity.
`extractor.bluesky.videos` defaults to true; false omits video blobs.
`extractor.bluesky.quoted` defaults to false; true follows quoted-post media
using hydrated quote records when available, fetching missing records only as
the stream reaches them. Quote URI, CID and author identities are checked,
blocked/detached/deleted quote views are skipped, and chains stop on cycles or
fail beyond 32 quotes. Date filters preserve later newer posts after older pins,
reposts, or quotes. Bare profiles use the posts feed when `quoted` or
`reposts` is true; explicit `/media` retains its media filter. Media-free posts
are skipped. `post_id` is the underlying record key, `media_id`/`id` the blob
CID, `username` the underlying handle, `caption` the post text, `timestamp`
from `createdAt`; files carry aspect-ratio width/height, alt text and MIME type.
Default filenames are `{post_id}_{index}.{extension}`; archive identity is
(DID, CID), including quoted and reposted media. Compatibility mode sends
Bluesky URLs to the backend.

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
`index.php?page=post&s=list&tags=...`) and Danbooru/e621 pools (`/pools/ID`).
Danbooru/e621 page with an id cursor (`page=bID`, 200 per page) and Gelbooru-style
sites with `pid` (100 per page), both capped at 100 pages; pools are fetched in
pool order, up to 10,000 posts. Posts without a file URL (deleted, banned,
restricted) are skipped, and files are accepted only from the site's own HTTPS
CDN. `post_id` is the post ID, `media_id` the MD5, `content`/caption the
space-separated tag string, `username` the uploader name when the API provides
one (Gelbooru `owner`). Default filenames are `{category}_{post_id}_{md5}.{extension}`;
archive identity is (category, post ID). API-reported file size is exposed as
`filesize`; `rating` and signed `score` are available to post filters and filename
templates (e621 uses `score.total`). Per-category tags are not yet exposed.
Danbooru/e621 accept category-scoped `username` and `password` (the account's API
key) for HTTP Basic authentication. Gelbooru accepts category-scoped `user-id` and
`api-key`; both must be nonempty to authenticate. Credentials are sent only to the
API origin, never attached to media requests or public JSON, and redirects to
another origin are refused. Authentication can expose posts hidden from public
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
requires `extractor.pixiv.refresh-token` (accepted only in the `pixiv`
category, never echoed); without it Pixiv URLs fail before any network access.
The token is exchanged via upstream's iOS App OAuth refresh grant and the
access token cached in memory (refreshed once on `invalid_grant`). Supported:
`www.pixiv.net/[en/]artworks/ID`, `member_illust.php?illust_id=ID` (App API
`illust/detail`) and `users/ID` (illustrations then manga),
`users/ID/illustrations`, `users/ID/manga` (`user/illusts`, `next_url` paging
capped at 200 pages per type). Public illustration bookmarks support
`users/ID/bookmarks/artworks[/TAG]` and `bookmark.php[?id=ID]` through
`user/bookmarks/illust` (`restrict=public`, at most 200 pages). Without `id`,
the legacy bookmark URL uses the authenticated user's ID from OAuth.
A `tag` query overrides the path tag, following upstream's URL decoding.
Pagination preserves the selected user, public visibility, and tag;
private (`rest=hide`), following (`type=user`), and novel bookmarks remain
unsupported. Bookmarked files retain the original artist metadata and
(illustration, page) archive identity. Media are original `i.pximg.net` files
(`meta_single_page`/`meta_pages`) downloaded with
`Referer: https://app-api.pixiv.net/` (same-origin redirects only). Ugoira
animations fetch `ugoira/metadata` lazily per work and download the frame ZIP
(upstream's `600x600` ZIP URL is upgraded to `1920x1080`). Ordered `frames`
metadata contains each ZIP filename and its millisecond `delay`, available in
JSON inspection, sidecars, and filter/naming context; `ugoira: false` skips
animations without fetching their metadata. Bookmark date selection preserves
later newer artworks in source order. Original individual-frame mode
and animation conversion are not implemented. `post_id`/`id` is
the illustration ID, `media_id` `ID_pN`, `username` the account, caption the
title plus HTML-stripped caption, with `create_date` timestamp and page-0
width/height. Default filenames are `{id}_p{N}.{extension}`; archive identity
is (illustration, page). Plain `i.pximg.net` links stay direct downloads and
usually fail without the Referer.

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
`extractor.flickr.api-key` is set (accepted only for `tumblr`/`flickr`/`wallhaven`/`weasyl`/`pixeldrain`/`gelbooru`, never
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
post, optionally followed by its preview. Inspection output supports these jobs;
downloading them still fails explicitly because the companion video adapter is
not implemented. Direct MP4 downloads do not need that adapter. Library callers
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
then emits a typed first-party video-page job for an unhandled card. Delegated
or manifest jobs without a direct HTTP fallback require the companion adapter.

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

List membership/follow relations, arbitrary
root-profile dispatch selection, cursor-resume configuration,
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
before Rust receives media. Child ranges and child expression filters remain
unimplemented.

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
CLI values replace configured values. Counters start at one for each input URL.

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
dimensions, `metadata.file`, `media_id`, and `index`/`num` are absent. Missing keys
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
cover flattened child extractors for the whole input; nested child boundaries
and child predicates remain unimplemented.

Library callers can consume `ExtractionEvent::Post`, `Media`, and `EndPost`
through native `events` methods or `Backend::events_with_options`. Existing
media-only stream methods remain available; `event::media_only` adapts event
streams without inferring boundaries from file identities.

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
(epoch seconds), `index`/`num`, file properties, `community`, `article`, and the
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
When no explicit native configuration is given, discovery checks
`$GALLERY_DL_RS_CONFIG` first. On Unix it then checks
`$XDG_CONFIG_HOME/gallery-dl-rs/config.json` (or
`~/.config/gallery-dl-rs/config.json`) and `~/.gallery-dl-rs.conf`; on Windows,
`%APPDATA%/gallery-dl-rs/config.json` and `%USERPROFILE%/gallery-dl-rs.conf`.
The first existing file is loaded. `--native-config-ignore` disables discovery.
Upstream gallery-dl config files are not discovered because the native schema
is a strict subset. Compatibility config is loaded only when explicitly selected.

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
`file-filter`, `image-filter`, `post-range`, `post-filter`,
`retweets`, `quoted`, `pinned`, `replies`, `tweet-endpoint`, `conversations`,
`expand`, `unique`, `videos`, `previews`, `mp4`, `reblogs`, `reposts`, `ugoira`, `posts`, `inline`, `original`, `files`, `duplicates`, `text-posts`,
`unavailable`, `text-tweets`, `articles`, `cards`,
`cards-blacklist`, `size`, `fallback`, `max-posts`, `revisions`, `order-revisions`, `strategy`, `search-pagination`,
`search-results`, `limit`, `search-limit`, `search-stop`, `metadata`, `endpoint`, `order-posts` (the last three for Kemono/Coomer), `retries` (integer 0–10), and `timeout` (positive seconds, including fractions).
They can appear at the root, under `extractor`, or under a supported category
such as `extractor.instagram`, `extractor.imgur`, or `extractor.mastodon`.
Site-specific media options affect their respective extractors. Booru
`username`/`password` and Gelbooru `user-id` are category-scoped credentials;
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
unsupported types, hooks, environment expansion, and other conditional values fail
explicitly. Filenames use the native template subset described below; archive
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
configuration compatibility and postprocessors remain
unimplemented.

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
- Filenames use a SHA-256 digest of the media URL and job/request context plus a
  sanitized filename, under `destination/category/`. Exact duplicate jobs within
  a run are removed, and existing paths are skipped for direct downloads. Bare
  direct links keep their original URL-only digest; media with headers now use a
  different digest and may be downloaded again after upgrading.
- CLI deduplication includes validated social post/file identity as well as request
  context. Distinct Instagram, X, and Facebook files sharing a URL are retained;
  repeated copies of the same file and request are suppressed across inputs.
  Rows without usable social IDs retain request-based deduplication. This also
  applies without a download archive; X `unique: false` bypasses deduplication.
- `-f` / `--filename` replaces the default digest-prefixed basename with a native
  template under `destination/category/`. Available fields are `{filename}`
  (including extension), `{name}` (without the final extension), `{extension}`
  (without the dot), `{category}`, `{post_id}`, `{media_id}`, `{shortcode}`,
  `{username}`, `{index}`/`{num}`, `{timestamp}`, `{date}` (a UTC datetime),
  `{caption}`/`{content}`, `{description}`, `{community_id}`, `{community_name}`,
  and the nested `{community}`, `{article}`, `{source_user}`, and `{metadata}`
  maps. File properties also support `{width}`, `{height}`, `{width_original}`,
  `{height_original}`, `{duration}`, `{bitrate}`, `{media_type}`/`{type}`,
  `{sensitive_flags}`, and `{source_id}`/`{source_username}`; X adds
  `{tweet_id}`, `{retweet_id}`, and `{quote_id}`, and Facebook adds `{id}`.
  Unlike upstream, `{filename}` includes the extension, and `{url}` is
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

Native Instagram, Facebook, and Twitter/X remain experimental, and live
authenticated social downloads have not been verified in this repository.
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
Without a direct HTTP fallback, video-page and manifest downloads fail explicitly
before any request or file creation, even if a destination file exists. A stable
archive hit can skip an already completed logical media item before download-type
validation.
No companion dependency or video
adapter is enabled yet. The parser follows the
[upstream handoff protocol](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/downloader/ytdl.py).

HLS/DASH downloads, video merging, broader native browser cookie import,
broader native collection pagination, and full gallery-dl filename/config compatibility are
not implemented.

Direct HTTP fallback attempts are enabled by default. Scoped `"fallback": false`
disables them for Rust downloads; explicit values also forward to compatibility
extraction for all social categories. After a remote/request failure and its
configured retries, the downloader tries the next distinct valid HTTP(S) URL in
`_fallback` order. Lists are limited to 128 entries and URLs to 16 KiB; invalid,
repeated, credentialed,
and manifest targets are skipped. A delegated/manifest job can also use a direct
fallback without enabling the companion adapter. Local filesystem errors and
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
