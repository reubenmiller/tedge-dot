*** Settings ***
Documentation       End-to-end tests for tedge-dot against a real Modbus
...                 simulator (pymodbus). The connector reads the simulator and publishes raw
...                 samples + status to a local MQTT broker; these tests assert on that output.
...                 No cloud (Cumulocity) is involved.
...
...                 Run via:  just test-e2e   (brings the Docker stack up/down automatically)

Resource            ../../_shared/stack.resource
Library             Collections

Suite Setup         Setup OT Stack    modbus
Suite Teardown      Teardown OT Stack


*** Variables ***
${DEVICE}               plc1
${PROTOCOL}             modbus
${SERVICE}              tedge-dot

${SAMPLE_PREFIX}        te/device/${DEVICE}/ot/${PROTOCOL}/sample
${CMD_PREFIX}           te/device/${DEVICE}/ot/${PROTOCOL}/cmd/write
${LINK_TOPIC}           te/device/${DEVICE}/ot/${PROTOCOL}/status/link
${MANIFEST_TOPIC}       te/device/${DEVICE}/ot/${PROTOCOL}/manifest
${CAPS_TOPIC}           te/device/main/service/${SERVICE}/ot/capabilities
${HEALTH_TOPIC}         te/device/main/service/${SERVICE}/status/health
${BATCH_PREFIX}         te/device/${DEVICE}/ot/${PROTOCOL}/cmd/write-batch
# Management verbs change this instance's configuration, so they address its service (§6.3).
${MGMT_PREFIX}          te/device/main/service/${SERVICE}/ot/cmd
${PARAM_CMD_PREFIX}     te/device/${DEVICE}///cmd/parameter_update
# The device type (from the point library, §3.1) qualifies the parameter set names, so this is
# `<type>_<group>_parameters` with the type's punctuation folded to '_' (§5.2).
${DEVICE_TYPE}          modbus-plc-sim
${PARAM_SET}            modbus_plc_sim_control_parameters
${PARAM_TWIN}           te/device/${DEVICE}///twin/${PARAM_SET}
# The flows container installs thin-edge from the main channel at build time; give it time.
${FLOWS_TIMEOUT}        120

# Generous timeout: the connector waits for the simulator/broker before it starts.
${READY_TIMEOUT}        90
${SAMPLE_TIMEOUT}       15


*** Test Cases ***
Connector Publishes Capability Descriptor
    [Documentation]    The connector advertises its protocol and supported command verbs.
    ${payload}=    Wait For Retained    ${CAPS_TOPIC}    timeout=${READY_TIMEOUT}
    ${protocol}=    Get Json Field    ${payload}    protocol
    Should Be Equal    ${protocol}    modbus
    ${verbs}=    Get Json Field    ${payload}    command_verbs
    List Should Contain Value    ${verbs}    write

A Disabled Device Is Left Out Of The Connector
    [Documentation]    `enabled = false` (§3.3) takes a device out of the configuration: plc-off is
    ...                never connected, polled or advertised, and the point library it names — not
    ...                installed — is not even looked up, so the connector starts all the same.
    Wait For Message Containing    ${LINK_TOPIC}    "status":"connected"    timeout=${READY_TIMEOUT}
    No Messages On Topic    te/device/plc-off/#    timeout=5
    ${write}=    Set Variable    te/device/plc-off/ot/${PROTOCOL}/cmd/write/off-1
    Publish Message    ${write}    {"status":"init","point":"temp_u16","value":1}    retain=True
    Sleep    3s
    ${payloads}=    Get Messages    ${write}
    ${answers}=    Evaluate
    ...    [p for p in $payloads if p and json.loads(p).get("status") != "init"]    modules=json
    Should Be Empty    ${answers}    nothing owns a disabled device, so its commands go unanswered
    Publish Message    ${write}    ${EMPTY}    retain=True

