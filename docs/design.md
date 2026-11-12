# Cordis Rust — 设计文档

本文档记录 yacordis 的概念来源、与 TypeScript 版（cordis v4）的对应关系、
有意的分歧，以及二进制组件分发（cdylib + C ABI）的预留设计。

参考资料：

- 论文：*A Programming Paradigm for Spatiotemporal Composability*（arXiv:2608.25512）
- 参考实现：<https://github.com/cordiverse/cordis>（main 分支 = v4）
- 本仓库里程碑 1 范围：同步核心（Context / Fiber / Effect / 事件 / 服务）

## 1. 概念映射（论文 ↔ v4 ↔ yacordis）

| 论文概念 | cordis v4 (TS) | yacordis |
|---|---|---|
| context（效应与余效应的统一中介） | `Context`（Proxy 对象） | `Context` 句柄 = `Arc<Core>` + 当前 `Weak<Fiber>`；服务解析显式沿 fiber 父链回溯后落全局 store |
| revertible effect（可逆效应，时间性可组合） | `ctx.effect()` + Disposable | `Context::effect(label, setup)`；setup 返回 `Result<E: Into<EffectOutcome>, Box<dyn Error>>`，其执行期间经上下文注册的清理函数全部被收集，卸载时逆序执行 |
| coeffect 规约（余效应声明） | `inject`（string 数组或 `{name: config}`） | `Inject`（`required` / `optional` / builder `req`/`opt`） |
| reactive coeffect resolution（响应式余效应解析，空间性可组合） | fiber epoch：依赖 fiber uid 串接的签名，变更驱动 `_reload`/`_unload` | `epoch: Option<Vec<u64>>`（必选依赖的提供者 fiber id 序列）；`None` = 依赖缺失；变更经 `refresh() → drive_ops()` 同步驱动 Reload/Unload |
| component（组件 = effect + coeffect 的结合） | `Fiber`（组件实例，六态） | `Fiber`（四态：`Pending / Active / Failed / Disposed`） |
| 组件安装 / 移除 | `ctx.component()` 返回 thenable；`dispose()` | `ctx.component()/component_dyn()` 返回 `FiberHandle`；`handle.dispose()`（幂等） |
| 声明式组件加载器 | `@cordisjs/loader`（配置调和、HMR） | 里程碑 2：`yacordis-loader`（cdylib + libloading，见 §4） |

同步世界里 `LOADING`/`UNLOADING` 即时完成，状态收敛为四态；
`internal/status` 事件携带 `(新状态摘要, 旧状态)`，与 v4 一致。

## 2. 核心机制

### 2.1 Effect 认领（嵌套作用域）

与 v4 `fiber.effect()` 的 collect 语义对齐：

- setup 执行期间，同 fiber 上嵌套注册的一切（`on`/`once`/`provide`/`effect`）
  被当前最内层 effect batch 认领（线程局部作用域栈按 fiber id 匹配路由）。
- 因此 dispose 外层 effect 会连带回滚内层 effect。
- 组件体本身就是一次 batch 收集（v4 的 `_execute(this._runner)`）。
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
  （监听器、disposer、组件体）都在不持有任何锁时调用（快照后调用），
  这是支撑 FFI 回调重入的硬约束。

### 2.3 失败语义（有意偏离 v4）

- 组件体出错（含 panic，`catch_unwind` 转换）：**立即回滚**该次加载
  已收集的全部 effect，fiber 进入 `Failed`，错误保留至 `update()`；
  「Failed 即无副作用」兑现论文的时间性可组合承诺。
- `handle.update(config)`：经 `internal/update` waterfall（中间件可
  改写配置）→ 清除错误 → 强制重启（epoch 先置 `None` 再重算，保证
  依赖未变时也重跑组件体）→ 若仍失败则返回错误。

### 2.4 事件系统

同步世界保留三种分发模式（v4 五模式的同步子集）：

| v4 | yacordis | 说明 |
|---|---|---|
| `emit` | `emit` | 注册序全部调用，忽略返回值 |
| `bail` / `serial` | `bail` | 注册序，首个非 `None` 返回值短路（异步聚合待异步里程碑） |
| `parallel` | —（暂缺） | 需要 async 聚合，见路线图 |
| `waterfall` | `middleware` + `waterfall` | v4 用同一 hook 表加 `next` 参数约定；本实现 `middleware`/`waterfall` 各用独立注册表、静态签名显式。panic 的中间件使链**中止**并返回错误（值通道只承载完成的值）；`emit`/`bail` 的监听器 panic 则吞掉转日志（§5.2） |

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
  `global` 选项待事件分发的按组过滤能力落地。

