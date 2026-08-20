# Changelog

## [0.3.0](https://github.com/RainyPixel/open-wake/compare/v0.2.4...v0.3.0) (2026-08-20)


### ⚠ BREAKING CHANGES

* **lifecycle:** remove --interval and --check-every. Use --poll-every for external predicates and --checkpoint-every for model-visible progress checkpoints.

### Features

* **lifecycle:** recover interrupted hooks ([5fd4bb8](https://github.com/RainyPixel/open-wake/commit/5fd4bb847e2423e6dc0fca1e8a5afa662cd986e2))


### Bug Fixes

* **release:** keep breaking bumps pre-1.0 ([9277b76](https://github.com/RainyPixel/open-wake/commit/9277b76dddc6d3b4daba4b93cd36d7e85d19fb35))

## [0.2.4](https://github.com/RainyPixel/open-wake/compare/v0.2.3...v0.2.4) (2026-08-16)


### Bug Fixes

* **lifecycle:** make cancellation terminal ([179cc43](https://github.com/RainyPixel/open-wake/commit/179cc43dcda9674a2e0f97a8deb38b0c79fbce72))

## [0.2.3](https://github.com/RainyPixel/open-wake/compare/v0.2.2...v0.2.3) (2026-08-15)


### Bug Fixes

* **setup:** enable installed hooks ([74d6bc5](https://github.com/RainyPixel/open-wake/commit/74d6bc5e0acd839d392ced432a82c9b06cba7bb4))

## [0.2.2](https://github.com/RainyPixel/open-wake/compare/v0.2.1...v0.2.2) (2026-08-15)


### Bug Fixes

* **doctor:** detect missed stop hooks ([dad2582](https://github.com/RainyPixel/open-wake/commit/dad2582a7bc59ef5bb9d00a48f738276ecb16e73))
