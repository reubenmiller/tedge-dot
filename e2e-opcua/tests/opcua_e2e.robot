*** Settings ***
Documentation       End-to-end tests for the Rust tedge-dot (opcua module) against a real
...                 OPC-UA server (python-asyncua). The connector reads the simulator's nodes and
...                 publishes samples + status to a local MQTT broker; these tests assert on that
...                 output. No cloud (Cumulocity) is involved. This proves the connector contract
...                 and SDK runtime are protocol-neutral: the same envelopes a Modbus driver emits
...                 are produced here by an OPC-UA driver with NodeId addressing.
...
...                 Run via:  just test-e2e-opcua   (brings the Docker stack up/down automatically)

Library             MqttClient.py
Library             Collections

Suite Setup         Connect And Subscribe
Suite Teardown      Disconnect Broker


*** Variables ***
${BROKER_HOST}          localhost
${BROKER_PORT}          12883

${DEVICE}               opc1
${PROTOCOL}             opcua
${SERVICE}              tedge-dot

${SAMPLE_PREFIX}        te/device/${DEVICE}/ot/${PROTOCOL}/sample
${CMD_PREFIX}           te/device/${DEVICE}/ot/${PROTOCOL}/cmd/write
${LINK_TOPIC}           te/device/${DEVICE}/ot/${PROTOCOL}/status/link
${CAPS_TOPIC}           te/device/main/service/${SERVICE}/ot/capabilities
${HEALTH_TOPIC}         te/device/main/service/${SERVICE}/status/health

# Generous timeout: the connector waits for the simulator/broker before it starts.
${READY_TIMEOUT}        90
${SAMPLE_TIMEOUT}       15


*** Test Cases ***
Connector Publishes Capability Descriptor
    [Documentation]    The connector advertises its protocol and supported command verbs.
    ${payload}=    Wait For Retained    ${CAPS_TOPIC}    timeout=${READY_TIMEOUT}
    ${protocol}=    Get Json Field    ${payload}    protocol
    Should Be Equal    ${protocol}    opcua
    ${verbs}=    Get Json Field    ${payload}    command_verbs
    List Should Contain Value    ${verbs}    write

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


*** Keywords ***
Connect And Subscribe
    Connect Broker    ${BROKER_HOST}    ${BROKER_PORT}
    Subscribe    te/#

Sample Should Be Good
    [Arguments]    ${payload}
    ${quality}=    Get Json Field    ${payload}    quality
    Should Be Equal    ${quality}    good
    ${mode}=    Get Json Field    ${payload}    mode
    Should Be Equal    ${mode}    typed
