# Copyright (c) 2026 The Crossbar Contributors
#
# This source code is licensed under the MIT license or Apache License 2.0,
# at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
# root for details.
#
# SPDX-License-Identifier: MIT OR Apache-2.0

"""Low-level ctypes bindings to libcrossbar_ffi."""

from __future__ import annotations

import ctypes
import ctypes.util
import os
import platform
import sys
from pathlib import Path

# ---------------------------------------------------------------------------
# crossbar_config_t
# ---------------------------------------------------------------------------


class CrossbarConfig(ctypes.Structure):
    """Mirrors the C ``crossbar_config_t`` struct."""

    _fields_ = [
        ("max_topics", ctypes.c_uint32),
        ("block_count", ctypes.c_uint32),
        ("block_size", ctypes.c_uint32),
        ("ring_depth", ctypes.c_uint32),
        ("heartbeat_ms", ctypes.c_uint64),
        ("stale_timeout_ms", ctypes.c_uint64),
    ]


# ---------------------------------------------------------------------------
# Opaque handle types
# ---------------------------------------------------------------------------

class _OpaquePublisher(ctypes.Structure):
    """Opaque handle for crossbar_publisher_t."""


class _OpaqueTopic(ctypes.Structure):
    """Opaque handle for crossbar_topic_t."""


class _OpaqueLoan(ctypes.Structure):
    """Opaque handle for crossbar_loan_t."""


class _OpaqueSubscriber(ctypes.Structure):
    """Opaque handle for crossbar_subscriber_t."""


class _OpaqueSubscription(ctypes.Structure):
    """Opaque handle for crossbar_subscription_t."""


class _OpaqueSample(ctypes.Structure):
    """Opaque handle for crossbar_sample_t."""


# ---------------------------------------------------------------------------
# Library discovery
# ---------------------------------------------------------------------------

_LIB_NAME_LINUX = "libcrossbar_ffi.so"
_LIB_NAME_MACOS = "libcrossbar_ffi.dylib"


def _lib_filename() -> str:
    """Return the platform-appropriate shared library filename."""
    if platform.system() == "Darwin":
        return _LIB_NAME_MACOS
    return _LIB_NAME_LINUX


def _find_library() -> str:
    """Locate ``libcrossbar_ffi`` and return its filesystem path.

    Search order:
      1. ``CROSSBAR_LIB_PATH`` environment variable (explicit override).
      2. ``../../target/release/`` relative to this file (Cargo workspace build).
      3. ``../../target/debug/`` relative to this file (Cargo workspace build).
      4. System library paths via ``ctypes.util.find_library``.

    Raises:
        OSError: If the library cannot be found in any of the search paths.
    """
    # 1. Explicit override
    env_path = os.environ.get("CROSSBAR_LIB_PATH")
    if env_path:
        if os.path.isfile(env_path):
            return env_path
        candidate = os.path.join(env_path, _lib_filename())
        if os.path.isfile(candidate):
            return candidate

    # 2-3. Cargo workspace target directories
    pkg_dir = Path(__file__).resolve().parent  # crossbar/
    workspace_root = pkg_dir.parent.parent.parent  # bindings/python/ -> viaduct/
    for profile in ("release", "debug"):
        candidate = workspace_root / "target" / profile / _lib_filename()
        if candidate.is_file():
            return str(candidate)

    # 4. System paths
    system_path = ctypes.util.find_library("crossbar_ffi")
    if system_path:
        return system_path

    search_dirs = [
        f"  $CROSSBAR_LIB_PATH (not set)",
        f"  {workspace_root / 'target' / 'release'}",
        f"  {workspace_root / 'target' / 'debug'}",
        f"  system library paths",
    ]
    raise OSError(
        f"Cannot find {_lib_filename()}. Searched:\n"
        + "\n".join(search_dirs)
        + "\n\nBuild with: cargo build --release -p crossbar-ffi"
    )


def _load_library() -> ctypes.CDLL:
    """Load the shared library and configure all function signatures."""
    path = _find_library()
    lib = ctypes.cdll.LoadLibrary(path)
    _bind_signatures(lib)
    return lib


# ---------------------------------------------------------------------------
# Function signature bindings
# ---------------------------------------------------------------------------


