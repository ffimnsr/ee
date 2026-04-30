# Issues

## Warning cleanup follow-up

- Existing compiler warning noise during targeted test runs is not caused by the recent rope and annotation fixes.
- Main buckets observed:
  - old `serde_derive` "non-local impl definition" warnings across `crates/core-lib` and `crates/rope`
  - lifetime elision style warnings in existing signatures
  - future incompatibility notice for `serde_test v1.0.110`
  - dead-code warning in `crates/core-lib/src/watcher.rs`
- Deferred for a later cleanup pass.