Device Manifest Describes The Device And Its Points
    [Documentation]    Every static fact about a device is on its retained manifest (§8.2): the
    ...                contract version, the connector serving it, its type, the descriptor its
    ...                connect reported, and every point keyed by id with its datatype, access,
    ...                unit, labels, free-form meta and the parameter sets the connector
    ...                resolved. Labels come from the point library here, which is where a
    ...                shared list documents itself once for every instance that references it.
    # The first manifest goes out before connect; the descriptor (`info`) follows on the
    # republish the connect report triggers, so wait for that one.
    ${payload}=    Wait For Message Containing    ${MANIFEST_TOPIC}    "info"    timeout=${READY_TIMEOUT}
    ${contract}=    Get Json Field    ${payload}    contract
    Should Be Equal    ${contract}    0.2
    ${service}=    Get Json Field    ${payload}    service
    Should Be Equal    ${service}    ${SERVICE}
    ${type}=    Get Json Field    ${payload}    type
    Should Be Equal    ${type}    modbus-plc-sim
    ${transport}=    Get Json Field    ${payload}    info.transport
    Should Be Equal    ${transport}    tcp
    ${points}=    Get Json Field    ${payload}    points
    Should Be Equal    ${points}[count_u32][name]    Cycle count
    Should Be Equal    ${points}[count_u32][description]    Completed pump cycles since power-on
    Should Be Equal    ${points}[count_u32][access]    read
    Should Be Equal    ${points}[count_u32][datatype]    uint32
    # temp_u16 declares no labels, so it carries none rather than an empty one.
    Dictionary Should Not Contain Key    ${points}[temp_u16]    name
    Should Be Equal    ${points}[temp_u16][access]    read_write
    # The parameter sets are resolved by the connector (RFC 0005: qualified by the type).
    Should Be Equal    ${points}[temp_u16][parameter][sets]    ${{ ["modbus_plc_sim_control_parameters"] }}
    # A read-only point is no parameter, so it carries no `parameter` at all.
    Dictionary Should Not Contain Key    ${points}[level_f32]    parameter
    # The inline device override wins over the library ("packaged" -> "m").
    Should Be Equal    ${points}[level_f32][unit]    m
    # The typed signal metadata of §5 is on the manifest: `measurement` names (or refuses)
    # the series, `range` is the bound the connector enforces on write, `publish` is the
    # policy the runtime has already applied to the stream.
    Should Be Equal    ${points}[twin_only_u16][measurement]    ${False}
    Should Be Equal    ${points}[temp_scaled][unit]    kV
    Should Be Equal    ${points}[bounded_u16][range]    ${{ {"min": 10, "max": 500} }}
    Should Be Equal    ${points}[quiet_u16][publish][on_change]    ${True}
    Should Be Equal    ${points}[bounded_u16][measurement][series]    Bounded
    # `meta` is free-form again: the site's own tags, published verbatim and read by nobody.
    Should Be Equal    ${points}[bounded_u16][meta]    ${{ {"asset_tag": "B-17"} }}

Capability Descriptor Describes The Build Only
    [Documentation]    What the configuration says about a device is on its manifest; the
    ...                capability descriptor (§7) is a property of the build and carries no
    ...                per-point labels any more.
    ${payload}=    Wait For Retained    ${CAPS_TOPIC}    timeout=${READY_TIMEOUT}
    ${caps}=    Evaluate    json.loads($payload)    modules=json
    Dictionary Should Not Contain Key    ${caps}    point_labels
    Dictionary Should Contain Key    ${caps}    command_verbs

Service Health Is Up
    [Documentation]    The connector publishes a retained service health status of "up".
    ${payload}=    Wait For Retained    ${HEALTH_TOPIC}    timeout=${READY_TIMEOUT}
    ${status}=    Get Json Field    ${payload}    status
    Should Be Equal    ${status}    up

Device Link Is Connected
    [Documentation]    The connector reports the Modbus device link as connected.
    ${payload}=    Wait For Retained    ${LINK_TOPIC}    timeout=${READY_TIMEOUT}
    ${status}=    Get Json Field    ${payload}    status
    Should Be Equal    ${status}    connected

Reads Uint16 Holding Register
    [Documentation]    Reads a uint16 holding register seeded to 17001 in the simulator.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${datatype}=    Get Json Field    ${payload}    datatype
    Should Be Equal    ${datatype}    uint16
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    17001

Reads Uint32 Across Two Registers
    [Documentation]    Reads a uint32 value (617001) spanning two registers.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/count_u32    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    617001

Reads Float32 Across Two Registers
    [Documentation]    Reads a float32 value (~404.17) spanning two registers.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/level_f32    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${value}=    Get Json Field    ${payload}    value
    Should Be True    abs(${value} - 404.17) < 0.05

Library Point Keeps Its Definition Under An Inline Override
    [Documentation]    level_f32 comes from the point library points.d/modbus/plc-sim.toml and is
    ...                overridden inline with `unit = "m"` alone (contract §3.4). The resolved
    ...                point must take the override's unit while keeping the library's datatype
    ...                and address -- the whole point of referencing a list you do not own.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/level_f32    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${datatype}=    Get Json Field    ${payload}    datatype
    Should Be Equal    ${datatype}    float32
    ${value}=    Get Json Field    ${payload}    value
    Should Be True    abs(${value} - 404.17) < 0.05
    # The unit is static per point, so it is on the manifest (§8.2), not in every sample (§5).
    ${manifest}=    Wait For Retained    ${MANIFEST_TOPIC}    timeout=${READY_TIMEOUT}
    ${points}=    Get Json Field    ${manifest}    points
    Should Be Equal    ${points}[level_f32][unit]    m

