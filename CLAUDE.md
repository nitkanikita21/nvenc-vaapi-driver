# Contributor guidance

This project implements a Linux libva encode backend in Rust using CUDA and
NVENC. Keep changes independent of particular applications or development
machines. Do not publish personal hardware details, session logs or local
benchmark reports in project documentation.

- Preserve the libva ABI and return explicit VA errors for unsupported input.
- Catch unwinding at exported ABI boundaries; never unwind into C.
- Document unsafe operations and validate sizes, identifiers and ownership.
- Keep input registrations session-local and unregister before freeing memory.
- Each successful encode must publish the matching coded picture.
- Keep mapped buffer pointers valid until unmap; reject conflicting operations.
- Run CPU tests and Clippy. Hardware tests are opt-in and require full output
  decoding; do not equate build success with live client compatibility.
- Do not add application-name checks, machine-specific paths or GPU models.
- Commit or push only on explicit request.

See README.md and docs/ for supported behavior and validation procedures.
