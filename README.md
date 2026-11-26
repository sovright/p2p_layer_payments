# p2p_layer_payments

Prototypes for paying for services over Zakura's P2P layer, for example paying in ZEC for access to a provider's stream.

This is early prototype work. Nothing here moves real funds.

## paid-stream-bridge (prototype, simulation only)

> **SIMULATION ONLY.** This bridge talks to a **mock node** and a **mock issuer**.
> - It opens no wallet and starts no payment.
> - It holds no seed, spending key, viewing key or wallet credential.
> - It talks to no real Zakura node.
>
> **Do not open a payment request it prints in a wallet.**
>
> **Wallet expiry support is not solved.** No wallet examined can set a transaction's `nExpiryHeight` from a payment request. This prototype does not work around that.

The prototype explores a standalone bridge between a user's own Zcash wallet and a Zakura node that buys access to a provider's paid stream.

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
paid-stream-bridge --config FILE buy SERVICE        # new purchase, or continue the open one for SERVICE
paid-stream-bridge --config FILE status [PURCHASE]  # ask the provider (through the node) for the status
paid-stream-bridge --config FILE resume             # finish what a lost answer or a restart left open
paid-stream-bridge --config FILE show PURCHASE      # show a still-pending payment request again
```

The mock side:

```
paid-stream-bridge mock-node --dir NEW_DIR [--port PORT] [--serve-secs SECS]
paid-stream-bridge mock-control --config FILE pay PURCHASE_ID [--amount ZAT] [--expiry HEIGHT]
paid-stream-bridge mock-control --config FILE mine BLOCKS
paid-stream-bridge mock-control --config FILE drop-next METHOD
paid-stream-bridge mock-control --config FILE tamper MODE
```

#### Demo

```
paid-stream-bridge mock-node --dir /tmp/psb-demo-1 --serve-secs 600 &
paid-stream-bridge --config /tmp/psb-demo-1/bridge.json buy synthetic-10
paid-stream-bridge mock-control --config /tmp/psb-demo-1/bridge.json pay <purchase_id>
paid-stream-bridge mock-control --config /tmp/psb-demo-1/bridge.json mine 2
paid-stream-bridge --config /tmp/psb-demo-1/bridge.json status
```

`mock-node` refuses to overwrite an existing cookie or config, so each demo uses a new directory.

### What the bridge checks

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
- the provider still reports the purchase as `Pending`;
- the local node's height is below the deadline.

Otherwise it is withheld. Using the local height only to withhold is conservative; it is never used to grant.

**Grant** (signed in the grant domain): checked like the invoice, plus the service, the `purchase_id`, and that the duration equals the quoted access. A second, different grant for the same purchase is refused, and the first is kept.

**State:**
- One JSON file. The `request_id` is written before the invoice request goes out.
- Every write goes to a synced temporary file, which is then renamed over the state file.
- A lock file stops a second process on the same state.
- A corrupt or inconsistent state file is refused and left untouched.

### Proposed contract error codes

These are prototype values, defined in `src/contract.rs` and shared by the bridge and the mock node. They are not Zakura error codes.

| Code | Name | Effect on a `paidstream_requestinvoice` purchase |
|---|---|---|
| 1 | `Disabled` | Kept. The service is off in the node's config. |
| 2 | `ProviderUnreachable` | Kept. The provider may or may not have recorded the request. |
| 3 | `Rejected` | Final. The provider refused the request, for example with `RequestConflict` or `UnknownService`. |
| 4 | `InvalidGrant` | `paidstream_setgrant` only: the node refused the grant. |
| 9 | mock control | Returned only by the mock node's control methods. |

Only code 3 ends a purchase. Every other error answer leaves the purchase in `requested` with its request_id, and so does a missing answer. That covers codes the bridge does not recognise and standard JSON-RPC codes. `resume`, or the next `buy` for the same service, retries the same request. A new request_id is never created for it.

### Known limits and open integration points

- **The deadline rule cannot be guaranteed before payment by a user's own wallet.** ZIP 321 has no expiry field. ZIP 316 Revision 2 address expiry is an option whose wallet support still has to be verified. It is not implemented here.
- **`Pending` does not mean "not paid".** The proposed `GetGrant` status `Pending` also covers a payment still short of its confirmations. Showing the QR code or link again cannot prevent a second payment: `show` only warns. A separate status would be a protocol change.
- **A mock grant install is not stream consumption.** `paidstream_setgrant` on the mock node only records the grant. No node receives or consumes a paid stream.
- **No wallet result reaches the bridge.** QR and deep-link wallets return nothing to the caller, so the bridge cannot detect a lost wallet answer.
- **The real node side does not exist yet:** the three RPCs, the paid-stream P2P service, grant persistence in the node, and capability allocation.

### Build and test

Dependency choice:
- Rust, like Zakura and zecd.
- `ed25519-dalek` for the issuer signatures.
- `zip321`, `zcash_address` and `zcash_protocol`: the reference ZIP 321 parser and the address crates it uses.
- `serde` and `serde_json` for config, state and JSON-RPC.
- `base64`, already pulled in by `zip321`.

There is no async runtime, HTTP framework or argument-parser dependency. The loopback JSON-RPC client and mock server use `std`.

Commands:

```
cargo test
cargo clippy --all-targets -- -D warnings
```

Tests write only under `target/tmp`, each in a new directory, and delete nothing.
