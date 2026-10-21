`descriptors.json` records actual gallery-dl 1.32.17 `extractor.find()` results:
class, category, subcategory and the inherited `archive_fmt`, including empty
formats. Regenerate with an installed gallery-dl environment:

```
/private/tmp/gdlvenv/bin/python tests/fixtures/source_archive/generate.py
```

The generator records source hashes and performs no HTTP extraction. `key_cases`
run real Pixiv and ArtStation `items()` with synthetic API responses; real source
formatters produce the expected keys. Native tests parse those same responses.
The Fur Affinity favorites example is marked outside the native route set.

Descriptors are source format strings, never native hashed download identities.
A valid empty format differs from an unknown descriptor. Unknown URLs and direct
media without identifiable source extractor classes return `None`.

Media fallback deliberately declines Imgur, Imgbox, DeviantArt, Flickr,
Wallhaven, Pinterest and booru formats because ordinary native file metadata
cannot distinguish all relevant route classes. Root URLs retain those distinctions.
Child queues may carry only producer metadata: Pixiv and Twitter profile queues
cannot identify avatar/background/artworks/favorites from that metadata alone.
Explicit child descriptor annotations resolve this; unannotated ambiguous queues
return `None` instead of selecting a category default.

Format descriptors do not supply absent source keywords. Native context still
lacks several source fields: Flickr `user[nsid]`/`album[id]`, Pinterest `board[id]`,
Imgur `album[id]`, Imgbox `gallery_key`/`image_key`, DeviantArt `_username` and
`folder[uuid]`, booru `pool`, and some source-specific Newgrounds,
Postimg and Tumblr aliases. A descriptor for these routes does not imply
full default-key parity. Only proven aliases are projected: real ArtStation
asset IDs, Weasyl submission IDs, Mastodon attachment IDs, and Pixiv work IDs,
source numbers and producer-recorded suffixes. Mastodon defaults remain gated
because preview-card keys are not yet proven. Empty Imgur gallery and bare
DeviantArt descriptors also remain gated until native child framing matches.
Projection is for initial snapshots before hooks, never live contexts mutated
by earlier hooks. Existing complete raw novel/Sketch snapshots stay authoritative.

`booru_search.json` contains 110 actual-source cases: 87 matched tag searches and
23 unmatched URLs. Its adjacent generator exercises the constructors, real
`items()`, job keyword merging, archive formatting and default filenames with
synthetic API posts. It records duplicate parameters, decoding, whitespace,
empty searches, DAPI `all` wire sentinels, query-order differences and source
pagination arguments. Four additional cases apply the real MetadataPP modify
hook between source post/file yields, retaining dictionary identity and real
job keyword updates. Native tests replay keys and keyword ownership. Pool
metadata and ascending/non-ID pagination policies remain separate gaps.
