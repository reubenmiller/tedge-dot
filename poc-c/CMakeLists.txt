cmake_minimum_required(VERSION 3.16)
project(tedge-dot-poc C)

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)
if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE MinSizeRel) # PoC goal: small binaries
endif()

# Protocol feature flags (mirror the Rust cargo features)
option(TDOT_MODBUS "Build the Modbus connector (libmodbus, LGPL-2.1+)" ON)
option(TDOT_OPCUA "Build the OPC UA connector (open62541, MPL-2.0)" ON)

find_package(PkgConfig REQUIRED)
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
                      PkgConfig::MOSQUITTO m)

# --- connectors ----------------------------------------------------------------
if(TDOT_MODBUS)
  pkg_check_modules(MODBUS REQUIRED IMPORTED_TARGET libmodbus)
  target_sources(tedge_dot_sdk PRIVATE connectors/modbus/connector_modbus.c)
  target_link_libraries(tedge_dot_sdk PUBLIC PkgConfig::MODBUS)
  target_compile_definitions(tedge_dot_sdk PUBLIC TDOT_FEATURE_MODBUS)
endif()

if(TDOT_OPCUA)
  find_package(open62541 REQUIRED)
  target_sources(tedge_dot_sdk PRIVATE connectors/opcua/connector_opcua.c)
  target_link_libraries(tedge_dot_sdk PUBLIC open62541::open62541)
  target_compile_definitions(tedge_dot_sdk PUBLIC TDOT_FEATURE_OPCUA)
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
