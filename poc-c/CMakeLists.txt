cmake_minimum_required(VERSION 3.16)
project(tedge-dot-poc C)

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)
if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE MinSizeRel) # PoC goal: small binaries
endif()

# Protocol feature flags (mirror the Rust cargo features). The SocketCAN
# connectors are Linux-only, like their Rust counterparts.
option(TDOT_MODBUS "Build the Modbus connector (libmodbus, LGPL-2.1+)" ON)
option(TDOT_OPCUA "Build the OPC UA connector (open62541, MPL-2.0)" ON)
option(TDOT_PROFIBUS "Build the PROFIBUS-DP connector (built-in DP master)" ON)
if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
  option(TDOT_CANBUS "Build the CAN bus connector (SocketCAN + DBC)" ON)
  option(TDOT_CANOPEN "Build the CANopen connector (SDO over SocketCAN)" ON)
  option(TDOT_J1939 "Build the J1939 connector (Open-SAE-J1939, MIT)" ON)
else()
  set(TDOT_CANBUS OFF)
  set(TDOT_CANOPEN OFF)
  set(TDOT_J1939 OFF)
endif()

find_package(PkgConfig REQUIRED)
find_package(Threads REQUIRED)
pkg_check_modules(MOSQUITTO REQUIRED IMPORTED_TARGET libmosquitto)
pkg_check_modules(CJSON REQUIRED IMPORTED_TARGET libcjson)

# --- vendored tomlc99 (MIT) --------------------------------------------------
add_library(tomlc99 STATIC third_party/tomlc99/toml.c)
target_include_directories(tomlc99 PUBLIC third_party/tomlc99)

# --- SDK ----------------------------------------------------------------------
add_library(tedge_dot_sdk STATIC
  sdk/src/model.c
  sdk/src/decode.c
  sdk/src/config.c
  sdk/src/envelope.c
  sdk/src/runtime.c
  sdk/src/factory.c
)
target_include_directories(tedge_dot_sdk PUBLIC sdk/include)
target_link_libraries(tedge_dot_sdk PUBLIC tomlc99 PkgConfig::CJSON
                      PkgConfig::MOSQUITTO m Threads::Threads)

# --- connectors ----------------------------------------------------------------
if(TDOT_MODBUS)
  pkg_check_modules(MODBUS REQUIRED IMPORTED_TARGET libmodbus)
  target_sources(tedge_dot_sdk PRIVATE connectors/modbus/connector_modbus.c)
  target_link_libraries(tedge_dot_sdk PUBLIC PkgConfig::MODBUS)
  target_compile_definitions(tedge_dot_sdk PUBLIC TDOT_FEATURE_MODBUS)
endif()

if(TDOT_OPCUA)
  # Prefer an installed open62541; fall back to building v1.5.5 from source
  # (client-only, reduced feature set) so CI needs no distro package.
  find_package(open62541 QUIET)
  if(NOT open62541_FOUND)
    include(FetchContent)
    set(UA_ENABLE_SUBSCRIPTIONS ON CACHE BOOL "" FORCE)
    set(UA_ENABLE_METHODCALLS OFF CACHE BOOL "" FORCE)
    set(UA_ENABLE_NODEMANAGEMENT OFF CACHE BOOL "" FORCE)
    set(UA_ENABLE_AMALGAMATION OFF CACHE BOOL "" FORCE)
    set(UA_ENABLE_ENCRYPTION OFF CACHE BOOL "" FORCE)
    set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)
    FetchContent_Declare(open62541
      GIT_REPOSITORY https://github.com/open62541/open62541.git
      GIT_TAG v1.5.5
      GIT_SHALLOW TRUE)
    FetchContent_MakeAvailable(open62541)
  endif()
  target_sources(tedge_dot_sdk PRIVATE connectors/opcua/connector_opcua.c)
  target_link_libraries(tedge_dot_sdk PUBLIC open62541::open62541)
  target_compile_definitions(tedge_dot_sdk PUBLIC TDOT_FEATURE_OPCUA)
endif()

if(TDOT_CANBUS)
  target_sources(tedge_dot_sdk PRIVATE connectors/canbus/connector_canbus.c
                                       connectors/canbus/dbc.c)
  target_compile_definitions(tedge_dot_sdk PUBLIC TDOT_FEATURE_CANBUS)
endif()

if(TDOT_CANOPEN)
  target_sources(tedge_dot_sdk PRIVATE connectors/canopen/connector_canopen.c)
  target_compile_definitions(tedge_dot_sdk PUBLIC TDOT_FEATURE_CANOPEN)
endif()

if(TDOT_J1939)
  # Vendor Open-SAE-J1939 (MIT) via FetchContent, download-only: SOURCE_SUBDIR
  # points at a non-existent dir so the library's own CMake is not run — we
  # compile its Src/*.c straight into the SDK with the SocketCAN platform
  # selected (non-selected backends are #ifdef'd out by the platform macro).
  include(FetchContent)
  FetchContent_Declare(open_sae_j1939
    GIT_REPOSITORY https://github.com/DanielMartensson/Open-SAE-J1939.git
    GIT_TAG 315a9556692c2c58fe6909749811bb380698ef36 # pinned (2026-06-25)
    SOURCE_SUBDIR _download_only_)
  FetchContent_MakeAvailable(open_sae_j1939)
  file(GLOB_RECURSE J1939_LIB_SRCS ${open_sae_j1939_SOURCE_DIR}/Src/*.c)
  # Exclude the library's example entry point (has its own main(), guarded out
  # under SOCKETCAN but excluded for cleanliness).
  list(FILTER J1939_LIB_SRCS EXCLUDE REGEX "/Main\\.c$")

  target_sources(tedge_dot_sdk PRIVATE
                 connectors/j1939/connector_j1939.c ${J1939_LIB_SRCS})
  # Reuse the canbus DBC parser for SPN bit layout (add it once if canbus is off).
  if(NOT TDOT_CANBUS)
    target_sources(tedge_dot_sdk PRIVATE connectors/canbus/dbc.c)
  endif()
  target_include_directories(tedge_dot_sdk PRIVATE
                             ${open_sae_j1939_SOURCE_DIR}/Src
                             connectors/canbus)
  target_compile_definitions(tedge_dot_sdk PUBLIC
                             TDOT_FEATURE_J1939
                             OPENSAE_J1939_TARGET_PLATFORM=SOCKETCAN)
endif()

if(TDOT_PROFIBUS)
  target_sources(tedge_dot_sdk PRIVATE
                 connectors/profibus/connector_profibus.c)
  target_compile_definitions(tedge_dot_sdk PUBLIC TDOT_FEATURE_PROFIBUS)
endif()

# --- binary ---------------------------------------------------------------------
add_executable(tedge-dot src/main.c)
target_link_libraries(tedge-dot PRIVATE tedge_dot_sdk)

# --- tests -----------------------------------------------------------------------
enable_testing()
add_executable(tedge-dot-golden tests/golden.c)
target_link_libraries(tedge-dot-golden PRIVATE tedge_dot_sdk)
add_test(NAME golden-vectors
         COMMAND tedge-dot-golden
                 ${CMAKE_CURRENT_SOURCE_DIR}/../crates/sdk/conformance/vectors.json)

# Strip in release-style builds for a honest size comparison
if(CMAKE_BUILD_TYPE STREQUAL "MinSizeRel" AND NOT APPLE)
  target_link_options(tedge-dot PRIVATE -s)
endif()