### 2.5 服务双通道

- 类型化：`provide::<T>` / `get::<T>`（`Arc<dyn Any>` downcast）。
- 裸通道：`provide_raw` / `get_raw`（`Arc<dyn Any + Send + Sync>`）。
  这是里程碑 2 跨 C ABI 边界的传输形态。
- `set` 只允许提供者 fiber 自身调用（v4 的 multiple-fibers 检查）。

### 2.6 框架中介并发（async as plumbing）

异步里程碑的落地形态（§4.1 I1 的实现）：**生命周期保持同步串行**（
结构性免疫的来源，见 §7），并发作为框架管道提供；与 v4 的分歧
记录于 §3。

- **`ctx.run_blocking(label, work, on_done)`**：`work` 在专用框架线程
  上运行，仅持协作式取消 token；完成后框架在工作线程上发起
  `on_done(&Context, outcome)`
  ——这是框架发起的调用，可用全套 context API（与 FFI 回调同一重入
  契约）。fiber 已亡则投递丢弃。
- **`ctx.spawn_future(label, fut, on_done)`**：future 在框架的
  per-`Core` 执行器线程上轮询（惰性启动，存活至进程退出——keep-mapped
  同款诚实）。中止在下一个 await 点生效；不 yield 的 poll 无法被中断。
- **排空先于回滚**：每个工作单元双重登记（fiber 的 effect = 作用域
  生命周期；fiber 的 works 列表 = 排空清单）。`unload` 与失败体回滚
  先 cancel + join 全部在途单元，再逆序 disposal——回滚期间不存在
  并发组件代码（`drain_fiber_works`）。join 对「正在执行本单元完成
  回调的线程」自跳（thread-local 守卫），回调里 dispose 自身 fiber
  不会自死锁。
- **执行器中断语义**：任务停泊在 in-flight 注册表中，中止请求对
  Pending 任务保持可见（测试锁定于 `tests/work.rs`）。
- `parallel`/`serial` 分发模式需要 async 监听器聚合，待需求出现。

### 2.7 服务拦截（intercept）

上游 PR #90/#206 的 intercept 语义研读结论：intercept 是**沿上下文树
的服务配置分层覆盖**——每层 ctx 可为某服务注入一段配置覆盖，解析时
沿原型链（我们这里是 fiber 链）收集各层值，浅合并（`Object.assign`
语义），**最近祖先优先**，服务自身 inject 的元数据为基底。

yacordis 的首期落地（`ctx.intercept(name, value)`，JSON 服务）：

- **可见性向下**：fiber 上登记的层，自己与后代可见；兄弟与祖先
  不可见（与上游原型链语义一致）。解析沿请求 ctx 的 fiber 祖先链
  向上收集，折叠方向为从最远到最近（最近者胜）。
- **浅合并**：对象层逐键覆盖基底；非对象层整体替换。typed 服务
  不参与（返回原值）。
- **登记即 effect**：层随登记 fiber 卸载而移除——I2 的可证明清理
  自动覆盖 intercept。
- **纯视图层**：不影响 epoch、不触发 notify、不参与依赖解析——
  提供者与依赖方对此无感知，provider 不因 intercept 重载。
- 与上游的偏差：v4 的 Service 类携带 metadata 并自定义
  `mergeIntercept`；本实现 typed 服务为无元数据的裸值，拦截合并由
  JSON 通道承载。`resolveConfig`
  的职责由 schema 默认值（§6.4）承担；`check` 谓词已落地为
  `ctx.provide_schema`（§2.8）。

### 2.8 服务值校验（check 谓词的 Rust 形态）

`ctx.provide_schema(name, schema)`：为某 JSON 服务（按
`(name, isolate label)` 键控）登记一个值 schema——此后该服务的每次
provide/set 都先过校验，违规即整次调用失败并保留原值。对应上游
`provide(name, service, check)` 的 check：以「声明式 schema 门控」
承载，复用配置校验的 DSL；typed 服务
旁路（与 intercept 同一取舍）。登记是 effect——随 fiber 卸载解除，
所有权以 schema 节点指针判等（防重注册竞争下的误删）。

