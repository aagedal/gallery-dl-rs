# Native album parser protocol fixtures

These are **synthetic minimal protocol examples**, not captured authenticated Facebook responses. The JSON envelope and the nesting of the ordered collection are not live verified. They exercise conservative accepted structures and error behavior; they do not establish production album support.

Primary upstream references:

- https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/facebook.py (`parse_set_page`, `parse_photo_page`, `extract_set`, `FacebookSetExtractor`)
- https://github.com/mikf/gallery-dl/blob/master/test/results/facebook.py (public Facebook-page album and resume URL tests)

Upstream identifies sets through `mediaSetToken` / `mediasetToken`, photo membership through the photo URL's `fbid` and `set`, and next photos through `nextMediaAfterNodeId`. Its implementation uses page-wide text matching; these fixtures additionally require structural identity binding. The seed must be the first edge of an ordered `photos.edges` or `pageItems.edges` collection within a record owning the requested token. Every token alias present must agree. A Photo URL proving membership alone cannot establish first position and is never used as a seed. Navigation must belong to the requested Photo record, whose own URL identifies the requested photo and set. A missing navigation field is an error; explicit null means the end.

On 2026-10-18, anonymous bounded requests to the upstream public test URLs `https://www.facebook.com/media/set/?set=a.10152716010956729&type=3` and `https://www.facebook.com/photo/?fbid=10152716011076729&set=a.10152716010956729` returned HTTP 200 HTML shells without Photo payloads, set token fields, or navigation data. No media was extracted. No authenticated browser or cookie data was inspected. Authenticated album seed and navigation placement still require verification before native extraction can become the default.
