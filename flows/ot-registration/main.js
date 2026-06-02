// ot-registration: register an OT device as a thin-edge.io child device the first time the
// connector reports its link as connected.
//
// Direction: OT protocol format -> thin-edge.io data model.
//   in:  te/device/<device>/ot/<protocol>/status/link  (connector link status)
//   out: te/device/<device>//                          (retained child-device registration)
//
// Protocol-neutral: works for any connector. The connector only reports link status per device;
// this flow turns the first "connected" sighting into a retained registration so the cloud
// mappers create the child device.

const decoder = new TextDecoder();

export function onMessage(message, context) {
  // Topic shape: te/device/<device>/ot/<protocol>/status/link
  const parts = message.topic.split("/");
  const device = parts[2];
  const protocol = parts[4];
  if (!device || device === "main") return []; // never register the main device as a child

  let status = "connected";
  let info = null;
  try {
    const payload = JSON.parse(decoder.decode(message.payload));
    if (typeof payload?.status === "string") status = payload.status;
    if (payload && typeof payload.info === "object" && payload.info !== null) info = payload.info;
  } catch (_e) {
    // tolerate empty/non-JSON payloads
  }
  if (status !== "connected") return []; // only register on a healthy link

  const deviceType = context.config?.device_type || `${protocol}-device`;

  // Register each device only once per mapper lifetime.
  const key = `registered:${device}`;
  if (context.mapper.get(key)) return [];
  context.mapper.set(key, true);

  const out = [{
    topic: `te/device/${device}//`,
    payload: JSON.stringify({
      "@type": "child-device",
      name: device,
      type: deviceType,
      "ot-protocol": protocol,
    }),
    mqtt: { retain: true, qos: 1 },
  }];

  // Advertise the generic OT command capabilities on the device so the cloud mapper routes the
  // matching operations (e.g. ot_write backs c8y_SetRegister / c8y_SetCoil). Each capability is a
  // retained empty message on te/device/<device>///cmd/<type>.
  const caps = context.config?.command_capabilities || "ot_write";
  for (const cap of String(caps).split(",").map((c) => c.trim()).filter((c) => c)) {
    out.push({
      topic: `te/device/${device}///cmd/${cap}`,
      payload: "{}",
      mqtt: { retain: true, qos: 1 },
    });
  }

  // Optionally publish the connector's device descriptor as a digital-twin fragment
  // (parity with the legacy c8y_ModbusDevice twin). Opt-in via params.twin_fragment.
  const twinFragment = context.config?.twin_fragment || "";
  if (twinFragment && info) {
    out.push({
      topic: `te/device/${device}///twin/${twinFragment}`,
      payload: JSON.stringify(info),
      mqtt: { retain: true, qos: 1 },
    });
  }

  return out;
}