## 3. 与 TS 版的有意分歧（截至 M1）

1. **纯同步**：生命周期签名保持同步（模型选择）；并发需求由框架管道
   承担（§2.6），`serial` 语义并入 `bail`；v4 的 fiber
   inertia 对应本实现的 `quiesce`/排空语义。
2. **显式服务访问**：一律 `ctx.get("name")`（对应 v4 的
   traceable/shadow、mixin/accessor、callable service、`ctx.xxx`
   动态属性等访问形态）。
3. **服务可见性**（M1 时点：全局可见，按名字）：isolate 已于 M3
   落地（§5.3）；服务 intercept 已落地（§2.7，
   fiber 链分层覆盖）；`check` 谓词已落地（§2.8，声明式 schema 门控）；
   配置拦截合并（`Config.merge`）、typed 服务自定义 `mergeIntercept`
   待后续里程碑。
4. **组件身份** = `Arc` 数据指针（v4 按解析出的函数身份）。同一组件
   值的多次安装共享 runtime 条目；重新包装 `Arc` 视为新组件。
5. **Failed 即回滚**（见 2.3）。
6. 错误链由 logger 承载（对应 v4 的 `composeError` 长栈拼接）。
7. `Service` 基类模式 → 惯用法：组件体内 `ctx.provide(...)`；`#[component]`
   属性宏已落地（`yacordis-macros`，§6 第 7 条），Service 模式宏待需求。
8. root fiber 即装即活。

## 4. 里程碑 2（已实现）：二进制组件分发（cdylib + C ABI）

硬性需求：组件以预编译 cdylib 分发，用户从互联网/文件安装，宿主
全程保持运行；「现场编写并构建」仅为开发者路径，产物仍走同
一加载管线。

宿主与组件之间以 C ABI 为边界（`yacordis-component-api`，`ABI_VERSION = 4`；
v2 增加 `run_blocking`/`work_cancelled`/`work_cancel`，v3 将导出符号
更名为 `cordis_component_*`，v4 随项目更名改为 `yacordis_component_*`），组件产物与
宿主的 rustc/依赖版本两相独立：

- **传输介质**：JSON 字节（`RawBuffer`，配置/事件/服务值/错误消息）、
  u64 句柄（宿主侧句柄表持有的 Context/Fiber/Effect）、
  `Callback`（函数指针 + user_data + drop_fn，组件代码跨界执行）。
- **缓冲区所有权（唯一规则）**：被调方释放输入缓冲区；调用方读取后
  释放输出缓冲区。所有缓冲区走进程共享的 `malloc`/`free`（同进程
  只有一个 libc 实例），任意一侧皆可释放，无需配对各侧分配器。
  **CRT 注意**：组件必须动态链接 CRT——Windows 静态 CRT（/MT）构建的
  组件自带独立堆，跨界 `free` 违反共享分配器假设（UB）。
- **组件导出**：`yacordis_abi_version()`（握手，不符拒绝加载）、
  `yacordis_component_manifest()`（名称 + required/optional 依赖）、
  `yacordis_component_apply()`（apply 回调）、`yacordis_component_free_buffer()`；
  由 `register_component!` 宏生成（edition 2024 的 unsafe attribute 语法）。
- **宿主 vtable**（单个静态实例）：effect/on/once/dispose_handle/
  provide/set/get/has/emit/bail/component/fiber_dispose/log/free_buffer/
  run_blocking/work_cancelled/work_cancel。工作回调的 `ctx` 参数承载
  取消 token 句柄（经 `work_cancelled` 轮询），非 context 句柄；
  完成回调以 JSON 信封
  （`{"tag": "done|cancelled|failed|panicked", ...}`）交付结果。
  蹦床 `catch_unwind` 保证组件 panic → fiber `Failed`。
- **回调状态机**：一次性回调（effect setup/disposer）被二次调用返回
  `CODE_REUSED`；状态盒由 `drop_fn` 释放，与调用解耦。
- **卸载策略**：保留代码映射——状态效应完全回滚，机器码页留驻至进程
  退出。
- **加载源**：`Loader::load_file`（本地文件）、`Loader::fetch`
  （URL 下载，`download` feature，ureq）、`Loader::build_and_load`
  （调用 cargo 构建源码后加载，`--message-format=json` 解析 cdylib
  产物并合并编译诊断）。
