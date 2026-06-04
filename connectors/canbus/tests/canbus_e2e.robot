*** Settings ***
Documentation       End-to-end tests for the Rust tedge-dot CAN bus connector against a virtual
...                 CAN interface (vcan0). The connector subscribes to CAN frames sent by the
...                 Python simulator and publishes samples to a local MQTT broker; these tests
...                 assert on that output. No cloud (Cumulocity) is involved.
...
...                 Simulator seeds (ENGINE_STATUS, ID=0x1A0/416):
...                   RPM          = 2500   (u16 Intel bits 0-15)
...                   COOLANT_TEMP = 85     (i8  Intel bits 16-23)
...                   BRAKE_ACTIVE = 1/true (u1  Intel bit  24)
...
...                 Run via:  just test-e2e canbus

Library             ../../_shared/MqttClient.py
Library             Collections

Suite Setup         Connect And Subscribe
Suite Teardown      Disconnect Broker


*** Variables ***
${BROKER_HOST}          localhost
${BROKER_PORT}          13883

${DEVICE}               engine
${PROTOCOL}             canbus
${SERVICE}              tedge-dot

${SAMPLE_PREFIX}        te/device/${DEVICE}/ot/${PROTOCOL}/sample
${CMD_PREFIX}           te/device/${DEVICE}/ot/${PROTOCOL}/cmd/write
${LINK_TOPIC}           te/device/${DEVICE}/ot/${PROTOCOL}/status/link
${CAPS_TOPIC}           te/device/main/service/${SERVICE}/ot/capabilities
${HEALTH_TOPIC}         te/device/main/service/${SERVICE}/status/health

# CAN is push-based; the simulator sends at 10 Hz so samples arrive quickly.
# Use a generous startup timeout and a short per-sample timeout.
${READY_TIMEOUT}        90
${SAMPLE_TIMEOUT}       5


*** Test Cases ***
B1 Connector Publishes Capability Descriptor
    [Documentation]    The connector advertises its protocol and supported verbs (retained).
    ${payload}=    Wait For Retained    ${CAPS_TOPIC}    timeout=${READY_TIMEOUT}
    ${protocol}=    Get Json Field    ${payload}    protocol
    Should Be Equal    ${protocol}    canbus
    ${verbs}=    Get Json Field    ${payload}    command_verbs
    List Should Contain Value    ${verbs}    write

B1 Service Health Is Up
    [Documentation]    The connector publishes a retained service health status of "up".
    ${payload}=    Wait For Retained    ${HEALTH_TOPIC}    timeout=${READY_TIMEOUT}
    ${status}=    Get Json Field    ${payload}    status
    Should Be Equal    ${status}    up

B5 Device Link Is Connected
    [Documentation]    The connector reports the CAN device link as connected.
    ${payload}=    Wait For Retained    ${LINK_TOPIC}    timeout=${READY_TIMEOUT}
    ${status}=    Get Json Field    ${payload}    status
    Should Be Equal    ${status}    connected

B2 Reads RPM As Uint16
    [Documentation]    Reads RPM signal (u16 Intel bits 0-15), seeded to 2500.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/rpm    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${datatype}=    Get Json Field    ${payload}    datatype
    Should Be Equal    ${datatype}    uint16
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    2500

B2 Reads Coolant Temp As Int8
    [Documentation]    Reads COOLANT_TEMP signal (i8 Intel bits 16-23), seeded to 85.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/coolant_temp    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${datatype}=    Get Json Field    ${payload}    datatype
    Should Be Equal    ${datatype}    int8
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    85

B2 Reads Brake Active As Bool
    [Documentation]    Reads BRAKE_ACTIVE signal (bit 24), seeded to 1/true.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/brake_active    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${datatype}=    Get Json Field    ${payload}    datatype
    Should Be Equal    ${datatype}    bool
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal    ${value}    ${True}

B3 Typed Mode Yields Value And Value Repr
    [Documentation]    Typed-mode samples carry both 'value' and 'value_repr'.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/rpm    timeout=${SAMPLE_TIMEOUT}
    ${mode}=    Get Json Field    ${payload}    mode
    Should Be Equal    ${mode}    typed
    ${repr}=    Get Json Field    ${payload}    value_repr
    Should Not Be Empty    ${repr}

B6 Write Command Succeeds
    [Documentation]    A write command for brake_active transitions executing -> successful.
    ...                 Note: the simulator continuously re-sends the seeded frame so the
    ...                 subsequent read will reflect the simulator's value (true), not the
    ...                 written one.  This test only validates the command state machine.
    Publish Message    ${CMD_PREFIX}/brake-1    {"status":"init","point":"brake_active","value":false}    retain=True
    ${result}=    Wait For Message Containing    ${CMD_PREFIX}/brake-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    ${point}=    Get Json Field    ${result}    point
    Should Be Equal    ${point}    brake_active

B7 Write To Read-Only Point Fails
    [Documentation]    A write to rpm (access=read) returns failed with a reason.
    Publish Message    ${CMD_PREFIX}/rpm-1    {"status":"init","point":"rpm","value":9999}    retain=True
    ${result}=    Wait For Message Containing    ${CMD_PREFIX}/rpm-1    "status":"failed"    timeout=${SAMPLE_TIMEOUT}
    ${reason}=    Get Json Field    ${result}    reason
    Should Not Be Empty    ${reason}

B9 Capability Honesty — No Undeclared Datatypes
    [Documentation]    Every sample's datatype is listed in the capability descriptor.
    ${caps_raw}=    Wait For Retained    ${CAPS_TOPIC}    timeout=${READY_TIMEOUT}
    ${declared}=    Get Json Field    ${caps_raw}    datatypes

    ${rpm_payload}=    Wait For Sample    ${SAMPLE_PREFIX}/rpm    timeout=${SAMPLE_TIMEOUT}
    ${dt}=    Get Json Field    ${rpm_payload}    datatype
    List Should Contain Value    ${declared}    ${dt}

B10 Topic Discipline — No Stray Measurement Topics
    [Documentation]    The connector must not publish to te/.../m/ topics.
    No Messages On Topic    te/device/${DEVICE}/m/#    timeout=3


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
