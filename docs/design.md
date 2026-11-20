# Cordis in Rust — Design Document

This document records what yacordis keeps on purpose: the concept mapping to the paper and cordis v4, the intentional divergences, and the contract for binary component distribution (cdylib + C ABI).

## Concept Mapping (Paper ↔ v4 ↔ yacordis)

| Paper concept                                            | cordis v4 (TS)                                                                  | yacordis                                                                                                                                                                                                                                         |
| -------------------------------------------------------- | ------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| context (the unifying mediator of effects and coeffects) | `Context` (a Proxy object)                                                      | `Context` handle = `Arc<Core>` + current `Weak<Fiber>`; service resolution walks the fiber parent chain explicitly, then falls back to the global store                                                                                          |
| revertible effect (temporally composable)                | `ctx.effect()` + Disposable                                                     | `Context::effect(label, setup)`; setup returns `Result<E: Into<EffectOutcome>, Box<dyn Error>>`, and everything registered through the context during its execution is collected into the effect batch and disposed in reverse order on teardown |
| coeffect declaration                                     | `inject` (string array or `{name: config}`)                                     | `Inject` (`required` / `optional` / builder `req`/`opt`)                                                                                                                                                                                         |
| reactive coeffect resolution (spatially composable)      | fiber epoch: a signature keyed on fiber uids; changes drive `_reload`/`_unload` | `epoch: Option<Vec<u64>>` (sequence of provider fiber ids of required dependencies); `None` = dependencies missing; changes synchronously drive Reload/Unload via `refresh() → drive_ops()`                                                      |
| component (effect + coeffect combined)                   | `Fiber` (component instance, six states)                                        | `Fiber` (four states: `Pending / Active / Failed / Disposed`)                                                                                                                                                                                    |
| component install / remove                               | `ctx.component()` returns a thenable; `dispose()`                               | `ctx.component()/component_dyn()` returns a `FiberHandle`; `handle.dispose()` (idempotent)                                                                                                                                                       |
| declarative plugin loader                                | `@cordisjs/loader` (config reconciliation, HMR)                                 | Milestone 2: `yacordis-loader` (cdylib + libloading, see "Binary Component Distribution")                                                                                                                                                        |

In the synchronous world `LOADING`/`UNLOADING` complete immediately, so states collapse to four; the `internal/status` event carries `(new state summary, old state)`, same as v4.

## Intentional Divergences from the TS Version

These are deliberate. Do not "fix" them back to v4 behavior.

