// ot-parameter-state: keep the device twin's parameter sets in sync with the connector.
//
// Direction: OT protocol format -> thin-edge.io data model.
//   in:  te/device/<device>/ot/<protocol>/sample/<point>         (reads of parameter points)
//        te/device/<device>/ot/<protocol>/cmd/write/<id>         (single write results)
//        te/device/<device>/ot/<protocol>/cmd/write-batch/<id>   (batch write results)
//        te/device/<device>/ot/<protocol>/status/link            (retained: type and point list)
//        te/device/main/service/<service>/ot/capabilities        (retained: parameter_keys)
//   out: te/device/<device>///twin/<set>                         (retained: { <key>: value })
//
// A *parameter* is a point whose `access` (echoed in every sample) permits writes, or that opts
// in via meta.parameter (meta.parameter = false opts a writable point out). Parameters are
// grouped into *sets*; each set is one twin fragment — the same sets `tedge-dot describe`
// declares in the cloud, which is why the naming rule below has to match the SDK's
// (impl/rust/crates/sdk/src/descriptor.rs, impl/c/sdk/src/descriptor.c):
//
//   <device type, else the protocol>_<meta.parameter.group, default "control">_parameters
//
// `group` (and `set`) may be a list, so one point can belong to several sets — operators group
// signals by what they are for — and its value is published to each of their fragments.
//
// The device type is echoed in every sample and on the link status (contract §3.1/§5), so the
// flow never needs the connector's configuration file. meta.parameter.set bypasses the rule and
// is used verbatim.
//
// Inside a set, a point's value is published under its *key*: the point id, or
// meta.parameter.key when the point names one — so a point can keep an id that is unique on the
// device (`firmwareVersion`) and still be `version` in its `firmware` fragment:
// `key = "firmware.version"` names the set and the key at once, and a key without a dot stays in the
// point's usual set. The key comes from
// the connector's retained capability descriptor (parameter_keys, contract §7), so it is known
// before the point samples — right after a mapper restart, and for a write-only point, which
// never samples — and from the point's samples. This flow records which point each key of a set
// belongs to, and ot-command-forward uses that to turn an edit of the fragment back into point
// writes. Two points of a device sharing a key in a set is a configuration `describe` refuses;
// here the first to claim the key keeps it. Once that point leaves the set or is removed, the key is
// free, and the other point takes it at its own next sample, write, descriptor or link status.
//
// Where values come from:
//   * readable parameters: every good sample (so the twin follows the device, including
//     changes made locally on the PLC/HMI);
//   * write-only parameters (access = "write"): the last acknowledged write — the device cannot
//     be read back, so this is the *commanded* state, not a measured one. It is unknown (absent
//     from the twin) until the first successful write after the mapper started, and it lands in
//     the sets its declared key names, else in the default set (no sample ever tells us its
//     meta);
//   * read/write parameters are also updated optimistically from a successful write, then
//     confirmed/corrected by the next sample.
//
// Where values go away — a fragment carrying a point the device no longer has is not just
// untidy: Cumulocity sends the whole fragment back with an operator's edit, so one stale key
// fails every parameter update of that set:
//   * a point removed from the configuration: the link status lists the device's configured
//     points (contract §8) and is republished whenever the configuration changes, so every
//     point missing from it is dropped from each set — and releases every key it claimed, with or
//     without a value — and a retained write result replayed for it afterwards is ignored. The
//     lists are kept per protocol and a point stays while any of them has it; while one protocol
//     serving the device has not listed its points (a connector that predates the list), nothing
//     is dropped for that device at all;
//   * a point that left a set (its group, its type or its access changed): its next sample names
//     the sets it is in now, and it is dropped from the others;
//   * a point whose key changed: its next description (sample or descriptor) drops the old key
//     from every set;
//   * a set left with no points is cleared (an empty retained message) rather than published
//     as `{}`, which removes the fragment instead of keeping an empty one.
//
// Shared state (context.mapper):
//   "ot-protocol:<device>"                -> protocol segment seen for the device
//                                            (read by ot-command-forward)
//   "ot-device-type:<device>"             -> declared device type, when the connector reports one
//   "ot-parameter-values:<device>:<set>"  -> { <key>: value }
//   "ot-parameter-sets:<device>"          -> [set names] this flow has put values in
//   "ot-parameter-set:<device>:<point>"   -> [set names], or false for opted-out points
//                                            (from the point's samples or declared key, else from
//                                            the parameter_update request that wrote it)
//   "ot-parameter-key:<device>:<point>"   -> the point's key (absent: its id)
//   "ot-parameter-point:<device>:<set>:<key>" -> the point that key of the set belongs to
//                                            (read by ot-command-forward)
//   "ot-parameter-claims:<device>:<point>" -> [[set, key]] the point holds
//   "ot-parameter-claimants:<device>"     -> [points] that have claimed a key
//   "ot-parameter-declared:<device>:<protocol>" -> [{point, key, set, group}] from the descriptor
//   "ot-parameter-declaring:<service>"    -> [devices] that service's descriptor declared keys for
//   "ot-parameter-protocols:<device>"     -> [protocols] whose connector sampled or reported the
//                                            device (a device name is only unique per connector)
//   "ot-parameter-points:<device>:<protocol>" -> [point ids] that protocol's connector has
//                                            configured on the device, from its link status;
//                                            null while it has not listed them

