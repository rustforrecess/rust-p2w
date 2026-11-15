# Heterogeneous compute (CPU, GPU, NPU) — proposal

**Status: PROPOSAL, not built (2026-11-14).** Claims about p2w today were
checked against the code the same day. Claims about WebNN were checked
against the W3C draft and the WebNN project's tracker on 2026-11-14 (see
§ WebNN). Claims about sibling repos are from project notes and marked so.

## The proposal in one paragraph

Let a student hand numbers-only work to whatever accelerator the machine
has, through ordinary library calls — `compute.map(f, xs)`,
`compute.sum(xs)`, `compute.matmul(a, b)` — that are plain loops everywhere
no accelerator exists (CPython, the Pico, a test host). The compiler checks
that the work is eligible and turns it into a small **device-neutral compute
graph**; separate lowerings take that graph to a GPU (WGSL) or an NPU (ONNX
or WebNN). The program never names a device: the host picks one, as it
grants every other capability. No new syntax, so every program stays valid
Python, and the CPU loop defines the right answer.

```python
import compute

def brighten(p: float) -> float:
    return min(p * 1.5, 1.0)

out = compute.map(brighten, pixels)   # pixels: list[float]
```

## Why an NPU forces two tiers

A GPU and an NPU are not the same kind of machine.

- **A GPU runs your function**: thousands of copies of arbitrary small
  code, with branches and loops.
- **An NPU (and TPUs, and most AI accelerators) runs operator graphs**:
  matrix multiply, convolution, element-wise arithmetic, activations and
  reductions, usually in low precision (int8, fp16, bf16). You don't send it
  code; you send a graph of known operations to a vendor runtime.

So eligibility has two tiers, and the checker places each function in one
and says why:

| Tier | What's allowed | Runs on |
|---|---|---|
| **Graph** | straight-line element-wise arithmetic (`+ - * /`, `min`, `max`, `abs`, `exp`, comparisons as selects), and whole-array operations (`sum`, `max`, `matmul`) | NPU, GPU, CPU |
| **Kernel** | the graph tier plus bounded loops and branches | GPU, CPU |

"`blur` has a loop on line 3, so it can run on a GPU but not an NPU."

Both tiers share the base rules: every value is a raw int or float or a
packed number array (p2w's typed tier, `repr.rs`, already infers exactly
these); no allocation, strings, objects, closures, `print`, `input`, host
calls, written globals or recursion. A function that fails both tiers still
runs: `compute.map` falls back to the CPU loop, and a lint says why.

## One graph, several lowerings

```
p2w source ──(kernel profile check)──> compute graph
                                         ├─ CPU loop        (always; the reference)
                                         ├─ WGSL            → WebGPU (browser), wgpu (native)
                                         └─ ONNX / WebNN    → NPU, GPU or CPU, via the platform
```

- **CPU loop.** The function compiled as today. Always available, and the
  definition of the right answer.
- **WGSL** for GPUs: a small emitter, since a kernel's syntax tree is tiny.
  WebGPU in the browser; `wgpu` natively (Vulkan, Metal, DX12; desktops and
  the Raspberry Pi 5). From project notes, not re-checked here:
  `acornstem/render-core` already uses `wgpu` and `sequent-gpu` runs WebGPU
  compute in the browser.
- **ONNX** natively, through a runtime's execution providers, which is how
  the NPUs are reached today: DirectML or QNN on Windows/Qualcomm machines,
  Core ML on Apple, OpenVINO on Intel. (From general knowledge; to be
  checked when this step is planned.)
- **WebNN** in the browser, when it ships (next section).

**The compiler stays a compiler.** rust-p2w emits the graph (and the WGSL)
as data. The device runtimes — `wgpu`, ONNX Runtime, the browser's WebGPU
and WebNN — live in the hosts (the IDE runner, a native host), never in the
crate. That keeps its one-runtime-dependency rule (`ryu`).

**Device choice is the host's.** In the capability-broker model, hardware
is granted by the host, not reached for by the program. A program may not
name a device, so its output can't depend on which one ran it — subject to
the precision rules below.

## WebNN, checked 2026-11-14

- **Spec:** the W3C Web Neural Network API is a Candidate Recommendation
  Draft, dated 21 October 2026.
- **No "npu" device option.** The spec dropped explicit device types for
  abstract selection: `MLContextOptions { MLPowerPreference powerPreference
  = "default"; boolean accelerated = true; }`; `accelerated: false` means
  "prefers CPU inference", and otherwise "the user agent selects and creates
  the underlying execution device". So a page can't ask for the NPU, only
  for acceleration, which fits "the host picks the device".
