# crossbar-python

Python bindings for [crossbar](https://crates.io/crates/crossbar) — zero-copy pub/sub over shared memory.

## Install

```bash
pip install crossbar
```

## Usage

```python
import crossbar_python as crossbar

sub = crossbar.Subscriber("prices")
stream = sub.subscribe("/tick/AAPL")

sample = stream.recv()  # blocks until data available
print(f"received {len(sample)} bytes")
```

## Build from source

```bash
cd bindings/python
maturin develop
```
