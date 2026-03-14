# Copyright (c) 2026 The Crossbar Contributors
#
# This source code is licensed under the MIT license or Apache License 2.0,
# at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
# root for details.
#
# SPDX-License-Identifier: MIT OR Apache-2.0

"""High-level Pythonic API for crossbar zero-copy pub/sub over shared memory."""

from __future__ import annotations

import ctypes
import time
from dataclasses import dataclass, field
from typing import TYPE_CHECKING

from . import _ffi

if TYPE_CHECKING:
    pass


# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------


@dataclass
class Config:
    """Configuration for a crossbar pub/sub region.

    Mirrors the C ``crossbar_config_t`` struct with sensible defaults matching
    the Rust ``PoolPubSubConfig::default()``.
    """

    max_topics: int = 16
    """Maximum number of topics (default: 16)."""

    block_count: int = 256
    """Number of blocks in the shared pool (default: 256)."""

    block_size: int = 65536
    """Size of each block in bytes (default: 64 KiB)."""

    ring_depth: int = 8
    """Ring depth per topic (default: 8)."""

    heartbeat_ms: int = 100
    """Heartbeat interval in milliseconds (default: 100)."""

    stale_timeout_ms: int = 5000
    """Publisher considered dead after this many milliseconds (default: 5000)."""

    def _to_ffi(self) -> _ffi.CrossbarConfig:
        """Convert to the ctypes structure."""
        cfg = _ffi.CrossbarConfig()
        cfg.max_topics = self.max_topics
        cfg.block_count = self.block_count
        cfg.block_size = self.block_size
        cfg.ring_depth = self.ring_depth
        cfg.heartbeat_ms = self.heartbeat_ms
        cfg.stale_timeout_ms = self.stale_timeout_ms
        return cfg


# ---------------------------------------------------------------------------
# Exceptions
# ---------------------------------------------------------------------------


class CrossbarError(Exception):
    """Raised when a crossbar FFI call fails."""


def _check_ptr(ptr: ctypes.c_void_p | None, context: str) -> None:
    """Raise :class:`CrossbarError` if *ptr* is null."""
    if not ptr:
        msg = _ffi.last_error()
        raise CrossbarError(f"{context}: {msg}" if msg else context)


# ---------------------------------------------------------------------------
# TopicHandle
# ---------------------------------------------------------------------------


class TopicHandle:
    """Opaque handle for a registered topic.

    Returned by :meth:`PoolPublisher.register`. Pass it to
    :meth:`PoolPublisher.loan` to obtain a writable buffer.
    """

    def __init__(self, ptr: ctypes.c_void_p) -> None:
        self._ptr = ptr

    @property
    def _as_parameter_(self) -> ctypes.c_void_p:
        """Allow passing directly to ctypes calls."""
        return self._ptr


# ---------------------------------------------------------------------------
# Loan
# ---------------------------------------------------------------------------


class Loan:
    """A mutable buffer loaned from the shared memory pool.

    Write data into it and call :meth:`publish` to make it visible to
    subscribers. The transfer is O(1) regardless of payload size.

    If a ``Loan`` is garbage-collected without being published, the underlying
    block is returned to the pool.
    """

    def __init__(self, ptr: ctypes.c_void_p) -> None:
        self._ptr = ptr
        self._published = False

    @property
    def capacity(self) -> int:
        """Maximum number of bytes this loan can hold."""
        if self._published:
            raise CrossbarError("loan already published")
        lib = _ffi.get_lib()
        return lib.crossbar_loan_capacity(self._ptr)

    def write(self, data: bytes) -> None:
        """Write *data* into the loaned buffer starting at offset 0.

        Sets the valid data length to ``len(data)``.

        Args:
            data: Raw bytes to write. Must not exceed :attr:`capacity`.

        Raises:
            CrossbarError: If the loan has already been published.
            ValueError: If *data* exceeds the buffer capacity.
        """
        if self._published:
            raise CrossbarError("loan already published")

        lib = _ffi.get_lib()
        cap = lib.crossbar_loan_capacity(self._ptr)
        if len(data) > cap:
            raise ValueError(
                f"data length ({len(data)}) exceeds loan capacity ({cap})"
            )

        dst = lib.crossbar_loan_data(self._ptr)
        ctypes.memmove(dst, data, len(data))
        lib.crossbar_loan_set_len(self._ptr, len(data))

    def publish(self) -> None:
        """Publish the loan, making it visible to subscribers.

        Transfers ownership of the underlying block. The loan cannot be used
        after this call.

        Raises:
            CrossbarError: If the loan has already been published.
        """
        if self._published:
            raise CrossbarError("loan already published")
        self._published = True
        lib = _ffi.get_lib()
        lib.crossbar_loan_publish(self._ptr)
        self._ptr = None

    def __del__(self) -> None:
        if self._ptr is not None and not self._published:
            lib = _ffi.get_lib()
            lib.crossbar_loan_drop(self._ptr)
            self._ptr = None