- **组件作者视角**：依赖 `yacordis-component-api`，写普通 `impl Component` +
  `register_component!(...)`，crate-type = `["cdylib"]`；API 形状与 native
  `yacordis::Component` 一致。演示 fixture 与集成测试见
  `crates/yacordis-loader/tests/`（跨 edition：组件侧 2021，宿主 2024）。

core 为此提供的保证：统一 `Arc<dyn Component>` 形态（`component_dyn`）、
Value 优先的动态载体、无锁重入（§2.2，FFI 回调内再 install/provide/on
均安全，集成测试覆盖）。

**边界限制（有意为之）**：跨 ABI 的服务值是 `serde_json::Value`
（typed downcast 仅 native 侧可用）；组件侧监听器可经返回的
`ListenerHandle` 手动移除（随 fiber 卸载自动清理仍是主路径）。

### 4.1 卸载限制契约（组件规范不变量）

「干净卸载」分三层：①**语义干净**（经框架注册的一切可证明回收）——
由 effect 收集结构保证，无需组件作者配合；②**代码干净**（代码页
全程有效）——由 keep-mapped 策略承载（见上）；③
**进程干净**（长期运行无累积残留）——依赖组件作者遵守以下三条不变量：

- **I1（单调度域）**：组件代码只在框架发起的调用中执行（vtable 调用，
  或登记为 effect 的工作单元）。推论：卸载前框架可排空全部在途工作
  （落幕 → 排空 → 回滚三阶段），回滚期间不存在并发组件代码。线程与
  async 可以存在，但只能作为宿主管道（框架并发 API，或登记为 effect、
  排空阶段关闭的内部 runtime），不得建立框架看不见的第二调度域。
  已落地为 §2.6 的 `run_blocking`/`spawn_future` + `drain_fiber_works`。
- **I2（资源所有权）**：组件获取的一切资源，要么经 `ctx` 注册（框架
  持有，记入 handle 表），要么永不逃出组件体作用域。推论：dispose 时
  handle 表为空 ⇒ 语义干净。宿主半边机械可查（dispose 审计：表空
  断言、buffer 计数配平）；组件半边靠能力式 API + lint 封堵（裸
  `thread::spawn`、未登记 runtime、静态 CRT、可变 `static`）。
- **I3（无跨加载状态）**：组件不得持有跨加载存活的可变状态；行为是
  (config, 服务视图, 收到的事件) 的纯函数。推论：重载 = 全新实例，
  热更新语义正确。

强制执行矩阵：I1 由 ABI 结构（入口仅 vtable）+ lint 保证；I2 宿主半边
由 dispose 审计做运行时断言、组件半边由 lint profile 强制（
`build_and_load` 路径宿主持有源码可机械执行；预编译二进制分发退化为
信任发布者）；I3 由 lint；keep-mapped 兜底保证违规至多退化为内存泄漏。
子进程是 CPU 并行与故障隔离的合规逃生舱——进程死亡是唯一完美的卸载。

## 5. 设计评审记录（2026-09-06，里程碑 3 后）

对已实现部分的一次全面自查：每个决策复核理由，缺陷当场修复，债务显式记录。

### 5.1 评审中发现并已修复的缺陷

1. **fiber 调度丢唤醒（并发正确性）**：原实现 `busy: AtomicBool` 与
   `pending: Mutex<Vec<Op>>` 分离，CAS 失败入队与持锁者排空之间存在
   窗口——racer 在持锁者最后一次排空之后、busy 释放之前入队的 op 会
   悬挂。修复：busy 与队列收进单个 `Mutex<ScheduleState>`，push 与
   busy 检查共享一把锁，用户代码仍在锁外执行。
2. **FFI 回调状态泄漏 + 双重释放隐患**：`SendCallback` 无 `Drop`，
   成功注册路径的组件闭包状态（`State<F>` 盒）在 fiber 卸载时永不
   释放；手写的 `Clone` 共享 `user_data`，一旦被使用即双重释放。
   修复：实现 `Drop`（释放后置空、恰好一次）、删除 `Clone`；vtable
   错误路径的显式释放一并移除（与 Drop 并存即双重释放）。
3. **`Component::inject` 重复查询**：每次 `check_impls`/`refresh` 都重新
   调用 trait 并分配。修复：install 时缓存进 `Fiber.inject`。

### 5.2 复核后维持的决策（含理由）

