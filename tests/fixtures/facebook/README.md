All payloads here are synthetic, using reserved example.test media hosts and no credentials.

`photo.html` models JSON script envelopes and the Photo fields documented in upstream gallery-dl's Facebook extractor, inspected 2026-10-18:
https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/facebook.py
https://github.com/mikf/gallery-dl/blob/master/test/results/facebook.py

An anonymous HTTP GET to `https://www.facebook.com/photo/?fbid=10160743390456729&set=` on 2026-10-18 returned HTTP 200, 434363 bytes. JSON `data-sjs` script payloads were present, but the response contained no Photo identity nodes or `nextMediaAfterNodeId`. `CometErrorRoot.react` appeared only in module registry definitions, not the upstream `__dr` block marker. This verifies the JSON script transport only; it does not verify the successful Photo object placement, block marker, or authenticated extraction. The live response is intentionally not stored because it contains opaque request/session data. Album traversal and successful live extraction remain unverified.

Experimental album parsing and streaming now have separate
[synthetic protocol fixtures](album_README.md) and local HTTP tests. They cover
identity-bound seeds/navigation, complete ordered output, earlier-result retention,
backpressure, loops, deadlines, cancellation, and range/date cutoffs. Successful
authenticated album traversal remains a separate live verification gate.
