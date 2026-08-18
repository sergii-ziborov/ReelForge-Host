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

Missing ONNX weights → **exit 2** (CI-friendly):

```text
.sightloom-models/person_detect.onnx
.sightloom-models/person_reid.onnx
```

Weights are never committed.

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
