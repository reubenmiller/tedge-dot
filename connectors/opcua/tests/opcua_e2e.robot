*** Settings ***
Documentation       End-to-end tests for tedge-dot (opcua module) against a real
...                 OPC-UA server (python-asyncua). The connector reads the simulator's nodes and
...                 publishes samples + status to a local MQTT broker; these tests assert on that
...                 output. No cloud (Cumulocity) is involved. This proves the connector contract
...                 and SDK runtime are protocol-neutral: the same envelopes a Modbus driver emits
...                 are produced here by an OPC-UA driver with NodeId addressing.
...
...                 Run via:  just test-e2e-opcua   (brings the Docker stack up/down automatically)

Resource            ../../_shared/stack.resource
Library             Collections

Suite Setup         Setup OT Stack    opcua
Suite Teardown      Teardown OT Stack


*** Variables ***
${DEVICE}               opc1
${PROTOCOL}             opcua
${SERVICE}              tedge-dot

${SAMPLE_PREFIX}        te/device/${DEVICE}/ot/${PROTOCOL}/sample
${CMD_PREFIX}           te/device/${DEVICE}///cmd/ot_write
${LINK_TOPIC}           te/device/${DEVICE}/ot/${PROTOCOL}/status/link
${MANIFEST_TOPIC}       te/device/${DEVICE}/ot/${PROTOCOL}/manifest
${CAPS_TOPIC}           te/device/main/service/${SERVICE}/ot/capabilities
${HEALTH_TOPIC}         te/device/main/service/${SERVICE}/status/health
${BATCH_PREFIX}         te/device/${DEVICE}///cmd/ot_write_batch
${PARAM_CMD_PREFIX}     te/device/${DEVICE}///cmd/parameter_update
# The device type declared on [[device]] (§3.1) qualifies the parameter set names (§5.2).
${DEVICE_TYPE}          opcua-sim
${PARAM_SET}            opcua_sim_control_parameters
${PARAM_TWIN}           te/device/${DEVICE}///twin/${PARAM_SET}
# The flows container installs thin-edge from the main channel at build time; give it time.
${FLOWS_TIMEOUT}        120

# Generous timeout: the connector waits for the simulator/broker before it starts.
${READY_TIMEOUT}        90
${SAMPLE_TIMEOUT}       15
# Reconnect uses a 1s->60s exponential backoff, and re-subscribing needs a fresh session.
${RECOVERY_TIMEOUT}     90
# How long a frozen server must stay frozen for the connector to conclude the subscription is
# no longer delivering: operation_timeout (5s, see connector.toml) plus the subscription's
# publishing_interval x max_keep_alive_count (1s x 20), with margin.
${SUBSCRIPTION_INACTIVITY_WAIT}     35s


*** Test Cases ***
Connector Publishes Capability Descriptor
    [Documentation]    The connector advertises its protocol and supported command verbs.
    ${payload}=    Wait For Retained    ${CAPS_TOPIC}    timeout=${READY_TIMEOUT}
    ${protocol}=    Get Json Field    ${payload}    protocol
    Should Be Equal    ${protocol}    opcua
    ${verbs}=    Get Json Field    ${payload}    command_verbs
    List Should Contain Value    ${verbs}    write

Capability Descriptor Advertises Push Delivery
    [Documentation]    The descriptor's `subscribe` flag is what a mapper reads to know samples
    ...                can arrive without being asked for. It must agree with what the connector
    ...                actually does, which "Subscribed Node Pushes Value Changes" proves.
    [Tags]    requires:subscribe
    ${payload}=    Wait For Retained    ${CAPS_TOPIC}    timeout=${READY_TIMEOUT}
    ${subscribe}=    Get Json Field    ${payload}    subscribe
    Should Be Equal    ${subscribe}    ${True}

Service Health Is Up
    [Documentation]    The connector publishes a retained service health status of "up".
    ${payload}=    Wait For Retained    ${HEALTH_TOPIC}    timeout=${READY_TIMEOUT}
    ${status}=    Get Json Field    ${payload}    status
    Should Be Equal    ${status}    up

Device Link Is Connected
    [Documentation]    The connector reports the OPC-UA server link as connected.
    ${payload}=    Wait For Retained    ${LINK_TOPIC}    timeout=${READY_TIMEOUT}
    ${status}=    Get Json Field    ${payload}    status
    Should Be Equal    ${status}    connected