def _bind_signatures(lib: ctypes.CDLL) -> None:
    """Declare argtypes and restype for every FFI function."""

    # -- Config ---------------------------------------------------------------

    lib.crossbar_config_default.argtypes = []
    lib.crossbar_config_default.restype = CrossbarConfig

    # -- Publisher ------------------------------------------------------------

    lib.crossbar_publisher_create.argtypes = [
        ctypes.c_char_p,
        ctypes.POINTER(CrossbarConfig),
    ]
    lib.crossbar_publisher_create.restype = ctypes.POINTER(_OpaquePublisher)

    lib.crossbar_publisher_destroy.argtypes = [ctypes.POINTER(_OpaquePublisher)]
    lib.crossbar_publisher_destroy.restype = None

    lib.crossbar_publisher_register.argtypes = [
        ctypes.POINTER(_OpaquePublisher),
        ctypes.c_char_p,
    ]
    lib.crossbar_publisher_register.restype = ctypes.POINTER(_OpaqueTopic)

    lib.crossbar_topic_destroy.argtypes = [ctypes.POINTER(_OpaqueTopic)]
    lib.crossbar_topic_destroy.restype = None

    lib.crossbar_publisher_loan.argtypes = [
        ctypes.POINTER(_OpaquePublisher),
        ctypes.POINTER(_OpaqueTopic),
    ]
    lib.crossbar_publisher_loan.restype = ctypes.POINTER(_OpaqueLoan)

    lib.crossbar_loan_data.argtypes = [ctypes.POINTER(_OpaqueLoan)]
    lib.crossbar_loan_data.restype = ctypes.POINTER(ctypes.c_uint8)

    lib.crossbar_loan_capacity.argtypes = [ctypes.POINTER(_OpaqueLoan)]
    lib.crossbar_loan_capacity.restype = ctypes.c_size_t

    lib.crossbar_loan_set_len.argtypes = [ctypes.POINTER(_OpaqueLoan), ctypes.c_size_t]
    lib.crossbar_loan_set_len.restype = None

    lib.crossbar_loan_publish.argtypes = [ctypes.POINTER(_OpaqueLoan)]
    lib.crossbar_loan_publish.restype = None

    lib.crossbar_loan_drop.argtypes = [ctypes.POINTER(_OpaqueLoan)]
    lib.crossbar_loan_drop.restype = None

    # -- Subscriber -----------------------------------------------------------

    lib.crossbar_subscriber_connect.argtypes = [ctypes.c_char_p]
    lib.crossbar_subscriber_connect.restype = ctypes.POINTER(_OpaqueSubscriber)

    lib.crossbar_subscriber_destroy.argtypes = [ctypes.POINTER(_OpaqueSubscriber)]
    lib.crossbar_subscriber_destroy.restype = None

    lib.crossbar_subscriber_subscribe.argtypes = [
        ctypes.POINTER(_OpaqueSubscriber),
        ctypes.c_char_p,
    ]
    lib.crossbar_subscriber_subscribe.restype = ctypes.POINTER(_OpaqueSubscription)

    lib.crossbar_subscription_destroy.argtypes = [ctypes.POINTER(_OpaqueSubscription)]
    lib.crossbar_subscription_destroy.restype = None

    lib.crossbar_subscription_try_recv.argtypes = [
        ctypes.POINTER(_OpaqueSubscription),
    ]
    lib.crossbar_subscription_try_recv.restype = ctypes.POINTER(_OpaqueSample)

    lib.crossbar_sample_data.argtypes = [ctypes.POINTER(_OpaqueSample)]
    lib.crossbar_sample_data.restype = ctypes.POINTER(ctypes.c_uint8)

    lib.crossbar_sample_len.argtypes = [ctypes.POINTER(_OpaqueSample)]
    lib.crossbar_sample_len.restype = ctypes.c_size_t

    lib.crossbar_sample_drop.argtypes = [ctypes.POINTER(_OpaqueSample)]
    lib.crossbar_sample_drop.restype = None

    # -- Error ----------------------------------------------------------------

    lib.crossbar_last_error.argtypes = []
    lib.crossbar_last_error.restype = ctypes.c_char_p


# ---------------------------------------------------------------------------
# Module-level singleton
# ---------------------------------------------------------------------------

_lib: ctypes.CDLL | None = None


def get_lib() -> ctypes.CDLL:
    """Return the loaded ``libcrossbar_ffi`` library (lazy singleton).

    The library is loaded once on first access and cached for subsequent calls.

    Raises:
        OSError: If the library cannot be found or loaded.
    """
    global _lib
    if _lib is None:
        _lib = _load_library()
    return _lib


def last_error() -> str:
    """Return the last error message from the FFI layer, or an empty string."""
    lib = get_lib()
    msg = lib.crossbar_last_error()
    if msg:
        return msg.decode("utf-8", errors="replace")
    return ""
