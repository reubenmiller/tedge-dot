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

A Disabled Point Is Left Out Of Its Device
    [Documentation]    `enabled = false` on a point (§3.3) leaves it out of the resolved device:
    ...                spare_u16 comes from a point library and is switched off by a bare inline
    ...                definition, and draft_u16 is incomplete and loads only because it is switched
    ...                off. Neither is listed on the link status, sampled or written, while the
    ...                library's other point still is.
    ${link}=    Wait For Message Containing    ${LINK_TOPIC}    "status":"connected"    timeout=${READY_TIMEOUT}
    ${points}=    Get Json Field    ${link}    points
    List Should Contain Value    ${points}    temp_u16_alias
    List Should Not Contain Value    ${points}    spare_u16
    List Should Not Contain Value    ${points}    draft_u16
    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16_alias    timeout=${SAMPLE_TIMEOUT}
    No Messages On Topic    ${SAMPLE_PREFIX}/spare_u16    timeout=5
    ${write}=    Set Variable    ${CMD_PREFIX}/spare-1
    Publish Message    ${write}    {"status":"init","point":"spare_u16","value":1}    retain=True
    Wait For Message Containing    ${write}    "status":"failed"    timeout=${SAMPLE_TIMEOUT}
    Publish Message    ${write}    ${EMPTY}    retain=True

Capability Descriptor Carries The Point Labels
    [Documentation]    A point's `name`/`description` (§3.1) are static, so they are published
    ...                once in the retained capability descriptor (§7) rather than echoed in
    ...                every sample. Only labelled points appear — no entry means the id is the
    ...                label — and here they come from the point library, which is where a
    ...                shared list documents itself once for every instance that references it.
    ${payload}=    Wait For Retained    ${CAPS_TOPIC}    timeout=${READY_TIMEOUT}
    ${labels}=    Get Json Field    ${payload}    point_labels
    ${by_point}=    Evaluate    {l["point"]: l for l in $labels}
    Dictionary Should Contain Key    ${by_point}    count_u32
    Should Be Equal    ${by_point}[count_u32][device]    ${DEVICE}
    Should Be Equal    ${by_point}[count_u32][name]    Cycle count
    Should Be Equal    ${by_point}[count_u32][description]    Completed pump cycles since power-on
    # temp_u16 declares no labels, so it is absent rather than carrying an empty entry.
    Dictionary Should Not Contain Key    ${by_point}    temp_u16

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
    ${unit}=    Get Json Field    ${payload}    unit
    Should Be Equal    ${unit}    m
    ${datatype}=    Get Json Field    ${payload}    datatype
    Should Be Equal    ${datatype}    float32
    ${value}=    Get Json Field    ${payload}    value
    Should Be True    abs(${value} - 404.17) < 0.05

Reads A Point From A Path-Referenced Library
    [Documentation]    connector.toml references its second library by absolute path rather
    ...                than by name (contract §3.4). Both forms are legal in a config file —
    ...                and the define-device test below depends on this one being present, since
    ...                a pre-existing path reference must not stop the management verbs working.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16_alias    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${unit}=    Get Json Field    ${payload}    unit
    Should Be Equal    ${unit}    alias

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


Samples Carry The Point Access And The Device Type
    [Documentation]    Every sample echoes the point's declared access and the device's type, so
    ...                flows can tell writable points (parameters) apart and name their parameter
    ...                set without reading the config file.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16    timeout=${SAMPLE_TIMEOUT}
    ${type}=    Get Json Field    ${payload}    type
    Should Be Equal    ${type}    ${DEVICE_TYPE}
    ${access}=    Get Json Field    ${payload}    access
    Should Be Equal    ${access}    read_write
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/level_f32    timeout=${SAMPLE_TIMEOUT}
    ${access}=    Get Json Field    ${payload}    access
    Should Be Equal    ${access}    read

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
    # Writable, but switched off with `enabled = false` (§3.3), so not a parameter.
    Dictionary Should Not Contain Key    ${properties}    spare_u16
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

A Parameter Removed On Reload Leaves The Twin
    [Documentation]    (flows) A writable point removed from the configuration and another added in
    ...                its place (twin_only_u16 renamed to renamed_u16) must not stay on the parameter
    ...                twin fragment: Cumulocity sends the whole fragment back with an operator's
    ...                edit, so the stale key would fail every update of the set. The reload
    ...                republishes the link status with the configured points, and the flow drops
    ...                the ones it no longer lists.
    [Tags]    flows
    Wait For Message Containing    ${PARAM_TWIN}    "twin_only_u16":    timeout=${FLOWS_TIMEOUT}
    DeviceLibrary.Execute Command
    ...    cmd=sed -i '/^ *id *= *"twin_only_u16"/s/twin_only_u16/renamed_u16/' /etc/connector.toml
    Clear Messages
    DeviceLibrary.Execute Command    cmd=kill -HUP 1
    ${link}=    Wait For Message Containing    ${LINK_TOPIC}    "renamed_u16"    timeout=${SAMPLE_TIMEOUT}
    Should Not Contain    ${link}    "twin_only_u16"
    ${twin}=    Wait For Message Containing    ${PARAM_TWIN}    "renamed_u16":    timeout=${FLOWS_TIMEOUT}
    ${values}=    Evaluate    json.loads($twin)    modules=json
    Dictionary Should Not Contain Key    ${values}    twin_only_u16
    Dictionary Should Contain Key    ${values}    temp_u16

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
    # The capability descriptor's point_labels come from the configuration, so the retained
    # message must follow a reload — otherwise it keeps describing the config as it was at
    # startup, with no labels for the device just defined.
    ${payload}=    Wait For Message Containing    ${CAPS_TOPIC}    plc2    timeout=${SAMPLE_TIMEOUT}
    ${labels}=    Get Json Field    ${payload}    point_labels
    ${for_plc2}=    Evaluate    [l for l in $labels if l["device"] == "plc2"]
    Should Not Be Empty    ${for_plc2}    the reload must republish the labels of the new device
    Should Be Equal    ${for_plc2}[0][name]    Cycle count

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
