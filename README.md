# p2p_layer_payments

Prototypes for paying for services over Zakura's P2P layer, for example paying in ZEC for access to a provider's stream.

This is early prototype work. Nothing here moves real funds.

## paid-services sidecar (prototype, simulation only)

> **SIMULATION ONLY.** This sidecar talks to a **mock node** and a **mock issuer**.
> - It opens no wallet and starts no payment.
> - It holds no seed, spending key, viewing key or wallet credential.
> - It talks to no real Zakura node.
>
> **Do not open a payment request it prints in a wallet.**
>
> **Wallet expiry support is not solved.** No wallet examined can set a transaction's `nExpiryHeight` from a payment request. This prototype does not work around that.

The prototype explores a standalone sidecar (the "bridge") between a user's own Zcash wallet and a Zakura node that buys access to a provider's paid stream.

### What it is and is not

- **The `paidstream_*` methods are proposed RPC contracts, not existing Zakura RPCs.** The mock node implements them so the bridge can be exercised end to end:
  - `paidstream_requestinvoice`
  - `paidstream_getgrant`
  - `paidstream_setgrant`
- **A signed grant is the trusted issuer's access authorization, not on-chain proof of payment.** The bridge takes access decisions only from issuer-signed objects that verify against its pinned issuer key. It never derives a grant or a payment from any of these:
  - a local txid;
  - a user statement;
  - the node's chain RPC.
- **A mock `setgrant` install does not show that a node consumes a stream.** There is no P2P service here, no capability bit or stream kind, and no ZIP 316 Revision 2 support.
- **The mock node's client identity is a configured value.** It stands in for the TLS-proven NodeId of a real P2P session. A `NotFound` for another NodeId tests the mock contract only, not real P2P authentication.
- **All mock values are test data, not proposals:** heights, prices, confirmation counts and deadline margins.
- **Mock destinations are synthetic testnet unified addresses.** Their Orchard receiver deliberately holds an invalid point encoding, so no wallet should be able to pay them. This was not tested against a wallet.

### Commands

```
paid-services-sidecar --config FILE buy SERVICE        # new purchase, or continue the open one for SERVICE
paid-services-sidecar --config FILE status [PURCHASE]  # ask the provider (through the node) for the status
paid-services-sidecar --config FILE resume             # finish what a lost answer or a restart left open
paid-services-sidecar --config FILE show PURCHASE      # show a still-pending payment request again
```

The mock side:

```
paid-services-mock node --dir NEW_DIR [--port PORT] [--serve-secs SECS]
paid-services-mock control --config FILE pay PURCHASE_ID [--amount ZAT] [--expiry HEIGHT]
paid-services-mock control --config FILE mine BLOCKS
paid-services-mock control --config FILE drop-next METHOD
paid-services-mock control --config FILE tamper MODE
```

#### Demo

```
cargo build --workspace
target/debug/paid-services-mock node --dir /tmp/psb-demo-1 --serve-secs 600 &
target/debug/paid-services-sidecar --config /tmp/psb-demo-1/sidecar.json buy synthetic-10
target/debug/paid-services-mock control --config /tmp/psb-demo-1/sidecar.json pay <purchase_id>
target/debug/paid-services-mock control --config /tmp/psb-demo-1/sidecar.json mine 2
target/debug/paid-services-sidecar --config /tmp/psb-demo-1/sidecar.json status
```

`paid-services-mock node` refuses to overwrite an existing cookie or config, so each demo uses a new directory.

### What the sidecar checks

**Invoice** (signed in the invoice domain, `verify_strict` against the pinned key):
- network, issuer key, provider NodeId;
- client NodeId equals the node's own NodeId (`getminermeshinfo.node_id`);
- the service requested and the `purchase_id` of the answer;
- a nonzero amount within the purchase limit, and nonzero terms;
- a shielded destination on the configured network.

**Payment request:**
- The bridge builds `zcash:<address>?amount=<ZEC>[&memo=<base64url>]` itself: one payment, no `req-` parameters.
- It then reads the request back with the `zip321` reference parser and compares address, network, amount and memo with the invoice. This checks the bridge's own output only; it is not evidence that any wallet accepts the request.

**When a request is offered:** only while both hold:
- the provider still reports the purchase as `Pending`, which means it has seen no payment;
- the offer window is open: `tip + 1 + expiry_delta + offer_margin_blocks <= deadline`.

`expiry_delta` comes from an explicit wallet expiry profile (`wallet_expiry_profile` in the config, see `crates/paid-services-types/src/expiry.rs`). The only profile, `librustzcash-default`, was read from the source of zecd 0.10.0, Vizor v0.0.66 and Zodl's librustzcash:
- the target height plus 40 blocks before NU7;
- plus 120 blocks from NU7.

It was not tested against a running wallet. `offer_margin_blocks` is a configured value, not a recommendation.

The window is a heuristic. It limits when a request is shown, but it cannot stop a user from paying later, so a late payment can still miss the deadline rule. A test shows exactly that.

Otherwise the request is withheld. The local height is only ever used to withhold, never to grant.

**Statuses** of `paidstream_getgrant`:

| Status | Meaning |
|---|---|
| `Pending` | No payment seen. |
| `PaymentSeen` | A payment still short of its confirmations. |
| `Underpaid` | A payment below the invoice amount. |
| `NeedsReconciliation` | A payment that breaks the deadline rule. |
| `Issued` | A grant was issued. |
| `Expired` | The purchase or its grant expired. |
| `NotFound` | Unknown, or bound to another client. |

