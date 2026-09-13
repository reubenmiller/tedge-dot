*** Settings ***
Documentation       Several connector instances of one protocol in one process, each owning its own
...                 device (docker-compose.multi-device.yaml: ten configs, ten simulated devices).
...
...                 Every instance subscribes to the command topics of the whole protocol, so it
...                 receives the commands of all the others as well. Each command must still be
...                 acted on by exactly ONE instance: a device command by the instance whose
...                 configuration defines the device, a management command by the service it is
...                 addressed to. When every instance answered, a fast "unknown point" failure from
...                 an instance that does not own the device raced — and usually beat — the owner's
...                 real result, and a define-device was written into every config file.
...
...                 The retained command topic only ever shows the last transition, so these tests
...                 assert on the whole history of a command: one `executing`, one result.

Resource            ../../_shared/stack.resource
Library             Collections

Suite Setup         Setup OT Stack    modbus    compose_file=${CURDIR}/../docker-compose.multi-device.yaml
Suite Teardown      Teardown OT Stack


*** Variables ***
# Must match MULTI_DEVICE_COUNT / SIM_DEVICES in docker-compose.multi-device.yaml.
${DEVICE_COUNT}         10
${PROTOCOL}             modbus
${CONFIG_DIR}           /etc/tedge-dot/multi-device

${READY_TIMEOUT}        90
${SAMPLE_TIMEOUT}       15
${COMMAND_TIMEOUT}      15
# How long a second responder is given to show itself once the expected result arrived. An
# instance that does not own the device fails the command without any protocol round-trip, so it
# answers well within this.
${SETTLE}               3s
# The flows container installs thin-edge from the main channel at build time; give it time.
${FLOWS_TIMEOUT}        120


*** Test Cases ***
Every Instance Connects To Its Own Simulated Device
    [Documentation]    Each instance reports its service up, and its device's samples carry the
    ...                number the simulator seeded into that device — ten distinct devices, not one
    ...                shared datastore, so the tests below can tell where a write really landed.
    FOR    ${n}    IN RANGE    1    ${DEVICE_COUNT} + 1
        Wait For Message Containing    te/device/main/service/tedge-dot-${n}/status/health
        ...    "status":"up"    timeout=${READY_TIMEOUT}
        Wait For Message Containing    te/device/plc-${n}/ot/${PROTOCOL}/status/link
        ...    "status":"connected"    timeout=${READY_TIMEOUT}
        Point Should Read    plc-${n}    device_id    ${n}
    END

Describe Merges The Parameter Set The Instances Share
    [Documentation]    `tedge-dot describe` over the stack's config directory covers all ten files and
    ...                renders ONE definition for them: the devices are one device type, a config
    ...                file each, and a DTM identifier is tenant-wide, so ten copies of the same
    ...                definition could not all be registered. The untyped-device warning (§5.2) is
    ...                likewise given once, naming the devices of every file.
    # The entrypoint renders the files before it starts the process, so the last instance being
    # up means all ten exist (this test may run on its own, straight after the stack starts).
    Wait For Message Containing    te/device/main/service/tedge-dot-${DEVICE_COUNT}/status/health
    ...    "status":"up"    timeout=${READY_TIMEOUT}
    ${stdout}    ${stderr}=    DeviceLibrary.Execute Command
    ...    cmd=tedge-dot describe -c ${CONFIG_DIR} --compact    stdout=${True}    stderr=${True}
    ${definitions}=    Evaluate
    ...    [json.loads(l) for l in $stdout.splitlines() if l.startswith("{")]    modules=json
    Length Should Be    ${definitions}    1
    Should Be Equal    ${definitions}[0][identifier]    modbus_control_parameters
    Dictionary Should Contain Key    ${definitions}[0][jsonSchema][properties]    temp_u16
    Should Contain X Times    ${stderr}    declare no `type`    1
    Should Contain    ${stderr}    plc-9
    Should Contain    ${stderr}    plc-10

A Write Is Handled Only By The Instance Owning The Device
    ${topic}=    Set Variable    te/device/plc-3/ot/${PROTOCOL}/cmd/write/w-1
    Publish Message    ${topic}    {"status":"init","point":"temp_u16","value":3003}    retain=True
    Command Should Be Handled Once    ${topic}    executing    successful
    Point Should Read    plc-3    temp_u16    3003
    # The neighbouring device kept its seeded value: the write reached plc-3's device alone.
    Point Should Read    plc-4    temp_u16    17001

A Write Batch Is Handled Only By The Instance Owning The Device
    [Documentation]    The shape of the reported failure: a parameter update (one write-batch) for
    ...                one device of many, completed as failed by the instances that do not own it.
    ${topic}=    Set Variable    te/device/plc-7/ot/${PROTOCOL}/cmd/write-batch/b-1
    Publish Message    ${topic}    {"status":"init","writes":[{"point":"temp_u16","value":7007}]}    retain=True
    Command Should Be Handled Once    ${topic}    executing    successful
    ${result}=    Get Message    ${topic}
    ${results}=    Get Json Field    ${result}    results
    Length Should Be    ${results}    1
    Point Should Read    plc-7    temp_u16    7007

A Command For A Device No Instance Owns Is Left Unanswered
    [Documentation]    An instance cannot tell a device that nobody owns from one that another
    ...                process on the same broker owns, so none of them answers for it (§6).
    ${topic}=    Set Variable    te/device/plc-99/ot/${PROTOCOL}/cmd/write/w-2
    Publish Message    ${topic}    {"status":"init","point":"temp_u16","value":1}    retain=True
    Sleep    ${SETTLE}
    ${transitions}=    Connector Transitions On    ${topic}
    Should Be Empty    ${transitions}

