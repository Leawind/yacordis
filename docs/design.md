# Cordis Rust — 设计文档

本文档记录 cordis-rust 的概念来源、与 TypeScript 版（cordis v4）的对应关系、
有意的分歧，以及二进制插件分发（cdylib + C ABI）的预留设计。

参考资料：

- 论文：*A Programming Paradigm for Spatiotemporal Composability*（arXiv:2608.25512）
- 参考实现：<https://github.com/cordiverse/cordis>（main 分支 = v4）
- 本仓库里程碑 1 范围：同步核心（Context / Fiber / Effect / 事件 / 服务）

## 1. 概念映射（论文 ↔ v4 ↔ cordis-rust）

| 论文概念 | cordis v4 (TS) | cordis-rust |
|---|---|---|
| context（效应与余效应的统一中介） | `Context`（Proxy 对象） | `Context` 句柄 = `Arc<Core>` + 当前 `Weak<Fiber>`；无 Proxy，服务解析显式沿 fiber 父链回溯后落全局 store |
| revertible effect（可逆效应，时间性可组合） | `ctx.effect()` + Disposable | `Context::effect(label, setup)`；setup 返回 `Result<E: Into<EffectOutcome>, Box<dyn Error>>`，其执行期间经上下文注册的清理函数全部被收集，卸载时逆序执行 |
| coeffect 规约（余效应声明） | `inject`（string 数组或 `{name: config}`） | `Inject`（`required` / `optional` / builder `req`/`opt`） |
| reactive coeffect resolution（响应式余效应解析，空间性可组合） | fiber epoch：依赖 fiber uid 串接的签名，变更驱动 `_reload`/`_unload` | `epoch: Option<Vec<u64>>`（必选依赖的提供者 fiber id 序列）；`None` = 依赖缺失；变更经 `refresh() → drive_ops()` 同步驱动 Reload/Unload |
| component（组件 = effect + coeffect 的结合） | `Fiber`（插件实例，六态） | `Fiber`（四态：`Pending / Active / Failed / Disposed`） |
| 组件安装 / 移除 | `ctx.plugin()` 返回 thenable；`dispose()` | `ctx.plugin()/plugin_dyn()` 返回 `FiberHandle`；`handle.dispose()`（幂等） |
| 声明式组件加载器 | `@cordisjs/loader`（配置调和、HMR） | 里程碑 2：`cordis-loader`（cdylib + libloading，见 §4） |

v4 的六态中 `LOADING`/`UNLOADING` 在同步世界里是即时完成的中间态，
故收敛为四态；`internal/status` 事件携带 `(新状态摘要, 旧状态)`，与 v4 一致。

## 2. 核心机制

### 2.1 Effect 认领（嵌套作用域）

与 v4 `fiber.effect()` 的 collect 语义对齐：

- setup 执行期间，同 fiber 上嵌套注册的一切（`on`/`once`/`provide`/`effect`）
  被当前最内层 effect batch 认领（线程局部作用域栈按 fiber id 匹配路由）。
- 因此 dispose 外层 effect 会连带回滚内层 effect。
- 插件体本身就是一次 batch 收集（v4 的 `_execute(this._runner)`）。
- **逆序全序**：卸载时对 batch 列表逆序、batch 内逆序，整体等价于
  严格按注册先后逆序回滚。注意 setup 的「返回值 disposer」注册时机在
  setup 结束时（晚于期间嵌套注册者），因此最先被清理——与 v4 的
  `safeCollect(effect)` 时机一致。

### 2.2 依赖解析（epoch）

- `check_impls`：从全局 store 读取 inject 涉及的服务实现，按提供者
  `Arc` 指针比对是否有变化。
- `refresh`：全部**必选**依赖在场时 epoch = 提供者 fiber id 序列；
  缺失则 `None`。可选依赖永不阻塞激活，也不参与 epoch（对齐 koishi/v3
  语义，用户可按 `ctx.get` 判空）。
- epoch 变更 → `Reload`（None→Some）、`Unload`（Some→None）或
  `Unload+Reload`（Some→Some，服务热替换）。
- `provide` 撤销发生在 provide-disposer 内：先删全局条目，再 notify，
  依赖方在提供者完成自身收尾**之前**完成停用（对应 v4 在 provide
  disposer 中 `await` 依赖方重载完成的顺序）。