`PaymentSeen` and every status other than `Pending` withhold the request.

**Grant** (signed in the grant domain): checked like the invoice, plus the service, the `purchase_id`, and that the duration equals the quoted access. A second, different grant for the same purchase is refused, and the first is kept.

**State:**
- One JSON file. The `request_id` is written before the invoice request goes out.
- Every write goes to a synced temporary file, which is then renamed over the state file.
- A lock file stops a second process on the same state.
- A corrupt or inconsistent state file is refused and left untouched.

### Proposed contract error codes

These are prototype values, defined in `crates/paid-services-types/src/contract.rs` and shared by the sidecar and the mock node. They are not Zakura error codes.

| Code | Name | Effect on a `paidstream_requestinvoice` purchase |
|---|---|---|
| 1 | `Disabled` | Kept. The service is off in the node's config. |
| 2 | `ProviderUnreachable` | Kept. The provider may or may not have recorded the request. |
| 3 | `Rejected` | Final. The provider refused the request, for example with `RequestConflict` or `UnknownService`. |
| 4 | `InvalidGrant` | `paidstream_setgrant` only: the node refused the grant. |
| 9 | mock control | Returned only by the mock node's control methods. |

Only code 3 ends a purchase. Every other error answer leaves the purchase in `requested` with its request_id, and so does a missing answer. That covers codes the bridge does not recognise and standard JSON-RPC codes. `resume`, or the next `buy` for the same service, retries the same request. A new request_id is never created for it.

A malformed answer is kept for retry the same way: a missing field or bad hex. The provider may already hold an invoice for that request_id. An invoice that fails its signature or a check is rejected.

Zakura answers handler errors with HTTP 200 (`zakura-rpc/src/server/http.rs:180` at Zakura `cb148173`), so a code 3 rejection reaches the bridge in a 200 answer. Other HTTP statuses are transport-level problems, and the bridge keeps the purchase on them.

### Known limits and open integration points

- **The deadline rule cannot be guaranteed before payment by a user's own wallet.** ZIP 321 has no expiry field. The offer window only narrows when a request is shown. ZIP 316 Revision 2 address expiry is an option whose wallet support still has to be verified. It is not implemented here. What happens to money that misses the rule is still undecided.
- **`Pending` does not mean "not paid".** It means the provider has seen no payment yet. A payment that was just broadcast may not have been seen. Showing the QR code or link again cannot prevent a second payment: `show` only warns.
- **A mock grant install is not stream consumption.** `paidstream_setgrant` on the mock node only records the grant. No node receives or consumes a paid stream.
- **No wallet result reaches the bridge.** QR and deep-link wallets return nothing to the caller, so the bridge cannot detect a lost wallet answer.
- **The real node side does not exist yet:** the three RPCs, the paid-stream P2P service, grant persistence in the node, and capability allocation.

### Layout

The workspace mirrors Zakura's `crates/` layout, so it can move into that tree later.

| Crate | What it holds |
|---|---|
| `crates/paid-services-types` | Sync-only, no I/O: the signed invoice and grant codec, the checks, the contract error codes, the ZIP 321 code, the wallet expiry profile and the sidecar config type. |
| `crates/paid-services-sidecar` | The `paid-services-sidecar` CLI, the durable state file and the node RPC client. |
| `crates/paid-services-mock` | The mock node and issuer, and the `paid-services-mock` CLI. Used only by tests and the demo. |

Workspace settings follow zakura-core/zakura (main `cb148173`):
- `edition = "2021"`, `rust-version = "1.97"`;
- `license = "MIT OR Apache-2.0"`;
- `[workspace.lints]` copied from Zakura, so `unsafe_code` is denied and `missing_docs`, `print_stdout`, `print_stderr` and `unwrap_in_result` warn.

Dependencies are Zakura's, at Zakura's versions:
- `ed25519-zebra` 4.2 for the issuer signatures;
- Zakura's forks `zakura-address` and `zakura-protocol` 5.4.0, renamed to `zcash_address` and `zcash_protocol` as Zakura does;
- `serde`, `serde_json`, `thiserror`, `hex`, `base64`, `rand_core` 0.6, and `clap` 4 without default features.

The one dependency Zakura does not have is `zakura-zip321` 5.4.0, Zakura's own fork of the ZIP 321 parser from zakura-core/common. It is built on the same forked address crates, so there is no second copy of the upstream address crates.

`serde` and `thiserror` are locked to the versions in Zakura's `Cargo.lock`, so the tree has a single `syn`.

The loopback JSON-RPC client and the mock server use `std`; there is no async runtime or HTTP framework.

### Build and test

```
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

Tests write only under `target/tmp`, each in a new directory, and delete nothing.

The CI workflow (`.github/workflows/ci.yml`) runs fmt, clippy, the workspace tests, an MSRV check and the supply-chain checks:
- **cargo-deny:** `cargo deny --workspace check bans licenses sources`, with Zakura's policy in `deny.toml`.
- **cargo-vet:** `cargo vet check --store-path ./qa/supply-chain/`. It imports the same audits as Zakura, plus Zakura's own audits, with no exemptions.

Neither supply-chain check has been run locally. A crate without an imported audit fails `cargo vet` until an audit, an exemption or a dependency change is decided.

### License

Licensed under either of Apache License, Version 2.0 (`LICENSE-APACHE`) or MIT license (`LICENSE-MIT`), at your option.
