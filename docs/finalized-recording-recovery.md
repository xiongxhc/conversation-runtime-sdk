# Finalized recording recovery (control protocol v4)

Default gateway startup still announces protocol **3**, with unchanged v1–v3
frames and per-command compatibility rules. Opt in before starting the gateway:

```ts
const transport = await StdioGatewayTransport.start({
  gatewayPath, configPath, protocolVersion: 4,
});
const client = await RuntimeClient.connect(transport, 4);
const currentSessionId = client.currentSessionId;
if (currentSessionId === undefined) throw new Error("client is closed");
```

The equivalent CLI option is `--client-protocol-version 4`, after `--config PATH`.
Unsupported selections fail before configuration loading or provider startup.
An explicit-v4 connection remains pinned to 4; a later v1–v3 command is rejected
without changing queued response encoding. Default startup never upgrades to 4.

After stopping voice capture, v4 `finalizeRecording({ sessionId })` returns the
existing recording summary plus `recoveryCapability`. Its readonly fields are
`sourceSessionId: bigint`, `recordingHandle`, `sha256`, `keyId`, and opaque
`authorization`. The wire result adds the sibling `recovery_capability`; old
versions neither emit nor accept it. Failed finalization issues no capability.

Persist this bearer capability only in private application state. Never put it in
logs, general event history, shared meeting artifacts, command arguments, or
environment variables. Neither it nor the summary contains the encryption key.

After the original gateway exits, start another explicit-v4 connection:

```ts
const reconciliation = await client.reconcileFinalizedRecording({ capability });
const processed = await client.processText({
  sessionId: currentSessionId, instructions, input, outputSchema, maxOutputBytes,
});
```

`capability.sourceSessionId` always names the original recording's provenance.
`client.currentSessionId` names the current gateway connection and is cleared on
close/fatal failure. Batch text processing uses the latter without starting a
dummy voice session. Recovery requires idle voice and text lanes plus configured
recording and diarization. Legacy `reconcileRecording` retains both its current
gateway ID guard and original file-header provenance guard.

The same original root recording key must remain configured after restart.
`keyId` is an identifier, not a key-ring lookup or rotation mechanism. CRSREC02 and
its AES derivation are unchanged. A separate HMAC-SHA256 key is derived using ring
HKDF-SHA256 from decoded canonical root key bytes, with salt and info both
`conversation-runtime/finalized-recording-capability/v1\0`. The authenticated
message is that domain followed by big-endian u64 source identity, 16 decoded
handle bytes, 32 digest bytes, and 8 key-identity bytes. Tag verification uses ring's
constant-time verifier; Rust Debug and malformed-capability diagnostics redact it.

Finalized recovery accepts at most **8 GiB (8 × 1024³ bytes) of ciphertext**.
Oversized files are rejected before snapshot allocation/copy or v4 capability
issuance, including repeated finalization. Legacy v3 recording/reconciliation has
no new size policy. Unix source admission uses a no-follow, nonblocking descriptor
and verifies the opened handle is regular; Windows opens the reparse point itself
and rejects reparse/device/non-regular handles. Other platforms fail closed.

Recovery opens the canonical handle once, copies only ciphertext through a fixed
64 KiB hashing buffer, authenticates every chunk, requires exactly one terminal
Ended and immediate EOF, rehashes the snapshot, and rewinds its retained descriptor.
Only then may the diarizer be called or receive plaintext windows. The snapshot is
exclusive/private and unlinked immediately on Unix (delete-on-close on Windows),
so source rename or same-inode mutation cannot alter its second pass, and process
death cannot leave a named extra copy. Cancellation joins recovery cleanup.
No plaintext audio is written to disk; v3's partial-recording behavior is unchanged.
