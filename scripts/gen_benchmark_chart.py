#!/usr/bin/env python3
"""Generate benchmark comparison charts for README."""

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import matplotlib.ticker as ticker
import numpy as np

# -- Benchmark data (from cargo bench --features shm -- "head_to_head") --
# System: Intel i7-10700KF @ 3.80 GHz, Linux 6.8.0, rustc stable

# O(1) transport proof: 8B write, varying backing buffer size
buf_labels = ["64 B", "4 KB", "64 KB", "256 KB", "1 MB"]
buf_sizes = [64, 4096, 65536, 262144, 1048576]
crossbar_o1 = [64.4, 64.2, 63.8, 64.4, 64.6]  # ns
iceoryx2_o1 = [237.1, 238.5, 237.7, 242.4, 244.7]  # ns

# End-to-end: full payload write + transfer
e2e_labels = ["8 B", "1 KB", "64 KB", "256 KB", "1 MB"]
e2e_sizes = [8, 1024, 65536, 262144, 1048576]
crossbar_e2e = [62.9, 74.4, 1442, 6749, 30075]  # ns
iceoryx2_e2e = [236.6, 247.3, 1345.7, 6919.2, 30782]  # ns

# -- Chart style (clean, professional, iceoryx2-inspired) --
plt.rcParams.update({
    "font.family": "sans-serif",
    "font.size": 11,
    "axes.titlesize": 13,
    "axes.titleweight": "bold",
    "axes.grid": True,
    "grid.alpha": 0.3,
    "figure.facecolor": "white",
    "axes.facecolor": "#fafafa",
    "axes.spines.top": False,
    "axes.spines.right": False,
})

CROSSBAR_COLOR = "#059669"  # green
ICEORYX2_COLOR = "#6366f1"  # indigo

fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(14, 5.5))
fig.suptitle(
    "crossbar vs iceoryx2 — Pub/Sub Latency Comparison",
    fontsize=15, fontweight="bold", y=0.98,
)
fig.text(
    0.5, 0.92,
    "Intel i7-10700KF @ 3.80 GHz · Linux 6.8 · rustc stable · Criterion",
    ha="center", fontsize=9, color="#666",
)

# -- Left: O(1) transport proof --
x = np.arange(len(buf_labels))
w = 0.32

bars1 = ax1.bar(x - w/2, crossbar_o1, w, label="crossbar", color=CROSSBAR_COLOR, zorder=3)
bars2 = ax1.bar(x + w/2, iceoryx2_o1, w, label="iceoryx2", color=ICEORYX2_COLOR, zorder=3)

ax1.set_title("O(1) Transport — 8 B write, varying buffer")
ax1.set_xlabel("Backing buffer size")
ax1.set_ylabel("Latency (ns)")
ax1.set_xticks(x)
ax1.set_xticklabels(buf_labels, fontsize=9)
ax1.set_ylim(0, 310)
ax1.legend(loc="upper left", framealpha=0.9)

# Add value labels on bars
for bar in bars1:
    ax1.text(bar.get_x() + bar.get_width()/2, bar.get_height() + 5,
             f"{bar.get_height():.0f}", ha="center", va="bottom", fontsize=8,
             color=CROSSBAR_COLOR, fontweight="bold")
for bar in bars2:
    ax1.text(bar.get_x() + bar.get_width()/2, bar.get_height() + 5,
             f"{bar.get_height():.0f}", ha="center", va="bottom", fontsize=8,
             color=ICEORYX2_COLOR, fontweight="bold")

# Annotation
ax1.annotate("Both flat — O(1) confirmed\ncrossbar 3.7x faster",
             xy=(2, 65), fontsize=9, color="#059669", fontweight="bold",
             bbox=dict(boxstyle="round,pad=0.3", facecolor="white", edgecolor="#059669", alpha=0.9))

# -- Right: End-to-end (log scale) --
ax2.plot(range(len(e2e_labels)), crossbar_e2e, "o-", color=CROSSBAR_COLOR,
         label="crossbar", linewidth=2, markersize=7, zorder=3)
ax2.plot(range(len(e2e_labels)), iceoryx2_e2e, "s-", color=ICEORYX2_COLOR,
         label="iceoryx2", linewidth=2, markersize=7, zorder=3)

ax2.set_title("End-to-End — full payload write + transfer")
ax2.set_xlabel("Payload size")
ax2.set_ylabel("Latency (ns) — log scale")
ax2.set_xticks(range(len(e2e_labels)))
ax2.set_xticklabels(e2e_labels, fontsize=9)
ax2.set_yscale("log")
ax2.yaxis.set_major_formatter(ticker.FuncFormatter(
    lambda v, _: f"{v:.0f}" if v < 1000 else f"{v/1000:.1f} us" if v < 1000000 else f"{v/1000000:.1f} ms"
))
ax2.legend(loc="upper left", framealpha=0.9)

# Annotation for convergence
ax2.annotate("Both converge at 64 KB+\n(memcpy-bound)",
             xy=(3, 7000), fontsize=9, color="#666",
             bbox=dict(boxstyle="round,pad=0.3", facecolor="white", edgecolor="#ccc", alpha=0.9))

plt.tight_layout(rect=[0, 0, 1, 0.90])
plt.savefig("/home/theta-gamma/viaduct/assets/benchmark_comparison.svg",
            format="svg", dpi=150, bbox_inches="tight")
plt.savefig("/home/theta-gamma/viaduct/assets/benchmark_comparison.png",
            format="png", dpi=150, bbox_inches="tight")
print("Saved to assets/benchmark_comparison.svg and .png")
