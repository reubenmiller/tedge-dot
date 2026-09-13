*** Settings ***
Documentation       The connector must survive its MQTT broker going away. The connector's client
...                 uses a clean session, so after a reconnect the broker has forgotten the command
...                 subscriptions, and a broker without persistence (like this stack's) every
...                 retained message too.
...
...                 Before this was handled, a connector that lost the broker kept polling its
...                 devices but silently stopped receiving commands — a Cumulocity parameter
...                 update then stayed PENDING for good — and the C runtime spun a core in its
...                 loop on the dead connection.
...
...                 A suite of its own, with its own stack: restarting the broker would disturb
...                 the retained-message assertions of the main suite.

Resource            ../../_shared/stack.resource

Suite Setup         Setup OT Stack    modbus
Suite Teardown      Teardown OT Stack


*** Variables ***
${DEVICE}                   plc1
${PROTOCOL}                 modbus
${SERVICE}                  tedge-dot

${SAMPLE_PREFIX}            te/device/${DEVICE}/ot/${PROTOCOL}/sample
${BATCH_PREFIX}             te/device/${DEVICE}///cmd/ot_write_batch
${LINK_TOPIC}               te/device/${DEVICE}/ot/${PROTOCOL}/status/link
${CAPS_TOPIC}               te/device/main/service/${SERVICE}/ot/capabilities
${HEALTH_TOPIC}             te/device/main/service/${SERVICE}/status/health
${PARAM_CMD_PREFIX}         te/device/${DEVICE}///cmd/parameter_update
${PARAM_SET}                modbus_plc_sim_control_parameters

${READY_TIMEOUT}            90
${SAMPLE_TIMEOUT}           15
# Both runtimes retry the broker every second; this leaves room for the broker's own start-up.
${RECOVERY_TIMEOUT}         30
# The flows container installs thin-edge from the main channel at build time; give it time.
${FLOWS_TIMEOUT}            120
# Share of one CPU core the connector may use while the broker is down. Polling one device
# costs about 1%; the busy loop this guards against took a whole core.
${MAX_IDLE_CPU_PERCENT}     20


*** Test Cases ***
Connector Restores Its Retained Status After The Broker Restarts
    [Documentation]    Health, capability descriptor and link status are retained. The restarted
    ...                broker has none of them, so each one seen here was published again.
    Wait For Message Containing    ${HEALTH_TOPIC}    "status":"up"    timeout=${READY_TIMEOUT}
    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16    timeout=${SAMPLE_TIMEOUT}
    Restart Broker
    Wait For Message Containing    ${HEALTH_TOPIC}    "status":"up"    timeout=${RECOVERY_TIMEOUT}
    ${payload}=    Wait For Retained    ${CAPS_TOPIC}    timeout=${RECOVERY_TIMEOUT}
    ${protocol}=    Get Json Field    ${payload}    protocol
    Should Be Equal    ${protocol}    modbus
    ${payload}=    Wait For Retained    ${LINK_TOPIC}    timeout=${RECOVERY_TIMEOUT}
    ${status}=    Get Json Field    ${payload}    status
    Should Be Equal    ${status}    connected

Samples Resume After The Broker Restarts
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16    timeout=${RECOVERY_TIMEOUT}
    ${quality}=    Get Json Field    ${payload}    quality
    Should Be Equal    ${quality}    good

Commands Are Received After The Broker Restarts
    [Documentation]    The broker forgot the connector's command subscriptions with the old
    ...                session; unless they are made again this command is never seen.
    Publish Message    ${BATCH_PREFIX}/after-restart-1
    ...    {"status":"init","writes":[{"point":"temp_u16","value":4321}]}    retain=True
    Wait For Message Containing    ${BATCH_PREFIX}/after-restart-1    "status":"successful"
    ...    timeout=${RECOVERY_TIMEOUT}
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    4321

Parameter Update Completes After The Broker Restarts
    [Documentation]    (flows) The reported failure end to end: a c8y_ParameterUpdate-shaped
    ...                command published after the broker restarted is bridged by the flows to
    ...                a write-batch, written by the connector and completed.
    [Tags]    flows
    Publish Message    ${PARAM_CMD_PREFIX}/c8y-mapper-after-restart
    ...    {"status":"init","operation":{"deviceId":"1","c8y_ParameterUpdate":{},"c8y_ParameterUpdate_${PARAM_SET}":{},"${PARAM_SET}":{"temp_u16":1111,"coil_rw":true}},"c8y-mapper":{"on_fragment":"c8y_ParameterUpdate","output":null}}
    ...    retain=True
    Wait For Message Containing    ${PARAM_CMD_PREFIX}/c8y-mapper-after-restart    "status":"successful"
    ...    timeout=${FLOWS_TIMEOUT}
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    1111

Connector Does Not Spin While The Broker Is Down
    [Documentation]    With no connection libmosquitto's loop returns immediately instead of
    ...                waiting out its timeout; a runtime that does not sleep then burns a core
    ...                for as long as the broker is away. Recovery after the longer outage is
    ...                checked too. Last: it takes the broker down.
    Stop Stack Service    broker
    Sleep    2s    let the connector notice the connection is gone
    ${percent}=    Connector CPU Percent    seconds=5
    Start Stack Service    broker
    Reconnect To Broker
    Should Be True    ${percent} < ${MAX_IDLE_CPU_PERCENT}
    ...    msg=the connector used ${percent}% of a core while the broker was down
    Wait For Message Containing    ${HEALTH_TOPIC}    "status":"up"    timeout=${RECOVERY_TIMEOUT}
    Publish Message    ${BATCH_PREFIX}/after-outage-1
    ...    {"status":"init","writes":[{"point":"temp_u16","value":17001}]}    retain=True
    Wait For Message Containing    ${BATCH_PREFIX}/after-outage-1    "status":"successful"
    ...    timeout=${RECOVERY_TIMEOUT}


*** Keywords ***
Connector CPU Percent
    [Documentation]    Share of one CPU core the connector process (PID 1 of its container, the
    ...    entrypoint execs it) used over `seconds`, from the utime + stime of /proc/1/stat.
    [Arguments]    ${seconds}=5
    ${percent}=    DeviceLibrary.Execute Command
    ...    cmd=t() { set -- $(cut -d' ' -f14,15 /proc/1/stat); echo $(($1 + $2)); }; a=$(t); sleep ${seconds}; b=$(t); echo $(( (b - a) * 100 / $(getconf CLK_TCK) / ${seconds} ))
    ...    strip=${True}
    Log    connector CPU while the broker was down: ${percent}% of a core
    RETURN    ${percent}