Reads A Point From A Path-Referenced Library
    [Documentation]    connector.toml references its second library by absolute path rather
    ...                than by name (contract §3.4). Both forms are legal in a config file —
    ...                and the define-device test below depends on this one being present, since
    ...                a pre-existing path reference must not stop the management verbs working.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16_alias    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    # The library's `unit` proves which list the point came from; it is on the manifest (§8.2).
    ${manifest}=    Wait For Retained    ${MANIFEST_TOPIC}    timeout=${READY_TIMEOUT}
    ${points}=    Get Json Field    ${manifest}    points
    Should Be Equal    ${points}[temp_u16_alias][unit]    alias

Invalid Register Reports Bad Quality
    [Documentation]    Reading a flagged-invalid address yields a bad-quality sample with an error.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/bad_point    timeout=${SAMPLE_TIMEOUT}
    ${quality}=    Get Json Field    ${payload}    quality
    Should Be Equal    ${quality}    bad
    ${error}=    Get Json Field    ${payload}    error
    Should Not Be Empty    ${error}

Writes A Coil And Reads It Back
    [Documentation]    A write command sets coil 48 true; the next sample reflects it.
    Publish Message    ${CMD_PREFIX}/coil-1    {"status":"init","point":"coil_rw","value":true}    retain=True
    ${result}=    Wait For Message Containing    ${CMD_PREFIX}/coil-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    ${point}=    Get Json Field    ${result}    point
    Should Be Equal    ${point}    coil_rw
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/coil_rw    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal    ${value}    ${True}

Writes A Holding Register And Reads It Back
    [Documentation]    A write command sets holding register 3 to 4242; the next sample reflects it.
    ...                 Runs after the uint16 read assertion (the stack is recreated per run).
    Publish Message    ${CMD_PREFIX}/reg-1    {"status":"init","point":"temp_u16","value":4242}    retain=True
    ${result}=    Wait For Message Containing    ${CMD_PREFIX}/reg-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    ${point}=    Get Json Field    ${result}    point
    Should Be Equal    ${point}    temp_u16
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    4242

Writes A Scaled Register In Engineering Units
    [Documentation]    A write carries the value in the SAME units a read reports (§4.2), so the
    ...                runtime inverts the point's transform before the module encodes the
    ...                register. temp_scaled is register 3 with decimal_shift = -3: writing 20
    ...                must leave 20000 in the register, and both the result and the next read
    ...                must say 20. The 0.1 connector encoded the request value verbatim, so
    ...                this wrote register 20 and the next read said 0.02.
    Publish Message    ${CMD_PREFIX}/scaled-1    {"status":"init","point":"temp_scaled","value":20}    retain=True
    ${result}=    Wait For Message Containing    ${CMD_PREFIX}/scaled-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    # The result echoes the engineering value, not the 20000 that went on the wire.
    ${echoed}=    Get Json Field    ${result}    value
    Should Be Equal As Numbers    ${echoed}    20
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_scaled    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    20
    # ...and the unscaled view of the same register proves 20000 really was written.
    ${raw}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16    timeout=${SAMPLE_TIMEOUT}
    ${wire}=    Get Json Field    ${raw}    value
    Should Be Equal As Numbers    ${wire}    20000

The Runtime Applies A Point's Publish Policy
    [Documentation]    §5.4 / RFC 0006 §5.1 option A: `publish` is applied by the CONNECTOR to
    ...                the sample stream, not by each flow that wants it. quiet_u16 reads a
    ...                register whose value never changes and declares publish.on_change, so
    ...                after its first sample the connector goes quiet — while the points
    ...                around it keep publishing every poll, which is what proves the polling
    ...                loop is still running and only the publishing was suppressed.
    # Make the value change, so there is a sample to catch deterministically: a point that
    # publishes only once, at startup, races the test client's subscription.
    Publish Message    ${CMD_PREFIX}/quiet-1    {"status":"init","point":"quiet_u16","value":1234}    retain=True
    Wait For Message Containing    ${CMD_PREFIX}/quiet-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    # The change passes the gate...
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/quiet_u16    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    1234
    # ...and every identical re-read after it is suppressed. (`No New Messages` ignores
    # history, so the retained traffic other tests rely on is left alone.)
    No New Messages On Topic    ${SAMPLE_PREFIX}/quiet_u16    timeout=5
    # Meanwhile a point with no policy kept publishing right through that window, which is what
    # shows the poll loop never stopped and only the publishing was suppressed.
    Wait For Sample    ${SAMPLE_PREFIX}/count_u32    timeout=${SAMPLE_TIMEOUT}

