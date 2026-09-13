*** Settings ***
Documentation       Stack lifecycle for the connector e2e suites: DeviceLibrary starts the
...                 protocol's docker compose file as an isolated project (named after a
...                 randomly generated device serial) and tears it down when the suite ends,
...                 so no stack has to be started before the tests run.
...
...                 Use it per suite (one stack for the whole file):
...                 | Suite Setup       Setup OT Stack    modbus
...                 | Suite Teardown    Teardown OT Stack
...
...                 or per test (a fresh stack for every test case — slower, but fully
...                 isolated; the containers of each test are reclaimed when the suite ends):
...                 | Test Setup        Setup OT Stack    modbus
...                 | Test Teardown     Teardown OT Stack
...
...                 IMPLEMENTATION PARITY
...
...                 The same suites run against both implementations (IMPL=rust|c, see
...                 `just test-e2e` / `just test-e2e-c` and the robot.toml profiles). That is
...                 how parity is proven, so a test belongs to BOTH unless the feature it
...                 covers genuinely cannot exist in one of them.
...
...                 When that happens, tag the test with the capability it needs:
...                 | [Tags]    requires:subscribe
...
...                 The runner turns the capabilities an implementation lacks into
...                 `robot --skip requires:<capability>`, so the test is reported as SKIPPED
...                 for that implementation instead of failing or being silently dropped. The
...                 list of what each implementation lacks lives in ONE place — the
...                 C_MISSING_CAPABILITIES variable in the justfile — and is mirrored by the
...                 parity table in impl/c/README.md.
...
...                 Two rules for such a test: it must FAIL (not pass) on an implementation
...                 lacking the capability if the tag were removed — otherwise it is not really
...                 testing the feature — and the capability must be a genuine limitation, not
...                 a shortcut around a bug.

Library             DeviceLibrary
Library             MqttClient.py
Library             OperatingSystem


*** Variables ***
# The broker publishes its MQTT port without a fixed host port, so every stack gets an
# ephemeral one and parallel runs cannot clash; `Setup OT Stack` resolves it. The host-networked
# stacks (canbus, canopen) instead pass `broker_port` explicitly — see the keyword.
${BROKER_CONTAINER_PORT}        1883
# Seconds allowed for `docker compose up --wait` (image builds are not counted against it).
${STACK_WAIT_TIMEOUT}           240
# Set to a truthy value to leave the stack running after the suite, to inspect it by hand:
#   robot --variable KEEP_STACK:true --test "..." connectors/modbus/tests/
# The project name is logged by the setup (docker compose -p <project> logs -f).
${KEEP_STACK}                   ${EMPTY}


*** Keywords ***
Setup OT Stack
    [Documentation]    Start the protocol's compose stack and subscribe to the broker.
    ...    `broker_port` pins the host port for stacks whose broker runs in the host network
    ...    namespace (canbus, canopen); leave it empty to resolve the published ephemeral port.
    ...    `compose_file` selects another stack of the protocol (e.g. its multi-device one);
    ...    empty means `connectors/<protocol>/docker-compose.yaml`.
    [Arguments]    ${protocol}    ${broker_port}=${EMPTY}    ${device_service}=${EMPTY}
    ...    ${compose_file}=${EMPTY}

    ${compose_file}=    Set Variable If    "${compose_file}" == "${EMPTY}"
    ...    ${CURDIR}/../${protocol}/docker-compose.yaml    ${compose_file}
    ${cleanup}=    Evaluate    not $KEEP_STACK
    ${serial}=    DeviceLibrary.Setup
    ...    compose_file=${compose_file}
    ...    device_service=${device_service}
    ...    skip_bootstrap=${True}
    ...    build=${True}
    ...    wait_timeout=${STACK_WAIT_TIMEOUT}
    ...    cleanup=${cleanup}
    Set Suite Variable    $STACK_SERIAL    ${serial}
    ${project}=    DeviceLibrary.Get Compose Project Name
    Log    Started ${protocol} stack as compose project '${project}' (device ${serial})
    # Which implementation the stack built (see robot.toml profiles / just test-e2e-c):
    # IMPL selects the image name, CONNECTOR_DOCKERFILE the build recipe.
    ${impl}=    Get Environment Variable    IMPL    rust
    ${container}=    DeviceLibrary.Get Container Name
    Log    Connector under test: ${impl} implementation (container ${container})
    Connector Implementation Should Be    ${impl}

    # Before BROKER_PORT is set below: Robot variable names ignore case, so setting that suite
    # variable also overwrites this keyword's own `${broker_port}` argument.
    Set Suite Variable    $BROKER_PORT_PINNED    ${broker_port}
    IF    "${broker_port}" == "${EMPTY}"
        ${host}    ${port}=    DeviceLibrary.Get Service Port
        ...    service=broker    port=${BROKER_CONTAINER_PORT}
    ELSE
        ${host}    ${port}=    Set Variable    localhost    ${broker_port}
    END
    Set Suite Variable    $BROKER_HOST    ${host}
    Set Suite Variable    $BROKER_PORT    ${port}
    Log    Broker reachable on ${host}:${port}

    Connect Broker    ${host}    ${port}
    Subscribe    te/#

