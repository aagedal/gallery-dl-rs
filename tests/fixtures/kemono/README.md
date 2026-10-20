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
The same Coomer global-feed inspection returned non-JSON data and was not a
successful live verification.
