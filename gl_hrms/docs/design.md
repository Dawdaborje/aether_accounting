# Ledger entries from HR (`gl_hrms`)

## Purpose

Thin bridge: HR announces payroll and expense approvals; this plugin hands each event to `gl.on_posting_event`. The ledger and HR plugins stay unaware of each other. Install only when both accounting and the HR plugins are in use.

## Dependencies

| Dependency | Why |
|---|---|
| `gl` | Posts via `plugins::call("gl", "on_posting_event", …)` |
| `hr_payroll` | Emits `hr_payroll.payroll_run_approved` |
| `hr_expense` | Emits `hr_expense.report_approved` |

Capabilities: `events::subscribe`, `plugins::call`. No DB access.

## Models

None. No `access_models`, no local tables. Chart, journals, periods, and posting rules live in `gl`.

## Design

- **Event-driven only**: listen → forward; no transformation beyond packaging the `Received` envelope for `gl`.
- **Posting rules in `gl`**: an administrator creates versioned `gl_posting_rule` rows for each event (accounts, journals, amount paths). Until a rule exists, delivery fails and retries.
- **Idempotency**: owned by `gl` (`idem_key` from event + document + optional document version).
- **No reverse dependency**: HR never imports accounting; `gl` never imports HR.

## Functions, commands, events

**Handlers**

| Function | Role |
|---|---|
| `forward` | Kernel-delivered listener; calls `gl.on_posting_event` with `event`, `source`, `payload`, `emitted_by`, `depth` |

**Listen** (declared in `plugin.toml`)

| Event | Source plugin |
|---|---|
| `hr_payroll.payroll_run_approved` | `hr_payroll` |
| `hr_expense.report_approved` | `hr_expense` |

**Emit**: none.

No scheduled jobs, no app route, no roles of its own.

## Roles

None. Authorization for resulting entries is whatever `gl` enforces when posting from an event (kernel context, not a member).

## Boundaries

- Does not create accounts, journals, or posting rules; that is `gl` admin work.
- Does not validate HR payloads beyond what `gl` posting rules require.
- Not a substitute for installing `hr_payroll` / `hr_expense`; those must emit the named events.