A Write Outside The Declared Range Fails Before The Device Is Touched
    [Documentation]    `range` (§5.3) is enforced by the connector, not only by the cloud form:
    ...                a write outside it fails with a reason and never reaches the device, so a
    ...                script or a typo in an operation meets the same bound an operator does.
    ...                bounded_u16 declares range = { min = 10, max = 500 }.
    Publish Message    ${CMD_PREFIX}/ok-1    {"status":"init","point":"bounded_u16","value":400}    retain=True
    Wait For Message Containing    ${CMD_PREFIX}/ok-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/bounded_u16    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    400
    # Above the ceiling: refused, with the range named, and the register keeps the old value.
    Publish Message    ${CMD_PREFIX}/hi-1    {"status":"init","point":"bounded_u16","value":501}    retain=True
    ${result}=    Wait For Message Containing    ${CMD_PREFIX}/hi-1    "status":"failed"    timeout=${SAMPLE_TIMEOUT}
    ${reason}=    Get Json Field    ${result}    reason
    Should Contain    ${reason}    outside range [10, 500] of bounded_u16
    # Below the floor: likewise.
    Publish Message    ${CMD_PREFIX}/lo-1    {"status":"init","point":"bounded_u16","value":9}    retain=True
    ${result}=    Wait For Message Containing    ${CMD_PREFIX}/lo-1    "status":"failed"    timeout=${SAMPLE_TIMEOUT}
    ${reason}=    Get Json Field    ${result}    reason
    Should Contain    ${reason}    outside range
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/bounded_u16    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    400    nothing was applied

A Batch With One Out-Of-Range Entry Applies Nothing
    [Documentation]    In a write-batch the range check runs for EVERY entry before the first
    ...                write is executed (§5.3), so an out-of-range value fails the batch with
    ...                nothing applied — the only failure mode guaranteed to have left the
    ...                device untouched, which is what lets an operator retry it safely.
    Publish Message    ${BATCH_PREFIX}/range-1
    ...    {"status":"init","writes":[{"point":"coil_rw","value":true},{"point":"bounded_u16","value":9999}]}    retain=True
    ${result}=    Wait For Message Containing    ${BATCH_PREFIX}/range-1    "status":"failed"    timeout=${SAMPLE_TIMEOUT}
    ${reason}=    Get Json Field    ${result}    reason
    Should Contain    ${reason}    outside range
    # The first entry was legal, but nothing ran: the batch never started.
    ${results}=    Get Json Field    ${result}    results
    Should Be Empty    ${results}
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/bounded_u16    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    400

A Write Outside The Datatype Fails Before The Device Is Touched
    [Documentation]    70 on temp_scaled inverts to 70000, which a uint16 register cannot hold,
    ...                so the write fails with a reason instead of wrapping on the wire (§4.2).
    Publish Message    ${CMD_PREFIX}/scaled-2    {"status":"init","point":"temp_scaled","value":70}    retain=True
    ${result}=    Wait For Message Containing    ${CMD_PREFIX}/scaled-2    "status":"failed"    timeout=${SAMPLE_TIMEOUT}
    ${reason}=    Get Json Field    ${result}    reason
    Should Contain    ${reason}    does not fit uint16 after transform
    # The register still holds what the previous test wrote: nothing was applied.
    ${raw}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16    timeout=${SAMPLE_TIMEOUT}
    ${wire}=    Get Json Field    ${raw}    value
    Should Be Equal As Numbers    ${wire}    20000


Samples Carry Only What Changes Per Read
    [Documentation]    A sample is a time series row (§5): identity, value, quality. The point's
    ...                access, unit, labels and free-form meta — and the device's type — are on
    ...                the retained manifest (§8.2), published once, so a reading of a point
    ...                sampled every second no longer repeats them thousands of times a day.
    ...                `raw` and `addr` are opt-in behind [connector] sample_debug, which the
    ...                packaged configuration leaves off.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16    timeout=${SAMPLE_TIMEOUT}
    ${sample}=    Evaluate    json.loads($payload)    modules=json
    FOR    ${gone}    IN    type    access    unit    meta    value_repr    ts_ms    raw    addr
        Dictionary Should Not Contain Key    ${sample}    ${gone}
    END
    Should Be Equal    ${sample}[device]    ${DEVICE}
    Should Be Equal    ${sample}[protocol]    ${PROTOCOL}
    Should Be Equal    ${sample}[point]    temp_u16
    Should Be Equal    ${sample}[datatype]    uint16
    Should Be Equal    ${sample}[quality]    good
    # ...and the facts that left the envelope are on the manifest, where a flow looks them up.
    ${manifest}=    Wait For Retained    ${MANIFEST_TOPIC}    timeout=${READY_TIMEOUT}
    ${points}=    Get Json Field    ${manifest}    points
    Should Be Equal    ${points}[temp_u16][access]    read_write
    Should Be Equal    ${points}[level_f32][access]    read
    ${type}=    Get Json Field    ${manifest}    type
    Should Be Equal    ${type}    ${DEVICE_TYPE}

