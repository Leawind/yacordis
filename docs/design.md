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
| `waterfall` | `middleware` + `waterfall` | v4 用同一 hook 表加 `next` 参数约定；Rust 静态签名不同，故注册表分开。panic 的中间件视为弃权，链在其余中间件上继续 |

- 监听器统一签名 `Fn(&Context, &Value) -> Option<Value>`，字符串名
  注册；`Event` trait（`NAME` + `Args`/`Ret`）提供 serde 类型化封装。
- 约定：多参数事件用 `Value::Array` 承载（与 `internal/*` 一致）。
- `internal/*` 事件：`internal/plugin`（安装/卸载公告）、
  `internal/status`（状态迁移，携带旧状态）、`internal/service`
  （服务出现/撤回；值仅当服务本身是 `serde_json::Value` 时透出）、
  `internal/dispatch`（非 internal 事件的观测钩子）、
  `internal/update`（配置更新 waterfall）。
- v4 的 thisArg 过滤（`Context.filter`）依赖 isolate；M1 无 isolate，
  故无分发过滤，`global` 选项一并延后。

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
3. **无 isolate / intercept**：服务全局可见（按名字）；配置拦截合并、
   `check` 谓词、`Config.merge` 等留待后续里程碑。
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

## 5. 路线图

1. ~~M2：`cordis-plugin-api` + `cordis-loader`~~（✅ 见 §4）。
2. 异步生命周期：async apply/dispose、fiber inertia 互斥、
   `parallel`/`serial` 分发模式。
3. ~~`cordis-timer` / `cordis-logger`~~（✅ M3：同步线程定时轮 + 分级
   日志服务；`sleep` 以 effect 挂靠 fiber，卸载即唤醒，详见各自 crate
   文档）.
4. 配置校验（Standard Schema 等价物）、isolate/intercept。
5. 插件仓库索引（版本解析、依赖声明、签名校验，叠加在 `fetch` 之上）。
6. `cordis-macros`（`#[plugin]`/`#[inject]` 派生宏、Service 模式宏）。
