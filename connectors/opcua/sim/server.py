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
"""

import asyncio
import os

from asyncua import Server, ua

ENDPOINT_HOST = os.environ.get("OPCUA_ENDPOINT_HOST", "0.0.0.0")
NS_URI = "urn:tedge:opcua-sim"


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

    await plc.add_variable(
        ua.NodeId("Temperature", idx), ua.QualifiedName("Temperature", idx), 21.5
    )
    await plc.add_variable(
        ua.NodeId("Count", idx),
        ua.QualifiedName("Count", idx),
        ua.Variant(617001, ua.VariantType.UInt32),
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
        f"OPC-UA simulator listening on opc.tcp://{ENDPOINT_HOST}:4840/ (namespace idx={idx})",
        flush=True,
    )
    async with server:
        n = 0
        while True:
            await asyncio.sleep(1)
            n += 1
            await ticks.write_value(ua.Variant(n, ua.VariantType.UInt32))


if __name__ == "__main__":
    asyncio.run(main())
