// ot-alarm/alarm-state: remember which alarms are standing, for ot-alarm.
//
//   in:  te/device/<device>///a/<type>   (every alarm, retained or live)
//   out: nothing
//
// ot-alarm publishes its alarms retained, but it cannot read them back: the mapper drops a flow's
// output to its own input topics to prevent loops. This flow subscribes to the alarm topics in its
// place and records in the mapper-wide store whether each alarm is standing:
//
//   "ot-alarm-retained:<alarm topic>" -> true (raised) / false (cleared)
//
// After a mapper restart the broker replays the retained alarms, so ot-alarm learns that an alarm
// it raised before is still standing and does not raise it again — Cumulocity counts every repeat
// of an active alarm as a new occurrence. A cleared alarm leaves no retained message, so it is not
// learned this way, and ot-alarm settles it with a clear of its own.

export function onMessage(message, context) {
  context.mapper.set(`ot-alarm-retained:${message.topic}`, message.payload.length > 0);
  return [];
}
