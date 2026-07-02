# Draft upstream issue: async-opcua server can strand a deferred monitored-item sample

Target repository: https://github.com/FreeOpcUa/async-opcua (verify the current canonical
repo before filing). Applies to `async-opcua` 0.18.0, server part.

---

**Title:** Server: monitored-item value deferred by `maybe_enqueue_skipped_value` can be
stranded forever when its rewritten timestamp lands after the drain tick

**Description**

When a value is written to a monitored variable less than one sampling interval after the
previous value's source timestamp, the server defers it via
`MonitoredItem::maybe_enqueue_skipped_value`, rewriting its timestamp to
`last_timestamp + sampling_interval` — a timestamp slightly in the future.

The next subscription tick (`Subscription::tick_monitored_items`) then drains the item from
`notified_monitored_items`. If that tick's `now` is still *before* the rewritten timestamp
(a sub-millisecond window), the deferred value is neither published nor re-queued: the item
has been removed from the notified set, and no later tick re-examines the skipped value. The
write is lost permanently; subsequent notifications are empty keep-alives.

**Reproduction**

Observed deterministically-enough for CI flakes (~40–50% failure at 2 CPUs) with an
in-process server and a client that reacts to each data-change notification by immediately
writing the next value: notifications arrive on the publishing-interval tick, so each
reactive write lands at `last_ts + interval ± ε`, exactly inside the race window. A trace of
the failing runs shows the write entering `maybe_enqueue_skipped_value` and then vanishing —
followed only by keep-alive notifications.

Client workaround: space server-side writes by more than one sampling interval (we sleep
1.5 × interval in our integration test). Real-world clients cannot generally control write
timing, so a server-side fix (re-check `sample_skipped_data_value` on subsequent ticks, or
compare against the pre-rewrite timestamp when draining) seems warranted.

Context: found while testing a client (monitored-item subscription) against the in-process
test server in https://github.com/reubenmiller/tedge-dot
(`crates/connector-opcua/tests/subscription_integration.rs` documents the analysis).