1. **Purely synchronous**: lifecycle signatures stay synchronous (a model choice); concurrency needs are handled by framework plumbing (`run_blocking`/`spawn_future`, drain-before-rollback), and the `serial` semantics is folded into `bail`; v4's fiber inertia corresponds to this implementation's `quiesce`/drain semantics.
2. **Failed means rollback**: on a component body error (including panics, converted via `catch_unwind`) all effects collected by that install are rolled back immediately, in reverse order; the fiber enters `Failed` with the error kept until `update()`. "Failed means no side effects" honors the paper's promise of temporal composability. v4 keeps the collected disposers until the next unload instead — do not adopt that residual semantics in new failure paths.
3. **Explicit service access**: always `ctx.get("name")` (corresponding to v4's traceable/shadow, mixin/accessor, callable service, `ctx.xxx` dynamic property, and other access forms).
4. **Service visibility**: isolate keyed by `(name, label)` (notify isolated per group, event cascades do not cross groups); service intercept (layered override along the fiber chain, see "Intercept — Rationale"); the `check` predicate as declarative schema gating (`ctx.provide_schema`, see "Service Value Validation — Rationale"). Config interception merge (`Config.merge`) and typed service custom `mergeIntercept` await later milestones.
5. **Component identity** = the `Arc` data pointer (v4 uses the resolved function identity). Multiple installs of the same component value share a runtime entry; re-wrapping an `Arc` counts as a new component.
6. Error chains are carried by the logger (corresponding to v4's `composeError` long-stack concatenation).
7. `Service` base-class pattern → idiom: `ctx.provide(...)` inside the component body; the `#[component]` attribute macro (`yacordis-macros`); a Service-pattern macro awaits demand.
8. The root fiber is installed and active immediately.

### Intercept — Rationale

Intercept (upstream PR #90/#206) is layered per-context override of service config: layers are collected along the fiber ancestor chain of the requesting context, shallowly merged (`Object.assign` semantics), nearest wins, visible only to the registering fiber and its descendants. Registration is an effect (removed on fiber unload) and a pure view: it does not affect epoch, trigger notify, or reload providers. Typed services bypass it. Divergence from upstream: v4's `Service` class carries metadata and customizes `mergeIntercept`; here interception merging is carried by the JSON channel only.

### Service Value Validation — Rationale

`ctx.provide_schema(name, schema)` corresponds to upstream's `provide(name, service, check)`: a declarative schema gate validating every subsequent provide/set of a JSON service, reusing the config validation DSL. Registration is an effect; ownership is identified by schema node pointer equality. Typed services bypass it (the same trade-off as intercept). The duty of `resolveConfig` falls to schema defaults.

## Binary Component Distribution (cdylib + C ABI)

Hard requirement: components are distributed as prebuilt cdylibs, installed by users from the internet or files while the host stays running. The C ABI (`yacordis-component-api`, `ABI_VERSION = 4`) is the boundary, keeping component artifacts independent of the host's rustc/dependency versions.

- **Transport media**: JSON bytes (`RawBuffer`), u64 handles (Context/Fiber/Effect held in the host-side handle table), `Callback` (function pointer + user_data + drop_fn).
- **Buffer ownership (the single rule)**: the callee frees the input buffer; the caller frees the output buffer after reading. All buffers go through the process-shared `malloc`/`free`. **CRT caveat**: components must link the CRT dynamically — a Windows static CRT (/MT) build ships its own heap, and freeing across the boundary violates the shared allocator assumption (UB).
- **Component exports** (`register_component!` macro, edition 2024 unsafe attribute syntax): `yacordis_abi_version()` (handshake), `yacordis_component_manifest()`, `yacordis_component_apply()`, `yacordis_component_free_buffer()`.
- **Callback state machine**: one-shot callbacks return `CODE_REUSED` when invoked a second time; the state box is freed by `drop_fn`, decoupled from invocation.
- **Unload strategy**: verified unload — `ComponentLibrary::unload` unmaps the binary once the host has proven a quiet state (no in-flight component code, every fiber disposed, no component instance held outside its own adapter), and a failed proof is a refusal naming the violation point that degrades to keep-mapped. Every entry into component code passes through the library lease's gate, which is serialized against unmapping — that gate, not the mapping, is what makes unmap safe.
- **Component author's view**: depend on `yacordis-component-api`, write an ordinary `impl Component` + `register_component!(...)`, crate-type = `["cdylib"]`; the API shape matches the native `yacordis::Component`. See `crates/yacordis-loader/tests/` for fixtures.

What core provides for this: a uniform `Arc<dyn Component>` shape (`component_dyn`), a Value-first dynamic carrier, and lock-free reentrancy (installing/providing/on inside FFI callbacks is safe). **Boundary limits (intentional)**: service values across the ABI are `serde_json::Value` (typed downcast is native-only).

### Unloading Limitation Contract (Component Spec Invariants)

"Clean unloading" has three layers: ① **semantically clean** (everything registered through the framework is provably reclaimed) — guaranteed by the effect collection structure; ② **code clean** (code pages valid throughout) — carried by verified unload: the mapping is revoked only after the host proves no component code can ever run again (process quiescence, the in-flight gate at zero, every adapter of the library released); a failed proof keeps the library mapped; ③ **process clean** (no accumulated residue in long-running processes) — relies on the component author honoring these three invariants:

- **I1 (single scheduling domain)**: component code only executes within framework-initiated calls (vtable calls, or work units registered as effects). Corollary: before unload the framework can drain all in-flight work (curtain call → drain → rollback), so no concurrent component code exists during rollback. Threads and async may exist, but only as host plumbing.
- **I2 (resource ownership)**: every resource a component acquires is either registered through `ctx` (framework-held, recorded in the handle table) or never escapes the component body scope. Corollary: at dispose time an empty handle table ⇒ semantically clean. The host half is mechanically checkable (dispose audit); the component half is sealed by capability-style APIs + lints.
- **I3 (no cross-install state)**: a component must not hold mutable state surviving across installs; behavior is a pure function of (config, service view, received events). Corollary: reload = a fresh instance, giving correct hot-reload semantics.

Enforcement: I1 by ABI structure (entry only via vtable) + lints; I2 host half by runtime assertions in the dispose audit, component half by lint profile (the `build_and_load` path holds the sources so it can be mechanically enforced; prebuilt binary distribution degrades to trusting the publisher); I3 by lints. What the host can see, unload refuses: in-flight component code and undisposed fibers turn into refusals naming the violation point, and a refused unload keeps the library mapped — keep-mapped is the backstop so anything that refuses to prove itself quiet degrades at worst to memory leaks. An invisible I1 violation (raw threads running component code that never passes back through the framework) cannot be detected by the host, and unloading such a binary is undefined behavior — which is why unload requires trust, not just the runtime audit. Subprocesses are the compliant escape hatch for CPU parallelism and fault isolation — process death is the only perfect unloading.

### The Invariants in Code Terms (For Plugin Authors)

What I1–I3 mean at the keyboard — the bundled lints catch the mechanical parts at build time; the runtime refusal audit is the remainder:

- **I1**: run code only when the framework calls you — the component body, effect setups, listeners, work callbacks, disposers. Do not `std::thread::spawn`, do not run your own scheduler: a thread that outlives the framework's calls is in flight forever, and unloading the binary under it is undefined behavior. Framework-mediated work (`run_blocking`/`spawn_future`) is the supported form — dispose drains it, so it never blocks an unload.
- **I2**: acquire resources through the `Context` and free buffers through the buffer protocol; nothing escapes the component body scope. The host reclaims every context handle bound to a dying fiber; your half is what makes the effect rollback actually clean.
- **I3**: no mutable state that survives across installs — behavior is a pure function of (config, service view, received events). A reloaded component is a fresh instance, which is what makes the unload → rebuild → reload loop (on Windows, the artifact file becomes writable again) produce correct semantics.

With all three honored, the lifecycle `install → use → dispose → unload` returns the process to the state before the load: the binary is unmapped and its file writable again. The unload tests (`crates/yacordis-loader/tests/unload.rs`) pin both this happy path and the refusal paths.

## Accepted Debt

Known limitations accepted in the current design:

1. Typed events re-deserialize per listener (small scale).
2. `Fiber.isolate` is currently write-only (reserved for scope introspection); `Fiber.parent` is read by intercept visibility (the fiber-chain walk).

Resolved: the former concurrency-interleaving debt (a cascade triggered by another thread may land after `install`/`dispose` returned) is closed for unload — the one operation that could not tolerate it. Unload waits for process quiescence (`yacordis::quiesce`) and then verifies its preconditions against the live gate instead of trusting returned flags; callers that need a settled view have `quiesce`/`wait_for`. Lifecycle calls themselves remain eventually-consistent under interleaving, by design.