Reads Float64 Node
    [Documentation]    Reads the Temperature node (Double 21.5) addressed by NodeId ns=2;s=Temperature.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temperature    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${datatype}=    Get Json Field    ${payload}    datatype
    Should Be Equal    ${datatype}    float64
    ${value}=    Get Json Field    ${payload}    value
    Should Be True    abs(${value} - 21.5) < 0.05

Sample Carries No Address Unless Debugging Is On
    [Documentation]    The NodeId a point is read from is static, so it is not echoed in every
    ...                sample (§5); `addr` returns only under [connector] sample_debug, which
    ...                the packaged configuration leaves off. (The conformance harness turns it
    ...                on, which is where the address echo is asserted against the wire.)
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temperature    timeout=${SAMPLE_TIMEOUT}
    ${sample}=    Evaluate    json.loads($payload)    modules=json
    Dictionary Should Not Contain Key    ${sample}    addr
    Dictionary Should Not Contain Key    ${sample}    raw

Reads Uint32 Node
    [Documentation]    Reads the Count node (UInt32 617001).
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/count_u32    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${datatype}=    Get Json Field    ${payload}    datatype
    Should Be Equal    ${datatype}    uint32
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    617001

Unknown Node Reports Bad Quality
    [Documentation]    Reading a non-existent NodeId yields a bad-quality sample with an error.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/bad_point    timeout=${SAMPLE_TIMEOUT}
    ${quality}=    Get Json Field    ${payload}    quality
    Should Be Equal    ${quality}    bad
    ${error}=    Get Json Field    ${payload}    error
    Should Not Be Empty    ${error}

Writes An Int32 Node And Reads It Back
    [Documentation]    A write command sets Setpoint to 4242; the next sample reflects it.
    Publish Message    ${CMD_PREFIX}/sp-1    {"status":"init","point":"setpoint","value":4242}    retain=True
    ${result}=    Wait For Message Containing    ${CMD_PREFIX}/sp-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    ${point}=    Get Json Field    ${result}    point
    Should Be Equal    ${point}    setpoint
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/setpoint    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    4242

Writes A Scaled Node In Engineering Units
    [Documentation]    A write carries the value in the SAME units a read reports (§4.2), so the
    ...                runtime inverts the point's transform before the module builds the
    ...                variant. setpoint_scaled is the Setpoint node with decimal_shift = -3:
    ...                writing 12.345 must leave 12345 on the node — which `setpoint`, the
    ...                unscaled view of the same node, reads back. The 0.1 connector encoded the
    ...                request value verbatim, so this wrote 12 and the next read said 0.012.
    Publish Message    ${CMD_PREFIX}/sps-1    {"status":"init","point":"setpoint_scaled","value":12.345}    retain=True
    ${result}=    Wait For Message Containing    ${CMD_PREFIX}/sps-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    # The result echoes the engineering value, not the 12345 that went on the wire.
    ${echoed}=    Get Json Field    ${result}    value
    Should Be True    abs(${echoed} - 12.345) < 1e-6
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/setpoint_scaled    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be True    abs(${value} - 12.345) < 1e-6
    ${raw}=    Wait For Sample    ${SAMPLE_PREFIX}/setpoint    timeout=${SAMPLE_TIMEOUT}
    ${wire}=    Get Json Field    ${raw}    value
    Should Be Equal As Numbers    ${wire}    12345

Writes A Boolean Node And Reads It Back
    [Documentation]    A write command sets Running true; the next sample reflects it.
    Publish Message    ${CMD_PREFIX}/run-1    {"status":"init","point":"running","value":true}    retain=True
    ${result}=    Wait For Message Containing    ${CMD_PREFIX}/run-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    ${point}=    Get Json Field    ${result}    point
    Should Be Equal    ${point}    running
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/running    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal    ${value}    ${True}