const decoder = new TextDecoder();

function canWrite(access) {
  const a = String(access || "read").toLowerCase();
  return a === "write" || a === "read_write" || a === "readwrite";
}

const DEFAULT_GROUP = "control";

// Every RUN of characters outside [A-Za-z0-9] becomes a single "_", so a device type can be
// written the way it reads ("acme-meter-v2") and still be a valid fragment key. A run rather
// than a character because the C SDK folds bytes and this folds characters: collapsing runs is
// what makes them agree on a name with a non-ASCII character in it.
function sanitize(s) {
  return String(s).replace(/[^A-Za-z0-9]+/g, "_");
}

// How this device's sets are named: by its declared type, else by the protocol. `forced` is the
// flow's default_set (and `tedge-dot describe --set`): one name for every point that does not
// give an absolute one.
// Trimmed with the SDKs' definition of whitespace (C's isspace, which `tedge-dot describe`
// applies to --set) rather than JS's Unicode-aware trim: the flow and the CLI must agree on
// what a blank `default_set` is, and on the exact spelling of a padded one.
function trimC(s) {
  return String(s).replace(/^[ \t\n\v\f\r]+|[ \t\n\v\f\r]+$/g, "");
}

function naming(context, device, protocol) {
  return {
    forced: trimC(context.config?.default_set || ""),
    qualifier: context.mapper.get(`ot-device-type:${device}`) || protocol,
  };
}

// The set for a point in `group` (undefined = the default group).
// Sanitized as a whole, so a qualifier that already ends in a separator does not produce a
// doubled "_" — the SDKs assemble the name the same way.
function setFor(names, group) {
  if (names.forced) return names.forced;
  return sanitize(`${names.qualifier}_${group || DEFAULT_GROUP}_parameters`);
}

// A set name becomes BOTH a twin fragment key and a segment of the topic it is published on,
// so it must be a plain identifier — the same rule `tedge-dot describe` refuses to render
// without (descriptor.rs::is_valid_key). Enforced here because an absolute set name can come
// from outside the device: `origin.set` is derived from the Cumulocity operation fragment the
// cloud sent. Unchecked, `#` or `+` would make an illegal PUBLISH topic and a name with `/`
// would publish outside te/<device>///twin/.
function isValidSet(name) {
  return /^[A-Za-z0-9_]+$/.test(name);
}

// The names a `set`/`group` option holds: one string, or an array of them. Empty, non-string
// and unusable entries are ignored, so a mistyped entry degrades to the default group rather
// than inventing a set name. Mirrors descriptor.rs::names_of / descriptor.c::names_of.
function namesOf(value, validate) {
  const out = [];
  for (const name of Array.isArray(value) ? value : [value]) {
    if (typeof name !== "string" || !name || out.includes(name)) continue;
    if (validate && !isValidSet(name)) continue;
    out.push(name);
  }
  return out;
}

