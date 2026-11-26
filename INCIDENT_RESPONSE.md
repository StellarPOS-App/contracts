# Incident Response Runbook

This document outlines the procedures for responding to security incidents, vulnerabilities, or operational emergencies affecting the StellarPOS contract system.

## Table of Contents

1. [Overview](#overview)
2. [Severity Levels](#severity-levels)
3. [Emergency Response Procedures](#emergency-response-procedures)
4. [Contract Pause Procedures](#contract-pause-procedures)
5. [Fund Recovery Procedures](#fund-recovery-procedures)
6. [Communication Procedures](#communication-procedures)
7. [Post-Incident Review](#post-incident-review)

## Overview

The incident response procedures ensure rapid containment and recovery from security or operational issues. All procedures prioritize:

1. **Containment** - Stop the incident from spreading
2. **Preservation** - Keep evidence and logs intact
3. **Recovery** - Restore normal operations
4. **Communication** - Keep stakeholders informed

## Severity Levels

### Critical (P0)
- Active exploit in production
- Large-scale fund theft or loss
- Contract completely unavailable
- **Response Time:** Immediate (< 15 minutes)

### High (P1)
- Significant vulnerability discovered
- Partial fund loss or compromise
- Core functionality degraded
- **Response Time:** < 30 minutes

### Medium (P2)
- Minor vulnerability identified
- Limited financial impact
- Non-critical functionality affected
- **Response Time:** < 2 hours

### Low (P3)
- Potential issue requiring investigation
- Minimal risk
- Documentation or minor operational issues
- **Response Time:** < 24 hours

## Emergency Response Procedures

### Step 1: Incident Detection & Initial Response

1. **Alert the incident response team** via established channels (e.g., Slack emergency channel)
2. **Assign an incident commander** (IC) responsible for coordination
3. **Document the incident** with timestamp, affected contracts, and initial impact assessment
4. **Engage on-call security lead** if severity is P0 or P1

### Step 2: Situation Assessment

The incident commander should:

1. **Gather information:**
   - What was detected? (alerts, user reports, automated monitoring)
   - When did it start? (timestamp)
   - Which contracts are affected?
   - What is the financial impact?

2. **Classify severity** using the severity levels above

3. **Brief the response team:**
   - Current status
   - Suspected root cause
   - Recommended immediate actions

### Step 3: Activate Response Protocol

Based on severity:

- **P0/P1:** Activate full response team and proceed to Contract Pause
- **P2:** Assess containment options; pause if financial risk is significant
- **P3:** Create incident ticket for investigation; escalate if new info changes classification

## Contract Pause Procedures

The payment-processor and payment-stream contracts support emergency pause functionality to prevent new transactions.

### For Payment Processor

#### Pausing All Payments

```bash
# Call the pause function (admin-only)
stellar contract invoke \
  --network testnet \
  --source-account ADMIN_ADDRESS \
  --contract-id PAYMENT_PROCESSOR_CONTRACT_ID \
  -- pause
```

**Effect:** 
- All `pay`, `pay_delegated`, and `pay_batch` calls will immediately fail with `PaymentError::Paused`
- Refunds can still be processed
- Delegation revocation still works
- Recurring payment execution is blocked

#### Unpausing Payments

```bash
stellar contract invoke \
  --network testnet \
  --source-account ADMIN_ADDRESS \
  --contract-id PAYMENT_PROCESSOR_CONTRACT_ID \
  -- unpause
```

### For Payment Stream

#### Pausing All Streams

```bash
stellar contract invoke \
  --network testnet \
  --source-account ADMIN_ADDRESS \
  --contract-id PAYMENT_STREAM_CONTRACT_ID \
  -- pause
```

**Effect:**
- New streams cannot be created
- Recipients cannot withdraw accrued funds
- Either party can still cancel active streams (returns funds)

#### Unpausing Streams

```bash
stellar contract invoke \
  --network testnet \
  --source-account ADMIN_ADDRESS \
  --contract-id PAYMENT_STREAM_CONTRACT_ID \
  -- unpause
```

## Fund Recovery Procedures

### Payment Processor Fund Recovery

If funds are stuck or misrouted:

#### 1. Identify Affected Payments

Query the ledger to find payments with issues:

```bash
# Get payment details
stellar contract invoke \
  --network testnet \
  --contract-id PAYMENT_PROCESSOR_CONTRACT_ID \
  -- get_payment \
  --payment-id PAYMENT_ID
```

#### 2. Process Refunds

The settlement account (merchant's account) can refund affected customers:

```bash
stellar contract invoke \
  --network testnet \
  --source-account SETTLEMENT_ACCOUNT \
  --contract-id PAYMENT_PROCESSOR_CONTRACT_ID \
  -- refund \
  --payment-id PAYMENT_ID \
  --amount REFUND_AMOUNT \
  --reason "Incident recovery: [DESCRIPTION]"
```

#### 3. Settlement Outbound Transfers

Settlement accounts can transfer funds to recovery addresses:

```bash
# Using Stellar CLI to transfer funds from settlement account
stellar account merge \
  --network testnet \
  --source-account SETTLEMENT_ACCOUNT \
  --destination RECOVERY_ADDRESS
```

### Payment Stream Fund Recovery

If stream funds are locked:

#### 1. Identify Affected Streams

```bash
stellar contract invoke \
  --network testnet \
  --contract-id PAYMENT_STREAM_CONTRACT_ID \
  -- get_stream \
  --id STREAM_ID
```

#### 2. Cancel Streams to Release Funds

The payer or recipient can cancel an active stream:

```bash
stellar contract invoke \
  --network testnet \
  --source-account CALLER_ADDRESS \
  --contract-id PAYMENT_STREAM_CONTRACT_ID \
  -- cancel \
  --caller CALLER_ADDRESS \
  --id STREAM_ID
```

This immediately:
- Returns accrued (but not yet withdrawn) funds to the recipient
- Returns remaining unaccrued funds to the payer

#### 3. Recover Funds via Settlement

For unrecoverable stream funds, escalate to blockchain recovery procedures.

## Communication Procedures

### Internal Communication

1. **Immediate:** Notify incident response team and security lead
2. **Within 10 min:** Post incident summary in ops channel
3. **During response:** Provide updates every 15 minutes (P0/P1) or hourly (P2/P3)

### External Communication

**Do not communicate details publicly until incident is resolved and analysis is complete.**

#### For P0/P1 Incidents:

1. **Within 30 min:** Notify affected merchants (email + dashboard notification)
   - Incident has been detected
   - Transactions may be temporarily paused
   - Our team is investigating

2. **Within 2 hours:** Provide status update
   - Action taken (e.g., contract paused)
   - Estimated time to resolution
   - What merchants should do

3. **Post-resolution:** Send detailed communication
   - What happened
   - Impact and scope
   - Mitigations implemented
   - Follow-up improvements planned

#### For P2/P3 Incidents:

- P2: Notify affected parties within 24 hours
- P3: Include in next status page update or incident retrospective

### Message Templates

#### P0/P1 Merchant Alert (Initial)

```
Subject: [URGENT] StellarPOS Service Incident

We've detected an operational issue affecting payment processing.

Status: Under investigation
Impact: [Payment processing may be temporarily paused]
Duration: [Expected resolution time if known]

Your merchant account is [affected/not affected].

We will provide updates every 15 minutes. Please do not retry failed transactions.

https://status.stellarpos.app - Real-time updates
```

#### All-Clear Message

```
Subject: [RESOLVED] StellarPOS Service Incident

The incident has been resolved.

Timeline:
- Detected: [TIME]
- Paused: [TIME]
- Root Cause: [BRIEF DESCRIPTION]
- Resumed: [TIME]

Actions taken:
- [Mitigation 1]
- [Mitigation 2]

We've implemented [improvements] to prevent this in the future.

For details: [link to incident report when available]
```

## Post-Incident Review

### Immediate (Within 1 hour of resolution)

- [ ] Verify all systems are functioning normally
- [ ] Confirm no data loss or integrity issues
- [ ] Collect logs and evidence
- [ ] Document timeline and actions taken

### Short-term (Within 24 hours)

Conduct incident retrospective:

1. **What happened?** - Factual description of the incident
2. **Why did it happen?** - Root cause analysis
3. **What did we do?** - Chronological log of response actions
4. **What worked well?** - Positive aspects of response
5. **What can we improve?** - Gaps in process or tooling

### Medium-term (Within 1 week)

1. **Implement fixes:** Deploy code/config changes to prevent recurrence
2. **Update safeguards:** Improve monitoring, alerting, or automated responses
3. **Share findings:** Publish sanitized incident report to team
4. **Training:** Update incident response procedures based on lessons learned

### Long-term

- Monitor for similar issues
- Plan for systematic improvements discovered during the incident
- Schedule incident response drills quarterly

## Contacts & Escalation

| Role | Contact | On-Call Schedule |
|------|---------|------------------|
| Incident Commander (rotation) | [Team Slack] | On-call rotation |
| Security Lead | [Name/Email] | 24/7 emergency |
| Product Manager | [Name/Email] | Business hours |
| Merchant Support Lead | [Name/Email] | Business hours |
| Blockchain Ops | [Name/Email] | Business hours |

## Related Resources

- [Contract Security Considerations](SECURITY.md)
- [Deployment Procedures](docs/deployment.md)
- [Monitoring & Alerts](docs/monitoring.md)
- [Contributing Guidelines](CONTRIBUTING.md)