- **同步优先**：M1 阶段正确的复杂度取舍；`Component::
  apply`/`Disposable` 为同步签名（单向门），`sleep` 阻塞调用线程，
  组件体内不得长阻塞（timer 文档已写明）。
- **effect 认领模型**（嵌套作用域 + 卸载逆序展平）：与 v4 逐点对齐，
  测试覆盖嵌套与手动中途释放。
- **epoch = `Option<Vec<u64>>`**：与 v4 字符串签名等价且更廉价；
  `u64::MAX` 兜底（依赖实现者已死）实践中不可达，仅防御性保留。
- **Failed 即立即回滚**：兑现时间性可组合承诺。
- **C ABI 边界**：组件产物与宿主的 rustc/依赖版本两相独立；
  `malloc/free` 共享分配器统一配对。限制：Windows
  静态 CRT（/MT）构建的组件自带独立堆，已注明于 §4。
- **卸载保留代码映射**：机器码页留驻换取卸载安全，
  语义上状态层面完全可逆。
- **waterfall 与普通 hook 分表**：`middleware` 与 `waterfall` 注册表
  独立、静态签名显式；已文档化。
- **监听器 panic 吞掉转日志**：宿主健壮性优先，
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
| 核心无内省 API | ✅ `installed_components()` / `provided_services()` |
| FFI 句柄表只增不减 | ✅ `get_context` 顺手 GC（fiber 已销毁的条目即取即删），FfiComponent 缓存条目先验证后复用 |
| 并发调度无 stress 测试 | ✅ 8 线程 × 40 轮 install/dispose 环 + 看门狗 |

**stress 测试揪出的真 bug 与语义决策**（本轮最有价值的产出）：

1. **stale op 重放**：级联中排队的 Reload/Unload op 与真实 epoch 可能
   脱节，多线程交错下会把已停用的 fiber 拉回 Active。修复：op 携带
   epoch 快照，执行时不匹配即丢弃（`run_op` stale-guard）。
2. **同步边界**：`install`/`dispose`/`restart` 的级联在无竞争时同步
   完成；有并发交错时级联可能「稍后」落地。为提供与 v4 `inertia`
   对应的同步语义，新增 `Core::quiesce()`（等待全部 op 执行完毕）与
   `FiberHandle::wait_for(state, timeout)`，并在 install/dispose/restart
   内部调用——【重入场景（组件 body 内再 install/dispose）自动跳过】，
   否则 quiesce 会等待自身正在执行的外层 op，形成自死锁。
3. **重入 quiesce 自死锁**（上面第 2 条的防御本身曾是死锁源）：以
   thread-local `EXECUTING_OPS` 检测「本线程正处于某个 op 内」来跳过。

**仍然接受的债**（更新后余量）：

1. 并发交错下 `install`/`dispose` 返回时，【其他线程】触发的级联可能
   尚未落地——强断言需配 `wait_for`/`quiesce`。彻底消除需把生命周期
   改造成单线程 actor 执行器，属异步里程碑的架构决策。
2. typed 事件每监听器重复反序列化（不变，量级小）。
3. ~~组件侧监听器无手动移除句柄~~（✅ 已解决：`ctx.on`/`ctx.once`
   返回 `ListenerHandle`，可 `dispose()` 手动移除；§4 边界限制同步更新）。
4. `Fiber.parent`/`Fiber.isolate` 字段暂只写不读（为 scope 内省预留）。

## 6. 路线图

**状态（2026-11-10）：1–8 全部落地**。余量：FFI 侧 `spawn_future`
（future 构造跨界是独立设计问题）、typed 服务自定义 `mergeIntercept`
（与 §2.7/§2.8 同族）、非对称签名（需加密依赖）、Service 模式宏、
ABI 遥测（buffer 配平计数）。

1. ~~M2：`yacordis-component-api` + `yacordis-loader`~~（✅ 见 §4）。
2. ~~异步生命周期~~ → 重定义为**框架中介并发**并已落地（§2.6：
   `run_blocking` / `spawn_future` + 排空先于回滚；FFI 侧经 ABI v2
   暴露 `run_blocking`，组件 ABI 现为 v3）。
   `parallel`/`serial` 分发待 async 监听器需求；
   FFI 侧 `spawn_future` 待设计（future 构造跨界是独立问题）。
3. ~~`yacordis-timer` / `yacordis-logger`~~（✅ M3：同步线程定时轮 + 分级
   日志服务；`sleep` 以 effect 挂靠 fiber，卸载即唤醒，详见各自 crate
   文档）。