- 调度用 per-fiber `busy` 标志 + 待办队列防重入递归；所有用户代码
  （监听器、disposer、插件体）都在不持有任何锁时调用（快照后调用），
  这是支撑 FFI 回调重入的硬约束。

### 2.3 失败语义（有意偏离 v4）

- 插件体出错（含 panic，`catch_unwind` 转换）：**立即回滚**该次加载
  已收集的全部 effect，fiber 进入 `Failed`，错误保留至 `update()`。
  v4 会把部分 effect 留到下次卸载；我们认为「Failed 即无副作用」
  更符合论文的时间性可组合承诺。
- `handle.update(config)`：经 `internal/update` waterfall（中间件可
  改写配置）→ 清除错误 → 强制重启（epoch 先置 `None` 再重算，保证
  依赖未变时也重跑插件体）→ 若仍失败则返回错误。

### 2.4 事件系统

同步世界保留三种分发模式（v4 五模式的同步子集）：

| v4 | cordis-rust | 说明 |
|---|---|---|
| `emit` | `emit` | 注册序全部调用，忽略返回值 |
| `bail` / `serial` | `bail` | 注册序，首个非 `None` 返回值短路（异步聚合待异步里程碑） |
| `parallel` | —（暂缺） | 需要 async 聚合，见路线图 |
| `waterfall` | `middleware` + `waterfall` | v4 用同一 hook 表加 `next` 参数约定；Rust 静态签名不同，故注册表分开。panic 的中间件使链**中止**并返回错误（值通道永不混入 panic 副产物）；`emit`/`bail` 的监听器 panic 则吞掉转日志（§5.2） |

- 监听器统一签名 `Fn(&Context, &Value) -> Option<Value>`，字符串名
  注册；`Event` trait（`NAME` + `Args`/`Ret`）提供 serde 类型化封装。
- 约定：多参数事件用 `Value::Array` 承载（与 `internal/*` 一致）。
- `internal/*` 事件：`internal/plugin`（安装/卸载公告）、
  `internal/status`（状态迁移，携带旧状态）、`internal/service`
  （服务出现/撤回；值仅当服务本身是 `serde_json::Value` 时透出）、
  `internal/dispatch`（非 internal 事件的观测钩子）、
  `internal/update`（配置更新 waterfall）。
- isolate 已落地（§5.3：服务 store 按 `(name, label)` 键控，notify 按
  组隔离，事件级联不跨组）；v4 的 thisArg 过滤（`Context.filter`）与
  `global` 选项仍延后——事件分发本身尚无按组过滤。

### 2.5 服务双通道

- 类型化：`provide::<T>` / `get::<T>`（`Arc<dyn Any>` downcast）。
- 裸通道：`provide_raw` / `get_raw`（`Arc<dyn Any + Send + Sync>`）。
  这是里程碑 2 跨 C ABI 边界的传输形态。
- `set` 只允许提供者 fiber 自身调用（v4 的 multiple-fibers 检查）。

## 3. 与 TS 版的有意分歧（截至 M1）

1. **纯同步**：disposer/apply 不返回 Future；`parallel`/`serial` 的
   异步聚合暂缺（`serial` 语义并入 `bail`）。
2. **无 Proxy 系特性**：traceable/shadow、mixin/accessor、callable
   service、`ctx.xxx` 动态属性均不移植；一律显式 `ctx.get("name")`。
3. **无 isolate / intercept**（M1 时点的决策）：服务全局可见（按名字）。
   其中 isolate 已于 M3 落地（§5.3）；intercept、配置拦截合并、
   `check` 谓词、`Config.merge` 等仍留待后续里程碑。
4. **插件身份** = `Arc` 数据指针（v4 按解析出的函数身份）。同一插件
   值的多次安装共享 runtime 条目；重新包装 `Arc` 视为新插件。
5. **Failed 即回滚**（见 2.3）。
6. **长栈拼接**（`composeError`）舍弃；用 logger 的错误链替代。
7. `Service` 基类模式 → 惯用法：插件体内 `ctx.provide(...)`；宏封装
   留待 `cordis-macros`。
8. ready/dispose 生命周期事件（v3）不提供；root fiber 即装即活。

