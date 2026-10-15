# Expat Junk remaining audit findings

Updated October 15, 2026. This document tracks outstanding issues and validation
work only; completed changes have been removed.

## Remaining priorities

| Priority | Problem and consequence | Next change |
| --- | --- | --- |
| Medium | The cache is an unbounded JSON map rewritten on each insertion; multiple app instances can overwrite each other's entries. Settings/secrets likewise do not merge concurrent instance edits. | Use per-entry cache files or transactional storage with eviction, privacy controls and persistence diagnostics. Keep project saves independent of cache failures. |
| Medium | Each API call creates a new HTTP client. There is no retry/backoff policy, quota progress, or network cancellation. Discarding a worker's result does not cancel an already-sent request. | Reuse provider clients and define bounded retries for transient statuses, honoring Retry-After. Add batch cancellation and request-size limits. |
| Medium | Projects refer to an absolute source path. Fingerprints detect changed content but do not help locate a moved file. Legacy projects also have unknown translation provenance and no historical source fingerprint. | Add source relinking and save-time relative paths or portable projects; show unknown provenance for legacy results. |
| Medium | Translations are raster overlays: translated text cannot be searched or selected, and original source text remains underneath. Export is not redaction. | Decide whether to add an accessible/searchable translation layer and a separate explicit redaction mode. |
| Lower | The app has no password-entry workflow, bookmark navigation or UserUnit support. Upstream metadata exposes outlines but the adapter currently uses only page labels. | Extend the document/provider UI around typed errors and upstream metadata. |
| Lower | Desktop file associations are advertised but startup arguments/macOS open-file events are not consumed. The release workflow fetches an unpinned continuous AppImage packaging tool; macOS entitlements allow executable memory without an established requirement. | Add native open-file handling, pin/checksum the packaging tool, and reduce signing entitlements after testing signed bundles. |

The next substantial investment should be real-document evaluation, manual source
correction, and transactional cache storage. Keep the existing GUI and shared
rendering boundaries.
For translation quality, build a small Japanese/English evaluation corpus covering
forms, labels, tables, vertical text, ambiguity, numbers and names. Compare providers
on reviewed outputs and cost/latency before claiming a quality winner. Supplying
context is useful infrastructure; it is not a substitute for that evaluation.

## Remaining PDF limitations

- Spatial reading order is heuristic for untagged PDFs. Complex tables and mixed
  orientations still need representative fixtures and manual source correction.
- The lazy page cache bounds resident raster/text data, but excludes encoded
  source bytes, transient parser/render allocations, font caches and GPU textures.
  Measure peak memory on large documents before setting broader memory guarantees.

## Outstanding validation

- Run the optional OCR CI job with Tesseract/Leptonica; those dependencies were
  unavailable locally.
- Exercise live provider requests, graphical interaction and signed release
  bundles. Local provider payload tests do not cover these integrations.
- Run a dependency vulnerability scan.
