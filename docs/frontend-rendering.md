# Frontend rendering: would Rust/WASM help?

Question: a heavy browser view (the K2 printer's live 3D G-code preview, an iframe on the HA
dashboard) is slow. Would rewriting it in Rust compiled to WASM make it more efficient?

**Short answer: only partly, and not where it hurts most.** WASM speeds up the CPU work that's
a tight loop (parsing the file), but most of the cost is rebuilding 3D geometry and sending it
to the GPU. Doing less of that work helps more than making it faster.

This runs in the viewer's browser, not on the hub, so it doesn't affect lean-home's own footprint.
It matters only for which frontend pieces would be worth porting to Rust.

## Measurement

2026-11-22, the dev PC ([measurement rules](architecture.md#measurement-rules)). Headless Chromium
with software WebGL (SwiftShader), canvas 1600×900, [gcode-preview](https://github.com/remcoder/gcode-preview)
2.18.0 + three.js 0.159.0. Input: a real OrcaSlicer print for the K2, 8.6 MB, 289k lines, 20,935 arc
moves (G2/G3), whole file drawn. Single run each.

| Mode | Parse (JS) | Rebuild all geometry |
|---|---|---|
| Lines (1 px `LineSegments`, the default) | ~450 ms | **57 ms** |
| Tubes (`renderTubes`, 0.42 × 0.2 mm, a real 3D extrusion like a slicer) | ~450 ms | **603 ms** |

Parse = first draw − rebuild (the two runs agree: 447 and 449 ms). Each rebuild regenerates every
layer: the library has no incremental mode. Tubes are 10× the rebuild cost of lines for the same
file. On a phone or wall tablet, expect several times slower (not measured).

## Where WASM would and wouldn't help

| Cost | Nature | Would Rust/WASM help? |
|---|---|---|
| Parse, ~450 ms once per file | Byte/number scanning loop over 8.6 MB | **Yes.** Typically several times faster than JS (estimate, not measured). It runs once per file, so it only shortens the first load. |
| Rebuilding tube geometry, ~600 ms per rebuild | Generating vertices for 280k segments, then the upload to WebGL | **Partly.** Vertex generation would get faster, but the upload to WebGL and three.js scene handling stay in JS. Copying the output out of WASM adds work. |
| Rebuilding *everything* every time | Algorithmic: O(whole print) for each new layer | **No.** The same waste in Rust is still waste. |

What helps more, in order:

1. **Rebuild less.** Build geometry once per layer and keep it, instead of regenerating the whole print
   (gcode-preview rebuilds everything; a layer-at-a-time cache makes each update O(new layer)).
   The homelab page approximates this: a full rebuild only when a new layer starts, with the moves since
   then drawn as cheap lines every 0.5 s.
2. **Let the GPU make the tubes.** Instanced rendering: one tube mesh, one per-segment
   (start, end, width) attribute. The CPU only writes 7 floats per segment and the GPU does the rest,
   so rebuild cost falls to the size of that buffer.
3. **Then** move the parser to WASM (or a Web Worker, JS or WASM) so the first load doesn't freeze the page.

## Takeaway for lean-home

Don't port frontend views to Rust/WASM for speed by default. Fix the algorithm first, use the GPU second.
WASM only helps hot loops over large data, like parsing. Prefer a Worker for those so the page stays
responsive. The same rule as the core applies: measure first.
