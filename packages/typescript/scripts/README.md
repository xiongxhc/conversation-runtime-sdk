# Installed final-frame reopen proof

`installed-final-frame-reopen.mjs` is the consumer driver for
`test/installed-final-frame-reopen.test.ts`. The test installs the packaged SDK
tarball into a fresh directory outside this repository, copies this driver there,
and runs it once per scenario. The driver imports `@conversation/runtime` from
that installed package and drives the packaged gateway binary, so the oracle
never touches `packages/typescript/src`.

Point `RUNTIME14_INSTALLED_ARTIFACTS` at the artifacts directory holding
`stage/conversation-runtime-0.1.0.tgz` and
`target/debug/conversation-runtime-gateway`. The proof fails with a clear message
when that variable is unset; it is never skipped.

```sh
export PATH=/opt/homebrew/opt/node@24/bin:$PATH
npm run build --workspace @conversation/runtime
cd packages/typescript
RUNTIME14_INSTALLED_ARTIFACTS=/path/to/runtime14 \
  node --test --test-concurrency=1 dist/test/installed-final-frame-reopen.test.js
```

The unread-acknowledgement scenario deliberately waits past the gateway's own
ten-second capture-finish budget, so a full run takes about fifteen seconds. No
production timeout is widened anywhere in this proof.

The recognition-provider negatives live in
`apps/runtime-gateway/tests/media_isolation.rs`, because a gateway may only reach
a non-loopback HTTPS or WSS provider and an installed consumer therefore cannot
host a failing recognition provider on loopback:

```sh
cargo test -p conversation-runtime-gateway --test media_isolation drainable_finish
```
