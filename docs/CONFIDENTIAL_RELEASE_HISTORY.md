# Confidential deployment authorization history

The closed-chain inference consumer will not send plaintext merely because a
hardware quote verifies. It also requires the measured-guest manifest to be
currently authorized by a public, signed, append-only history.

Each compact JSONL entry binds:

- an exact sequence and predecessor-entry digest;
- the domain-separated `MeasuredGuestManifestV1` digest;
- an immutable release identifier;
- `authorize` or `withdraw` policy;
- the effective Unix time; and
- an Ed25519 signature under the independently pinned authorization root.

The genesis predecessor is SHA-256 of the empty string,
`e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`.
Later entries use the domain-separated digest printed for the preceding entry.
Effective times cannot move backwards.

Create one entry without giving the tool write access to the history:

```console
cargo run --locked -p nanocodex-vm \
  --example authorize_confidential_manifest -- \
  --manifest release/inference-manifest.json \
  --signing-key /secure/offline/authorization-seed.hex \
  --sequence 0 \
  --previous-head e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 \
  --release-id tdx-h100/2026-08-11.1 \
  --action authorize \
  --effective 1786492800 \
  > release/deployment-history.next.jsonl
```

The command emits only the entry on stdout and prints the public key and new
history head on stderr. Review the exact manifest and entry before appending it
to the public history. Never store the signing seed in this repository.

`VerifiedDeploymentHistory::from_jsonl` verifies every signature, physical
sequence, predecessor link, monotonic effective time, fixed bounds, and an
expected head obtained separately from relying-party policy. Requiring the
head is essential: signatures and hash links alone cannot distinguish current
history from a valid truncated prefix. The public release process must publish
the new head as a signed transparency-log checkpoint before clients adopt it.

The vLLM consumer requires all three values:

```text
--authorization-history deployment-history.jsonl
--authorization-key 64_HEX_ED25519_PUBLIC_KEY
--authorization-head 64_HEX_TRANSPARENCY_CHECKPOINT_HEAD
```

It rejects an unlisted manifest, a future authorization, a withdrawal, a
forked or reordered history, signature tampering, and a history that does not
reach the separately pinned head. This controls new plaintext sessions. A
retained-state deployment still needs an explicit notice and data-withdrawal
policy outside this ephemeral consumer.
