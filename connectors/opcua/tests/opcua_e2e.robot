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
${CMD_PREFIX}           te/device/${DEVICE}/ot/${PROTOCOL}/cmd/write
${LINK_TOPIC}           te/device/${DEVICE}/ot/${PROTOCOL}/status/link
${CAPS_TOPIC}           te/device/main/service/${SERVICE}/ot/capabilities
${HEALTH_TOPIC}         te/device/main/service/${SERVICE}/status/health
${BATCH_PREFIX}         te/device/${DEVICE}/ot/${PROTOCOL}/cmd/write-batch
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

Sample Echoes The Node Id
    [Documentation]    The sample's addr field echoes the OPC-UA NodeId it was read from.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temperature    timeout=${SAMPLE_TIMEOUT}
    ${node}=    Get Json Field    ${payload}    addr.node_id
    Should Contain    ${node}    Temperature

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

Pushed Sample Echoes Point Meta
    [Documentation]    The point's free-form meta table (connector config) is echoed verbatim
    ...                in the sample envelope, so flows can apply per-signal behaviour. This
    ...                covers the PUSH envelope specifically; the polled one is covered by
    ...                "Samples Carry The Point Access".
    [Tags]    requires:subscribe
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/ticks    timeout=${SAMPLE_TIMEOUT}
    ${on_change}=    Get Json Field    ${payload}    meta.on_change
    Should Be Equal    ${on_change}    ${True}
    ${source}=    Get Json Field    ${payload}    meta.source
    Should Be Equal    ${source}    sim

Polled Sample Carries The Device Name
    [Documentation]    Regression: the runtime stamps the configured device name on polled
    ...                samples (topic AND envelope), even when the driver leaves it empty.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temperature    timeout=${SAMPLE_TIMEOUT}
    ${device}=    Get Json Field    ${payload}    device
    Should Be Equal    ${device}    ${DEVICE}


Samples Carry The Point Access And The Device Type
    [Documentation]    Every sample echoes the point's declared access and the device's type, so
    ...                flows can tell writable points (parameters) apart and name their parameter
    ...                set without reading the config file.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/setpoint    timeout=${SAMPLE_TIMEOUT}
    ${type}=    Get Json Field    ${payload}    type
    Should Be Equal    ${type}    ${DEVICE_TYPE}
    ${access}=    Get Json Field    ${payload}    access
    Should Be Equal    ${access}    read_write
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temperature    timeout=${SAMPLE_TIMEOUT}
    ${access}=    Get Json Field    ${payload}    access
    Should Be Equal    ${access}    read

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
    [Documentation]    A batch with an unknown point fails, but the result lists the write that
    ...                succeeded before it so the requester knows the device state.
    Publish Message    ${BATCH_PREFIX}/batch-2
    ...    {"status":"init","writes":[{"point":"setpoint","value":17001},{"point":"no_such_point","value":1},{"point":"running","value":false}]}    retain=True
    ${result}=    Wait For Message Containing    ${BATCH_PREFIX}/batch-2    "status":"failed"    timeout=${SAMPLE_TIMEOUT}
    ${reason}=    Get Json Field    ${result}    reason
    Should Contain    ${reason}    no_such_point
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
    Publish Message    te/device/${DEVICE}///cmd/ot_write/w-1    {"status":"init","point":"setpoint","value":17001}    retain=True
    ${result}=    Wait For Message Containing    te/device/${DEVICE}///cmd/ot_write/w-1    "status":"successful"    timeout=${FLOWS_TIMEOUT}
    ${twin}=    Wait For Message Containing    ${PARAM_TWIN}    "setpoint":17001    timeout=${FLOWS_TIMEOUT}

Alarm And Event Declared On A Point Follow Its Value
    [Documentation]    (flows) ot-alarm and ot-event act on the alarm and the event the running
    ...                point declares in its meta (connector.toml), from its samples alone — no
    ...                measurement or flow params involved. The alarm is retained: raised while
    ...                the value is true, cleared with an empty retained message once it is false.
    ...                The event is raised on a change of the value.
    [Tags]    flows
    ${alarm_topic}=    Set Variable    te/device/${DEVICE}///a/running_alarm
    ${event_topic}=    Set Variable    te/device/${DEVICE}///e/running_changed
    # Earlier tests leave running (and so this alarm) in either state, and the client keeps every
    # message it has seen: start from false, and only ever look at the LATEST message of a topic.
    Write Running    alarm-0    false
    Wait Until Keyword Succeeds    ${FLOWS_TIMEOUT}s    1s    Latest Message Should Be Empty    ${alarm_topic}
    Write Running    alarm-1    true
    Wait Until Keyword Succeeds    ${FLOWS_TIMEOUT}s    1s    Latest Message Should Contain    ${alarm_topic}    "severity":"minor"
    Latest Message Should Contain    ${alarm_topic}    running is true
    Wait Until Keyword Succeeds    ${FLOWS_TIMEOUT}s    1s    Latest Message Should Contain    ${event_topic}    Running changed to true
    Write Running    alarm-2    false
    Wait Until Keyword Succeeds    ${FLOWS_TIMEOUT}s    1s    Latest Message Should Be Empty    ${alarm_topic}
    Wait Until Keyword Succeeds    ${FLOWS_TIMEOUT}s    1s    Latest Message Should Contain    ${event_topic}    Running changed to false

Capability Descriptor Declares The Parameter Keys
    [Documentation]    A point naming its own key in its parameter set (meta.parameter.key) is listed
    ...                in the retained capability descriptor's parameter_keys (§7), so a consumer
    ...                knows the key before the point samples. Only keyed points appear.
    ${payload}=    Wait For Retained    ${CAPS_TOPIC}    timeout=${READY_TIMEOUT}
    ${keys}=    Get Json Field    ${payload}    parameter_keys
    Length Should Be    ${keys}    1
    Should Be Equal    ${keys}[0][device]    ${DEVICE}
    Should Be Equal    ${keys}[0][point]    cycle_count
    Should Be Equal    ${keys}[0][key]    counters.count

Parameter Twin Publishes A Point Under Its Key
    [Documentation]    (flows) ot-parameter-state publishes the keyed point under its key, not its id.
    [Tags]    flows
    ${payload}=    Wait For Message Containing    te/device/${DEVICE}///twin/counters    "count":    timeout=${FLOWS_TIMEOUT}
    ${twin}=    Evaluate    json.loads($payload)    modules=json
    Dictionary Should Not Contain Key    ${twin}    cycle_count
    Should Be Equal As Numbers    ${twin}[count]    617001


*** Keywords ***
Write Running
    [Documentation]    Write the running point through a connector write command and wait for it.
    [Arguments]    ${id}    ${value}
    Publish Message    ${CMD_PREFIX}/${id}    {"status":"init","point":"running","value":${value}}    retain=True
    Wait For Message Containing    ${CMD_PREFIX}/${id}    "status":"successful"    timeout=${SAMPLE_TIMEOUT}

Latest Message Should Contain
    [Arguments]    ${topic}    ${substring}
    ${payload}=    Get Message    ${topic}
    Should Contain    ${payload}    ${substring}

Latest Message Should Be Empty
    [Documentation]    An empty payload is how a retained alarm is cleared.
    [Arguments]    ${topic}
    ${payload}=    Get Message    ${topic}
    Should Be Empty    ${payload}

Sample Should Be Good
    [Arguments]    ${payload}
    ${quality}=    Get Json Field    ${payload}    quality
    Should Be Equal    ${quality}    good
    ${mode}=    Get Json Field    ${payload}    mode
    Should Be Equal    ${mode}    typed
