//! Error codes of the proposed `paidstream_*` contract, shared by the bridge and the mock node.
//!
//! These are prototype values for a proposed contract. They are not Zakura RPCs or Zakura
//! error codes.
//!
//! For `paidstream_requestinvoice` only [`CODE_REJECTED`] is final. Every other code, including
//! codes the bridge does not recognise and standard JSON-RPC codes, leaves the purchase and its
//! request_id in place. `resume` or the next `buy` then retries the same request.

/// The paid-stream service is disabled in the node's config. This says nothing about the purchase.
pub const CODE_DISABLED: i64 = 1;

/// The node could not reach the provider, or the provider did not answer. The provider may or
/// may not have recorded the request, so retry with the same request_id.
pub const CODE_PROVIDER_UNREACHABLE: i64 = 2;

/// The provider definitively refused this request, for example `RequestConflict` or
/// `UnknownService`. Final for this request_id.
pub const CODE_REJECTED: i64 = 3;

/// `paidstream_setgrant`: the node refused the grant.
pub const CODE_INVALID_GRANT: i64 = 4;

/// The mock node's control methods only.
pub const CODE_MOCK_CONTROL: i64 = 9;

/// Whether an error answer to `paidstream_requestinvoice` ends the purchase.
pub fn is_final_invoice_error(code: i64) -> bool {
    code == CODE_REJECTED
}