A Management Command Is Applied Only By The Service It Addresses
    [Documentation]    A management command changes ONE configuration file, so it is addressed to
    ...                that instance's service (§6.3) — not to a device topic every instance hears.
    ${device}=    New Device    plc-11    port=503
    ${topic}=    Set Variable    te/device/main/service/tedge-dot-2/ot/cmd/define-device/m-1
    Publish Message    ${topic}    {"status":"init","device":${device}}    retain=True
    Command Should Be Handled Once    ${topic}    executing    successful
    Configs Defining Device Should Be    plc-11    ${CONFIG_DIR}/plc-2.toml
    # plc-11 is wired to simulated device 2.
    Point Should Read    plc-11    device_id    2
    # Command ownership follows the reload: the new device's commands reach its new owner alone.
    ${write}=    Set Variable    te/device/plc-11/ot/${PROTOCOL}/cmd/write/w-3
    Publish Message    ${write}    {"status":"init","point":"temp_u16","value":1111}    retain=True
    Command Should Be Handled Once    ${write}    executing    successful

A Management Verb On A Device Topic Is Refused By The Owner Alone
    [Documentation]    A management verb published on a device topic is not applied by anyone:
    ...                the instance owning the device refuses it and names the service topic, and
    ...                no other instance answers.
    ${device}=    New Device    plc-12    port=502
    ${topic}=    Set Variable    te/device/plc-1/ot/${PROTOCOL}/cmd/define-device/m-2
    Publish Message    ${topic}    {"status":"init","device":${device}}    retain=True
    Command Should Be Handled Once    ${topic}    failed
    ${result}=    Get Message    ${topic}
    ${reason}=    Get Json Field    ${result}    reason
    Should Contain    ${reason}    te/device/main/service/
    Configs Defining Device Should Be    plc-12

A Device Is Removed Only By The Service It Is Addressed To
    ${topic}=    Set Variable    te/device/main/service/tedge-dot-2/ot/cmd/remove-device/m-3
    Publish Message    ${topic}    {"status":"init","device":"plc-11"}    retain=True
    Command Should Be Handled Once    ${topic}    executing    successful
    Configs Defining Device Should Be    plc-11

Flows Route A Management Command To The Service It Names
    [Documentation]    (flows) A thin-edge `ot_define_device` naming a `service` is bridged to that
    ...                instance's service topic, applied there alone, and its result completes the
    ...                thin-edge command on the device it was issued for.
    [Tags]    flows
    ${device}=    New Device    plc-13    port=506
    Publish Message    te/device/main///cmd/ot_define_device/f-1
    ...    {"status":"init","service":"tedge-dot-5","device":${device}}    retain=True
    Wait For Message Containing    te/device/main///cmd/ot_define_device/f-1    "status":"successful"
    ...    timeout=${FLOWS_TIMEOUT}
    Command Should Be Handled Once
    ...    te/device/main/service/tedge-dot-5/ot/cmd/define-device/ot--f-1    executing    successful
    Configs Defining Device Should Be    plc-13    ${CONFIG_DIR}/plc-5.toml


*** Keywords ***
Connector Transitions On
    [Documentation]    The statuses published on a command topic, oldest first, without the
    ...                requester's own `init` (this client is subscribed to everything, so it sees
    ...                its request echoed back).
    [Arguments]    ${topic}
    ${payloads}=    Get Messages    ${topic}
    ${transitions}=    Evaluate
    ...    [s for s in (json.loads(p).get("status") for p in $payloads if p) if s != "init"]
    ...    modules=json
    RETURN    ${transitions}

Command Should Be Handled Once
    [Documentation]    Wait for the command's final transition, give any second responder time to
    ...                show itself, then require exactly the `expected` sequence of transitions.
    [Arguments]    ${topic}    @{expected}
    Wait For Message Containing    ${topic}    "status":"${expected}[-1]"    timeout=${COMMAND_TIMEOUT}
    Sleep    ${SETTLE}
    ${transitions}=    Connector Transitions On    ${topic}
    Should Be Equal    ${transitions}    ${expected}
    ...    msg=expected one responder (${expected}), saw transitions ${transitions}    values=False

Point Should Read
    [Arguments]    ${device}    ${point}    ${expected}
    ${sample}=    Wait For Sample    te/device/${device}/ot/${PROTOCOL}/sample/${point}
    ...    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${sample}    value
    Should Be Equal As Integers    ${value}    ${expected}

Configs Defining Device Should Be
    [Documentation]    The config files of the stack that define `device`; none when no path is given.
    [Arguments]    ${device}    @{paths}
    ${output}=    DeviceLibrary.Execute Command
    ...    cmd=grep -l 'name *= *"${device}"' ${CONFIG_DIR}/*.toml || true    strip=${True}
    ${found}=    Evaluate    sorted($output.split())
    ${paths}=    Evaluate    sorted($paths)
    Should Be Equal    ${found}    ${paths}    msg=config files defining ${device}: ${found}    values=False

New Device
    [Documentation]    A define-device `device` object (as JSON) wired to the simulated device on `port`,
    ...                with the same points as the stack's own devices.
    [Arguments]    ${name}    ${port}
    ${device}=    Evaluate
    ...    json.dumps({"name": $name, "protocol_address": {"transport": "tcp", "host": "simulator", "port": int($port), "unit_id": 1}, "default_mode": "typed", "point": [{"id": "device_id", "datatype": "uint16", "address": {"table": "holding", "address": 2100, "count": 1}}, {"id": "temp_u16", "datatype": "uint16", "access": "read_write", "address": {"table": "holding", "address": 3, "count": 1}}]})
    ...    modules=json
    RETURN    ${device}