// EVERY set a point belongs to: `set` and `group` each accept a string or a list, so one point
// can appear on several operator screens and its value reaches each of their fragments.
// `set` is absolute and wins over `group`. Mirrors SetNaming::sets_of in both SDKs.
function setsOf(options, names) {
  // A key that names its set (`key = "<set>.<key>"`) is absolute, like `set`, and wins; an
  // unusable set in it falls back like an unusable `set` does.
  const dotted = dottedKey(options.key);
  if (dotted && isValidSet(dotted.set)) return [dotted.set];
  // An absolute name is used verbatim, so it is the one that has to be checked; a group name
  // is folded into a derived name and cannot produce anything but [A-Za-z0-9_].
  const absolute = namesOf(options.set, true);
  if (absolute.length) return absolute;
  if (names.forced) return [names.forced];
  const groups = namesOf(options.group);
  if (!groups.length) return [setFor(names)];
  // Deduped on the resulting names: two group names can fold to the same set name.
  const sets = [];
  for (const group of groups) {
    const set = setFor(names, group);
    if (!sets.includes(set)) sets.push(set);
  }
  return sets;
}

// The sets a sampled point belongs to (false when it is not a parameter).
function setsFromSample(sample, names) {
  const mp = sample.meta?.parameter;
  if (mp === false) return false;
  // A bare string names the set, absolutely.
  if (typeof mp === "string" && mp) return isValidSet(mp) ? [mp] : [setFor(names)];
  if (mp && typeof mp === "object" && !Array.isArray(mp)) return setsOf(mp, names);
  if (mp === undefined || mp === null) return canWrite(sample.access) ? [setFor(names)] : false;
  return [setFor(names)]; // `true`, or any other scalar
}

// A key that names its set too, `<set>.<key>`: both halves, split at its only dot, or null for a
// plain key — and for one with more dots or an empty half. A dot can only be this separator:
// neither a set name nor a key may contain one. Mirrors descriptor.rs::dotted_key.
function dottedKey(key) {
  if (typeof key !== "string") return null;
  const dot = key.indexOf(".");
  if (dot <= 0 || dot === key.length - 1 || key.indexOf(".", dot + 1) !== -1) return null;
  return { set: key.slice(0, dot), key: key.slice(dot + 1) };
}

// The key a `key` option publishes under — its part after the set, for a key naming its set — or
// null when it is unusable. A key is a fragment key, so a set name's rule applies.
function keyName(key) {
  if (typeof key !== "string") return null;
  const dotted = dottedKey(key);
  const name = dotted ? dotted.key : key;
  return isValidSet(name) ? name : null;
}

// The key a sampled point is published under: meta.parameter.key when it names a usable one,
// else the point id. An unusable key falls back to the id rather than inventing one, and
// `tedge-dot describe` refuses it.
function keyFromSample(sample, point) {
  const mp = sample.meta?.parameter;
  const key = mp && typeof mp === "object" && !Array.isArray(mp) ? keyName(mp.key) : null;
  return key ?? point;
}

// The key a point was last seen under: its id until a sample or its declaration names another.
function keyOf(context, device, point) {
  const key = context.mapper.get(`ot-parameter-key:${device}:${point}`);
  return typeof key === "string" && key ? key : point;
}

// The point a key of a set belongs to, or null when no point has claimed it.
function ownerOf(context, device, set, key) {
  const owner = context.mapper.get(`ot-parameter-point:${device}:${set}:${key}`);
  return typeof owner === "string" && owner ? owner : null;
}

// The [set, key] pairs a point holds.
function claimsOf(context, device, point) {
  const claims = context.mapper.get(`ot-parameter-claims:${device}:${point}`);
  return Array.isArray(claims) ? claims : [];
}

