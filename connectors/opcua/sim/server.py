"""OPC-UA simulator for the tedge-dot e2e harness.

Exposes a handful of nodes with stable string NodeIds under namespace index 2
(urn:tedge:opcua-sim) so the connector can address them as `ns=2;s=<name>`:

    ns=2;s=Temperature   Double  21.5            (read)
    ns=2;s=Count         UInt32  617001          (read)
    ns=2;s=Setpoint      Int32   0   (writable)  (read/write round-trip)
    ns=2;s=Running       Boolean false (writable)(read/write round-trip)
    ns=2;s=Ticks         UInt32  incremented every second (subscription/push tests)

Reading ns=2;s=DoesNotExist yields a Bad status, exercising bad-quality handling.

The endpoint host is taken from OPCUA_ENDPOINT_HOST so the advertised endpoint URL
matches the docker service name (avoids OPC-UA hostname-rewrite connection failures).

OPCUA_SIM_DYNAMIC=1 makes Temperature drift (21.5 +/- 2.5 over a 5 minute cycle) and
Count increment every second. The demo stacks enable it: a subscription only notifies
on change, so with static values a push-delivered device goes silent after its first
notification and the cloud marks it unavailable. Off by default because the e2e and
smoke tests assert the static values above.
"""

import asyncio
import math
import os

from asyncua import Server, ua

ENDPOINT_HOST = os.environ.get("OPCUA_ENDPOINT_HOST", "0.0.0.0")
DYNAMIC = os.environ.get("OPCUA_SIM_DYNAMIC", "").strip().lower() in ("1", "true", "yes", "on")
NS_URI = "urn:tedge:opcua-sim"

TEMPERATURE = 21.5
COUNT = 617001


async def main():
    server = Server()
    await server.init()
    server.set_endpoint(f"opc.tcp://{ENDPOINT_HOST}:4840/")
    server.set_server_name("tedge OPC-UA simulator")
    server.set_security_policy([ua.SecurityPolicyType.NoSecurity])

    idx = await server.register_namespace(NS_URI)
    plc = await server.nodes.objects.add_object(
        ua.NodeId("Plc", idx), ua.QualifiedName("Plc", idx)
    )

    temperature = await plc.add_variable(
        ua.NodeId("Temperature", idx), ua.QualifiedName("Temperature", idx), TEMPERATURE
    )
    count = await plc.add_variable(
        ua.NodeId("Count", idx),
        ua.QualifiedName("Count", idx),
        ua.Variant(COUNT, ua.VariantType.UInt32),
    )
    setpoint = await plc.add_variable(
        ua.NodeId("Setpoint", idx),
        ua.QualifiedName("Setpoint", idx),
        ua.Variant(0, ua.VariantType.Int32),
    )
    running = await plc.add_variable(
        ua.NodeId("Running", idx), ua.QualifiedName("Running", idx), False
    )
    await setpoint.set_writable()
    await running.set_writable()

    # Changes every second so OPC-UA subscriptions (monitored items) have data-change
    # notifications to deliver; the static nodes above only ever notify once.
    ticks = await plc.add_variable(
        ua.NodeId("Ticks", idx),
        ua.QualifiedName("Ticks", idx),
        ua.Variant(0, ua.VariantType.UInt32),
    )

    print(
        f"OPC-UA simulator listening on opc.tcp://{ENDPOINT_HOST}:4840/ "
        f"(namespace idx={idx}, dynamic={DYNAMIC})",
        flush=True,
    )
    async with server:
        n = 0
        while True:
            await asyncio.sleep(1)
            n += 1
            await ticks.write_value(ua.Variant(n, ua.VariantType.UInt32))
            if DYNAMIC:
                drift = 2.5 * math.sin(2 * math.pi * n / 300)
                await temperature.write_value(round(TEMPERATURE + drift, 2))
                await count.write_value(
                    ua.Variant((COUNT + n) & 0xFFFFFFFF, ua.VariantType.UInt32)
                )


if __name__ == "__main__":
    asyncio.run(main())