Subscribed Node Pushes Value Changes
    [Documentation]    The ticks point is delivered by an OPC-UA subscription (monitored item),
    ...                not polling: the simulator increments it every second and each change
    ...                arrives as a pushed sample with a strictly increasing value.
    ...
    ...                This checks that push delivers the changes correctly (good quality,
    ...                strictly increasing). It does NOT by itself prove delivery is push --
    ...                polling would produce the same series -- which is what
    ...                "Subscribed Static Node Falls Silent After Its First Value" is for.
    [Tags]    requires:subscribe
    ${first}=    Wait For Sample    ${SAMPLE_PREFIX}/ticks    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${first}
    ${v1}=    Get Json Field    ${first}    value
    ${second}=    Wait For Sample    ${SAMPLE_PREFIX}/ticks    timeout=${SAMPLE_TIMEOUT}
    ${v2}=    Get Json Field    ${second}    value
    Should Be True    ${v2} > ${v1}

Subscribed Static Node Falls Silent After Its First Value
    [Documentation]    The decisive push test. temperature_pushed is subscribed to a node whose
    ...                value never changes, so a subscription delivers its initial value and
    ...                then reports nothing; polling would republish it every poll_interval
    ...                (1s here) for as long as the connector runs.
    ...
    ...                A rate check cannot make this distinction: the runtime passes each
    ...                point's resolved poll interval to the connector as the monitored item's
    ...                sampling interval, so push and polling run at the same rate by design.
    ...                Silence is the only behaviour polling cannot imitate.
    [Tags]    requires:subscribe
    # The initial notification arrives when the connector subscribes, i.e. at stack startup —
    # so look it up in the recorded traffic rather than waiting for a FRESH one, which is
    # precisely what will never come again.
    ${payload}=    Wait For Message Containing
    ...    ${SAMPLE_PREFIX}/temperature_pushed    "quality"    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${value}=    Get Json Field    ${payload}    value
    Should Be True    abs(${value} - 21.5) < 0.05
    # ...and then nothing more, because the node never changes.
    No New Messages On Topic    ${SAMPLE_PREFIX}/temperature_pushed    timeout=5

Push Delivery Recovers After The Server Restarts
    [Documentation]    A subscribed point is OFF the polling schedule, so if push stops the
    ...                device goes silent and nothing else notices. When the server restarts,
    ...                the connector must drop the link, reconnect and re-arm the subscription
    ...                — otherwise samples never come back.
    [Tags]    requires:subscribe
    Wait For Message Containing    ${SAMPLE_PREFIX}/ticks    "quality"    timeout=${SAMPLE_TIMEOUT}
    Restart Stack Service    simulator
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/ticks    timeout=${RECOVERY_TIMEOUT}
    Sample Should Be Good    ${payload}

Push Delivery Recovers From A Silent Server
    [Documentation]    Freezing the server leaves the TCP connection ESTABLISHED and simply
    ...                stops every answer — the contract's silent-peer case (§8.1), but on the
    ...                PUSH path, where the conformance suite's B5 checks do not reach: every
    ...                point in the conformance config is `subscribe = false`. A subscribed
    ...                point is off the polling schedule, so if push stalls there is nothing
    ...                else to notice it.
    ...
    ...                Measured: this trips the client's request timeout
    ...                (connector.operation_timeout), which the connector reports as a dead
    ...                transport. It does NOT cover a subscription that dies while the session
    ...                stays healthy — see the note in impl/c/README.md; that path is guarded
    ...                by explicit checks but cannot be provoked with this simulator.
    [Tags]    requires:subscribe
    Wait For Message Containing    ${SAMPLE_PREFIX}/ticks    "quality"    timeout=${SAMPLE_TIMEOUT}
    Freeze Stack Service    simulator
    # Long enough for the keep-alive window to lapse: the connector's operation_timeout plus
    # publishing_interval x max_keep_alive_count (see connector.toml).
    Sleep    ${SUBSCRIPTION_INACTIVITY_WAIT}
    Thaw Stack Service    simulator
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/ticks    timeout=${RECOVERY_TIMEOUT}
    Sample Should Be Good    ${payload}
    [Teardown]    Run Keyword And Ignore Error    Thaw Stack Service    simulator

