# Confidential-compute leaderboard gates

Nanocodex's confidential-VM work is accepted only when an independent verifier
can rank a deployed consumer ahead of every current entry in Andrew Miller's
[`awesome-private-inference`](https://github.com/amiller/awesome-private-inference)
dashboard. Passing internal tests or producing a valid hardware quote is not
enough.

This mapping is pinned to dashboard commit `152ad39` and the DevProof guide at
commit `7edaafd`, reviewed on 2026-08-11. The dashboard's bars are editorial and
architecture-specific, so Nanocodex applies all relevant fields rather than
selecting the smallest favorable denominator.

Status vocabulary:

- `implemented`: the typed implementation and deterministic tests exist;
- `partial`: a primitive exists, but an external verifier cannot yet prove the
  complete plaintext path;
- `missing`: the load-bearing enforcement or public evidence does not exist;
- `n/a`: the architecture removes the hop, with the reason stated explicitly.

## Dashboard field mapping

| Dashboard field | Nanocodex status | Required external evidence |
|---|---|---|
| Client nonce bound | implemented | Fresh caller nonce and expiry are included in the native report-data transcript; replay and mutation fail. |
| CPU quote verified | implemented | Raw SNP, TDX, or Nitro evidence, exact endorsements/collateral, appraisal time, reference values, and verifier output are publishable. A live positive record is still required for every advertised backend. |
| Report data binds key | implemented | The quote binds both the guest-generated Ed25519 receipt key and X25519 encryption key, not a server-provided key. |
| GPU attested | partial | Every advertised GPU and switch has fresh vendor evidence, exact-count topology, a protected binding to the parent VM, and locally verified certificate/RIM/revocation policy. Managed TDX+H100 evidence does not complete the native libkrun GPU gate. |
| Key derives to address | n/a | Nanocodex binds the complete public-key bytes directly; it does not weaken identity to an Ethereum-address derivation. |
| Workload commitment | partial | `MeasuredGuestManifestV1` commits source, boot artifacts, root, code, model, configuration, argv, resources, and reference values. Confidential configuration now rejects plain ext4 and emits the exact dm-verity kernel table, but this becomes green only after the reviewed libkrun/libkrunfw revisions are pinned and live evidence proves enforcement. |
| Production OS image | partial | The minimal image has no SSH/admin service and now includes a reproducible dm-verity tree. It becomes green only when that root is launch-measured and enforced before any external-root byte is read. |
| Serving code and model attested | partial | The closed-chain vLLM policy now requires a complete measured-guest manifest whose component set contains the exact supervisor, immutable OCI digest, model-snapshot digest, and domain-separated inference-configuration digest. Independent artifact-to-component reconstruction and a live authenticated-root deployment remain required. |
| Backend attested | n/a for direct mode | The first ranked consumer has one direct VM endpoint and no gateway-to-model hop. Any later gateway makes this field mandatory for every request. |
| Catalog model is served | partial | The thin consumer binds model ID and revision to the measured configuration, requires a model-snapshot digest, checks the live container command and returned model, and carries the prompt/response only through the closed encrypted channel. A retained live record is still missing. |
| Code measurement reproducible | partial | Independent builds match for the static supervisor, ext4-plus-verity root artifact, and both pinned SEV and TDX firmware/kernel/initrd chains. The application, model, and complete CPU reference measurement must also reproduce from published source. |
| Live TLS key pinned | n/a if no TLS terminator exists | A direct protocol may pin the quote-bound X25519 key. If TLS is introduced, the live leaf SPKI must be quote-bound and verified rather than trusted through WebPKI alone. |
| Encryption key attested | implemented | The quote binds a guest-generated X25519 key. The only appraisal-gated command protocol encrypts its command policy and input to that key, retains a relying-party-only response key, and returns the entire signed proof under distinct response-domain AEAD. A live managed TDX+H100 round trip now exercises this path; the authenticated-root libkrun consumer record is still required. |
| Client nonce supported | implemented | The relying party chooses the nonce; provider-generated freshness is not accepted. |
| Runtime configuration fully attested | partial | The measured launch fixes resources, kernel command line, network, debug, migration, complete workload argv, and a digest of the vLLM image/model configuration. Live negative probes must still exclude mounts, environment, writable model state, admin endpoints, and operator logging. |

## DevProof Stage 1 overlay

The dashboard's automated surface is necessary but not sufficient. The deployed
consumer must also satisfy every DevProof Stage 1 item:

| DevProof requirement | Current status | Completion gate |
|---|---|---|
| Public attestation/upgrade history | partial | The consumer now requires a signed, hash-chained JSONL history, current manifest authorization, and a separately pinned head so a valid truncated prefix fails. The first real history and head still need publication in an external transparency log. |
| Auditable code | implemented | Public source, exact revision, lockfile, toolchain, and build instructions remain available. |
| Reproducible measurement | partial | Two independent builders recompute the complete CPU-specific reference measurement, not only the supervisor/root bytes. |
| Developer cannot access secrets | partial | The vLLM consumer now appraises TDX+H100 before encrypting its prompt and receives only an encrypted signed proof over the untrusted transport. Model credentials, persistent state, live operator paths, and negative logging probes remain to be closed. |
| Upgrade process | partial | Every newly authorized manifest must enter the verified public history before the ephemeral consumer releases plaintext, and a withdrawal blocks new sessions. External checkpoint publication plus retained-state notice/withdrawal policy remain. |
| No centralized privacy/integrity dependency | partial | The direct local VM removes a gateway and external database. Public appraisal must depend only on pinned source/artifacts, the selected TEE vendor roots, and explicitly declared transparency roots. |
| No debug/backdoor path | partial | Debug and migration are rejected by policy and the minimal image has no SSH. Live negative tests must also prove no unmeasured root, environment, mount, admin endpoint, console, or recovery key reaches plaintext. |

## Managed TDX+H100 encrypted-transport record

On 2026-08-11, a fresh GCP Spot `a3-highgpu-1g` VM ran the current static
supervisor under Intel TDX with one H100 in confidential-compute mode. The
outside verifier fetched quote-specific Intel PCS collateral, rejected stale
collateral from a different machine, and then accepted fresh DCAP and NVAT
evidence under the pinned H100 policy. Both components reported trusted boot
with debug disabled.

After that appraisal, the verifier encrypted a random 64-byte input to the
quote-bound guest X25519 key. The untrusted SSH transport carried only the
confidential-command envelope and encrypted response. The verifier decrypted
and authenticated the signed proof locally; request and response plaintext
SHA-256 values both equaled
`5cc743d14172723540154aadd9e54d6a99afed8714e47e2b4817528c5b5178b2`.
The current supervisor SHA-256 was
`276335175718acae6c28f867ee5bc5a0473099a394f645cde9988a64b3bec741`.

This is positive evidence for the CPU/GPU appraisal and encrypted transport
contracts. It is not the number-one release record: GCP measured its managed
boot chain, not the uploaded supervisor or Nanocodex dm-verity root, and the VM
did not expose `/dev/kvm`. It therefore could not execute the pinned libkrun
fork or close the native authenticated-root/GPU gate. The temporary verifier
key, plaintext, VM, boot disk, and local SSDs were deleted after evidence
capture.

## Number-one release gate

Do not describe Nanocodex as dashboard-leading until all of the following are
true for one real, directly probeable consumer:

1. Every applicable dashboard field above is independently green; every `n/a`
   is demonstrated by architecture rather than omitted evidence.
2. All seven DevProof Stage 1 requirements pass.
3. A standalone verifier starts from a fresh nonce, fetches raw evidence and
   published artifacts, recomputes the manifest/reference measurement, verifies
   the quote and guest encryption key, encrypts the request only after success,
   and validates the encrypted response.
4. Retained positive and adversarial live records cover root, code, model,
   configuration, key, nonce, GPU, routing, debug, migration, and upgrade
   substitution.
5. The public probe can run continuously without operator-only state and names
   one immutable content digest for every observed deployment.

The immediate blockers are a libkrun-capable TDX+H100 host that can enforce the
authenticated root, complete serving-code/model/configuration closure on that
host, an externally published provenance/upgrade checkpoint, and continuous
positive plus adversarial probes of the resulting immutable deployment.
