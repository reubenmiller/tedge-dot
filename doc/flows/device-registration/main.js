// device-registration: register an OT device as a thin-edge child device the first time the
// connector reports a link for it.
//
// Replaces the legacy reader.py register_child_devices(). The connector only reports link
// status per device; this flow turns the first sighting into a retained registration message
// on te/device/<device>// so the cloud mappers create the child device.

const decoder = new TextDecoder();

export function onMessage(message, context) {
  // Topic shape: te/device/<device>/ot/<protocol>/status/link
  const parts = message.topic.split("/");
  const device = parts[2];
  const protocol = parts[4];
  if (!device) return [];

  const { device_type = "ot-device" } = context.config;

  // Register each device only once per mapper lifetime.
  const key = `registered:${device}`;
  if (context.mapper.get(key)) return [];
  context.mapper.set(key, true);

  // Retained registration message on the device's root topic.
  return [{
    topic: `te/device/${device}//`,
    payload: JSON.stringify({
      "@type": "child-device",
      name: device,
      type: device_type,
      // surface which OT protocol this device is reached through
      "ot-protocol": protocol,
    }),
    mqtt: { retain: true, qos: 1 },
  }];
}