// Claim `key` of `set` for `point`, unless another point holds it (the first keeps it). The claim
// is also recorded against the point, because it is made before the point's first good reading:
// pruning the point must release it even when the point never put a value in the set.
function claim(context, device, set, key, point) {
  const owner = ownerOf(context, device, set, key);
  if (owner && owner !== point) return false;
  if (owner) return true;
  context.mapper.set(`ot-parameter-point:${device}:${set}:${key}`, point);
  const claims = claimsOf(context, device, point);
  context.mapper.set(`ot-parameter-claims:${device}:${point}`, [...claims, [set, key]]);
  const claimants = context.mapper.get(`ot-parameter-claimants:${device}`) || [];
  if (!claimants.includes(point)) {
    context.mapper.set(`ot-parameter-claimants:${device}`, [...claimants, point]);
  }
  return true;
}

// The recorded sets of a point as a list. Tolerates the pre-list shape (a bare set name) in case
// state outlives a flow upgrade; `false` (opted out) and unknown are both no sets.
function recordedSets(known) {
  if (typeof known === "string") return [known];
  return Array.isArray(known) ? known : [];
}

// The points a write/write-batch message names — the `writes` of a request, the `results` of a
// terminal transition, or the single `point` of a `write` in any of its states.
function writtenPoints(verb, payload) {
  if (verb === "write") return typeof payload.point === "string" ? [payload.point] : [];
  if (verb === "write-batch") {
    const listed = payload.writes ?? payload.results ?? [];
    return listed.map((w) => w?.point).filter((p) => typeof p === "string" && p);
  }
  return [];
}

// Apply {point: value} updates for a device, adding every set it changed to `changed`.
// A point in several sets updates each of them, so the groups never disagree about its value.
function applyValues(context, device, updates, resolveSets, changed) {
  for (const [point, value] of Object.entries(updates)) {
    if (value === undefined) continue;
    const sets = resolveSets(point);
    if (!sets) continue;
    const key = keyOf(context, device, point);
    for (const set of sets) {
      if (!claim(context, device, set, key, point)) continue;
      const valuesKey = `ot-parameter-values:${device}:${set}`;
      const values = context.mapper.get(valuesKey) || {};
      if (JSON.stringify(values[key]) === JSON.stringify(value)) continue;
      values[key] = value;
      context.mapper.set(valuesKey, values);
      changed.add(set);
      const known = context.mapper.get(`ot-parameter-sets:${device}`) || [];
      if (!known.includes(set)) context.mapper.set(`ot-parameter-sets:${device}`, [...known, set]);
    }
  }
}

// Remove `point`'s `key` from each of `sets` where it holds that key — its value and its claim —
// adding every set it changed to `changed`. A key with no recorded owner is taken to be its
// point's id.
function dropValue(context, device, point, key, sets, changed) {
  for (const set of sets) {
    if ((ownerOf(context, device, set, key) ?? key) !== point) continue;
    context.mapper.set(`ot-parameter-point:${device}:${set}:${key}`, null);
    const claims = claimsOf(context, device, point);
    const remaining = claims.filter(([s, k]) => s !== set || k !== key);
    if (remaining.length !== claims.length) {
      context.mapper.set(`ot-parameter-claims:${device}:${point}`, remaining);
    }
    const valuesKey = `ot-parameter-values:${device}:${set}`;
    const values = context.mapper.get(valuesKey);
    if (!values || !Object.prototype.hasOwnProperty.call(values, key)) continue;
    delete values[key];
    context.mapper.set(valuesKey, values);
    changed.add(set);
  }
}

// Put `point` where its latest description (a sample, or its declared key) says it belongs: it
// leaves every set it was in and is not in any more, and when its key changed it leaves the old
// key of every set; then it claims its key in each of its sets — before any value arrives, so an
// edit of the fragment can already reach it. `sets` is false for a point that is not a parameter.
function placePoint(context, device, point, sets, key, changed) {
  context.mapper.set(`ot-parameter-set:${device}:${point}`, sets);
  const current = sets || [];
  // What the point leaves is worked out from what it HOLDS — its claims — not from its recorded
  // sets: those can be unknown (a write-only point written without origin.set landed in the
  // default set), and a key the point no longer has must leave every set it is in.
  for (const [set, held] of claimsOf(context, device, point)) {
    if (held === key && current.includes(set)) continue;
    dropValue(context, device, point, held, [set], changed);
  }
  context.mapper.set(`ot-parameter-key:${device}:${point}`, key);
  for (const set of current) claim(context, device, set, key, point);
}