Capability Descriptor Advertises Write Batch
    [Documentation]    The runtime adds the write-batch verb for every module that implements write.
    ${payload}=    Wait For Retained    ${CAPS_TOPIC}    timeout=${READY_TIMEOUT}
    ${verbs}=    Get Json Field    ${payload}    command_verbs
    List Should Contain Value    ${verbs}    write-batch

Write Batch Writes Several Points In One Command
    [Documentation]    One write-batch request writes a register and a coil in order and reports
    ...                a per-point result; the next samples reflect both values.
    Publish Message    ${BATCH_PREFIX}/batch-1
    ...    {"status":"init","writes":[{"point":"temp_u16","value":4243},{"point":"coil_rw","value":true}]}    retain=True
    ${result}=    Wait For Message Containing    ${BATCH_PREFIX}/batch-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    ${results}=    Get Json Field    ${result}    results
    Length Should Be    ${results}    2
    Should Be Equal    ${results}[0][point]    temp_u16
    Should Be Equal    ${results}[0][status]    successful
    Should Be Equal    ${results}[1][point]    coil_rw
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    4243
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/coil_rw    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal    ${value}    ${True}

Write Batch Stops At The First Failure And Reports What Was Applied
    [Documentation]    A batch with an unknown point fails, but the result lists the write that
    ...                succeeded before it so the requester knows the device state.
    Publish Message    ${BATCH_PREFIX}/batch-2
    ...    {"status":"init","writes":[{"point":"temp_u16","value":17001},{"point":"no_such_point","value":1},{"point":"coil_rw","value":false}]}    retain=True
    ${result}=    Wait For Message Containing    ${BATCH_PREFIX}/batch-2    "status":"failed"    timeout=${SAMPLE_TIMEOUT}
    ${reason}=    Get Json Field    ${result}    reason
    Should Contain    ${reason}    no_such_point
    ${results}=    Get Json Field    ${result}    results
    Length Should Be    ${results}    2
    Should Be Equal    ${results}[0][status]    successful
    Should Be Equal    ${results}[1][status]    failed
    # the coil after the failing entry was never written
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/coil_rw    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal    ${value}    ${True}

Write Batch Rejects An Empty Request
    Publish Message    ${BATCH_PREFIX}/batch-3    {"status":"init","writes":[]}    retain=True
    ${result}=    Wait For Message Containing    ${BATCH_PREFIX}/batch-3    "status":"failed"    timeout=${SAMPLE_TIMEOUT}
    ${reason}=    Get Json Field    ${result}    reason
    Should Contain    ${reason}    no writes

Describe Renders The Parameter Set Definition
    [Documentation]    `tedge-dot describe` renders this config's writable points as the
    ...                Cumulocity DTM property definition a tenant admin registers once, with
    ...                the same keys the parameter twin fragment carries. Runs against whichever
    ...                implementation the stack was built with (IMPL=rust|c).
    ${output}=    DeviceLibrary.Execute Command
    ...    cmd=tedge-dot describe -c /etc/connector.toml --compact    strip=${True}
    # The first JSON line, not the first line: a warning on stderr (§5.2) can be interleaved.
    ${definition}=    Evaluate    json.loads([l for l in $output.splitlines() if l.startswith("{")][0])    modules=json
    Should Be Equal    ${definition}[identifier]    ${PARAM_SET}
    ${properties}=    Set Variable    ${definition}[jsonSchema][properties]
    Dictionary Should Contain Key    ${properties}    temp_u16
    Dictionary Should Contain Key    ${properties}    coil_rw
    Dictionary Should Not Contain Key    ${properties}    level_f32
    Should Be Equal    ${properties}[coil_rw][type]    boolean
    Should Be Equal    ${definition}[contexts]    ${{['asset', 'event', 'operation']}}

Describe Renders Every Config In A Directory
    [Documentation]    `tedge-dot describe` takes a directory, like `run`: one service runs every
    ...                connector config it finds there, so the definitions a tenant admin registers
    ...                have to cover all of them. The second config here is another device type on
    ...                another protocol — describe needs neither the protocol module nor a device.
    Write Describe Configs    /tmp/describe-dir
    ${identifiers}=    Describe Identifiers    -c /tmp/describe-dir
    Should Be Equal    ${identifiers}    ${{[$PARAM_SET, "acme_boiler_control_parameters"]}}
    # A device filter applies across every file, not only the first one.
    ${identifiers}=    Describe Identifiers    -c /tmp/describe-dir -d boiler-*
    Should Be Equal    ${identifiers}    ${{["acme_boiler_control_parameters"]}}

Describe Defaults To The Connector Config Directory
    [Documentation]    Without `-c`, describe renders the directory the packaged service runs
    ...                (/etc/tedge/plugins/ot) — every connector in it, not just modbus.toml.
    Write Describe Configs    /etc/tedge/plugins/ot
    ${identifiers}=    Describe Identifiers
    Should Be Equal    ${identifiers}    ${{[$PARAM_SET, "acme_boiler_control_parameters"]}}