4. ~~配置校验~~（✅ `yacordis::schema`：可组合 DSL——类型/长度/范围/
   枚举/数组/对象字段校验，`opt_default` 即 resolveConfig 等价的默认值
   解析；经 `Component::schema` 挂接，install 前拒绝 → `Failed` 零副作用，
   update 拒绝 → 保留旧配置，通过的配置以解析后形态持久化。FFI 组件
   暂在 body 内自行校验）。isolate 已落地（§5.3），依赖环诊断已落地
   （§7）；**服务 intercept 首期已落地**（§2.7：fiber 链分层覆盖、
   nearest-wins 浅合并、JSON 服务，effect 登记随卸载回滚）；**`check`
   谓词已落地**（§2.8：`ctx.provide_schema` 服务值门控）；typed 服务
   自定义 `mergeIntercept` 待需求。
5. 原位组件替换（对齐上游 `registry.replace()`，#266）：✅ native 侧
   已落地（`Context::replace_component` + `internal/replace`，见 §7.1）；
   FFI 侧对应 `ComponentLibrary` 级替换（keep-mapped 下旧库留驻、新库接管
   fibers）待做。
6. ~~组件仓库索引~~（✅ `yacordis-loader::registry`：静态 index.json +
   最小 semver（`^`/`~`/比较/精确；预发布待支持）+ 依赖无关的 sha-256
   （FIPS 180-4，NIST 向量锁定）内容寻址校验 + ABI 门；本地路径直取、
   否则走 `fetch` 下载，按内容哈希入缓存。非对称签名待加密依赖，
   `Verifier` 是插点）。
7. ~~`yacordis-macros`~~（✅ `#[component]` 属性宏落地：普通函数 + name/
   inject(…)/provides/schema 声明 → 生成 Component 适配器，声明进入响应
   式门控与依赖环诊断，`schema = <expr>` 经 `Component::schema` 在
   install/update 时校验并解析配置；async fn 显式拒绝，指回 §2.6）。
   Service 模式宏待需求。
8. ~~§4.1 契约的机械化~~（✅ ast-grep lint profile 五条规则随 crate
   分发——裸 spawn / tokio 调度 / 可变 static / exit 钩子 / FFI 手工
   分配；`BuildOptions.lint` 门控构建，`lint_source` 可独立调用，
   ast-grep 未安装时尽力跳过。dispose 审计收集绑死于已亡 fiber 的
   context 句柄——I2 宿主半边，测试锁定于 `tests/contract.rs`；
   buffer 计数配平待 ABI 层遥测）。

## 7. TS 上游对照与跟进（2026-11-12，rc.9 → HEAD @ 261c5fda）

cordis TS（v4）自上次快照（2026-09-06，rc.9）推进至
261c5fda（2026-11-10，http/cli 包迁入 monorepo），共 85 个提交。
跟进策略：**盯 cordiverse/cordis 的 issue 列表**——那里的
问题就是这个范式在真实负载下暴露的裂缝预告片。逐项对照：

| 上游 issue | 主题 | yacordis 状态 |
|---|---|---|
| #34 | epoch ABA：async 加载期间 update 改配置，epoch 感知不到 | **双双闭合**：上游以 fiber revision 计数实现 stale-guard（#189）；我们的 op epoch 快照 + stale-guard（§5.3）机制对等，测试锁定于 `tests/stale_ops.rs` |
| #96 / #91 | 卸载中注册 effect/listener → 泄漏 | 上游仍 open；**已覆盖**：`unloading` 态注册一律 `InactiveContext`、effect 即时回滚，测试锁定于 `tests/unloading.rs` |
| #110 | provider 先于 consumer 卸载（`Promise.all` 竞态） | **双双闭合**：上游按服务绑定跟踪 dependents（#196）；我们的同步级联（consumer 在 provider 的 provide-disposer 内完成停用）顺序契约一致，测试锁定于 `tests/lifecycle.rs`、`tests/cascade.rs` |
| #66 | 循环组件依赖无检测 | 上游仍 open，**我们领先**：`Component::provides` 声明 + SCC 检测 + 日志/内省报告（`src/diagnose.rs`）；body-only provide 不可检测，与上游同为开放边界 |
| #43 | waterfall 结构化取消语义 | 上游仍 open；**部分对齐**：全链 `Result` 化、panic 转 Err（§2.4）；完整的取消语义上游也未定论 |
| #90 | intercept 重设计 | **上游已收敛**（PR #90 重写 resolveConfig；#206 定 metadata 合并次序），§6.4 设计启动前提满足 |
| #102 | Standard Schema 诊断化 | 上游仍 open，继续观察 |

