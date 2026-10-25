# Development

Build with `cargo build --release --locked`. See README.md for dependencies and
backend selection. Avoid global environment overrides during development when
other VA-API clients require decoding.

## Source map

| Path | Responsibility |
| --- | --- |
| `src/lib.rs` | Driver-init aliases and backend vtable |
| `src/driver/` | VA objects, encode lifecycle, image upload and synchronization |
| `src/ids.rs` | Typed object pools and non-reused public IDs |
| `src/h264/` | VA parameter parsing |
| `src/nvenc/` | NVENC resources, configuration and presets |
| `src/cuda/` | Device selection, memory allocation and external imports |
| `crates/va-sys/` | Generated libva backend bindings |
| `tests/` | ABI regressions and opt-in hardware integration |
| `tools/` | Installation, discovery and benchmark helpers |

Use explicit errors for unsupported layouts and controls. Preserve input and
output ownership on every failure path. Avoid allocation or copying of raw
frames when an existing CUDA surface can be reused. Keep compressed-output
copies distinct from raw-frame transfers when discussing performance.

Run the checks in [VALIDATION.md](VALIDATION.md). Keep local measurement
artifacts under ignored build output rather than project documentation.