- **Not shipped.** Per the WebNN project tracker (issue #964, 20 October
  2026, written by a Microsoft engineer and not contradicted by the
  maintainer who replied): Chrome's origin trial for versions 147–149 and
  Edge's both closed on 11 August 2026; the trial had been disabled in
  March over "more release blocking issues"; the feature flag defaults to
  off on Chrome's M155 branch and on main; webnn.io lists Chrome 160–164 as
  "TBD". Firefox and Safari have no support. Secondary sources estimate
  default availability in 2027.
- **What people use instead today** (secondary sources): WebGPU, through
  ONNX Runtime Web's WebGPU execution provider.

**Consequence for this plan:** the browser NPU path waits for WebNN to ship.
The browser path to start with is WebGPU, which is shipped. Native NPU use,
through ONNX Runtime, doesn't depend on browsers and could come first if a
lesson needs it.

## The hard part: matching CPython

"Every program prints what CPython prints" is the promise this threatens,
and it threatens it differently per device.

| Issue | CPython / p2w today | GPU | NPU |
|---|---|---|---|
| Float width | 64-bit | 32-bit in WGSL (64-bit is not portable) | often fp16 or int8 |
| Rounding | deterministic | deterministic per operation | the runtime may fuse operations or reorder sums, so the same graph can round differently on different hardware |
| Int overflow | p2w raises OverflowError | wraps silently | wraps or saturates |
| Division by zero | ZeroDivisionError | no trap | no trap |

- **GPU: exact matching is achievable.** Make 32-bit floats a visible type
  (`f32`): the CPU fallback and the CPython stub round to 32 bits too, so
  all three agree bit for bit. Range-check kernel ints rather than let them
  wrap; collect errors during a launch and raise the first one after it,
  naming the element.
- **NPU: exact matching is generally not achievable for floats.** Two
  honest options:
  1. **Exact only.** NPU work is limited to what is exact, mainly integer
     arithmetic; float work runs on the GPU or CPU.
  2. **Declared precision.** The program spells the precision out (an `f16`
     type, say) and the promise becomes "equal to the CPU reference within a
     stated tolerance", tested that way.

  Option 2 changes what the language promises, so it is a decision, not a
  detail.

**Recommendation:** start exact. Ints and `f32` run on CPU and GPU and match
bit for bit. NPU use comes later as an explicit opt-in with declared
precision, when a lesson needs it.

## Plan

| Step | Work | Gate |
|---|---|---|
| 1 | The compute graph, the two-tier checker, and `compute.map` / `compute.sum` as loops on both backends and in a CPython stub | the tiers' accept/reject cases as tests; probes print the same on CPython, WASM and native |
| 2 | The `f32` type on every surface | f32 probes agree on all surfaces, including the CPython stub |
| 3 | WGSL lowering + WebGPU host in the IDE | the step 1 probes print the same on the GPU path in headless Chrome (the e2e harness) |
| 4 | Native `wgpu` host (desktop, Pi 5) | the same probes on a native GPU |
| 5 | The precision decision; then an ONNX lowering + native NPU host | probes within the declared tolerance on an NPU machine |
| 6 | WebNN host, once Chrome or Edge ship it on by default | the same probes in the browser |
| 7 | 2-D grids, matmul, convolution, shared memory | when a lesson needs them |

Rough sizes: step 1 is days (it reuses the typed-tier inference); step 3
about one to two weeks; step 4 about a week. Steps 5 and 6 wait on a
decision and on browsers respectively.

## Not in scope

- **The Pico.** No accelerator; `compute.*` stays a loop. Some newer
  microcontrollers have small NPUs (Arm Ethos-U), a possible later target
  through the same graph. Heterogeneous computing on the RP2350 would mean
  its second CPU core, a separate and smaller project.
- **Arbitrary accelerator programming** by students: threads, barriers,
  shared memory. Map, reduce and whole-array operations first.
- **Existing Python accelerator libraries** (CuPy, Numba, PyTorch).
  Different languages and runtimes; not a p2w target.

## Open questions

1. **Precision promise for NPUs:** exact only, or declared precision with
   tolerances? (Decides step 5.)
2. **`f32` as a visible type**, or 64-bit kernels only where the hardware
   supports `f64`? This document leans `f32`.
3. Does `compute.map` choose the device by array size, or always use an
   accelerator when present? Choosing changes timing only, never output,
   but it makes "why was this slow" lessons harder to read.
4. How does the step debugger show accelerated work? Probably as the CPU
   loop it is, stepping one element.

## Sources (WebNN)

- W3C, Web Neural Network API, Candidate Recommendation Draft,
  21 October 2026: <https://www.w3.org/TR/webnn/>
- webmachinelearning/webnn issue #964, "What is the current testing/shipping
  status of WebNN in Chrome and Edge": <https://github.com/webmachinelearning/webnn/issues/964>
- Secondary: <https://www.utsubo.com/blog/frontier-web-apis-2026-production-ready>,
  <https://www.neowin.net/news/google-promotes-chrome-146-to-the-beta-channel-with-webnn-support-sanitizer-api-and-more/>
