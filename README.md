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
  --work-dir ./work \
  --sample-fps 2 \
  --max-frames 30

# detect+reid FPS only (no photo / encode)
reelforge-host ingest --video scene.mp4 --sample-fps 2 --max-frames 6 --embed-every 2

# live camera (Windows dshow) or synthetic
reelforge-host ingest --video cam --live-secs 3
reelforge-host ingest --video "lavfi:testsrc=size=640x360:rate=10" --live-secs 2
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

Two layers. **Orchestrator** (Criterion, no models). **Vision+encode** (release wall-clock on this Windows host, YOLOv8n + OSNet via tract CPU, 1280×720 two-person scene).

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
| `run_graph` encode + gaussian ROI | 1 s **1280×720** | **~310 ms** |

**Vision + encode (release, tract CPU, 1280×720, 2 people, 6 sampled frames):**

| Phase | Time | Notes |
| --- | ---: | --- |
| extract @ 2 fps | **174 ms** | skip-frame vs 10 fps source |
| enroll photo | **424 ms** | OSNet |
| ingest detect+track+embed | **7.2 s** | **~0.83 FPS** (`embed-every 1`) |
| ingest `--embed-every 2` | **4.7 s** | **~1.27 FPS** (track every sample, embed half) |
| search Accept | **479 ms** | score 1.000 |
| compile / package / graph | **46 ms** | Intelligence |
| encode 3 s 720p | **4.7 s** | ~0.64× realtime |
| **e2e privacy-except** | **13.5 s** | |

vs Fluendo **~22 FPS** detect+reid+anonymize on MX550 GPU — we lose ~25× on ingest (CPU tract, no skip-inside-frame, no CUDA). Skip-frame (`--sample-fps`) is the Host lever, not a faster detector.

Notes:

- Rewrite is essentially free. MCP JSON dominates (~50× the pure function).
- `resolve_bridge` / compile (~46 ms) is not the bottleneck. Ingest + encode are.
- Azure / CaseGuard publish **job minutes**, not microbenchmarks.

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
