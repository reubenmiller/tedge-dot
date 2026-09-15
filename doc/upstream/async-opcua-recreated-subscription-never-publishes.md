# Draft upstream issue: async-opcua client never publishes for a subscription re-created on reconnect

Target repository: https://github.com/FreeOpcUa/async-opcua (verify the current canonical
repo before filing). Applies to `async-opcua-client` 0.18.0.

---

**Title:** Client: after a reconnect that re-creates subscriptions, no Publish request is ever
sent, so the re-created subscription is silent

**Description**

When the session event loop reconnects and the old subscriptions cannot be transferred (e.g. the
server restarted, or it rejects TransferSubscriptions), `transfer_subscriptions_from_old_session`
re-creates them. The re-creation succeeds (`create_subscription` and `create_monitored_items`
return Good), but the client never sends another Publish request for that session: no data
change is ever delivered, and nothing reports an error. Keep-alive reads on the same session keep
succeeding, so the session looks healthy.

**Cause (from the 0.18.0 source)**

1. `SessionConnector::ensure_and_activate_session` (session/connect.rs) re-creates subscriptions
   during `try_connect`, i.e. *before* the session event loop builds the new
   `SubscriptionEventLoop` for the connected state (session/event_loop.rs, `Reconnected`).
   `create_subscription_inner` calls `trigger_publish_now()`, updating the trigger watch.
2. `SubscriptionEventLoopState::new` (subscriptions/event_loop_state.rs) then starts with
   `no_active_subscription: true` and `last_external_trigger = *trigger_publish_recv.borrow()` --
   the value the re-creation just set, so that trigger counts as already seen.
3. In `tick`, the internal publishing tick only sends a Publish when `!no_active_subscription`,
   and `no_active_subscription` is only cleared by a *newer* external trigger or a successful
   publish response. Neither can happen: the loop never publishes.

Initial subscriptions are not affected, because they are created after the event loop exists, so
their trigger is newer than the one it recorded.

**Reproduction**

1. Connect a client with `session_retry_limit` > 0 and create a subscription with a monitored item.
2. Restart the server (same endpoint) quickly enough for the client's own retries to reconnect.
3. The client logs "Some or all of the existing subscriptions could not be transferred and must be
   created manually", then "Recreating subscription", "create_subscription, created a subscription
   with id ...", "create_monitored_items, 1 items created" (debug).
4. Afterwards, with `opcua_client=debug`, there are no `publish` lines for that session at all
   (measured: 0 publish lines against 9 successful keep-alive reads over ~90 s, while another
   session in the same process published ~once a second), and no data change callback fires.

Seen against python-asyncua (which rejects TransferSubscriptions with BadUserAccessDenied) and
against an in-process async-opcua 0.18 server.

**Suggested fix**

Initialise `last_external_trigger` so a trigger sent before the loop was built is not lost (e.g.
from a timestamp taken before re-creating subscriptions), or start the state with
`no_active_subscription` derived from whether the session already has subscriptions, or trigger a
publish after the connected state (and its subscription loop) is set up.

**Context**

Found in https://github.com/thin-edge/tedge-dot: a device whose points were all delivered by
subscription stayed silent for good after any server restart. The connector now checks push
liveness itself (`check_subscription`: no publish response within the keep-alive window) and
replaces the session. `impl/rust/crates/connector-opcua/tests/integration.rs`
(`check_subscription_reports_push_the_client_did_not_restore`) exercises the case.