Flows Register The Device And Advertise The Parameter Capability
    [Documentation]    (flows) ot-registration turns the link status into a child-device
    ...                registration and advertises parameter_update so a cloud mapper routes
    ...                c8y_ParameterUpdate operations to it.
    [Tags]    flows
    ${payload}=    Wait For Retained    te/device/${DEVICE}//    timeout=${FLOWS_TIMEOUT}
    ${type}=    Get Json Field    ${payload}    @type
    Should Be Equal    ${type}    child-device
    # The connector reports the configured device type on its link status, and the registration
    # flow uses it as the entity type instead of the generic "<protocol>-device" (§3.1).
    ${entity_type}=    Get Json Field    ${payload}    type
    Should Be Equal    ${entity_type}    ${DEVICE_TYPE}
    Wait For Retained    ${PARAM_CMD_PREFIX}    timeout=${FLOWS_TIMEOUT}

Parameter Twin Follows The Device
    [Documentation]    (flows) ot-parameter-state publishes the writable points of the device as
    ...                one twin fragment per parameter set, fed by the connector's samples.
    [Tags]    flows
    ${payload}=    Wait For Message Containing    ${PARAM_TWIN}    "temp_u16":    timeout=${FLOWS_TIMEOUT}
    ${twin}=    Evaluate    json.loads($payload)    modules=json
    Dictionary Should Contain Key    ${twin}    temp_u16
    Dictionary Should Contain Key    ${twin}    coil_rw
    Dictionary Should Not Contain Key    ${twin}    level_f32

A Parameter Opted Out Of Measurements Reaches Only Its Twin
    [Documentation]    (flows) `meta.measurement = false` keeps a parameter off the measurement
    ...                path: twin_only_u16 is sampled and lands on the parameter twin fragment, but
    ...                ot-measurement never publishes it as a series. temp_u16 — a parameter on the
    ...                same register without the opt-out — is published both ways, the default.
    [Tags]    flows
    Wait For Message Containing    ${PARAM_TWIN}    "twin_only_u16":    timeout=${FLOWS_TIMEOUT}
    Wait For Message Containing    te/device/${DEVICE}///m/${PROTOCOL}    "temp_u16":
    ...    timeout=${FLOWS_TIMEOUT}
    # A sample of both points passes through the flows every second, so give the opted-out one
    # several chances to show up as a measurement before judging.
    Sleep    5s
    Wait For Message Containing    ${SAMPLE_PREFIX}/twin_only_u16    "quality":"good"
    ...    timeout=${SAMPLE_TIMEOUT}
    ${measurements}=    Get Messages    te/device/${DEVICE}///m/${PROTOCOL}
    Should Not Be Empty    ${measurements}
    FOR    ${measurement}    IN    @{measurements}
        Should Not Contain    ${measurement}    twin_only_u16
    END

Parameter Update Command Writes The Points And Completes
    [Documentation]    (flows) A Cumulocity-shaped parameter_update command (as the c8y mapper
    ...                would publish for a c8y_ParameterUpdate operation) is bridged to ONE
    ...                connector write-batch, completes with the mapper metadata preserved, and
    ...                the twin reflects the new values.
    [Tags]    flows
    Publish Message    ${PARAM_CMD_PREFIX}/c8y-mapper-1
    ...    {"status":"init","operation":{"deviceId":"1","c8y_ParameterUpdate":{},"c8y_ParameterUpdate_${PARAM_SET}":{},"${PARAM_SET}":{"temp_u16":1234,"coil_rw":false}},"c8y-mapper":{"on_fragment":"c8y_ParameterUpdate","output":null}}    retain=True
    ${result}=    Wait For Message Containing    ${PARAM_CMD_PREFIX}/c8y-mapper-1    "status":"successful"    timeout=${FLOWS_TIMEOUT}
    ${meta}=    Get Json Field    ${result}    c8y-mapper.on_fragment
    Should Be Equal    ${meta}    c8y_ParameterUpdate
    ${results}=    Get Json Field    ${result}    results
    Length Should Be    ${results}    2
    ${batch}=    Wait For Message Containing    ${BATCH_PREFIX}/ot--c8y-mapper-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    # The connector echoes the request's `origin` into its result (§6.4). The command topic is
    # retained and holds one message, so without this a mapper that restarts replays only this
    # result and can no longer tell which parameter set the write belonged to.
    ${origin_set}=    Get Json Field    ${batch}    origin.set
    Should Be Equal    ${origin_set}    ${PARAM_SET}
    ${twin}=    Wait For Message Containing    ${PARAM_TWIN}    "temp_u16":1234    timeout=${FLOWS_TIMEOUT}
    ${coil}=    Get Json Field    ${twin}    coil_rw
    Should Be Equal    ${coil}    ${False}
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    1234