// Place the points a connector's descriptor declares keys for on `device` (parameter_keys,
// contract §7). Re-applied when the link status reports the device type, which names the sets.
function applyDeclarations(context, device, protocol, changed) {
  const names = naming(context, device, protocol);
  const entries = [];
  for (const e of context.mapper.get(`ot-parameter-declared:${device}:${protocol}`) || []) {
    const key = keyName(e?.key);
    if (typeof e?.point !== "string" || !e.point || !key) continue;
    entries.push({ point: e.point, key, options: { set: e.set, group: e.group, key: e.key } });
  }
  // Two passes: every point whose key changed lets go of what it holds first, so keys swapped
  // between points by one reload are free by the time they are claimed again.
  for (const { point, key } of entries) {
    if (keyOf(context, device, point) === key) continue;
    for (const [set, held] of claimsOf(context, device, point)) {
      dropValue(context, device, point, held, [set], changed);
    }
  }
  for (const entry of entries) {
    placePoint(context, device, entry.point, setsOf(entry.options, names), entry.key, changed);
  }
}

// The twin messages of the changed sets. A set with nothing left in it is cleared with an empty
// retained message, which removes the fragment, rather than published as an empty object.
function twinMessages(context, device, changed) {
  return [...changed].map((set) => {
    const values = context.mapper.get(`ot-parameter-values:${device}:${set}`) || {};
    return {
      topic: `te/device/${device}///twin/${set}`,
      payload: Object.keys(values).length ? JSON.stringify(values) : "",
      mqtt: { retain: true, qos: 1 },
    };
  });
}

// Every point configured on the device, across the protocols serving it — or null while one of
// them has not listed its points, since a point of that connector cannot be told from a removed
// one.
function configuredPoints(context, device) {
  const configured = [];
  for (const protocol of context.mapper.get(`ot-parameter-protocols:${device}`) || []) {
    const points = context.mapper.get(`ot-parameter-points:${device}:${protocol}`);
    if (!Array.isArray(points)) return null;
    configured.push(...points);
  }
  return configured;
}

// Forget what was recorded for a removed point, so a point later added back under the same id
// starts from its own samples again.
function forgetPoint(context, device, point) {
  context.mapper.set(`ot-parameter-set:${device}:${point}`, null);
  context.mapper.set(`ot-parameter-key:${device}:${point}`, null);
}

// Drop every point the device no longer has from every set — its values, and every key it
// claimed, with or without a value — adding every set it changed to `changed`.
function pruneRemovedPoints(context, device, changed) {
  const listed = configuredPoints(context, device);
  if (!listed) return;
  const configured = new Set(listed); // a device can have thousands of points
  for (const set of context.mapper.get(`ot-parameter-sets:${device}`) || []) {
    const values = context.mapper.get(`ot-parameter-values:${device}:${set}`) || {};
    for (const key of Object.keys(values)) {
      const point = ownerOf(context, device, set, key) ?? key;
      if (configured.has(point)) continue;
      dropValue(context, device, point, key, [set], changed);
      forgetPoint(context, device, point);
    }
  }
  const claimants = context.mapper.get(`ot-parameter-claimants:${device}`) || [];
  const kept = [];
  for (const point of claimants) {
    if (configured.has(point)) {
      kept.push(point);
      continue;
    }
    for (const [set, key] of claimsOf(context, device, point)) {
      dropValue(context, device, point, key, [set], changed);
    }
    context.mapper.set(`ot-parameter-claims:${device}:${point}`, null);
    forgetPoint(context, device, point);
  }
  if (kept.length !== claimants.length) context.mapper.set(`ot-parameter-claimants:${device}`, kept);
}

