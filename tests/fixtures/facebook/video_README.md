# Native video protocol fixtures

`video.json` is synthetic, with reserved example.test hosts and synthetic signatures.
It models an identity-bound Video with an inline DASH manifest, two full-resource
MP4 representations, and two M4A audio representations. It is not a captured response.

Primary references inspected 2026-10-18:

- [gallery-dl Facebook extractor](https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/facebook.py): watch/user-video URL forms, embedded MPD BaseURLs, highest video quality, and separate optional audio output.
- [gallery-dl public tests](https://github.com/mikf/gallery-dl/blob/master/test/results/facebook.py): the two public IDs below, expecting one and two files respectively.
- [yt-dlp Facebook extractor](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/facebook.py): identity-bound GraphQL Video delivery fields, `videoDeliveryLegacyFields`, and modern `videoDeliveryResponseFragment.videoDeliveryResponseResult` inline manifests/progressive URLs.

Anonymous checks with the native CLI on 2026-10-18:

```sh
gallery-dl-rs --extractor native --extraction-timeout 25 --timeout 20 --retries 1 -j \
  'https://www.facebook.com/watch/?v=1165557851291824'
gallery-dl-rs --extractor native --extraction-timeout 25 --timeout 20 --retries 1 -j \
  'https://www.facebook.com/watch/?v=644342003942740'
```

The first returned one `1165557851291824.mp4` item; the second returned
`644342003942740.mp4` and `644342003942740.m4a`. An initial second-video request
with a 10-second request budget failed reading its response. The longer bounded
check succeeded. A subsequent first-video check without retries also returned one
item. Only output counts, filenames, category, and metadata field names were
reported; signed URLs and raw responses were not saved.

Separate urllib and reqwest attempts to capture a sanitized source response
returned logged-out shells without identity-bound Video delivery. Those attempts
did not yield real response fixtures. Live responses vary; the successful checks
do not establish reliable authenticated access, complete downloads, or coverage
of all response variants. No browser profile or supplied cookie file was read.

Parser/local transport tests cover identity/alias conflicts, modern/legacy delivery,
whole-resource quality selection, separate archive IDs, optional versus malformed
audio, XML entities/namespaces/inherited URLs, private diagnostics, bounds, retries,
cookie rotation, and user-agent propagation. Segmented, dynamic, encrypted, and
external-only manifests remain explicit unsupported cases.