## 4. 里程碑 2（已实现）：二进制插件分发（cdylib + C ABI）

硬性需求：插件以预编译 cdylib 分发，用户从互联网/文件安装，宿主不
重启、不要求编译环境；「现场编写并构建」仅为开发者路径，产物仍走同
一加载管线。

宿主与插件之间**不做 Rust 类型直通**（预编译产物无法保证与宿主同
rustc/同依赖版本），边界为 C ABI（`cordis-plugin-api`，`ABI_VERSION = 1`）：

- **传输介质**：JSON 字节（`RawBuffer`，配置/事件/服务值/错误消息）、
  u64 句柄（宿主侧句柄表持有的 Context/Fiber/Effect）、
  `Callback`（函数指针 + user_data + drop_fn，插件代码跨界执行）。
- **缓冲区所有权（唯一规则）**：被调方释放输入缓冲区；调用方读取后
  释放输出缓冲区。所有缓冲区走进程共享的 `malloc`/`free`（同进程
  只有一个 libc 实例），任意一侧皆可释放，无需配对各侧分配器。
  **CRT 注意**：插件必须动态链接 CRT——Windows 静态 CRT（/MT）构建的
  插件自带独立堆，跨界 `free` 违反共享分配器假设（UB）。
- **插件导出**：`cordis_abi_version()`（握手，不符拒绝加载）、
  `cordis_plugin_manifest()`（名称 + required/optional 依赖）、
  `cordis_plugin_apply()`（apply 回调）、`cordis_plugin_free_buffer()`；
  由 `register_plugin!` 宏生成（edition 2024 的 unsafe attribute 语法）。
- **宿主 vtable**（单个静态实例）：effect/on/once/dispose_handle/
  provide/set/get/has/emit/bail/plugin/fiber_dispose/log/free_buffer。
  蹦床 `catch_unwind` 保证插件 panic → fiber `Failed` 而非 abort。
- **回调状态机**：一次性回调（effect setup/disposer）被二次调用返回
  `CODE_REUSED`；状态盒由 `drop_fn` 释放，与调用解耦。
- **卸载策略**：保留代码映射（状态效应完全回滚，机器码页留驻至进程
  退出），规避「卸载仍在执行的代码」的 UB。
- **加载源**：`Loader::load_file`（本地文件）、`Loader::fetch`
  （URL 下载，`download` feature，ureq）、`Loader::build_and_load`
  （调用 cargo 构建源码后加载，`--message-format=json` 解析 cdylib
  产物并合并编译诊断）。
- **插件作者视角**：依赖 `cordis-plugin-api`，写普通 `impl Plugin` +
  `register_plugin!(...)`，crate-type = `["cdylib"]`；API 形状与 native
  `cordis::Plugin` 一致。演示 fixture 与集成测试见
  `crates/cordis-loader/tests/`（跨 edition：插件侧 2021，宿主 2024）。

core 为此提供的保证：统一 `Arc<dyn Plugin>` 形态（`plugin_dyn`）、
Value 优先的动态载体、无锁重入（§2.2，FFI 回调内再 install/provide/on
均安全，集成测试覆盖）。

**边界限制（有意为之）**：跨 ABI 的服务值是 `serde_json::Value`
（typed downcast 仅 native 侧可用）；插件侧监听器不提供手动移除句柄
（随 fiber 卸载自动清理）。

## 5. 设计评审记录（2026-09-06，里程碑 3 后）

对已实现部分的一次全面自查：每个决策复核理由，缺陷当场修复，债务显式记录。

### 5.1 评审中发现并已修复的缺陷

1. **fiber 调度丢唤醒（并发正确性）**：原实现 `busy: AtomicBool` 与
   `pending: Mutex<Vec<Op>>` 分离，CAS 失败入队与持锁者排空之间存在
   窗口——racer 在持锁者最后一次排空之后、busy 释放之前入队的 op 会
   悬挂。修复：busy 与队列收进单个 `Mutex<ScheduleState>`，push 与
   busy 检查共享一把锁，用户代码仍在锁外执行。