Pushed Sample Carries No Point Meta Either
    [Documentation]    The push path produces the same slim envelope as the polled one (§5):
    ...                the point's free-form meta table is on the device manifest, not on every
    ...                sample. This covers the PUSH envelope specifically; the polled one is
    ...                covered by "Samples Carry Only What Changes Per Read".
    [Tags]    requires:subscribe
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/ticks    timeout=${SAMPLE_TIMEOUT}
    ${sample}=    Evaluate    json.loads($payload)    modules=json
    FOR    ${gone}    IN    meta    unit    access    type    value_repr    ts_ms    raw    addr
        Dictionary Should Not Contain Key    ${sample}    ${gone}
    END
    # The typed `publish` policy and the site's own `meta` are on the manifest, for the
    # subscribed point as for any other.
    ${manifest}=    Wait For Retained    ${MANIFEST_TOPIC}    timeout=${READY_TIMEOUT}
    ${points}=    Get Json Field    ${manifest}    points
    Should Be Equal    ${points}[ticks][publish][on_change]    ${True}
    Should Be Equal    ${points}[ticks][meta][source]    sim

Polled Sample Carries The Device Name
    [Documentation]    Regression: the runtime stamps the configured device name on polled
    ...                samples (topic AND envelope), even when the driver leaves it empty.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temperature    timeout=${SAMPLE_TIMEOUT}
    ${device}=    Get Json Field    ${payload}    device
    Should Be Equal    ${device}    ${DEVICE}


Samples Carry Only What Changes Per Read
    [Documentation]    A sample is a time series row (§5). The point's access and the device's
    ...                type — what a flow needs to tell parameters apart and name their set —
    ...                are on the retained manifest (§8.2), published once per device instead
    ...                of on every read.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/setpoint    timeout=${SAMPLE_TIMEOUT}
    ${sample}=    Evaluate    json.loads($payload)    modules=json
    FOR    ${gone}    IN    type    access    unit    meta    value_repr    ts_ms    raw    addr
        Dictionary Should Not Contain Key    ${sample}    ${gone}
    END
    ${manifest}=    Wait For Retained    ${MANIFEST_TOPIC}    timeout=${READY_TIMEOUT}
    ${type}=    Get Json Field    ${manifest}    type
    Should Be Equal    ${type}    ${DEVICE_TYPE}
    ${points}=    Get Json Field    ${manifest}    points
    Should Be Equal    ${points}[setpoint][access]    read_write
    Should Be Equal    ${points}[temperature][access]    read

Capability Descriptor Advertises Write Batch
    [Documentation]    The runtime adds the write-batch verb for every module that implements write.
    ${payload}=    Wait For Retained    ${CAPS_TOPIC}    timeout=${READY_TIMEOUT}
    ${verbs}=    Get Json Field    ${payload}    command_verbs
    List Should Contain Value    ${verbs}    write-batch

Write Batch Writes Several Points In One Command
    [Documentation]    One write-batch request writes an int32 and a boolean node in order and reports
    ...                a per-point result; the next samples reflect both values.
    Publish Message    ${BATCH_PREFIX}/batch-1
    ...    {"status":"init","writes":[{"point":"setpoint","value":4243},{"point":"running","value":true}]}    retain=True
    ${result}=    Wait For Message Containing    ${BATCH_PREFIX}/batch-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    ${results}=    Get Json Field    ${result}    results
    Length Should Be    ${results}    2
    Should Be Equal    ${results}[0][point]    setpoint
    Should Be Equal    ${results}[0][status]    successful
    Should Be Equal    ${results}[1][point]    running
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/setpoint    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    4243
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/running    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal    ${value}    ${True}

Write Batch Stops At The First Failure And Reports What Was Applied
    [Documentation]    A batch whose second entry fails at the DEVICE fails, but the result lists
    ...                the write that succeeded before it so the requester knows the device
    ...                state, and the entry after it was never attempted. `temperature` is
    ...                read-only, so the module refuses it — a failure only the device can
    ...                report, unlike the ones the runtime catches up front.
    Publish Message    ${BATCH_PREFIX}/batch-2
    ...    {"status":"init","writes":[{"point":"setpoint","value":17001},{"point":"temperature","value":1},{"point":"running","value":false}]}    retain=True
    ${result}=    Wait For Message Containing    ${BATCH_PREFIX}/batch-2    "status":"failed"    timeout=${SAMPLE_TIMEOUT}
    ${reason}=    Get Json Field    ${result}    reason
    Should Contain    ${reason}    temperature
    ${results}=    Get Json Field    ${result}    results
    Length Should Be    ${results}    2
    Should Be Equal    ${results}[0][status]    successful
    Should Be Equal    ${results}[1][status]    failed
    # the coil after the failing entry was never written
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/running    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal    ${value}    ${True}

