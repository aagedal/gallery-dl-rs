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
Introduce explicit optional FFmpeg/yt-dlp integration for segmented video rather
than saving manifests as media. Document compatibility differences as features land.