// A point its descriptor no longer declares a key for goes back to its id: its key leaves every
// set it was in, and its recorded sets are forgotten, so its next sample places it again — or, for
// a write-only point, which never samples, the request that writes it, else the default set.
// Only while the point still has the key that was declared: a sample that already placed it again
// is not undone.
function releaseDeclaration(context, device, entry, changed) {
  const point = entry.point;
  if (keyOf(context, device, point) !== keyName(entry.key)) return;
  for (const [set, held] of claimsOf(context, device, point)) {
    dropValue(context, device, point, held, [set], changed);
  }
  forgetPoint(context, device, point);
}

// A connector's retained capability descriptor lists every point of it that names its own key
// (parameter_keys, contract §7), grouped here per device; a device the service declared keys for
// before and lists no more has none left, and a point no longer listed is released. Placing the
// points claims their keys at once, so an edit of the fragment reaches the right point — and a
// replayed write result lands under the right key — even before any sample.
function onCapabilities(context, service, caps) {
  const protocol = typeof caps.protocol === "string" && caps.protocol ? caps.protocol : null;
  if (!protocol) return [];
  const byDevice = {};
  for (const entry of Array.isArray(caps.parameter_keys) ? caps.parameter_keys : []) {
    if (!entry || typeof entry.device !== "string" || !entry.device) continue;
    if (!byDevice[entry.device]) byDevice[entry.device] = [];
    byDevice[entry.device].push({ point: entry.point, key: entry.key, set: entry.set, group: entry.group });
  }
  const devices = Object.keys(byDevice);
  const previous = context.mapper.get(`ot-parameter-declaring:${service}`) || [];
  const out = [];
  for (const device of new Set([...previous, ...devices])) {
    const declared = byDevice[device] || [];
    const changed = new Set();
    for (const entry of context.mapper.get(`ot-parameter-declared:${device}:${protocol}`) || []) {
      const point = entry?.point;
      if (typeof point !== "string" || !point || declared.some((e) => e.point === point)) continue;
      releaseDeclaration(context, device, entry, changed);
    }
    context.mapper.set(`ot-parameter-declared:${device}:${protocol}`, declared);
    applyDeclarations(context, device, protocol, changed);
    out.push(...twinMessages(context, device, changed));
  }
  context.mapper.set(`ot-parameter-declaring:${service}`, devices);
  return out;
}