2. **FFI 回调状态泄漏 + 双重释放隐患**：`SendCallback` 无 `Drop`，
   成功注册路径的插件闭包状态（`State<F>` 盒）在 fiber 卸载时永不
   释放；手写的 `Clone` 共享 `user_data`，一旦被使用即双重释放。
   修复：实现 `Drop`（释放后置空、恰好一次）、删除 `Clone`；vtable
   错误路径的显式释放一并移除（与 Drop 并存即双重释放）。
3. **`Plugin::inject` 重复查询**：每次 `check_impls`/`refresh` 都重新
   调用 trait 并分配。修复：install 时缓存进 `Fiber.inject`。

### 5.2 复核后维持的决策（含理由）

- **同步优先**：M1 阶段正确的复杂度取舍。代价已知且接受：`Plugin::
  apply`/`Disposable` 的同步签名是单向门，引入异步时现存插件代码
  必然破坏；`sleep` 阻塞调用线程，插件体内不得长阻塞（timer 文档已写明）。
- **effect 认领模型**（嵌套作用域 + 卸载逆序展平）：与 v4 逐点对齐，
  测试覆盖嵌套与手动中途释放。
- **epoch = `Option<Vec<u64>>`**：与 v4 字符串签名等价且更廉价；
  `u64::MAX` 兜底（依赖实现者已死）实践中不可达，仅防御性保留。
- **Failed 即立即回滚**（偏离 v4 的残留语义）：保证时间性可组合承诺，
  不回退。
- **C ABI 而非 Rust 类型直通**：预编译插件与宿主 rustc/依赖解耦的
  唯一可行解；`malloc/free` 共享分配器消除配对负担。限制：Windows
  静态 CRT（/MT）构建的插件会破坏共享堆假设，已注明于 §4。
- **卸载保留代码映射**：Rust 无法证明代码静默，泄漏机器码页换取
  卸载安全，语义上状态层面仍完全可逆。
- **waterfall 与普通 hook 分表**：静态签名无法复用 v4 的 `next` 参数
  约定；已文档化。
- **监听器 panic 吞掉转日志**（v4 会向 emit 传播）：宿主健壮性优先，
  与 FFI 侧行为一致。

### 5.3 设计债清偿（2026-09-06 第二轮）

评审列出的债务已按依赖顺序逐项处理：

| 债务 | 处置 |
|---|---|
| 服务名全局唯一，无 isolate | ✅ `ctx.isolate(name)` 落地：store 按 `(name, label)` 键控，notify 按组隔离，同名服务跨组共存 |
| `visible`/`dep_store` 双映射 | ✅ 删除快照表；可见性坍缩为单一规则（provider active 或请求者即 provider），fiber 链回溯解析一并移除 |
| `dispose()` 返回永不 Err 的 `Result` | ✅ 改为 `bool` |
| waterfall panic 返回 `Null` | ✅ `waterfall`/`Middleware`/`Next` 全链 `Result` 化，panic 转错误 |
| 命名撞车 `Disposable` | ✅ core 侧别名改名 `Disposer`；`on/once` 返回 `ListenerHandle` 别名 |
| 核心无内省 API | ✅ `installed_plugins()` / `provided_services()` |
| FFI 句柄表只增不减 | ✅ `get_context` 顺手 GC（fiber 已销毁的条目即取即删），FfiPlugin 缓存条目先验证后复用 |
| 并发调度无 stress 测试 | ✅ 8 线程 × 40 轮 install/dispose 环 + 看门狗 |

**stress 测试揪出的真 bug 与语义决策**（本轮最有价值的产出）：

1. **stale op 重放**：级联中排队的 Reload/Unload op 与真实 epoch 可能
   脱节，多线程交错下会把已停用的 fiber 拉回 Active。修复：op 携带
   epoch 快照，执行时不匹配即丢弃（`run_op` stale-guard）。
2. **同步边界**：`install`/`dispose`/`restart` 的级联在无竞争时同步
   完成；有并发交错时级联可能「稍后」落地。为提供与 v4 `inertia`
   对应的同步语义，新增 `Core::quiesce()`（等待全部 op 执行完毕）与
   `FiberHandle::wait_for(state, timeout)`，并在 install/dispose/restart
   内部调用——【重入场景（插件 body 内再 install/dispose）自动跳过】，
   否则 quiesce 会等待自身正在执行的外层 op，形成自死锁。
3. **重入 quiesce 自死锁**（上面第 2 条的防御本身曾是死锁源）：以
   thread-local `EXECUTING_OPS` 检测「本线程正处于某个 op 内」来跳过。