### 7.1 rc.9 → HEAD 语义对齐清单

- **#242（采纳对齐）**：dispatch 中实时尊重监听器移除——被移除的监听器
  在同一轮 dispatch 中不再被调用。快照迭代语义已按此修正（`src/events.rs`），
  回归测试锁定。
- **#56（补回归测试）**：`internal/update` hooks 随 fiber 释放——我们经
  effect 收集天然覆盖，测试补齐。
- **#296（采纳对齐）**：`DisposableList.unshift()` 让 fiber-local
  `internal/update` 监听器的 `{ prepend: true }` 生效。我们的
  waterfall 中间件注册表本只有 append 语义，现补 prepend
  （`Context::middleware_prepend`）：prepend 项整体先于 append 项、
  彼此按注册逆序，移除单个 prepend 项不动链条其余部分——语义与上游
  测试逐点对齐，回归测试锁定于 `tests/update_hooks.rs`。同 PR 的另
  两项：`clear()` 返回顺序属上游内部重构（两侧 dispose 顺序契约一致）；
  「disposed fiber 不被后续通知复活」已由 `refresh`/`reload` 的
  `disposed` 守卫覆盖（§5.3 stale-guard 体系）。#297（side-effect
  标记）为 TS 打包事务，Rust 侧无对应物。
- **#277（早已对齐）**：notify 按 `(name, isolate label)` 定向——
  `reflect.rs::notify` 一直如此，无需改动。
- **#111 HMR 三阶段排空（范式背书）**：上游 HMR 采用「落幕 → 排空 →
  调和」的 per-fiber drain——与 §4.1 I1 的卸载协议同构。模块图与缓存
  失效（#269/#298）为 require 机制专属，cdylib 结构性不适用。
- **#266 `registry.replace()`（native 侧已落地）**：原位组件替换——fiber
  保身份（id/parent/config）、换 component+inject+provides、重跑体；依赖方
  经常规 provide/withdraw 级联感知服务变更，Failed fiber 可被替换恢复，
  新增 `internal/replace` 事件（`Context::replace_component`）。FFI 侧对应
  `ComponentLibrary` 级替换（keep-mapped 下旧库留驻、新库接管 fibers）
  仍在路线图（§6.5）。
- **#109/#133/#189 等并发修复**：同步核中同类窗口结构性不存在或已有
  对应机制（§5.3），逐一核对过，无需改动。
- **#307（采纳改名，2026-11-12 落地）**：上游将范式主术语 plugin 改为
  component——`ctx.plugin()` → `ctx.use()`（保留弃用别名）、loader 导出
  契约 `exports.plugin` → `exports.component`（无别名），零语义变化。
  本项目全库跟随（分支 feature/component）。Rust 侧命名决策：方法取
  名词 `ctx.component`（`use` 是 Rust 关键字，弃 TS 动词取名词）；crate
  随名 `yacordis-component-api`；导出符号 `cordis_component_*` 且
  `ABI_VERSION` 升 3；仓库索引键 `"plugins"` → `"components"`；项目未
  发布，不留弃用别名。事件名 `internal/plugin` 上游改名后仍保留，两侧
  一致不动。另三项同日提交均 N/A：#303（package-manager 的 pnpm/yarn
  isolated layouts）、#308（http/cli 迁入 monorepo）、#311（发布 CI）——
  Rust 侧无对应物。

同步模型的两个定位差异点（模型选择带来的结构性红利）：其一，
epoch ABA、provider/consumer 卸载顺序等竞态窗口在同步核中
结构性不存在（§7 #34/#110）；其二，
二进制组件分发（cdylib + C ABI）是约 1,400 行的一等能力（§4）。

code size 对比（2026-09-06 时点，仅产品代码）：核心 TS 1,592 行 vs
Rust 2,059 行（+29%，多出的是 TS 靠事件循环与 Proxy 免费获得的部分：
调度队列、stale-op 防护、quiesce、显式服务解析）；reflect 层反而更短
（-40%）；FFI 三件套（component-api + loader）约 1,400 行为纯增量。
