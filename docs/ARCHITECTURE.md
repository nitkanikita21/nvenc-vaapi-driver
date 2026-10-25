# Architecture

## Loading and state

A VA-API client calls libva, which loads `nvidia_nvenc_drv_video.so` and resolves
an exported `__vaDriverInit_1_*` entry point. The backend is a userspace library,
not a kernel driver or a daemon. Driver state belongs to `VADriverContext` and
is released by `vaTerminate`.

`DriverState` owns lazy CUDA selection and `Mutex<Pools>`. Typed object pools
store configs, surfaces, contexts, buffers and images. IDs are monotonically
allocated nonzero 32-bit values and are never reused during the display's
lifetime. Allocation fails before the invalid-ID sentinel or wraparound.

## Encode lifecycle

1. Config creation validates profile, entry point and rate-control mode.
2. Surfaces own pitched CUDA NV12 memory or imported external memory.
3. Context creation retains geometry and configuration. NVENC remains unopened.
4. Begin/render collect a picture's input and parameter buffers.
5. End validates output, merges sequence and misc controls, lazily opens or
   reconfigures NVENC, and registers input in that session.
6. NVENC maps and submits input with a distinct output buffer. A session worker
   blocks for that output, copies compressed bytes, unlocks and unmaps input.
   Linux uses synchronous NVENC calls; no B-frames or lookahead are enabled.
7. Sync or coded-buffer mapping waits for the matching completion and publishes
   its segment. Finite waits include pool-lock acquisition and preserve pending
   output on timeout. Mapping exposes stable storage until unmap.

Each session has four reusable outputs and a bounded work queue. Submission can
continue while its worker waits for earlier outputs. Input reuse, CPU transfers,
reconfiguration and resource destruction wait for the affected work. The worker
never takes the display pool lock. Completed bytes belong to their original
coded buffer and remain accessible after encoder/context destruction.

## Resources and teardown

Each `NvencSession` owns its output worker, bounded output pool and input
registration cache. Registrations belong to a session, so two contexts may share
one surface safely. Surface destruction unregisters it from all sessions before
freeing CUDA memory. Driver termination destroys contexts before surfaces.

Encoder creation is ordered across displays. The first backend-owned encoder
on a CUDA device remains alive until its backend-owned peers close, even if
its VA context is destroyed earlier. This works around
[NVIDIA bug 4340965](https://forums.developer.nvidia.com/t/regression-with-use-after-free-linux-drivers-after-520-56-06-inside-nvenc/269974),
where a runtime timer can reference the first encoder after destruction.
Only one retired encoder and its CUDA context are retained per active device
group; its inputs, output buffers and worker are released immediately. This can
keep one NVENC session slot occupied until that group becomes idle. Native
encoders opened by other libraries are outside this backend's lifetime registry.

VA images use a host shadow for CPU access. Read mappings of derived internal
images download current pixels; write mappings upload on unmap. Write-only
access skips readback, and read-only access skips upload. Legacy mapping means
read/write. Standalone images use explicit get/put transfers. NV12 readback
supports even-aligned rectangles without scaling; puts require full frames.
Imported external surfaces do not support CPU image access.

Mapping a generic buffer retains its storage lock until unmap. Mutating,
rendering or destroying mapped buffers must be rejected before attempting to
take that lock. Failed transfers leave the buffer unmapped. Get/put using an
image derived from the same surface returns surface-busy; transfers involving
a different derived surface synchronise its host shadow with device memory.

## Rate control and format limits

Sequence parameters establish dimensions, cropping, bitrate, GOP and rational
frame rate. Misc rate-control and frame-rate parameters take precedence. VBR
keeps average and peak bitrate separate; CQP uses picture QP. Equal configs
skip NVENC reconfiguration. Rate changes do not force IDR.

NVENC generates H.264 headers. Arbitrary client packed headers, exact reference
picture programming, all VA misc controls and arbitrary slice layouts are not
fully implemented. Capability reporting must not promise those semantics.

## External memory

The import path validates a single-object, single-layer linear NV12 descriptor
with two tightly packed planes, matching pitch, offsets and bounded allocation.
Duplicated file descriptors transfer ownership only after successful CUDA
import; the owning CUDA context remains alive through external-memory teardown.
Descriptor validation alone does not establish interoperability with arbitrary
DMA-BUF exporters or compositor modifiers. Surface export is unsupported.

## Device selection

An explicit PCI address takes precedence, followed by the display's DRM PCI
address. A sole CUDA device may be selected without a DRM match; ambiguous
selection fails. No application names or GPU models participate in routing.
