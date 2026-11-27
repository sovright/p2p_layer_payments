# Payments that miss the rule: a policy proposal

**Status: proposal only.** Nothing here is implemented, adopted or agreed. The mock issuer records such payments as `Underpaid` or `NeedsReconciliation` and grants nothing. There is no refund code and no real payment.

## Why a rule is needed

A user who pays from their own wallet chooses the transaction's `nExpiryHeight`; a ZIP 321 request cannot set it. The sidecar's offer window narrows when a request is shown, but it cannot stop a late payment. Today such money is received and recorded, and nothing happens to it. The provider still holds the ZEC.

## The cases, kept apart

| Case | What happened | Today (mock) |
|---|---|---|
| A. In time, but the expiry misses the rule | Included at or below the deadline, with `nExpiryHeight` above the deadline or 0 | `NeedsReconciliation` |
| B. Late | Included above the deadline | `NeedsReconciliation` |
| C. Underpaid | Below the invoice amount | `Underpaid` |
| D. A second or extra payment | More than one payment for the same purchase | Recorded, never granted |

## Options for each case

**A. In time, expiry misses the rule.** The money arrived in time; only the transaction's expiry field breaks the rule.
- *Option A1, grant anyway:* a payment included by the deadline qualifies whatever its expiry. This drops the expiry condition, which is the "C" choice discussed in the bridge plan, and lets late payments in, because the expiry was the guard against inclusion after the deadline. The issuer still checks the inclusion height, so case B stays separate.
- *Option A2, keep the rule:* treat as case B.

**B. Late payment.**
- *Option B1, grace window:* grant if included within G blocks after the deadline. G is a configured value. The provider carries the cost of serving a little longer, and the window only moves the cliff.
- *Option B2, refund:* return the payment. See "What a refund needs".
- *Option B3, manual reconciliation:* an operator resolves it out of band. This is today's behaviour, made explicit.

**C. Underpaid.**
- *Option C1, top-up:* a second payment to the same invoice address that completes the amount, within the deadline rule.
- *Option C2, refund* the partial amount.
- *Option C3, manual reconciliation.*

**D. A second or extra payment.** Never a second grant. Refund or manual reconciliation, as in B2 or B3.

## What a refund needs

A refund is not a small addition:
- **A refund address.** A shielded payment does not reveal where it came from, so the buyer must give an address in advance. That means a field in the invoice request, which links the purchase to an address the buyer controls. This has a privacy cost, and the buyer may not want to share an address at all.
- **Fees.** Who pays the refund transaction's fee, and whether small amounts are refunded at all, below some dust limit.
- **Its own idempotency.** A refund is a payment with the same lost-answer and retry problems as the original: a refund id, a durable record, and no second refund after a lost answer.
- **Timing and finality.** When a refund is due: after N confirmations of the original, after the deadline, or after the grace window.
- **Abuse.** Refunds must not let someone probe the provider or move funds through it.

## A possible starting rule, for discussion

These are suggestions for the first real deployment, not decisions:
- **A:** grant (A1) only if the provider accepts a slightly late inclusion risk; otherwise manual reconciliation.
- **B:** a short grace window (B1), then manual reconciliation (B3) instead of automatic refunds.
- **C:** manual reconciliation (C3).
- **D:** manual reconciliation.

Automatic refunds (B2, C2) wait until the refund-address and fee questions are answered.

Whatever is chosen, the purchase status should say which case applied. Received money must stay accountable even where access is not granted.