**仍然接受的债**（更新后余量）：

1. 并发交错下 `install`/`dispose` 返回时，【其他线程】触发的级联可能
   尚未落地——强断言需配 `wait_for`/`quiesce`。彻底消除需把生命周期
   改造成单线程 actor 执行器，属异步里程碑的架构决策。
2. typed 事件每监听器重复反序列化（不变，量级小）。
3. 插件侧监听器无手动移除句柄（不变，随 fiber 卸载自动清理覆盖主场景）。
4. `Fiber.parent`/`Fiber.isolate` 字段暂只写不读（为 scope 内省预留）。

## 6. 路线图

1. ~~M2：`cordis-plugin-api` + `cordis-loader`~~（✅ 见 §4）。
2. 异步生命周期：async apply/dispose、fiber inertia 互斥、
   `parallel`/`serial` 分发模式。
3. ~~`cordis-timer` / `cordis-logger`~~（✅ M3：同步线程定时轮 + 分级
   日志服务；`sleep` 以 effect 挂靠 fiber，卸载即唤醒，详见各自 crate
   文档）.
4. 配置校验（Standard Schema 等价物）、intercept（isolate 已落地，
   见 §5.3；依赖环诊断已落地，见 §7）。
5. 插件仓库索引（版本解析、依赖声明、签名校验，叠加在 `fetch` 之上）。
6. `cordis-macros`（`#[plugin]`/`#[inject]` 派生宏、Service 模式宏）。

## 7. TS 上游对照与跟进（2026-09-06）

cordis TS（v4）处于 rc 阶段（v4.0.0-rc.9），核心范式一年无破坏性变更，
但其边界语义仍被 DeepSeek Harness 的实战持续打磨。本仓库的跟进策略：
**不追功能，盯 cordiverse/cordis 的 issue 列表**——那里的问题就是这个
范式在真实负载下暴露的裂缝预告片。逐项对照：

| 上游 issue | 主题 | cordis-rust 状态 |
|---|---|---|
| #34 | epoch ABA：async 加载期间 update 改配置，epoch 感知不到 | **结构性免疫**：同步核没有异步加载窗口；并发排队 op 另有 epoch 快照 + stale-guard（§5.3），测试锁定于 `tests/stale_ops.rs` |
| #96 / #91 | 卸载中注册 effect/listener → 泄漏 | **已覆盖**：`unloading` 态注册一律 `InactiveContext`、effect 即时回滚，测试锁定于 `tests/unloading.rs` |
| #110 | provider 先于 consumer 卸载（`Promise.all` 竞态） | **结构性免疫**：同步级联，consumer 在 provider 的 provide-disposer 内完成停用；一层与深链（A←B←C）均有顺序断言（`tests/lifecycle.rs`、`tests/cascade.rs`） |
| #66 | 循环插件依赖无检测 | **已落地**：`Plugin::provides` 声明 + SCC 检测 + 日志/内省报告（`src/diagnose.rs`）；body-only provide 不可检测，与上游同为开放边界 |
| #43 | waterfall 结构化取消语义 | **部分对齐**：全链 `Result` 化、panic 转 Err（§2.4）；完整的取消语义上游也未定论 |
| #90 / #102 | intercept 重设计、Standard Schema 诊断化 | **路线图**（§6.4），等上游语义收敛后再动 |

两个可以写进定位的差异点（同步模型的红利，均非有意为之的性能取舍，
而是模型选择带来的结构性消除）：其一，v4 的 #34/#110 类竞态根植于
「异步生命周期 + 事件循环」，同步核里这一整类窗口不存在；其二，
二进制插件分发（cdylib + C ABI）在 TS 里由 `require()` 免费提供，
在我们这里是约 1,400 行的一等能力（§4）——两边的成本结构互为镜像。

code size 对比（2026-09-06 时点，仅产品代码）：核心 TS 1,592 行 vs
Rust 2,059 行（+29%，多出的是 TS 靠事件循环与 Proxy 免费获得的部分：
调度队列、stale-op 防护、quiesce、显式服务解析）；reflect 层反而更短
（-40%）；FFI 三件套（plugin-api + loader）约 1,400 行为纯增量。
