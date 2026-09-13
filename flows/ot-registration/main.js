// ot-registration: register an OT device as a thin-edge.io child device the first time the
// connector reports its link as connected.
//
// Direction: OT protocol format -> thin-edge.io data model.
//   in:  te/device/<device>/ot/<protocol>/status/link  (connector link status: trigger + type)
//        te/device/<device>/ot/<protocol>/manifest     (device manifest: the descriptor `info`)
//   out: te/device/<device>//                          (retained child-device registration)
//        te/device/<device>///twin/<fragment>          (optional: the device descriptor)
//
// Protocol-neutral: works for any connector. This flow turns the first "connected" sighting of
// a device's link into a retained registration so the cloud mappers create the child device.
// The type comes from the link status itself: it is one retained message per device and is
// replayed with the manifest in no defined order after a mapper restart, and a registration
// happens once — so the trigger and the type travel together, and only the descriptor, which
// can arrive late without harm, is taken from the manifest.

const decoder = new TextDecoder();

export function onMessage(message, context) {
  // Topic shape: te/device/<device>/ot/<protocol>/status/link
  const parts = message.topic.split("/");
  const device = parts[2];
  const protocol = parts[4];
  if (!device || device === "main") return []; // never register the main device as a child

  const twinFragment = context.config?.twin_fragment || "";
  if (parts[5] === "manifest") {
    // Two things are taken from the manifest, both idempotent and both safe to see late:
    //   * `info`, the connector's device descriptor, as an optional twin fragment;
    //   * `commands`, the thin-edge command types the device answers — which this flow
    //     remembers so a registration advertises what the connector actually implements
    //     instead of a hard-coded list.
    let manifest = null;
    try {
      manifest = JSON.parse(decoder.decode(message.payload));
    } catch (_e) {
      return []; // a clearing message: the device is gone
    }
    if (Array.isArray(manifest?.commands)) {
      context.mapper.set(`ot-commands:${device}`, manifest.commands);
    }
    const info = manifest && typeof manifest.info === "object" && manifest.info !== null
      ? manifest.info
      : null;
    if (!twinFragment || !info) return [];
    return [{
      topic: `te/device/${device}///twin/${twinFragment}`,
      payload: JSON.stringify(info),
      mqtt: { retain: true, qos: 1 },
    }];
  }

  let status = "connected";
  let declaredType = null;
  try {
    const payload = JSON.parse(decoder.decode(message.payload));
    if (typeof payload?.status === "string") status = payload.status;
    // The device type the connector was configured with (contract §3.1) — what the points
    // describe, which is a far better entity type than "<protocol>-device".
    if (typeof payload?.type === "string" && payload.type) declaredType = payload.type;
  } catch (_e) {
    // tolerate empty/non-JSON payloads
  }
  if (status !== "connected") return []; // only register on a healthy link

  const deviceType = declaredType || context.config?.device_type || `${protocol}-device`;

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

  // Advertise the command capabilities on the device so the cloud mapper routes the matching
  // operations (ot_write backs c8y_SetRegister, ot_write_coil backs c8y_SetCoil,
  // parameter_update backs c8y_ParameterUpdate through the tedge-parameter-plugin's template).
  // Each capability is a retained empty message on te/device/<device>///cmd/<type>.
  //
  // The connector publishes the markers for the types IT answers too (contract §6.6). Both
  // doing it is harmless — the message is identical and retained — and this flow keeps doing
  // it because the connector may publish before the child-device registration exists, and
  // whether the c8y mapper holds such a message or drops it is open question 12.6. What this
  // flow adds beyond the connector's list is `parameter_update`, which no connector answers:
  // it is bridged by ot-parameter-update.
  const fromManifest = context.mapper.get(`ot-commands:${device}`);
  const caps = Array.isArray(fromManifest) && fromManifest.length
    ? fromManifest.join(",") + "," + (context.config?.command_capabilities || "parameter_update")
    : context.config?.command_capabilities || "ot_write,ot_write_coil,parameter_update";
  const seen = new Set();
  for (const cap of String(caps).split(",").map((c) => c.trim()).filter((c) => c && !seen.has(c) && seen.add(c))) {
    out.push({
      topic: `te/device/${device}///cmd/${cap}`,
      payload: "{}",
      mqtt: { retain: true, qos: 1 },
    });
  }

  return out;
}