export function onMessage(message, context) {
  const parts = message.topic.split("/");
  let payload;
  try {
    payload = JSON.parse(decoder.decode(message.payload));
  } catch (_e) {
    return [];
  }
  if (!payload || typeof payload !== "object") return [];
  // te/device/main/service/<service>/ot/capabilities
  if (parts[3] === "service" && parts[5] === "ot" && parts[6] === "capabilities") {
    return onCapabilities(context, parts[4], payload);
  }
  const device = parts[2];
  const protocol = parts[4];
  const kind = parts[5];
  context.mapper.set(`ot-protocol:${device}`, protocol);
  // Only from what a connector publishes on its own: a command request can name any protocol,
  // and one no connector serves would never list its points and so stop all pruning.
  if (kind === "sample" || kind === "status") {
    const protocols = context.mapper.get(`ot-parameter-protocols:${device}`) || [];
    if (!protocols.includes(protocol)) {
      context.mapper.set(`ot-parameter-protocols:${device}`, [...protocols, protocol]);
    }
  }
  // The device type qualifies every set name below. It arrives on the retained link status
  // (before any sample) and on every sample, so a device with only write-only points — which
  // never samples — still gets its sets named after its type.
  // Only where the contract puts it (§5, §8): a command payload that ever grew a top-level
  // `type` must not be able to redefine the device's type.
  if ((kind === "sample" || kind === "status") && typeof payload.type === "string" && payload.type) {
    context.mapper.set(`ot-device-type:${device}`, payload.type);
  } else if (kind === "status") {
    // The link status describes the whole device and is republished on every config reload, so
    // one without a type means the type is gone (a revert, or a switch to an untyped library)
    // and the sets go back to the protocol name — which is what `describe` now renders too.
    // Never inferred from a sample: a sample legitimately omits the type for a point the
    // runtime has no configuration entry for.
    context.mapper.set(`ot-device-type:${device}`, "");
  }
  if (kind === "status") {
    // The configured points, for the same reason: republished with every configuration
    // change, so a point missing from the list has been removed. A status without a list is
    // from a connector that does not report one, which says nothing about any point — so
    // nothing is dropped, and nothing is refused until a list arrives.
    // Kept per protocol: a device name is unique only within one connector, so a device served
    // by two protocols has two lists, and a point stays while either of them has it.
    const listed = Array.isArray(payload.points)
      ? payload.points.filter((p) => typeof p === "string")
      : null;
    context.mapper.set(`ot-parameter-points:${device}:${protocol}`, listed);
    const changed = new Set();
    pruneRemovedPoints(context, device, changed);
    // The type just reported names the sets the declared keys belong to.
    applyDeclarations(context, device, protocol, changed);
    return twinMessages(context, device, changed);
  }
  const names = naming(context, device, protocol);

  if (kind === "sample") {
    const point = payload.point || parts[6];
    const sets = setsFromSample(payload, names);
    // A sample is the authority on where its point belongs now: it leaves every set it was in
    // before and is not in any more (its group, type or access changed), whatever the quality,
    // and when its key changed it leaves the old key of every set. Only a sample that describes
    // the point, though: one without `access` or `meta` (from a connector outside the SDKs,
    // which always echo `access`) says nothing about its sets or key, so neither the recorded
    // sets, the key nor the values are touched.
    const changed = new Set();
    if (payload.access !== undefined || payload.meta !== undefined) {
      placePoint(context, device, point, sets, keyFromSample(payload, point), changed);
    }
    if (sets && payload.quality === "good" && payload.value !== undefined) {
      applyValues(context, device, { [point]: payload.value }, () => sets, changed);
    }
    return twinMessages(context, device, changed);
  }

  // A parameter_update names the set it edited (origin.set), and for a write-only point —
  // which never samples — that is the ONLY thing that can say which set its value belongs in.
  // The connector echoes `origin` into every transition it publishes (§6.4), so this survives
  // the request being overwritten on the retained command topic, and therefore survives a
  // mapper restart, which replays the terminal state alone.
  // Recorded, never overriding what a sample or a declared key already established.
  if (kind === "cmd") {
    const set = payload.origin?.set;
    if (typeof set === "string" && isValidSet(set)) {
      for (const point of writtenPoints(parts[6], payload)) {
        // A missing key reads back as undefined or null depending on the runtime; `false` is a
        // real value (an opted-out point) and must not be overwritten.
        const known = context.mapper.get(`ot-parameter-set:${device}:${point}`);
        if (known === undefined || known === null) {
          context.mapper.set(`ot-parameter-set:${device}:${point}`, [set]);
        }
      }
    }
    if (payload.status === "init") return [];
  }

  if (kind === "cmd" && payload.status === "successful") {
    const verb = parts[6];
    const updates = {};
    if (verb === "write" && typeof payload.point === "string" && payload.value !== undefined) {
      updates[payload.point] = payload.value;
    } else if (verb === "write-batch") {
      for (const r of payload.results ?? []) {
        if (r?.status === "successful" && typeof r.point === "string" && r.value !== undefined) {
          updates[r.point] = r.value;
        }
      }
    }
    // A write result is retained, so a mapper restart replays the last one of every command —
    // including writes to a point removed since. Once the connector has listed its points,
    // only those are taken; the others would put the removed point back on the twin.
    // Samples need no such check: the connector only samples the points it has, in the order
    // it publishes the link status that lists them.
    const configured = context.mapper.get(`ot-parameter-points:${device}:${protocol}`);
    // A point that was written is writable by definition: the sets learned from its samples, its
    // declared key or the request that wrote it, else the default one.
    const resolveSets = (point) => {
      if (Array.isArray(configured) && !configured.includes(point)) return false;
      const known = context.mapper.get(`ot-parameter-set:${device}:${point}`);
      if (known === undefined || known === null) return [setFor(names)];
      return known === false ? false : recordedSets(known);
    };
    const changed = new Set();
    applyValues(context, device, updates, resolveSets, changed);
    return twinMessages(context, device, changed);
  }
  return [];
}
