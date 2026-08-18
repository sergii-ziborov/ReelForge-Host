# Use cases, surfaces, and what not to build

ReelForge is a **compiler + freeze + encode** for “this person stays sharp, everyone else is gone.” Superpower is the contract, not a UI skin.

## Variants (who is talking)

| # | Who | What they want | Surface | Not this |
| --- | --- | --- | --- | --- |
| 1 | **Human operator** (FOIA, newsroom, court, school) | Video + photo → file they can publish | CLI / egui / Vite **on the same machine as Host** | Cloud Face IDs, Premiere plugin |
| 2 | **AI coding agent** | Call one tool, get a path or a hard error | **Host MCP** (`serve` stdio or `serve --http`) | LSP, a second tool catalog, `compile_plan` dump |
| 3 | **Hosted SaaS user** | Browser, upload, wait, download | Vite → **HTTPS Host** (same tools + token) | Electron, vendored ONNX in the frontend |
| 4 | **Library embedder** | Intent → freeze → graph in *their* process | Intelligence + ReelForge crates | Host MCP required |
| 5 | **Capture operator** | Screen session → privacy-except | Capture grab, then Host `capture:` / project.json | Host globbing `segments/` |

Finance / billing / “AI trading” is **not** a ReelForge slice. The agent slice is **video privacy as a tool**.

## Superpower (why this is not deface + a wrapper)

1. **Fail-closed identity.** Photo search must **Accept**. No “closest face.” Azure/CaseGuard guess or use *their* IDs. We stop.
2. **Freeze.** `ResolvedEditPlan` pins VisionIndex generation + hashes. Preview and final are not the same document. Agents can replay.
3. **Compiler, not a black box.** Intent (`SemanticEditPlan`) → IR → approve → `RenderGraph`. You can audit *why* a box was redacted.
4. **One MCP, four libraries.** Agents talk to Host, not SightLoom + Intelligence + ReelForge + Capture.
5. **Local by default.** Video never has to leave the machine. That is the product for anyone who cannot upload evidence.
6. **Anonymity vs preview.** Host default **pixelate**. Gaussian is recoverable — we say so.

Speed is **not** the superpower (tract CPU loses to Fluendo GPU). Honesty is.

## Money (not the agent slice, but you asked)

Do **not** plan “100% donations.” Donations can keep *libraries* (Intelligence, SightLoom) fed. They will not pay encode minutes or support.

| Model | Fits | Breaks |
| --- | --- | --- |
| OSS + donations | Crates, benches, issues | Hosted GPU, SLA, support |
| **Local license / support** for orgs that cannot upload | Superpower #5 | Consumer TikTok |
| Hosted SaaS (metered jobs) | Students, NGOs, “I have one clip” | Anyone with sealed evidence |
| Agent API (same MCP, token, per-job) | Coding agents / internal bots | Same as hosted, plus abuse |

Charge the **job** (minutes × resolution) or the **seat** (air-gapped newsroom). Do not charge “AI features.” The AI is the *caller*.

## Studio public or private?

**Keep [ReelForge-Studio](https://github.com/sergii-ziborov/ReelForge-Studio) public.**

- It is a thin client: no weights, no customer video, no secret protocol.
- Hiding it hides nothing Host does not already publish.
- Agents and contributors need the Vite/egui/mcport wiring.
- Go private only if you later add **billing UI, customer tenants, or unreleased SaaS chrome**. Not now.

Host, Intelligence, Capture, SightLoom stay public. That *is* the moat documentation.

## MCP vs hosted vs a new MCP repo

**Do not write a second MCP catalog. Do not split MCP into its own product repo.**

| Process | Role |
| --- | --- |
| `reelforge-host serve` | **The** MCP. Stdio for local agents (Cursor, Claude Code, Grok). |
| `reelforge-host serve --http` | Same tools. GUI + hosted + remote agents. Token off-loopback. |
| `reelforge-studio-mcp` | Optional **stdio adapter** when Host is already HTTP for the GUI. mcport, no Tokio. Not a new API. |
| Hosted | HTTPS front door to the **same** tool names. Auth + quotas. Not a new schema. |

Intelligence `serve` stays the **compiler** MCP (`compile_plan`, `rewrite_selectors`). Agents that only want “blur everyone except this photo” must **not** see that catalog.

Agent-facing tools that matter: `privacy_except`, `search_photo` (Accept or die), `health`. The rest is for humans/scripts.

## Slice for AI agents (not finance)

Agents need:

- one tool that does the killer path;
- structured JSON (`subject_id`, `output`, `audio`);
- a hard error they can show the user (`PhotoNotAccepted`, missing weights exit 2);
- no LSP.

They do **not** need: hover on `SemanticEditPlan`, workspace symbols, or a finance copilot.

## LSP — should we build one?

**No, not as a ReelForge product.**

[weavatrix-lsp](https://github.com/sergii-ziborov) is the right shape for **source code evidence**. ReelForge is media + freeze. An LSP for `intent.json` would give completion on `redact_pii` / `blur_everyone_except`. That is a weekend schema plugin, not a repo.

| Protocol | For ReelForge |
| --- | --- |
| **MCP** | Yes — agents *do* the job |
| **HTTP MCP** | Yes — GUI + hosted |
| **LSP** | No — unless editors demand JSON completions later |
| **mcport** | Yes — as the *runtime* for small stdio shims, not a new domain |

If an agent needs “explain this plan,” add an MCP tool `explain_plan` that calls Intelligence. Do not stand up `reelforge-lsp`.

## What to build next (agent slice)

1. Keep Host MCP as the source of truth.
2. Document Cursor/Claude `mcp.json` → `reelforge-host serve`.
3. Hosted later = same tools + token + upload. Not a new MCP.
4. Skip LSP. Skip a `ReelForge-MCP` repo. Skip donations-as-strategy.
