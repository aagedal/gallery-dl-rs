# Kemono public response fixtures

`global-posts.json` and `post.json` are sanitized, reduced captures of the public
`/api/v1/posts` response and its first post's `/api/v1/SERVICE/user/USER/post/ID`
detail response, inspected on 2026-10-20 with `Accept: text/css`.

The listing originally contained 50 posts under `posts`, alongside `count` and
`true_count`. It is reduced to one post and both counts are set to one. The detail
response retains the consumed `post` branch; unused outer attachment, preview,
video and props branches are omitted. Only fields used by the native parser and
representative content/date fields remain.

All creator/post IDs, service, titles, content, dates, file names, hashes, paths,
and count values are replacements. JSON field types, envelope shapes, file order,
and repeated file-path relationships are preserved. Raw captures stay outside the
repository. These fixtures verify the observed response schema; they do not verify
authenticated content, revisions, thumbnails, CDN transfers, or every service.

A native global-feed inspection and a separate `expand: true`, `max-posts: 1`
inspection each emitted two media jobs successfully. No media were downloaded.
The initial Coomer global-feed inspection encountered gzip bytes before API
decompression was supported. A later gzip-aware inspection on 2026-10-20
succeeded, emitting one original MP4 media job and a creator display name.
No media were downloaded.

`profile.json` is a sanitized, reduced capture of the matching public creator's
`/api/v1/SERVICE/user/USER/profile` response, inspected on 2026-10-20. The server
returned `Content-Encoding: gzip` even with `Accept-Encoding: identity`.
The fixture retains the decompressed JSON shape and field types; creator IDs,
name, service, dates, public ID, counts and flag values are replacements.
No compressed raw response or real creator identity is stored in the repository.


`discord-server.json` and `discord-channel.json` are sanitized, reduced schema
captures of public `/api/v1/discord/server/SERVER_ID` and
`/api/v1/discord/channel/CHANNEL_ID` responses inspected on 2026-10-20 with
`Accept: text/css`. The source server contained 27 channel records and the source
channel response contained 12 messages. These fixtures retain representative text
and category channels, image attachments, an attachment-free message, a mention,
and nullable fields. They preserve the direct server-object and message-array
envelopes and observed field types, including string IDs, integer channel types
and sequence numbers, boolean attachment image flags, and microsecond date strings.
The retained messages preserve the capture's ascending sequence order.

All IDs, server/channel/author names, avatar hashes, message text, mentions,
attachment names/paths/hashes, dates, counts, positions and flag values are synthetic.
The fixture server is `100`; channel `200` contains messages `400`–`402`, channel
`201` is empty, and category channel `300` is the parent of channel `200`.
Author `500` and mentioned user `501` are invented. Content hashes use repeated
`a` or `b` characters. No raw public identities or capture URLs are recorded.
These captures establish response schema only; they do not verify authenticated
channels, pagination, historical edits, attachment downloads or complete servers.
