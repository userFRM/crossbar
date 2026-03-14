# Copyright (c) 2026 The Crossbar Contributors
#
# This source code is licensed under the MIT license or Apache License 2.0,
# at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
# root for details.
#
# SPDX-License-Identifier: MIT OR Apache-2.0

cmake_minimum_required(VERSION 3.14)
project(crossbar-cpp-sdk
    VERSION     0.1.0
    LANGUAGES   CXX
    DESCRIPTION "C++17 RAII bindings for crossbar zero-copy IPC"
)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

# ---------------------------------------------------------------------------
# Find libcrossbar_ffi
# ---------------------------------------------------------------------------
# Users can point to the FFI library via:
#   -DCROSSBAR_FFI_DIR=/path/to/dir        (directory containing libcrossbar_ffi.{a,so,dylib})
#   -DCROSSBAR_FFI_INCLUDE_DIR=/path/to    (directory containing crossbar.h)
#
# Defaults assume a Cargo build at the project root:
#   ${PROJECT_SOURCE_DIR}/../../target/release    (library)
#   ${PROJECT_SOURCE_DIR}/../../crossbar-ffi/include  (header)

set(CROSSBAR_FFI_DIR
    "${PROJECT_SOURCE_DIR}/../../target/release"
    CACHE PATH "Directory containing libcrossbar_ffi")

set(CROSSBAR_FFI_INCLUDE_DIR
    "${PROJECT_SOURCE_DIR}/../../crossbar-ffi/include"
    CACHE PATH "Directory containing crossbar.h")

# Imported library target
add_library(crossbar_ffi STATIC IMPORTED)

find_library(CROSSBAR_FFI_LIB
    NAMES crossbar_ffi
    PATHS "${CROSSBAR_FFI_DIR}"
    NO_DEFAULT_PATH
)

if(CROSSBAR_FFI_LIB)
    set_target_properties(crossbar_ffi PROPERTIES
        IMPORTED_LOCATION "${CROSSBAR_FFI_LIB}"
    )
    message(STATUS "Found libcrossbar_ffi: ${CROSSBAR_FFI_LIB}")
else()
    message(WARNING
        "libcrossbar_ffi not found in ${CROSSBAR_FFI_DIR}. "
        "Build the Rust crate first:  cargo build --release -p crossbar-ffi"
    )
    # Set a placeholder so the build file generation still works; linking
    # will fail at build time with a clear error.
    set_target_properties(crossbar_ffi PROPERTIES
        IMPORTED_LOCATION "${CROSSBAR_FFI_DIR}/libcrossbar_ffi.a"
    )
endif()

set_target_properties(crossbar_ffi PROPERTIES
    INTERFACE_INCLUDE_DIRECTORIES "${CROSSBAR_FFI_INCLUDE_DIR}"
)

# ---------------------------------------------------------------------------
# Header-only SDK target
# ---------------------------------------------------------------------------
add_library(crossbar-cpp-sdk INTERFACE)
target_include_directories(crossbar-cpp-sdk INTERFACE
    "${PROJECT_SOURCE_DIR}/include"
)
target_link_libraries(crossbar-cpp-sdk INTERFACE crossbar_ffi)

# Platform-specific link dependencies for the Rust static library.
if(UNIX AND NOT APPLE)
    target_link_libraries(crossbar-cpp-sdk INTERFACE pthread dl m rt)
elseif(APPLE)
    target_link_libraries(crossbar-cpp-sdk INTERFACE pthread dl m
        "-framework Security" "-framework CoreFoundation"
    )
endif()

# ---------------------------------------------------------------------------
# Examples
# ---------------------------------------------------------------------------
option(CROSSBAR_BUILD_EXAMPLES "Build crossbar C++ examples" ON)

if(CROSSBAR_BUILD_EXAMPLES)
    add_executable(publisher examples/publisher.cpp)
    target_link_libraries(publisher PRIVATE crossbar-cpp-sdk)

    add_executable(subscriber examples/subscriber.cpp)
    target_link_libraries(subscriber PRIVATE crossbar-cpp-sdk)
endif()
