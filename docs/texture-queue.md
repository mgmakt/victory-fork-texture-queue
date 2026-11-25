# Texture-queue branch: what blocks textures at high draws, and three switches

This branch is a **work tree of `fxceless/victory-fork` at `39731795d2`** with three merged layers:

| layer | source | note |
|---|---|---|
| upstream | `fxceless/victory-fork` `39731795d2` (2026-11-25 15:38Z) | *What a frame is made of, measured; parts joined and copy-back through our own view* |
| water read-back | `Harveyweiii/victory-fork` `d36f9652` (`texback`/`water_readback`) | the title's CPU reads the ocean's height and displacement maps its compute passes wrote; without the read-back, boats float on unwritten memory. `VICTORY_TEXTURE_BACK=1` |
| mesh launch | `fablinoo/kelvaroq` patch `0041-mesh-launch-wide-hash` | an amplification stage of ours in front of each translated mesh shader launches the indirect records, so there is no clamp pass and no full wait between mesh draws. `VICTORY_MESH_LAUNCH` (on; off under ray tracing, where it measured slower) |
| **this branch** | texture queue probe + rotation + rate scale | below |

Everything else is upstream. Build with the same recipe as upstream (rustc 1.98.1, `x86_64-pc-windows-gnu`):

```powershell
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = '<llvm-mingw>\x86_64-w64-mingw32-clang.exe'
$env:RUSTFLAGS = '-C link-self-contained=yes -C dlltool=llvm-dlltool.exe'
cargo +1.98.1 build --release -p victory --features 'blake3/pure' --target-dir target/llvm-gnu
```

## What this branch adds

The pass over textures the title picks by index (`draw.rs::heap_textures`) was measured with a
probe on an RTX 4070 Ti at 1920×1080, same save, warm pipeline cache. What it said:

| run | per 5 s window | uploads | held back for card room | pass `room` |
|---|---|---|---|---|
| card copies for title buffers 1024 MB | due 53,248 | 3,509 | **8,353** | often `false` |
| the same at **256 MB** | due 57,344 | 3,473 | **0** in 5 of 6 samples | `true` |
| 256 MB + queue rotation | — | 2,367 | — | **34% fewer passes** |

Three findings, in order of size:

1. **The pass itself is the largest single block of a heavy frame.** One pass took **287 ms** on a
   **362 ms** frame, and the frame line reported `textures 4.5 ms` for it — under-reported more than
   fifty times. A backlog of 100,000+ slots drains at 2,000-3,000 a window.
2. **When the card has no room, no budget helps.** On a full card the pass holds slots back
   (`held_for_room`) and uploads 12-35 textures a window; lowering the room kept for card copies of
   title buffers (`VICTORY_CARD_BUFFERS_MB`, 1024 → 256) took `held_for_room` to zero in the sampled
   windows.
3. **The queue was always walked from its front.** Each pass re-scanned the same head and stopped at
   its budget, so slots behind it were never reached (136,000-168,000 left due every window).

### Switches (all on by default; `=0` restores the upstream behaviour)

| switch | what it does |
|---|---|
| `VICTORY_TEXTURE_PROBE` | one line every five seconds: `texture queue probe passes=… ms_in_them=… due=… uploaded=… held_for_room=… stopped_with_slots_due=… slots_left_due=… room=…`. `stopped_with_slots_due` counts the passes the layer's own budget cut short **while the card had room** — those are textures waiting on us, not on the card |
| `VICTORY_TEXTURE_RATE_SCALE` | the bytes a pass may stage are a rate at sixty frames a second; a frame heavy in draws lasts three to six of those, so the budget now grows with the frame's length (up to four times) instead of letting the rate collapse |
| `VICTORY_TEXTURE_ROTATE` | the pass starts after the slot the pass before stopped at, instead of at the queue's front |

Measured with the probe, not yet in a same-scene A/B for the last two: see
`docs/texture-queue.md` for the raw windows and the honest limits.

## Provenance and license

GPL-3.0-or-later, as upstream (`LICENSE`). The water read-back and the mesh launch are other
authors' work, credited above and in the commit message; this branch only adds the probe and the
two queue changes.
