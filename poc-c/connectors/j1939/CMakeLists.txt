# Step-0 validation harness for the j1939 connector. Linux-only (SocketCAN).
#
# Standalone mini-project (not part of poc-c's main build) because it compiles the
# runtime-fetched, gitignored Open-SAE-J1939 sources directly, with the SocketCAN
# platform selected. Non-selected platform backends are guarded out inside the library
# by OPENSAE_J1939_TARGET_PLATFORM.
#
#   ./scripts/fetch-lib.sh
#   cmake -B build -S . -G Ninja
#   cmake --build build
#   ./build/harness

cmake_minimum_required(VERSION 3.16)
project(j1939_step0 C)

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)

set(LIB_DIR ${CMAKE_CURRENT_SOURCE_DIR}/vendor/open-sae-j1939/Src)
if(NOT EXISTS ${LIB_DIR})
  message(FATAL_ERROR "vendor/ missing — run ./scripts/fetch-lib.sh first")
endif()

file(GLOB_RECURSE LIB_SRCS CONFIGURE_DEPENDS ${LIB_DIR}/*.c)

add_executable(harness harness.c ${LIB_SRCS})
target_include_directories(harness PRIVATE ${LIB_DIR})
target_compile_definitions(harness PRIVATE OPENSAE_J1939_TARGET_PLATFORM=SOCKETCAN)
target_compile_options(harness PRIVATE -Wall -Wextra -g)