Parameter Update With An Unknown Key Fails With The Connector Reason
    [Tags]    flows
    Publish Message    ${PARAM_CMD_PREFIX}/c8y-mapper-2
    ...    {"status":"init","operation":{"c8y_ParameterUpdate":{},"c8y_ParameterUpdate_${PARAM_SET}":{},"${PARAM_SET}":{"bogus":1}},"c8y-mapper":{"on_fragment":"c8y_ParameterUpdate","output":null}}    retain=True
    ${result}=    Wait For Message Containing    ${PARAM_CMD_PREFIX}/c8y-mapper-2    "status":"failed"    timeout=${FLOWS_TIMEOUT}
    ${reason}=    Get Json Field    ${result}    reason
    Should Contain    ${reason}    bogus

Generic Write Command Is Bridged By The Flows
    [Documentation]    (flows) The pre-existing ot_write bridge (c8y_SetRegister path) still works
    ...                alongside the parameter bridge.
    [Tags]    flows
    Publish Message    te/device/${DEVICE}///cmd/ot_write/w-1    {"status":"init","point":"temp_u16","value":17001}    retain=True
    ${result}=    Wait For Message Containing    te/device/${DEVICE}///cmd/ot_write/w-1    "status":"successful"    timeout=${FLOWS_TIMEOUT}
    ${twin}=    Wait For Message Containing    ${PARAM_TWIN}    "temp_u16":17001    timeout=${FLOWS_TIMEOUT}

Refuses A Point Library Path From A Management Command
    [Documentation]    `points_from` may name a library, never a path, when it arrives over
    ...                MQTT (contract §3.4). A config file is edited by root or tedge, but
    ...                anything able to publish on the broker must not be able to make the
    ...                connector open an arbitrary path and report what it found there — the
    ...                loader's error would otherwise carry file detail into this retained
    ...                result. Refused before the path is opened, in both implementations.
    Publish Message    ${MGMT_PREFIX}/define-device/lib-2
    ...    {"status":"init","device":{"name":"plc3","protocol_address":{"transport":"tcp","host":"simulator","port":502,"unit_id":1},"points_from":["../../etc/hostname"]}}
    ...    retain=True
    ${result}=    Wait For Message Containing    ${MGMT_PREFIX}/define-device/lib-2
    ...    "status":"failed"    timeout=${SAMPLE_TIMEOUT}
    ${reason}=    Get Json Field    ${result}    reason
    Should Contain    ${reason}    is a path
    # The refusal must not leak what is at that path.
    Should Not Contain    ${reason}    parse

A Config Edit Is Applied On Reload
    [Documentation]    SIGHUP (`systemctl reload`) makes the connector re-read its config file and
    ...                apply what changed in place, without a restart: a point added to the file is
    ...                sampled, and the service health never goes down. reload_e2e.robot covers a
    ...                directory of configs, with files added, removed and broken.
    # Inserted into plc1's own point list, right after its `points_from` (matched by plc1's
    # library, not just the key: the disabled plc-off has a `points_from` too), so it lands on plc1
    # whatever devices the file declares after it.
    DeviceLibrary.Execute Command
    ...    cmd=sed -i '/^points_from.*"plc-sim"/a [[device.point]]\\nid = "reloaded_u16"\\ndatatype = "uint16"\\naddress = { table = "holding", address = 3, count = 1 }' /etc/connector.toml
    Clear Messages
    DeviceLibrary.Execute Command    cmd=kill -HUP 1
    Wait For Sample    ${SAMPLE_PREFIX}/reloaded_u16    timeout=${SAMPLE_TIMEOUT}
    ${health}=    Get Messages    ${HEALTH_TOPIC}
    FOR    ${payload}    IN    @{health}
        Should Not Contain    ${payload}    "down"    the connector restarted instead of reloading
    END

A Connector That Cannot Restart After A Reload Is Retried
    [Documentation]    A reload that needs the connector restarted (a new [mqtt] port), into a
    ...                configuration it cannot run with (nothing listens there), takes the connector
    ...                down but not the service: the process keeps running and retrying, and the
    ...                next reload of a usable file brings the connector back. It is the only
    ...                connector, so a service that stopped with its last connector would be gone.
    ${started}=    DeviceLibrary.Execute Command    cmd=cut -d' ' -f22 /proc/1/stat
    DeviceLibrary.Execute Command
    ...    cmd=cp /etc/connector.toml /tmp/connector.toml && sed -i 's/^port *= *1883$/port = 1/' /etc/connector.toml
    Clear Messages
    DeviceLibrary.Execute Command    cmd=kill -HUP 1
    Wait For Message Containing    ${HEALTH_TOPIC}    "status":"down"    timeout=${SAMPLE_TIMEOUT}
    # Past the 5s restart delay, so a failed restart has been retried (and failed) again.
    Sleep    8s
    DeviceLibrary.Execute Command    cmd=cp /tmp/connector.toml /etc/connector.toml && kill -HUP 1
    Wait For Message Containing    ${HEALTH_TOPIC}    "status":"up"    timeout=${SAMPLE_TIMEOUT}
    Wait For Message Containing    ${LINK_TOPIC}    "status":"connected"    timeout=${SAMPLE_TIMEOUT}
    ${now}=    DeviceLibrary.Execute Command    cmd=cut -d' ' -f22 /proc/1/stat
    Should Be Equal    ${now}    ${started}    the tedge-dot process exited and was started again

