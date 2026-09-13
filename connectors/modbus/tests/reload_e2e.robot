*** Settings ***
Documentation       SIGHUP reloads the connector configuration without restarting the service —
...                 what `systemctl reload tedge-dot` sends through the unit's ExecReload.
...
...                 Runs against the multi-device stack (docker-compose.multi-device.yaml): one
...                 process running a directory of ten connector configs, one per simulated device,
...                 which is the layout a reload has to handle file by file:
...                 - every running connector re-reads its own file and applies what changed in
...                 place, while a connector whose file is unchanged is left alone;
...                 - a new file starts a connector, and a removed file stops one;
...                 - a file that cannot be used is reported and the running configuration kept;
...                 - a change a connector cannot adopt in place (its service name) restarts it.
...
...                 The process is PID 1 of the connector container: the entrypoint execs it.

Resource            ../../_shared/stack.resource
Library             Collections

Suite Setup         Setup OT Stack    modbus    compose_file=${CURDIR}/../docker-compose.multi-device.yaml
Suite Teardown      Teardown OT Stack


*** Variables ***
# Must match MULTI_DEVICE_COUNT / SIM_DEVICES in docker-compose.multi-device.yaml.
${DEVICE_COUNT}         10
${PROTOCOL}             modbus
${CONFIG_DIR}           /etc/tedge-dot/multi-device
${TEMPLATE}             /usr/share/tedge-dot/multi-device/device.toml.template

${READY_TIMEOUT}        90
${SAMPLE_TIMEOUT}       15
# Time for every connector to act on a reload: re-reading a file and reconnecting a device takes
# well under a second against the simulator, and each polls once a second.
${SETTLE}               5s


*** Test Cases ***
Every Connector Is Up Before Reloading
    FOR    ${n}    IN RANGE    1    ${DEVICE_COUNT} + 1
        Wait For Message Containing    te/device/main/service/tedge-dot-${n}/status/health
        ...    "status":"up"    timeout=${READY_TIMEOUT}
        Wait For Message Containing    te/device/plc-${n}/ot/${PROTOCOL}/status/link
        ...    "status":"connected"    timeout=${READY_TIMEOUT}
    END

A Changed Config Is Applied In Place
    [Documentation]    A point added to plc-3's file is sampled after a reload, without restarting
    ...                its connector: its service health never goes down. The connectors whose files
    ...                did not change are left alone entirely — their devices are not reconnected,
    ...                so no link status is published for them.
    DeviceLibrary.Execute Command
    ...    cmd=printf '\\n[[device.point]]\\nid = "reloaded_u16"\\ndatatype = "uint16"\\naddress = { table = "holding", address = 3, count = 1 }\\n' >> ${CONFIG_DIR}/plc-3.toml
    Clear Messages
    Send Reload
    Wait For Sample    te/device/plc-3/ot/${PROTOCOL}/sample/reloaded_u16    timeout=${SAMPLE_TIMEOUT}
    Sleep    ${SETTLE}
    Health Should Not Have Gone Down    tedge-dot-3
    ${links}=    Get Messages    te/device/plc-4/ot/${PROTOCOL}/status/link
    Should Be Empty    ${links}    an unchanged connector must not reconnect its device on a reload

A New Config File Starts A Connector
    [Documentation]    A file added to the directory starts a connector on the next reload (plc-11,
    ...                wired to simulated device 1).
    DeviceLibrary.Execute Command
    ...    cmd=sed -e 's/@N@/11/g' -e 's/@PORT@/502/g' ${TEMPLATE} > ${CONFIG_DIR}/plc-11.toml
    Send Reload
    Wait For Message Containing    te/device/main/service/tedge-dot-11/status/health
    ...    "status":"up"    timeout=${SAMPLE_TIMEOUT}
    Point Should Read    plc-11    device_id    1

A Removed Config File Stops Its Connector
    [Documentation]    A file removed from the directory stops its connector on the next reload: the
    ...                service reports itself down, and the device is no longer polled.
    DeviceLibrary.Execute Command    cmd=rm ${CONFIG_DIR}/plc-11.toml
    Send Reload
    Wait For Message Containing    te/device/main/service/tedge-dot-11/status/health
    ...    "status":"down"    timeout=${SAMPLE_TIMEOUT}
    No New Messages On Topic    te/device/plc-11/ot/${PROTOCOL}/sample/device_id    timeout=5

