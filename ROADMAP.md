# Native Rust roadmap

The compatibility backend is a bootstrap step. The intended end state is a native
Rust gallery downloader, starting with Instagram, Facebook, and Twitter/X.

## 1. Native single-post extractors

Implement one site at a time, with independently testable response parsers and a
shared extraction interface that emits media, post metadata, and child URLs.

- Instagram: photo posts and full carousels first, then direct reel/video media.
  Support logged-in sessions, redirects to login, expired sessions, and unavailable
  posts explicitly.
  **Implemented experimentally:** opt-in REST single-post extraction for photos,
  full mixed carousels, and direct video variants; scoped cookie loading, metadata,
  bounded requests, and explicit error handling. Synthetic parser fixtures and
  local HTTP transport checks are included. **Still required:** sanitized real
  response fixtures and a documented successful live authenticated manual check.
  Compatibility remains the default until those checks are complete.
- Facebook: photo permalinks and albums first, then video posts. Establish tested
  URL coverage and distinguish login pages from successful extraction.
- Twitter/X: single-post photos and direct video variants, including mixed-media
  posts and explicit unavailable/protected-post errors.

Before calling a site supported natively, require sanitized response fixtures for
single-media and multi-media posts, authentication failures, missing posts, and
rate limits, plus a documented manual check against a real accessible post.
Do not commit credentials or signed media URLs from private sessions.

## 2. Shared authenticated transport

Add domain/path-scoped Netscape cookies, user-agent/referer propagation, safe
redirect handling, Retry-After-aware rate limits, and cancellation. Replace each
compatibility extractor independently as native coverage is verified. Keep the
backend explicitly selectable until the three sites have sufficient parity.

Native Instagram now uses domain/path/secure/expiry-scoped input cookies and
propagates user-agent/referer without copying cookies to media hosts. Redirects
are currently rejected; rate limits return Retry-After advice rather than retrying.
Cookie updates, safe redirect following, shared rate-limit scheduling, and
cancellation remain to be implemented.

## 3. Large collections and reliable archives

Stream extraction into a bounded work queue; add pagination, stable site/post/media
identifiers, a SQLite download archive, media metadata sidecars, and resumable
downloads with range validation. URLs alone are not stable archive identifiers.

## 4. Broader compatibility

Add native configuration and filename templates, then prioritize additional sites.
Integrate the companion **yt-dlp-rs** project for video extraction and segmented
downloads. Document compatibility differences as features land.

### Companion yt-dlp-rs integration

Prefer an optional Rust library adapter once yt-dlp-rs has a stable, consumable
release. Keep both projects independently usable; do not add a dependency on a
developer's local checkout. A separately selected executable adapter can provide
compatibility with Python yt-dlp if needed, but is not a prerequisite for the Rust
integration.

- Gallery-dl-rs owns gallery/post traversal, ordered media jobs, metadata, archive
  identity, destination naming, concurrency, and final file publication.
- Yt-dlp-rs owns delegated video extraction, format selection, HLS/DASH download,
  and optional FFmpeg merging/remuxing. Agree on output ownership: it writes into
  a caller-owned temporary directory and returns the actual completed path and
  extension; gallery-dl-rs publishes it without overwriting existing files.
- Introduce typed jobs for direct HTTP media, video-page extraction, and HLS/DASH
  manifests. Manifest jobs must carry the base URL and optionally inline manifest
  data. A `ytdl:` handoff can refer to a page or carry a synthetic URL accompanied
  by manifest metadata; stripping the prefix and downloading the URL is insufficient.
- Preserve relevant compatibility-backend metadata (`_ytdl_manifest`, inline
  manifest data, headers, and fallback URLs) when translating to typed jobs. Keep
  session cookies scoped to each request URL; never flatten them into a global
  header. Define timeouts/cancellation, progress events, format selection, typed
  errors, and the completed-file result at the adapter boundary.
- Start integration tests with local progressive media, then local HLS/DASH
  fixtures, inline manifests, authenticated segments, merge failures, cancellation,
  and cleanup/no-overwrite behavior. Keep unsupported handoffs explicit until the
  companion implements the required capability.

The inspected local yt-dlp-rs prototype exposes `YoutubeDl`, `VideoInfo`,
`VideoFormat`, and an `Extractor` interface for direct media and limited HTML5
pages. It does not yet implement HLS/DASH, cookies, or FFmpeg postprocessing.
Those are prerequisites for the corresponding video handoffs, not functionality
that gallery-dl-rs can currently advertise. No companion dependency or runtime
delegation is enabled yet.
