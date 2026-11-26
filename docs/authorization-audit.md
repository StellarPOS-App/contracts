# Authorization audit

Every `#[contractimpl]` entry point in the workspace was checked for the
`require_auth` call that protects it. Soroban only verifies an address when
the contract asks for it, so an entry point that moves funds or changes state
without one is open to any caller.

## Result

| Contract | State-changing entry points | Status |
| --- | --- | --- |
| `payment-processor` | `pay`, `pay_with_fee`, `pay_with_royalties`, `pay_batch` | payer authorizes |
| | `split_payment` | every contributor authorizes their own share |
| | `pay_delegated` | the delegatee authorizes (**fixed**, see below) |
| | `create_delegation`, `revoke_delegation` | delegator authorizes |
| | `refund` | the payment's settlement account authorizes |
| | `set_merchant_callback` | the merchant authorizes (**added**) |
| | `set_fee_bps`, `set_treasury`, `pause`, `unpause`, `set_loyalty`, `enable_governance` | admin authorizes |
| | `propose_param_change`, `approve_param_change` | council member authorizes |
| `merchant-registry` | all | owner or admin authorizes |
| `loyalty-program` | all | admin, merchant or registered processor authorizes |
| `escrow-service` | all except `resolve_by_default` | the acting party authorizes |
| `payment-stream` | all | the acting party authorizes |
| `treasury` | all except `execute` | signer authorizes |

## Deliberately permissionless

- `escrow-service::resolve_by_default` can be called by anyone once the
  arbiter's deadline has passed. The outcome is fixed by the dispute reason,
  so the caller cannot influence it.
- `treasury::execute` can be called by anyone once enough signers have
  approved the proposal. The approvals are the authorization.

## Fixed in `payment-processor`

- `pay_delegated` derived the delegatee from `env.current_contract_address()`
  and never authenticated anyone. It now takes the `delegatee` as an argument
  and calls `delegatee.require_auth()`.
- `set_merchant_callback` did not exist even though `get_merchant_callback`
  and the callback hook did. It is now present and requires the merchant's
  authorization, so nobody else can point a merchant's payments at an
  arbitrary contract.

`entry_points_reject_unauthorized_callers` in the payment-processor tests runs
every state-changing entry point with no authorization and asserts that each
one fails.

## Known limitation

`pay_delegated` still moves tokens out of the delegator's account, and the
token requires the delegator's authorization for that transfer. A delegator
therefore has to authorize the token transfer as a nested call; the spending
limit and expiry are enforced by the processor but do not replace that token
authorization. Making delegation fully off-chain-signature-free needs a token
allowance flow (`approve` plus `transfer_from`), which is out of scope here.