Teardown OT Stack
    [Documentation]    Disconnect from the broker and, when the test failed, dump the stack's
    ...    container logs into the Robot log. DeviceLibrary stops and removes the compose
    ...    project (containers, network, volumes) itself when the suite ends.
    Run Keyword And Ignore Error    Collect Stack Logs
    Run Keyword And Ignore Error    Disconnect Broker

Connector Implementation Should Be
    [Documentation]    Assert the container really runs the implementation under test.
    ...
    ...    Without this the fallback is silent: the compose file defaults IMPL to `rust` and
    ...    CONNECTOR_DOCKERFILE to the stack's own Rust Dockerfile, so any path where the
    ...    variables do not reach compose (an editor profile, a stale shell, a wrapper that
    ...    forgets to export) builds the Rust connector and reports a green run as coverage of
    ...    the other implementation. The cloud harness has had this guard for the same reason
    ...    (cloud/_shared/device.resource); the connector suites had only a Log line.
    ...
    ...    The two are told apart by linkage rather than by version — both deliberately report
    ...    the same capability `version`, because they implement the same contract revision.
    ...    The C build dynamically links libcjson (see impl/c/README.md); the Rust build is
    ...    self-contained.
    [Arguments]    ${impl}
    ${links_cjson}=    DeviceLibrary.Execute Command
    ...    cmd=ldd /usr/bin/tedge-dot 2>/dev/null | grep -c libcjson || true    strip=${True}
    IF    "${impl}" == "c"
        Should Not Be Equal    ${links_cjson}    0
        ...    msg=IMPL=c but /usr/bin/tedge-dot does not link libcjson - this is the Rust build
    ELSE
        Should Be Equal    ${links_cjson}    0
        ...    msg=IMPL=${impl} but /usr/bin/tedge-dot links libcjson - this is the C build
    END

Run Compose
    [Documentation]    Run a docker compose subcommand against THIS suite's stack (the project
    ...    name is random per suite, so it has to be resolved at runtime).
    [Arguments]    ${args}
    ${project}=    DeviceLibrary.Get Compose Project Name
    ${rc}=    Run And Return Rc    docker compose -p ${project} ${args}
    Should Be Equal As Integers    ${rc}    ${0}
    ...    msg=`docker compose -p ${project} ${args}` failed
    RETURN    ${project}

Restart Stack Service
    [Documentation]    Restart one service of the stack — the peer goes away and comes back,
    ...    dropping the transport with it. Use it to exercise the connector's reconnect path.
    [Arguments]    ${service}
    Run Compose    restart ${service}

Freeze Stack Service
    [Documentation]    SIGSTOP one service of the stack. Unlike a restart this leaves the TCP
    ...    connection ESTABLISHED and simply stops all answers, which is the harder failure to
    ...    detect: nothing tells the client the peer is gone. Thaw it with `Thaw Stack Service`.
    [Arguments]    ${service}
    Run Compose    pause ${service}

Thaw Stack Service
    [Documentation]    Resume a service frozen with `Freeze Stack Service`.
    [Arguments]    ${service}
    Run Compose    unpause ${service}

Stop Stack Service
    [Documentation]    Stop one service of the stack; it stays down until `Start Stack Service`.
    [Arguments]    ${service}
    Run Compose    stop ${service}

Start Stack Service
    [Documentation]    Start a service stopped with `Stop Stack Service`.
    [Arguments]    ${service}
    Run Compose    start ${service}

Restart Broker
    [Documentation]    Restart the stack's MQTT broker and reconnect the suite's client to it.
    ...    The test broker has no persistence, so the restarted broker holds no retained
    ...    messages: any retained state seen afterwards was published again by a client that
    ...    noticed the restart.
    Restart Stack Service    broker
    Reconnect To Broker

Reconnect To Broker
    [Documentation]    Drop the suite's client, forget everything it recorded and connect again.
    ...    A restarted broker container can be published on a different ephemeral host port, so
    ...    the port is resolved again unless the stack pinned it.
    Run Keyword And Ignore Error    Disconnect Broker
    Clear Messages
    IF    "${BROKER_PORT_PINNED}" == "${EMPTY}"
        ${host}    ${port}=    DeviceLibrary.Get Service Port
        ...    service=broker    port=${BROKER_CONTAINER_PORT}
        Set Suite Variable    $BROKER_HOST    ${host}
        Set Suite Variable    $BROKER_PORT    ${port}
    END
    Wait Until Keyword Succeeds    30s    1s    Connect Broker    ${BROKER_HOST}    ${BROKER_PORT}
    Subscribe    te/#

Collect Stack Logs
    [Documentation]    Container logs of every service of the stack (docker compose logs).
    DeviceLibrary.Get Service Logs

Retained Message Should Be Cleared
    [Documentation]    The latest message seen on a retained topic is an empty payload — the
    ...                clearing message a connector publishes for a device that is gone (§8.2).
    ...                Waits for it, since it follows the command result on another topic.
    [Arguments]    ${topic}    ${timeout}=10s
    Wait Until Keyword Succeeds    ${timeout}    500ms    Latest Message Should Be Empty    ${topic}

Latest Message Should Be Empty
    [Arguments]    ${topic}
    ${payload}=    Get Message    ${topic}
    Should Be Empty    ${payload}    ${topic} still carries a retained payload