Write Batch Rejects An Empty Request
    Publish Message    ${BATCH_PREFIX}/batch-3    {"status":"init","writes":[]}    retain=True
    ${result}=    Wait For Message Containing    ${BATCH_PREFIX}/batch-3    "status":"failed"    timeout=${SAMPLE_TIMEOUT}
    ${reason}=    Get Json Field    ${result}    reason
    Should Contain    ${reason}    no writes

The Manifest CLI Prints What The Service Publishes
    [Documentation]    `tedge-dot manifest` (§8) prints the manifests a configuration would
    ...                publish, off the same code, with no broker and no device. Compared field
    ...                for field against the retained message, because the two drifting apart is
    ...                the failure this command exists to prevent. `info` is the exception: it
    ...                comes from a live `connect`, which a CLI that talks to nothing cannot
    ...                have. Runs against whichever implementation the stack was built with
    ...                (IMPL=rust|c).
    ${retained}=    Wait For Retained    ${MANIFEST_TOPIC}    timeout=${READY_TIMEOUT}
    ${printed}=    DeviceLibrary.Execute Command
    ...    cmd=tedge-dot manifest -c /etc/connector.toml -d ${DEVICE}    strip=${True}
    ${documents}=    Evaluate    json.loads($printed)    modules=json
    Length Should Be    ${documents}    1    one device asked for, one manifest printed
    Should Be Equal    ${documents}[0][device]    ${DEVICE}
    ${actual}=    Evaluate
    ...    {k: v for k, v in $documents[0].items() if k not in ("device", "info")}    modules=json
    ${expected}=    Evaluate
    ...    {k: v for k, v in json.loads($retained).items() if k != "info"}    modules=json
    Should Be Equal    ${actual}    ${expected}

The Manifest CLI Renders The Parameter Set Definition
    [Documentation]    `tedge-dot manifest --format c8y-dtm` (§8.1) renders this config's
    ...                writable points as the Cumulocity DTM property definition a tenant admin
    ...                registers once, with the same keys the parameter twin fragment carries —
    ...                from the device manifest, not from the configuration file.
    ${output}=    DeviceLibrary.Execute Command
    ...    cmd=tedge-dot manifest -c /etc/connector.toml --format c8y-dtm    strip=${True}
    # The first JSON line, not the first line: a warning on stderr (§5.2) can be interleaved.
    ${definition}=    Evaluate    json.loads([l for l in $output.splitlines() if l.startswith("{")][0])    modules=json
    Should Be Equal    ${definition}[identifier]    ${PARAM_SET}
    ${properties}=    Set Variable    ${definition}[jsonSchema][properties]
    Dictionary Should Contain Key    ${properties}    setpoint
    Dictionary Should Contain Key    ${properties}    running
    Dictionary Should Not Contain Key    ${properties}    temperature
    Should Be Equal    ${properties}[running][type]    boolean
    Should Be Equal    ${definition}[contexts]    ${{['asset', 'event', 'operation']}}

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
    ${payload}=    Wait For Message Containing    ${PARAM_TWIN}    "setpoint":    timeout=${FLOWS_TIMEOUT}
    ${twin}=    Evaluate    json.loads($payload)    modules=json
    Dictionary Should Contain Key    ${twin}    setpoint
    Dictionary Should Contain Key    ${twin}    running
    Dictionary Should Not Contain Key    ${twin}    temperature