# ---------------------------------------------------------------------------
# PoolPublisher
# ---------------------------------------------------------------------------


class PoolPublisher:
    """O(1) zero-copy publisher over shared memory.

    Creates a shared memory region backed by a block pool. Topics are
    registered with :meth:`register`, and data is published via the
    :meth:`loan` / :meth:`Loan.publish` protocol.

    Args:
        name: Name of the shared memory segment (appears as
              ``/dev/shm/crossbar-pool-<name>`` on Linux).
        config: Optional :class:`Config`. Uses defaults if not provided.

    Raises:
        CrossbarError: If the shared memory region cannot be created.

    Example::

        pub = PoolPublisher("market")
        topic = pub.register("prices/AAPL")
        loan = pub.loan(topic)
        loan.write(b"hello")
        loan.publish()
    """

    def __init__(self, name: str, config: Config | None = None) -> None:
        lib = _ffi.get_lib()
        if config is None:
            cfg = lib.crossbar_config_default()
        else:
            cfg = config._to_ffi()

        self._ptr = lib.crossbar_publisher_create(
            name.encode("utf-8"), ctypes.byref(cfg)
        )
        _check_ptr(self._ptr, f"failed to create publisher '{name}'")
        self._topics: list[TopicHandle] = []

    def register(self, topic: str) -> TopicHandle:
        """Register a topic URI and return a handle for :meth:`loan`.

        Args:
            topic: Topic URI string (max 64 bytes UTF-8).

        Returns:
            A :class:`TopicHandle` to pass to :meth:`loan`.

        Raises:
            CrossbarError: If the topic limit is reached or the URI is too long.
        """
        lib = _ffi.get_lib()
        ptr = lib.crossbar_publisher_register(self._ptr, topic.encode("utf-8"))
        _check_ptr(ptr, f"failed to register topic '{topic}'")
        handle = TopicHandle(ptr)
        self._topics.append(handle)
        return handle

    def loan(self, handle: TopicHandle) -> Loan:
        """Loan a mutable buffer from the pool for the given topic.

        Write data into the returned :class:`Loan`, then call
        :meth:`Loan.publish` to make it visible to subscribers.

        Args:
            handle: A :class:`TopicHandle` from :meth:`register`.

        Returns:
            A :class:`Loan` wrapping a pool block.

        Raises:
            CrossbarError: If the pool is exhausted.
        """
        lib = _ffi.get_lib()
        ptr = lib.crossbar_publisher_loan(self._ptr, handle._ptr)
        _check_ptr(ptr, "failed to loan block (pool exhausted?)")
        return Loan(ptr)

    def __del__(self) -> None:
        self._destroy()

    def close(self) -> None:
        """Explicitly release all resources.

        Destroys all registered topic handles and the publisher itself.
        """
        self._destroy()

    def _destroy(self) -> None:
        lib = _ffi.get_lib()
        for topic in self._topics:
            if topic._ptr is not None:
                lib.crossbar_topic_destroy(topic._ptr)
                topic._ptr = None
        self._topics.clear()
        if self._ptr is not None:
            lib.crossbar_publisher_destroy(self._ptr)
            self._ptr = None


# ---------------------------------------------------------------------------
# Subscription
# ---------------------------------------------------------------------------