An Unusable Config Keeps The Running Connector
    [Documentation]    A file that no longer loads is reported, and its connector keeps running on
    ...                the configuration it had: still polling, never restarted.
    DeviceLibrary.Execute Command
    ...    cmd=cp ${CONFIG_DIR}/plc-5.toml /tmp/plc-5.toml && printf '[[[\\n' >> ${CONFIG_DIR}/plc-5.toml
    Clear Messages
    Send Reload
    Sleep    ${SETTLE}
    Wait For Sample    te/device/plc-5/ot/${PROTOCOL}/sample/device_id    timeout=${SAMPLE_TIMEOUT}
    Health Should Not Have Gone Down    tedge-dot-5
    [Teardown]    Restore Config    plc-5

A Changed Service Name Restarts The Connector
    [Documentation]    The service name addresses a connector's MQTT session, last will and
    ...                management topic, so a changed one cannot be adopted in place: the connector
    ...                restarts under the new name — the old service goes down, the new one comes up
    ...                — and polls its device again.
    DeviceLibrary.Execute Command
    ...    cmd=sed -i 's/^service_name *= *"tedge-dot-6"/service_name = "tedge-dot-6b"/' ${CONFIG_DIR}/plc-6.toml
    Clear Messages
    Send Reload
    Wait For Message Containing    te/device/main/service/tedge-dot-6/status/health
    ...    "status":"down"    timeout=${SAMPLE_TIMEOUT}
    Wait For Message Containing    te/device/main/service/tedge-dot-6b/status/health
    ...    "status":"up"    timeout=${SAMPLE_TIMEOUT}
    Wait For Sample    te/device/plc-6/ot/${PROTOCOL}/sample/device_id    timeout=${SAMPLE_TIMEOUT}

A Config That Could Not Start Is Tried Again On Reload
    [Documentation]    A new file that does not load starts nothing; once it is fixed, the next
    ...                reload starts its connector.
    DeviceLibrary.Execute Command    cmd=printf '[[[\\n' > ${CONFIG_DIR}/plc-12.toml
    Clear Messages
    Send Reload
    Sleep    ${SETTLE}
    ${health}=    Get Messages    te/device/main/service/tedge-dot-12/status/health
    Should Be Empty    ${health}    a config that does not load must not start a connector
    DeviceLibrary.Execute Command
    ...    cmd=sed -e 's/@N@/12/g' -e 's/@PORT@/503/g' ${TEMPLATE} > ${CONFIG_DIR}/plc-12.toml
    Send Reload
    Wait For Message Containing    te/device/main/service/tedge-dot-12/status/health
    ...    "status":"up"    timeout=${SAMPLE_TIMEOUT}
    Point Should Read    plc-12    device_id    2


*** Keywords ***
Send Reload
    [Documentation]    SIGHUP to the tedge-dot process, PID 1 of the connector container.
    DeviceLibrary.Execute Command    cmd=kill -HUP 1

Health Should Not Have Gone Down
    [Documentation]    No health message recorded for `service` reports it down.
    [Arguments]    ${service}
    ${payloads}=    Get Messages    te/device/main/service/${service}/status/health
    FOR    ${payload}    IN    @{payloads}
        Should Not Contain    ${payload}    "down"    ${service} restarted instead of reloading
    END

Point Should Read
    [Arguments]    ${device}    ${point}    ${expected}
    ${sample}=    Wait For Sample    te/device/${device}/ot/${PROTOCOL}/sample/${point}
    ...    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${sample}    value
    Should Be Equal As Integers    ${value}    ${expected}

Restore Config
    [Documentation]    Put back the copy of `name`'s config saved in /tmp, and reload: the restored
    ...                file matches what is running, so the reload changes nothing.
    [Arguments]    ${name}
    DeviceLibrary.Execute Command    cmd=cp /tmp/${name}.toml ${CONFIG_DIR}/${name}.toml
    Send Reload