Parameter Update Command Writes The Points And Completes
    [Documentation]    (flows) A Cumulocity-shaped parameter_update command (as the c8y mapper
    ...                would publish for a c8y_ParameterUpdate operation) is bridged to ONE
    ...                connector write-batch, completes with the mapper metadata preserved, and
    ...                the twin reflects the new values.
    [Tags]    flows
    Publish Message    ${PARAM_CMD_PREFIX}/c8y-mapper-1
    ...    {"status":"init","operation":{"deviceId":"1","c8y_ParameterUpdate":{},"c8y_ParameterUpdate_${PARAM_SET}":{},"${PARAM_SET}":{"setpoint":1234,"running":false}},"c8y-mapper":{"on_fragment":"c8y_ParameterUpdate","output":null}}    retain=True
    ${result}=    Wait For Message Containing    ${PARAM_CMD_PREFIX}/c8y-mapper-1    "status":"successful"    timeout=${FLOWS_TIMEOUT}
    ${meta}=    Get Json Field    ${result}    c8y-mapper.on_fragment
    Should Be Equal    ${meta}    c8y_ParameterUpdate
    ${results}=    Get Json Field    ${result}    results
    Length Should Be    ${results}    2
    ${batch}=    Wait For Message Containing    ${BATCH_PREFIX}/ot--c8y-mapper-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    ${twin}=    Wait For Message Containing    ${PARAM_TWIN}    "setpoint":1234    timeout=${FLOWS_TIMEOUT}
    ${coil}=    Get Json Field    ${twin}    running
    Should Be Equal    ${coil}    ${False}
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/setpoint    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    1234

Parameter Update With A Stale Key Fails With The Connector Reason And Applies Nothing
    [Documentation]    (flows) A parameter set that has drifted from the configuration — a key
    ...                that is no longer a point — fails the whole operation, naming the key,
    ...                with nothing written (§6.4): the set is never half-applied, and the
    ...                operator sees a reason rather than an operation that hangs.
    [Tags]    flows
    Publish Message    ${PARAM_CMD_PREFIX}/c8y-mapper-2
    ...    {"status":"init","operation":{"c8y_ParameterUpdate":{},"c8y_ParameterUpdate_${PARAM_SET}":{},"${PARAM_SET}":{"setpoint":31337,"bogus":1}},"c8y-mapper":{"on_fragment":"c8y_ParameterUpdate","output":null}}    retain=True
    ${result}=    Wait For Message Containing    ${PARAM_CMD_PREFIX}/c8y-mapper-2    "status":"failed"    timeout=${FLOWS_TIMEOUT}
    ${reason}=    Get Json Field    ${result}    reason
    Should Contain    ${reason}    bogus
    # ...and the key that WAS valid was not written: the batch never started.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/setpoint    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Not Be Equal As Numbers    ${value}    31337

Write Batch With An Unknown Point Applies Nothing
    [Documentation]    A point the device does not define is caught before the first write, so
    ...                the batch fails with NOTHING applied (§6.4). The owner still answers it:
    ...                ownership needs one of the request's points to be its own, and a request
    ...                whose points are all typos would otherwise hang at `init` for ever.
    Publish Message    ${BATCH_PREFIX}/batch-4
    ...    {"status":"init","writes":[{"point":"setpoint","value":31337},{"point":"no_such_point","value":1}]}    retain=True
    ${result}=    Wait For Message Containing    ${BATCH_PREFIX}/batch-4    "status":"failed"    timeout=${SAMPLE_TIMEOUT}
    ${reason}=    Get Json Field    ${result}    reason
    Should Contain    ${reason}    no_such_point
    ${results}=    Get Json Field    ${result}    results
    Should Be Empty    ${results}    the batch never started

Generic Write Command Is Bridged By The Flows
    [Documentation]    (flows) The pre-existing ot_write bridge (c8y_SetRegister path) still works
    ...                alongside the parameter bridge.
    [Tags]    flows
    Publish Message    te/device/${DEVICE}///cmd/ot_write/w-1    {"status":"init","point":"setpoint","value":17001}    retain=True
    ${result}=    Wait For Message Containing    te/device/${DEVICE}///cmd/ot_write/w-1    "status":"successful"    timeout=${FLOWS_TIMEOUT}
    ${twin}=    Wait For Message Containing    ${PARAM_TWIN}    "setpoint":17001    timeout=${FLOWS_TIMEOUT}


*** Keywords ***
Sample Should Be Good
    [Arguments]    ${payload}
    ${quality}=    Get Json Field    ${payload}    quality
    Should Be Equal    ${quality}    good
    # `mode` is gone (§1): `datatype` is the only type system, and every sample carries one.
    ${sample}=    Evaluate    json.loads($payload)    modules=json
    Dictionary Should Not Contain Key    ${sample}    mode
    Dictionary Should Contain Key    ${sample}    datatype