Defines A Device From A Point Library Alone
    [Documentation]    A define-device command carrying only connection information and a
    ...                points_from reference must bring up a working device: this is what lets a
    ...                discovery mechanism (mDNS and friends) add instances of a known device
    ...                type at runtime without shipping their point lists. The persisted config
    ...                must keep the reference rather than the points it expands to.
    ...
    ...                Last in the suite: it rewrites /etc/connector.toml and reconnects every
    ...                device.
    Publish Message    ${MGMT_PREFIX}/define-device/lib-1
    ...    {"status":"init","device":{"name":"plc2","protocol_address":{"transport":"tcp","host":"simulator","port":502,"unit_id":1},"points_from":["plc-sim"]}}
    ...    retain=True
    Wait For Message Containing    ${MGMT_PREFIX}/define-device/lib-1
    ...    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    ${payload}=    Wait For Sample    te/device/plc2/ot/${PROTOCOL}/sample/count_u32    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    617001
    # The reference was written back, not the points it resolves to (count_u32 exists only in
    # the library). Both implementations rewrite this file on a management command; comments
    # are dropped by one and kept by the other, and the kept ones talk about these point ids,
    # so compare the settings only.
    # (chr(10) rather than a "\n" literal: Robot would turn that into a real newline inside
    # the Python expression.)
    # The device just defined gets a retained manifest of its own (§8.2), describing the
    # points its library reference resolved to.
    ${payload}=    Wait For Message Containing    te/device/plc2/ot/${PROTOCOL}/manifest    count_u32    timeout=${SAMPLE_TIMEOUT}
    ${points}=    Get Json Field    ${payload}    points
    Should Be Equal    ${points}[count_u32][name]    Cycle count

    ${config}=    DeviceLibrary.Execute Command    cmd=cat /etc/connector.toml    strip=${True}
    ${settings}=    Evaluate
    ...    chr(10).join(l for l in $config.splitlines() if not l.lstrip().startswith("#"))
    Should Contain    ${settings}    points_from
    # plc1's pre-existing path reference survived the rewrite, and did not cause the command
    # to be refused as a management-supplied path.
    Should Contain    ${settings}    plc-sim-extra.toml
    # count_u32 is the one point that exists ONLY in the library (level_f32 is also patched
    # inline by plc1, so it legitimately appears).
    Should Not Contain    ${settings}    count_u32


*** Keywords ***
Write Describe Configs
    [Documentation]    Fill `dir` with two connector configs: this stack's own (plus the point
    ...                libraries it references by relative path) and a second one declaring another
    ...                device type, on another protocol, with one writable point.
    [Arguments]    ${dir}
    DeviceLibrary.Execute Command
    ...    cmd=mkdir -p ${dir} && cp /etc/connector.toml ${dir}/modbus.toml && cp -r /etc/points.d ${dir}/
    ${lines}=    Create List
    ...    [connector]
    ...    protocol = "opcua"
    ...    [[device]]
    ...    name = "boiler-1"
    ...    type = "acme-boiler"
    ...    protocol_address = { endpoint = "opc.tcp://127.0.0.1:4840/" }
    ...    [[device.point]]
    ...    id = "setpoint"
    ...    datatype = "float32"
    ...    access = "read_write"
    ...    address = { node_id = "ns=2;s=Setpoint" }
    ${content}=    Evaluate    shlex.quote(chr(10).join($lines) + chr(10))    modules=shlex
    DeviceLibrary.Execute Command    cmd=printf '%s' ${content} > ${dir}/opcua.toml

Describe Identifiers
    [Documentation]    The set identifiers `tedge-dot describe` renders for `args`, in output order.
    [Arguments]    ${args}=${EMPTY}
    ${output}=    DeviceLibrary.Execute Command
    ...    cmd=tedge-dot describe ${args} --compact    strip=${True}
    ${identifiers}=    Evaluate
    ...    [json.loads(l)["identifier"] for l in $output.splitlines() if l.startswith("{")]
    ...    modules=json
    RETURN    ${identifiers}

Sample Should Be Good
    [Arguments]    ${payload}
    ${quality}=    Get Json Field    ${payload}    quality
    Should Be Equal    ${quality}    good
    ${mode}=    Get Json Field    ${payload}    mode
    Should Be Equal    ${mode}    typed
