# ReelForge Host

One process over the four libraries. **Talk to this MCP, not four.**

```text
  video / photo
        │
        ▼
 ┌──────────────┐
 │ ReelForge-Host│   reelforge-host  (this repo)
 │  MCP + CLI   │
 └──────┬───────┘
        │
        ├── SightLoom (+ sightloom-host)   detect / enroll / search / VisionIndex
        ├── Intelligence                   rewrite_selectors → resolve → graph
        ├── ReelForge                      run_render_graph / encode
        └── Capture                        not used (screen grab only)
```

Host does **not** invent VisionIndex, SemanticEditPlan, or RenderGraph. It calls the siblings.

## Killer path

```bash
reelforge-host privacy-except \
  --video scene.mp4 \
  --photo alice.jpg \
  --output out.mp4 \
  --work-dir ./work
```

1. ffmpeg frames (RGB + pts)
2. `HostPipeline.ingest_frame`
3. `enroll_photo` / `search_photo` → subject id
4. `session.save_package(work/vision_index)`
5. Intelligence `rewrite_selectors` (FramePick → SubjectIds)
6. `resolve-bridge --mode final --write-mask-package`
7. `run_render_graph` + `SightloomPackageHost`

If photo search is not **Accept**, the process stops. It does not guess a subject.

Missing ONNX weights → **exit 2** (CI-friendly). Host does not vendor models. It looks in:

1. `--models-dir` / MCP `models_dir`
2. `REELFORGE_MODELS` / `SIGHTLOOM_MODELS`
3. `./.sightloom-models`
4. sibling `../SightLoom/.sightloom-models` (`yolov8n.onnx` + `person_reid.onnx`)

SightLoom owns the ONNX loaders; this process only picks the cache and runs the pipeline.

## MCP

```bash
reelforge-host serve
reelforge-host methods
```

| Tool | Role |
| --- | --- |
| `ingest_video` | decode + detect/track/embed + save package |
| `enroll_photo` | JPEG/PNG → gallery subject |
| `search_photo` | JPEG/PNG → ranked hits |
| `rewrite_plan` | FramePick → SubjectIds |
| `resolve_bridge` | Intelligence freeze + graph JSON |
| `run_graph` | ReelForge encode |
| `privacy_except` | all of the above |

Intelligence compiler tools (`compile_plan`, …) are **not** proxied.

## Competitive landscape

This slice is **video + reference photo → freeze identity → blur everyone else → encode**, as an embeddable CLI/MCP. Nearby tools solve adjacent problems.

| Tool | What it does | vs Host |
| --- | --- | --- |
| [deface](https://github.com/ORB-HD/deface) | Per-frame CenterFace, blur/mosaic/solid **all** faces | No photo enroll, no re-id, no “except this person”, no freeze hashes, no MCP |
| [Azure Video Indexer](https://learn.microsoft.com/en-us/azure/azure-video-indexer/face-redaction-with-api) Face Redaction | Cloud: upload + analyze, then redact `Include`/`Exclude` **their** face IDs | Two billable jobs; IDs come from Azure’s index, not a photo you pass; limited-access Face; no local MCP |
| [CaseGuard Studio](https://caseguard.com/help-center/manual/blur-faces-keep-one-visible/) | Desktop NLE: AI Search + Group by Person, keep one visible | GUI / FOIA workflow, not a library; no SemanticEditPlan, no agent MCP |
| [Secure Redact](https://www.secureredact.ai/) / [brighter AI](https://brighter.ai/) | Enterprise auto-blur or generative face replace (GDPR SaaS) | Closed cloud/edge; no hash-bound freeze, no rewrite-to-SubjectIds contract |
| [Fluendo](https://fluendo.com/blog/ai-face-enhanced-anonymizer/) AI anonymizer | Closest: detect + gallery + photo enroll + selective blur, GStreamer, on-prem | Closed plugin; published **~22 FPS** on MX550; no Intelligence freeze / approve / RenderGraph / MCP |
| OpenCV + ffmpeg scripts | Detect-and-blur every frame | No identity, no plan document, not reproducible across preview/final |

**Differentiation:** one stdio MCP, fail-closed `Accept` (never guess), host-owned ONNX (not vendored), Intelligence `ResolvedEditPlan` pins, ReelForge `MaskPackage` encode. Host is the orchestrator, not a new vision or render engine.

## Benchmarks

Measured with Criterion on a Windows development host, **release** profile (`cargo bench --bench host`). Times are midpoints of Criterion `[lo mid hi]`. Re-run on your machine for CI gates.

These are **Host orchestrator** numbers: rewrite, MCP, package→graph, ffmpeg probe/extract, tiny encode. They are **not** ONNX detect/re-id FPS (no weights in this repo).

| Workload | N / input | Time (approx.) |
| --- | --- | ---: |
| `rewrite_selectors` | 1 FramePick | **~290 ns** |
| `rewrite_selectors` | 8 edits | **~1.3 µs** |
| `rewrite_selectors` | 32 edits | **~5.4 µs** |
| MCP `rewrite_plan` | 1 / 8 / 32 | **~14 / 58 / 210 µs** |
| MCP `tools/list` | JSON-RPC | **~21 µs** |
| `require_accept` | 2 hits | **~5 ns** |
| build `blur_everyone_except` + rewrite | 1 photo pick | **~480 ns** |
| `resolve_bridge` final (2 subjects, 30 frames, disk package) | photo-except | **~17 ms** |
| `ffprobe` | 1 s 320×180 | **~34 ms** |
| extract RGB @ 5 fps | 1 s clip | **~68 ms** |
| `run_graph` encode + gaussian ROI | 1 s 320×180 | **~290 ms** |

Notes:

- Rewrite is essentially free. MCP JSON dominates (~50× the pure function).
- `resolve_bridge` includes VisionIndex load, mask-package write, Intelligence freeze, and graph JSON — still tens of milliseconds, not encode-bound.
- Encode of a 1 s QCIF-ish clip is ~0.3 s wall (~3× realtime at 320×180). 1080p / ONNX ingest is a different budget (see Fluendo’s **~22 FPS** detect+reid+anonymize on MX550 — that is *their* published number, not this crate).
- Azure / CaseGuard publish **job minutes**, not microbenchmarks (CaseGuard: ~6 min AI BWC vs ~1 h Premiere, their comparison). Not comparable to Host’s local graph path.

```bash
cargo bench --bench host
# HTML: target/criterion/report/index.html
```

CI compiles benches (`--no-run`). Full `privacy-except` needs ONNX on disk and is skipped (exit 2) without them.

## Layout

This repo expects siblings next to it (same as Intelligence CI):

```text
MyProjects/
  SightLoom/
  ReelForge/
  ReelForge-Intelligence/
  ReelForge-Capture/
  ReelForge-Host/     ← here
```

## Requirements

- Rust **1.97+**
- Host **ffmpeg** / **ffprobe** on `PATH`
- ONNX weights on disk for the real privacy path

## License

MIT © Sergii Ziborov