class Subscription:
    """A subscription to a single topic on a shared memory pub/sub region.

    Provides both non-blocking (:meth:`try_recv`) and blocking (:meth:`recv`)
    receive methods.
    """

    def __init__(self, ptr: ctypes.c_void_p) -> None:
        self._ptr = ptr

    def try_recv(self) -> bytes | None:
        """Try to receive the latest sample without blocking.

        Returns:
            The sample data as ``bytes``, or ``None`` if no new data is
            available.

        Raises:
            CrossbarError: If the subscription has been destroyed.
        """
        if self._ptr is None:
            raise CrossbarError("subscription already destroyed")

        lib = _ffi.get_lib()
        sample = lib.crossbar_subscription_try_recv(self._ptr)
        if not sample:
            return None

        data_ptr = lib.crossbar_sample_data(sample)
        length = lib.crossbar_sample_len(sample)
        result = ctypes.string_at(data_ptr, length)
        lib.crossbar_sample_drop(sample)
        return result

    def recv(self, timeout: float = 1.0) -> bytes:
        """Block until a sample is available or the timeout expires.

        Polls :meth:`try_recv` with adaptive backoff: starts with a tight
        spin (0.1 ms) and backs off to 1 ms sleeps.

        Args:
            timeout: Maximum time to wait in seconds (default: 1.0).

        Returns:
            The sample data as ``bytes``.

        Raises:
            TimeoutError: If no sample arrives within *timeout* seconds.
            CrossbarError: If the subscription has been destroyed.
        """
        deadline = time.monotonic() + timeout
        spin_until = time.monotonic() + min(0.001, timeout)

        while True:
            sample = self.try_recv()
            if sample is not None:
                return sample

            now = time.monotonic()
            if now >= deadline:
                raise TimeoutError(
                    f"no sample received within {timeout:.3f}s"
                )

            # Adaptive backoff: tight spin for the first millisecond,
            # then sleep 1 ms between polls to avoid burning CPU.
            if now < spin_until:
                time.sleep(0.0001)  # 100 us
            else:
                time.sleep(0.001)  # 1 ms

    def __del__(self) -> None:
        if self._ptr is not None:
            lib = _ffi.get_lib()
            lib.crossbar_subscription_destroy(self._ptr)
            self._ptr = None


# ---------------------------------------------------------------------------
# PoolSubscriber
# ---------------------------------------------------------------------------


class PoolSubscriber:
    """Zero-copy subscriber for crossbar pool-backed pub/sub.

    Connects to an existing :class:`PoolPublisher` shared memory region and
    subscribes to topics.

    Args:
        name: Name of the shared memory segment to connect to (must match
              the name used by the publisher).

    Raises:
        CrossbarError: If the shared memory region cannot be opened or the
                       publisher is not alive.

    Example::

        sub = PoolSubscriber("market")
        stream = sub.subscribe("prices/AAPL")
        data = stream.recv(timeout=5.0)
    """

    def __init__(self, name: str) -> None:
        lib = _ffi.get_lib()
        self._ptr = lib.crossbar_subscriber_connect(name.encode("utf-8"))
        _check_ptr(self._ptr, f"failed to connect to publisher '{name}'")
        self._subscriptions: list[Subscription] = []

    def subscribe(self, topic: str) -> Subscription:
        """Subscribe to a topic by URI.

        Args:
            topic: Topic URI string (must match a topic registered by the
                   publisher).

        Returns:
            A :class:`Subscription` for receiving samples.

        Raises:
            CrossbarError: If the topic is not registered.
        """
        lib = _ffi.get_lib()
        ptr = lib.crossbar_subscriber_subscribe(
            self._ptr, topic.encode("utf-8")
        )
        _check_ptr(ptr, f"failed to subscribe to topic '{topic}'")
        sub = Subscription(ptr)
        self._subscriptions.append(sub)
        return sub

    def __del__(self) -> None:
        self._destroy()

    def close(self) -> None:
        """Explicitly release all resources.

        Destroys all subscriptions and the subscriber itself.
        """
        self._destroy()

    def _destroy(self) -> None:
        lib = _ffi.get_lib()
        for sub in self._subscriptions:
            if sub._ptr is not None:
                lib.crossbar_subscription_destroy(sub._ptr)
                sub._ptr = None
        self._subscriptions.clear()
        if self._ptr is not None:
            lib.crossbar_subscriber_destroy(self._ptr)
            self._ptr = None
