"""Measure model, prompt-cache, and process memory at known prompt lengths."""

import ctypes
import gc
import json
import os
import threading
import time
from dataclasses import dataclass
from pathlib import Path

import mlx.core as mx
import mlx_vlm
from mlx_vlm.apc import APCManager
from mlx_vlm.models import cache as cache_mod

TARGETS = (4096, 16384)
GIB = 1024**3
MIB = 1024**2


class RUsageInfoV2(ctypes.Structure):
    _fields_ = [
        ("ri_uuid", ctypes.c_uint8 * 16),
        *[
            (name, ctypes.c_uint64)
            for name in (
                "ri_user_time",
                "ri_system_time",
                "ri_pkg_idle_wkups",
                "ri_interrupt_wkups",
                "ri_pageins",
                "ri_wired_size",
                "ri_resident_size",
                "ri_phys_footprint",
                "ri_proc_start_abstime",
                "ri_proc_exit_abstime",
                "ri_child_user_time",
                "ri_child_system_time",
                "ri_child_pkg_idle_wkups",
                "ri_child_interrupt_wkups",
                "ri_child_pageins",
                "ri_child_elapsed_abstime",
                "ri_diskio_bytesread",
                "ri_diskio_byteswritten",
            )
        ],
    ]


_proc_pid_rusage = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True).proc_pid_rusage
_proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
_proc_pid_rusage.restype = ctypes.c_int


@dataclass(frozen=True)
class ProcessMemory:
    rss: int
    footprint: int


def process_memory() -> ProcessMemory:
    info = RUsageInfoV2()
    if _proc_pid_rusage(os.getpid(), 2, ctypes.byref(info)) != 0:
        errno = ctypes.get_errno()
        raise OSError(errno, os.strerror(errno))
    return ProcessMemory(info.ri_resident_size, info.ri_phys_footprint)


class ProcessPeak:
    def __init__(self) -> None:
        current = process_memory()
        self.rss = current.rss
        self.footprint = current.footprint
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._sample, daemon=True)

    def _sample(self) -> None:
        while not self._stop.wait(0.01):
            current = process_memory()
            self.rss = max(self.rss, current.rss)
            self.footprint = max(self.footprint, current.footprint)

    def __enter__(self) -> "ProcessPeak":
        self._thread.start()
        return self

    def __exit__(self, *_args: object) -> None:
        self._stop.set()
        self._thread.join()
        current = process_memory()
        self.rss = max(self.rss, current.rss)
        self.footprint = max(self.footprint, current.footprint)


def snapshot(stage: str, baseline: ProcessMemory) -> None:
    current = process_memory()
    print(
        f"stage={stage} mlx_gib={mx.get_active_memory() / GIB:.3f} "
        f"rss_gib={current.rss / GIB:.3f} footprint_gib={current.footprint / GIB:.3f} "
        f"footprint_delta_gib={(current.footprint - baseline.footprint) / GIB:.3f}",
        flush=True,
    )


def main() -> None:
    path = json.loads((Path.home() / ".config" / "wally" / "pls" / "config.json").read_text())["model_path"]
    mx.set_cache_limit(0)
    baseline = process_memory()
    snapshot("baseline", baseline)
    started = time.monotonic()
    model, processor = mlx_vlm.load(path)
    tokenizer = getattr(processor, "tokenizer", processor)
    print(f"model_load_sec={time.monotonic() - started:.1f}", flush=True)
    snapshot("model_loaded", baseline)

    def text_of(target: int) -> tuple[int, str]:
        lo, hi = 1, target
        best = (0, "")
        while lo <= hi:
            mid = (lo + hi) // 2
            messages = [{"role": "user", "content": "alpha " * mid}]
            text = mlx_vlm.apply_chat_template(
                processor, model.config, messages, add_generation_prompt=True,
            )
            count = len(tokenizer.encode(text))
            if abs(count - target) < abs(best[0] - target):
                best = (count, text)
            if count < target:
                lo = mid + 1
            elif count > target:
                hi = mid - 1
            else:
                best = (count, text)
                break
        return best

    points = []
    for target in TARGETS:
        count, text = text_of(target)
        snapshot(f"prompt_{target}_prepared", baseline)
        prompt_cache = cache_mod.make_prompt_cache(model.language_model)
        apc = APCManager(num_blocks=8192, overrides={
            "checkpoint_entries": 32,
            "checkpoint_interval_tokens": 4096,
        })
        mx.reset_peak_memory()
        started = time.monotonic()
        with ProcessPeak() as process_peak:
            result = mlx_vlm.generate(
                model,
                processor,
                text,
                max_tokens=1,
                temperature=0.0,
                verbose=False,
                prompt_cache=prompt_cache,
                apc_manager=apc,
            )

        def nbytes(item) -> int:
            size = item.nbytes
            return size() if callable(size) else int(size)

        mx.eval([item.state for item in prompt_cache])
        cache_bytes = sum(nbytes(item) for item in prompt_cache)
        by_type: dict[str, int] = {}
        for item in prompt_cache:
            by_type[type(item).__name__] = by_type.get(type(item).__name__, 0) + nbytes(item)
        parts = " ".join(f"{name}={size / MIB:.1f}MiB" for name, size in sorted(by_type.items()))
        current = process_memory()
        print(
            f"measurement_tokens={count} generated_tokens={result.generation_tokens} "
            f"sec={time.monotonic() - started:.1f} cache_mib={cache_bytes / MIB:.1f} "
            f"mlx_peak_gib={mx.get_peak_memory() / GIB:.3f} "
            f"rss_gib={current.rss / GIB:.3f} rss_peak_gib={process_peak.rss / GIB:.3f} "
            f"footprint_gib={current.footprint / GIB:.3f} "
            f"footprint_peak_gib={process_peak.footprint / GIB:.3f} {parts}",
            flush=True,
        )
        points.append((count, cache_bytes))
        del result
        del prompt_cache
        del apc
        del text
        gc.collect()
        mx.clear_cache()
        snapshot(f"prompt_{target}_released", baseline)

    (n1, b1), (n2, b2) = points
    per = (b2 - b1) / max(1, n2 - n1)
    fixed = b1 - per * n1
    cache_at_65k = fixed + per * 65536
    print(
        f"cache_growth_kib_per_token={per / 1024:.2f} "
        f"fixed_cache_mib={fixed / MIB:.1f} "
        f"estimated_cache_at_65k_gib={cache_at_65k / GIB:.3f}",
        flush=True,
    )


if __name__ == "__main__":
    main()
