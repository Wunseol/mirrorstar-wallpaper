# 壁纸轮换调度器设计（Rotation Scheduler）

> 状态：设计定稿（2026-09-08，经多轮深度讨论与边界审计）
> 定位：`docs/02-架构设计` 新增设计，作为壁纸轮换功能（随机壁纸 / 定时更换 / 开机更换 / 播放顺序）的唯一权威文档。

---

## 1. 背景与动机

### 1.1 现状缺口（已确认）

- 应用**开机后不会自动恢复或更换任何壁纸**，WorkerW 壁纸画面不跨重启保留 → 空屏缺口。
- 现有换壁纸**唯一入口**是 `set_wallpaper`（三阶段：关旧 → 锁外创建渲染器 → 锁内嵌入 WorkerW 注册），一切切换本质是它的重复调用。
- 现有配置层（`AppConfig`）无任何轮换概念；`Arrangement` 仅 `{PerMonitor, Span}`。
- 无任何调度 / 定时 / 随机 / 顺序机制。

### 1.2 目标

构建一套**可组合、可暂停、可持久、可扩展**的壁纸轮换体系，覆盖：随机壁纸、定时更换、开机更换、播放顺序控制、多屏协同，并修复开机空屏缺口。

### 1.3 非目标（本期明确不做）

本期不实现作息/锁屏/电源切换池触发、加权随机、轮换倒计时 UI、类型感知间隔建议等进阶能力；**完整清单见 §19（本期不做·二期）**。

---

## 2. 设计定位

**调度器 = 换壁纸动作的唯一决策者。**

- 决策三件事：**WHEN**（何时触发）、**WHICH**（从哪个候选池取）、**HOW**（用什么采样算法）→ 解析出目标壁纸 id。
- **渲染执行完全下放**给现有 `set_wallpaper` 三阶段流程，不进入渲染层。
- 负责轮换播放状态（游标 / 当前壁纸 / 洗牌袋）的持久化与恢复。

一句话：**调度器决策，渲染层执行，配置层落盘。**

### 2.1 复用清单（不侵入）

| 现有组件 | 复用方式 |
|---|---|
| `set_wallpaper` 三阶段流程 | apply 的唯一执行入口 |
| `PauseReason` 位图（FULLSCREEN/BATTERY/TRAY） | 轮换暂停的抑制信号源 |
| `pause_all_fast` / `resume_all_fast` | 轮换完成后跟随暂停 |
| `workerw_check` 桌面就绪兜底 | 启动阶段等待 WorkerW |
| `DISPLAYS_SETTING` per-display 防并发集合 | 串行化 guard 的接入点 |
| `ConfigManager.update_config` + 防抖 + 热重载 | 轮换配置读取 / 热重载响应 / 写盘模式 |
| `Arrangement` 枚举 | 重构为三值编排（§4.4） |
| `VolumeControl`（共享 COM 缓存） | 新壁纸音量自动继承 |
| `global_state_changed` broadcast 通道 | 轮换事件转发 |

---

## 3. 核心概念

| 概念 | 定义 |
|---|---|
| **调度单元（Rotation Unit）** | 轮换与状态的最小载体。由编排决定：`PerMonitor` → 每屏一个；`AllSame`/`Span` → 全体一个。 |
| **候选池（Pool）** | 一个**有序 id 列表**（自定义顺序的载体）。一张壁纸可属 0~N 池。每单元绑定一个激活池。 |
| **采样算法（Order）** | 池内取下一张的方式：顺序循环 / 洗牌袋 / 纯随机。 |
| **触发源（Trigger）** | 唤醒调度器的一类时机，可组合共存。 |
| **编排（Arrangement）** | 定义调度单元与物理屏的映射：`PerMonitor` / `AllSame` / `Span`。 |

---

## 4. 数据模型

### 4.1 config.toml — 轮换配置

遵循现有 `validate()` clamp 模式，非法值回退默认。

```toml
[rotation]
enabled = false          # 全局主开关；false 时仍执行"开机恢复当前壁纸"（见 §9）
on_boot = false          # 开机/唤醒是否"推进下一张"（而非仅恢复）
interval_minutes = 30    # 定时间隔（分钟）；下限 clamp 1 分钟（60s，防短间隔反复 spawn/kill 视频进程），见 DR-16
order = "shuffle_bag"    # sequential | shuffle_bag | pseudo_random
arrangement = "per_monitor"  # per_monitor | all_same | span（重构后的编排）
```

```rust
struct RotationConfig {
    enabled: bool,
    on_boot: bool,
    interval_minutes: u32,
    order: Order,            // Sequential | ShuffleBag | PseudoRandom
    arrangement: Arrangement, // PerMonitor | AllSame | Span
}
```

### 4.2 wallpapers.toml — 壁纸库扩展

```rust
struct Pool { id: String, name: String, member_ids: Vec<String> }  // 有序，成员去重（DR-24）
// WallpaperEntry 增加 groups: Vec<String>（所属池 id）
```

- 池即有序列表：**顺序算法追随用户自定义顺序**（拖拽排序），洗牌袋/纯随机忽略顺序（DR-3）。
- 池成员重复 id 在写入时去重。
- **隐式"全部"池（DR-35）**：`active_pool = None` 回退的"全部"池是一个隐式全集 = 库中所有壁纸（类型过滤后），**不随池成员变化，永远存在**。它不同于显式池：成员始终是全集，不存在"全部池被编辑"。

### 4.3 playback.toml — 播放状态（独立于配置）

**非配置、不参与 config 热重载**，与 config 解耦避免互相污染（DR-15）。

```rust
struct PlaybackState {
    version: u32,                 // schema 版本，损坏/旧版本回退重建
    units: HashMap<String, Unit>, // key: display_id | "all"
}
struct Unit {
    key: String,
    current_wallpaper_id: Option<String>,
    active_pool: Option<String>,  // None = 回退"全部"池
    order_cursor: Option<String>, // id 锚定（DR-5，重排后从当前 id 继续）
    bag_remaining: Vec<String>,   // 洗牌袋剩余（已剔除 Web）
    enabled: bool,                // 该单元是否参与轮换
}
```

### 4.4 编排（Arrangement 重构）与调度单元

将现有 `Arrangement::{PerMonitor, Span}` 重构为三值枚举：

```
enum Arrangement { PerMonitor, AllSame, Span }
```

| 编排 | 调度单元 | 物理 apply 分发 | 资源成本 |
|---|---|---|---|
| `PerMonitor` | N 个（每屏独立池/游标） | 1 屏 = 1 次 `set_wallpaper` | 1 渲染器/屏 |
| `AllSame` | 1 个（全体同图） | 1 张壁纸 → 每屏各 set 一次，**顺序执行** | N 渲染器 |
| `Span` | 1 个（跨屏贯通） | 1 个跨屏渲染器 | 1 渲染器 |

**结论**：资源成本 =（解析 1 次）＋（物理 apply N 次，PerMonitor/AllSame 为 N、Span 为 1）。"开机一次设置"指**一次解析**，物理分发量由编排决定（DR-12）。

**"同步随机 / 跨屏去重"不再是独立需求**——AllSame/Span 作为单一调度单元，天然全体同图、天然一池一致（DR-2）。

**编排切换迁移（DR-22）**：
- `PerMonitor → AllSame/Span`：保留主屏单元，合并/丢弃其他屏状态到 `all`。
- `AllSame/Span → PerMonitor`：从 `all` 分发到各屏，游标/袋重建。
- 切换时游标/袋失效重建，当前壁纸尽量保留。

> 注：`AllSame` 换图为顺序逐屏 N 次原子交换，换图窗口期各屏存在瞬时 新/旧 混杂，属**已知接受项**（DR-39）。

### 4.5 类型参与规则（三层模型）

**Web 类型壁纸不参与轮换**（轮换成本过重：WebView2 冷启动最长 8s）（DR-18）。

```
轮换候选 = 池成员 ∩ 类型允许          ← 两层过滤
手动/恢复豁免 = 不受上述限制          ← 第三层
```

| 层 | 规则 |
|---|---|
| 类型层 | `WallpaperType` 定义 `rotation_default`：Image/GIF/Video=true，Web=false |
| 池层 | 池成员决定候选范围；UI 允许把 Web 加入池（便于手动管理），采样自动跳过 |
| 豁免层 | 手动设 Web = 临时覆盖；开机恢复 Web（手动设的延续）**必须允许** |

采样实现：顺序游标 / 洗牌袋**初始化时即剔除 Web**，不靠运行时撞上再跳（DR-18）。

---

## 5. 调度器生命周期

单个后台 tokio 任务，随托盘常驻。

```
初始化
  ├─ 读 RotationConfig、Arrangement
  ├─ 读 playback.toml，按 key 重建单元状态
  ├─ **启动对账（DR-34）**：current / active_pool / order_cursor / bag_remaining 中引用已删壁纸或已删池的条目 → 清理 / 回退"全部"；
  │    显示器 key 与当前枚举不匹配的孤儿单元 → 忽略/归档重建（Windows 显示器 id 在拔插、
  │    驱动更新后可能变化，playback 的 key 会失联；三文件非原子写，崩溃后可能不一致）
  └─ 等 WorkerW/桌面就绪（复用 workerw_check）→ 枚举显示器 → 建/对齐单元
```
```
主循环
  loop {
    if 全局 enabled 且存在单元 enabled 且其池过滤后 ≥2 张:
        计算下一 deadline（取各触发源最早）
    else:
        sleep 至被 config/事件唤醒（不空转）
    到点/被唤醒 → 对每个参与单元 execute_unit()
  }
execute_unit(unit):
  1. 采样：order 决定 target id（池空/缺文件/Web → 跳过，DR-18/DR-19）
  2. apply(unit, target) —— 见 §8，串行执行（DR-17）
  3. 更新 unit current / cursor / bag → 写 playback（防抖，DR-29）
  4. emit `wallpaper-rotated` 事件（DR-30）
  5. 若 apply 完成时处于暂停态 → 新壁纸跟随暂停（DR-20）
```

**健壮性**：
- 调度器任务 panic → 看门狗/日志，配置变更事件可自愈（DR-32）。
- config / playback 文件损坏 → 回退默认值继续运行，不 panic（DR-32）。

---

## 6. 触发系统（可组合）

触发源集合，调度器合并 deadline 并监听事件源；任意源可唤醒调度器（DR-7）。

| 触发源 | 类型 | 行为 |
|---|---|---|
| 定时间隔 | 时间 | `sleep_until(next_deadline)`，无漂移；暂停期间持帧，恢复后重计（DR-9） |
| 开机 | 事件 | 启动、桌面就绪、枚举显示器后，走 boot 解析（§9） |
| 唤醒 | 事件 | 睡眠/休眠恢复/显示器重插 → **取消未完成 sleep，统一重解析**（DR-13，杜绝双触发） |
| 手动下一张 | 事件 | 托盘 / UI 触发，作用于当前/主单元；不受暂停限制（DR-9） |
| 暂停位图变化 | 事件 | pause_reasons 清空（resume_all_fast 后）→ 唤醒调度器重算 deadline（DR-38） |

**双触发源处理（DR-13）**：唤醒事件到达时取消未完成 sleep，避免"sleep 过期 + 唤醒事件"两次换图。

**唤醒信号源（实现要点）**：唤醒/睡眠恢复/显示器重插由现有 Win32 回调（`WM_POWERBROADCAST` / `WM_DISPLAYCHANGE`）捕获，需经一条 watch/broadcast 通道转发给调度器主循环（与 config 变更通知共用同一事件通道，见 §13）。开机解析在 setup 内同步完成，不存在"事件先于调度器初始化"的丢失问题。

---

## 7. 采样算法（池内，一单元一实例）

| 算法 | 行为 | 备注 |
|---|---|---|
| 顺序循环 `sequential` | 从激活池有序列表取 `order_cursor` 指向的 id，用后推进 | **id 锚定**，池重排后从当前 id 继续（DR-5） |
| 洗牌袋 `shuffle_bag` | 池复制 → 打乱成袋 → 抽一张移出；袋空重洗 | 袋内不重复；初始化剔除 Web |
| 纯随机 `pseudo_random` | 每次独立随机抽取 | 可能连续抽中同一张（接受） |

边界：
- 池过滤（类型 + 有效文件）后 <2 张 → 单元空闲，不轮换（DR-14）。
- 洗牌袋抽到已删除/缺文件 → 袋内移除重抽（DR-11/DR-19）。
- 顺序游标指向已删 id → 前跳到下一个有效 id。

---

## 8. apply 执行与编排

### 8.1 串行化（DR-17）

**串行化 guard 放在 `set_wallpaper` 命令入口**（复用/扩展 `DISPLAYS_SETTING` per-display 防并发集合），不是调度器内部——**手动命令与调度器轮换两条路径统一排队**，杜绝并发换图（手动点"设为壁纸"同时定时到点）。

- 轮换 apply 进行中：手动命令排队；调度器侧新触发丢弃/重排。
- AllSame 多屏 apply：顺序逐屏执行，避免同屏竞态。

实现注意：guard 需跨 prepare→build（含锁外冷启动，最长几秒）持有以挡住同屏手动/自动并发，但排队应为**异步感知**设计，避免用同步锁阻塞 tokio worker 线程。

### 8.2 属性继承（审计确认，DR-25/26/27）

| 属性 | 继承方式 |
|---|---|
| 音量 | 新渲染器创建时传入共享 `VolumeControl` → 自动一致（DR-27） |
| 缩放模式 | **继承该单元/屏当前生效的缩放模式**（读 `wallpaper_scaling_modes` per-display 记忆），而非回 Fill（DR-25） |
| 播放速度 | **新渲染器不继承 speed**（speed 是 per-renderer 独立命令）→ apply 完成后若全局 `video.speed ≠ 1.0`，调度器/命令层补应用（DR-26，审计发现的缺口） |

### 8.3 失败路径（DR-19）

apply 失败（文件损坏/离线/进程 spawn 失败）：
- 保持旧壁纸不动；
- 游标**推进跳过**（不原地卡死重试）；
- 记录日志（tracing）。

### 8.4 切换中间态：原子交换（方案 C，DR-33）

**问题**（已核实代码）：三阶段流程是**阶段 1 锁内先 `close_wallpaper` 关旧 → 阶段 2 锁外建新（视频/Web 冷启动）→ 阶段 3 锁内嵌入**。因此每次换图在"旧已关、新未就绪"之间存在无壁纸窗口（Image 瞬时 / GIF 短 / Video 约 1s / Web 最长 8s，Web 已排除出轮换 DR-18）。

**约束（代码核查结论）**：
- `construct_renderer`（锁外，含 `play()`）与 `embed_wallpaper`（锁内）**本就是两阶段分离**——"构建"≠"嵌入显示"。
- `embed_wallpaper` 已证明安全：`SetParent → SetWindowPos(HWND_BOTTOM) → 无 WS_VISIBLE 创建 → ShowWindow`，**新窗口嵌入路径自身不闪窗**。
- 视频媒体必须嵌入且定尺寸后才能 `after_embed` 加载（DR E：修复 E_OUTOFMEMORY）。

**选型：原子交换（方案 C）**——**旧壁纸保持显示作占位，新壁纸就绪后才关旧**，彻底消除无壁纸窗口，且不改、不增渲染器类型。

| 方案 | 消除窗口 | 需预知下张 | 内存 | 改动 |
|---|---|---|---|---|
| A 双缓冲预加载 | 完全 | 是 | 高/全程双持 | 大 |
| B 重排三阶段 | 完全 | 否 | 低 | 中 + 闪窗风险 |
| **C 保持显示+原子交换** | **完全** | **否** | **低/短暂重叠** | **小** |
| D 过渡纯色占位 | 部分/仍闪 | 否 | 极低 | 小 |

C 优于 A（不需预知、不长期双持）、优于 B（无需重构 prepare/complete 契约、无"新空窗压旧上"闪窗风险）。

> 注：`Span` 编排的交换单元是**整个虚拟桌面**（多物理屏聚合为一个 display 单元），不逐单屏做原子交换；下述 display_id 级各步在 Span 下作用于虚拟桌面单元。

#### 运行时序

```
prepare_atomic_swap(display_id, source, type)          [引擎锁内, 廉价]
  ├─ 快照 renderer_config / clear_native
  └─ 记录当前 renderer id —— 旧壁纸不动（不 close/不 hide/不 pause、
     不离开槽位），继续播放盖住屏幕 → 返回 AtomicSwapPending

build_new(out-of-lock)                                  [旧壁纸持续可见]
  └─ construct_renderer（视频 spawn mpv + IPC 连接 ≤2s）
     失败 → 返回 Err（旧壁纸原封不动，零回滚成本）
     new.id == 旧.id → 短路跳过整个 swap

commit_atomic_swap 分三步                             [旧壁纸全程稳定在上层]
  步骤A [锁内]: embed_wallpaper(new, HWND_BOTTOM) → 新窗藏在旧之下
                + new.after_embed()（视频: IPC loadfile, fire-and-forget）
                → 旧仍在槽位, 新窗在下加载
  步骤B [锁外]: wait_new_ready(new)
                - 视频: 轮询 mpv 至 width>0 且 idle-active=no（首帧就绪,
                 复用 diagnostic_playback_status）
                - 图片/GIF: 立即就绪（无首帧延迟）
                - 失败/超时 → terminate new, 返回 Err（旧照常, 零成本）
  步骤C [锁内]: 一次换槽 old→new（wallpapers / pause_senders / mode / sources）
                + 按当前全局 PauseReason 对新补发暂停（DR-20, 重读当前态）
                + terminate 旧（失败则强毁旧 hwnd 兜底）
                → 新（已就绪渲染）瞬时浮现, 无黑屏
```

#### 关键设计点

- **旧壁纸无需暂停**：占位 = 旧壁纸保持原状继续显示/播放。构建窗口内我们不关它、不碰它，它天然盖住屏幕（视频继续播、图片静止）。**暂停反而让视频停在某帧、体验更差，因此不做**。
- **z 序决定性（DR-33）**：新窗以 `HWND_BOTTOM` 藏在旧壁纸之下，故新窗加载期间**旧画面稳定在上层可见**；步骤 C terminate 旧、揭示**已就绪**的新窗 → 切换是一帧瞬时跳变，无黑屏。
- **首帧就绪等待（DR-33，锁外）**：`after_embed` 为 fire-and-forget（video.rs 注释），不保证首帧已渲染。**必须在步骤 B 锁外轮询 mpv 至 `width>0 且 idle-active=no`（复用 `diagnostic_playback_status`）后才 terminate 旧**；图片/GIF 无首帧延迟、立即就绪。等待在锁外进行且旧壁纸在上层盖屏，故不占引擎锁。
- **引擎单槽 / 回滚 / 一致**：旧壁纸从头到尾**不脱离槽位、不被关闭**，只在步骤 C 锁定临界区内"旧→新一次性换槽 + terminate 旧"。故全程 map 一致、**无瞬时不一致**；build / wait_new_ready 任一失败 → 返回 Err，旧壁纸原封不动，**无需 resume**（失败零代价）。
- **Native 图片路径**：`ScalingMode` 部分 Image 走 `set_native_wallpaper_internal`（SystemParametersInfo，无 WorkerW/hwnd），无旧窗可压在下面 → **该路径回退现行为**（先关后设），Native 设置本身很快，可接受。

其余细节（同目标短路、config 新鲜度、terminate 旧兜底、并发纪律、新壁纸暂停跟随 DR-20、内存、事件）均并入上文「运行时序」注释，不再单独展开。

#### 双窗安全（DR-40）

- 唯一权威 = `active_wallpapers` map：Explorer 重嵌 / 移除均按 map 读写，不扫描 WorkerW 子窗口（已核实 `desktop/mod.rs`）。因此**已嵌入但未注册进 map 的 pending 新窗，对布局逻辑天然不可见**，不会误被重嵌 / 误清除。
- **不采用全局事件屏蔽**。
- DPI / 显示器变化由各壁纸窗按自己的 hwnd 自处理（`gdi_base.rs` 的 `WM_DPICHANGED`/`WM_DISPLAYCHANGE` 处理），pending 新窗同样能正确自定位。
- **桌面重建（Explorer 重启）**：重嵌入口先**中止该 display 的 in-flight swap**（terminate 新窗、清 pending 注册），再按 map 重嵌旧窗，不产生孤儿窗口。
- **commit 前校验**：pending 窗的父窗口仍为当前 WorkerW，否则放弃 commit（terminate 新窗），由正常重嵌接管旧窗。

---

## 9. 开机 / 唤醒解析

**开机只做一次解析、一次 apply 流程**（DR-12），避免"先恢复再推进"开两次渲染器。

```
resolve_boot(unit):
  target =
    if current 存在:
        if rotation.enabled and on_boot and unit.enabled and 池过滤后 ≥2 张:
            next(unit)                  # 推进下一张（on_boot 门控）
        else:
            unit.current_wallpaper_id   # 恢复上次（可含 Web，豁免）
    else:                               # current 已删 / 无上次壁纸
        if rotation.enabled and unit.enabled and 池过滤后 ≥1 张:
            next(unit)                  # 填充池内下一张，避免开机空屏（DR-21）
        else:
            None                        # 清理留空，等手动或下次定时
  推进/重置游标 → apply → 写 playback → emit
```

- **恢复当前 = 无条件基础行为**（不受 `rotation.enabled` 门控），修开机空屏（DR-2/DR-12）。
- 开机恢复的壁纸若是 Web（手动设的延续）→ **允许恢复**（豁免层，DR-18）。
- on_boot 推进时跳过 Web 取池内下一张。

> 语义确认：睡眠 / 休眠恢复（也走 `resolve_boot`）同样套 `on_boot` 门控——仅 `on_boot=true` 时恢复后推进一张；否则只恢复 current，不强制换图（DR-13）。

---

## 10. 暂停机制（两套，必须分清）

### Layer 1 · 渲染器播放暂停（已有）

`PauseReason` 位图（FULLSCREEN / BATTERY / TRAY）→ `pause_all_fast` 逐渲染器下发命令：

| 类型 | 暂停成本 | 全屏暂停 |
|---|---|---|
| Image | 状态翻转 ≈0 | 状态翻转 |
| GIF | 冻结帧 | 冻结（内存按策略释放） |
| Video | mpv 暂停 | **杀进程，恢复=冷启动** |
| Web | 暂停 | **杀进程，恢复=冷启动** |

### Layer 2 · 调度器轮换暂停（新增）

- `pause_reasons` 非空 → **抑制轮换触发**（换图动作暂停，不是壁纸播放暂停）。
- 位图清空 → **重算下一次 deadline（不补时）**（DR-9）。
- 手动下一张不受暂停限制，但 apply 完成后若处于暂停态 → **新壁纸跟随暂停**（DR-20）。

**联动**：调度器读取 `pause_reasons.is_empty()` 决定是否抑制轮换；apply 完成瞬间检测暂停态补发暂停。

---

## 11. 错误处理与边界用例（穷举审计）

### 启动
- S1 无 playback（首次安装）→ 无 current 可恢复；若轮换开且池内有有效壁纸 → 推进填充，否则空屏待手动（DR-21）。
- S2 开机恢复壁纸已删 → 见 §9：轮换开且池按有效壁纸 → 推进填充（避免空屏），否则清理留空（DR-21）。
- S3 WorkerW 未就绪 → 复用 `workerw_check` 重试等待。
- S4 显示器枚举变化（外接屏未接）→ 孤儿 key 忽略，按现有屏重建。
- S5 池空 / 仅 1 张 → 只恢复不轮换。

### 触发
- T1 定时间隔到点 + 暂停中 → 抑制，恢复后重计。
- T2 手动下一张 + 暂停中 → 执行，完成后跟随暂停。
- T3/T4 唤醒 on_boot=false / 池<2 → 只恢复 current。
- T5 唤醒时 current 是临时覆盖的池外壁纸 → 恢复它，下次轮换回池内。

### 采样
- C1 洗牌袋抽到已删/缺文件 → 袋内移除重抽。
- C2 顺序游标指向已删 id → 前跳下一个有效 id。
- C3 纯随机连续抽同张 → 接受（特性）。
- C4 袋空重洗。
- C5 运行中改池（增删/重排/换 active_pool）→ 该单元 bag/cursor **失效重建**，当前壁纸保留（DR-23）。

### apply
- A1 apply 失败 → 保持旧壁纸、游标推进跳过（见 §8.3 / DR-19）。
- A2 apply 进行中又来触发 → 串行化 guard（DR-17）。
- A3 AllSame 多屏顺序 apply 到一半某屏失败 → 记录失败屏，其余继续。
- A4 apply 中退出 → 现有 RAII/terminate 兜底。

### 暂停/恢复
- P1~P3 抑制 / 重计 / 手动不受限（已定）。
- P4 apply 完成时处于暂停态 → 新壁纸跟随暂停（DR-20）。

### 持久化 / 删除 / 编排
- PS1 playback 写失败 → 内存态继续 + 日志。
- PS2 写频防抖（DR-29）。
- PS3 编排切换迁移（DR-22）。
- PS4 active_pool 指向已删池 → 回退"全部"池。
- D1 删当前壁纸 + 轮换开 → 换下一张。
- D2 删当前壁纸 + 轮换关 → 移除并留空。
- D3 删壁纸 → 从所有池 / 游标 / 袋 / current 清理。

### 多屏
- M1 PerMonitor 某屏池<2 → 该屏空闲，其他屏继续。
- M2 AllSame/Span 池<2 → 全体空闲。
- M3 屏拔掉 → 对应单元跳过、保留游标。
- M4 屏插回 → 唤醒重解析。
- M5 AllSame 手动设置 = 设一张全体生效（一体单元语义）。

---

## 12. 并发与一致性

- **锁序**（遵循现有约定）：`AppState.wallpaper_engine` (tokio Mutex) → engine 内部锁 → `desktop` (std Mutex)。
- **串行化 guard**（§8.1）：手动与自动两条 set 路径统一排队；guard 跨 build 持有须异步感知，不阻塞 tokio worker。
- **调度器内部无跨单元锁竞争**：采样纯内存计算；apply 复用现有三阶段锁序。
- **单实例**：已有 `ensure_single_instance` 保护（CreateMutexW，main.rs），天然杜绝多实例双调度器竞争（DR-31）。
- **暂停状态变化唤醒（DR-38）**：暂停抑制轮换期间，`pause_reasons` 清空必须经 watch/broadcast 通道立刻唤醒调度器重算 deadline（不补时）。

---

## 13. 配置热重载响应

接入现有 `ConfigManager` 热重载（`update_config` + 300ms 防抖 + 文件 watcher + 周期性保存）：

| 配置变更 | 调度器响应 |
|---|---|
| `interval_minutes` | 重算下次 deadline |
| `order` | 重建该单元采样器（袋重洗 / 游标保留） |
| `arrangement` | 编排迁移（§4.4） |
| `enabled`（全局/单元） | 立即参与/退出轮换 |
| 池成员 / 重排 | 该单元 bag/cursor **整体失效重建**（DR-23，不做增量合并） |
| `on_boot` | 下次开机/唤醒生效 |

**通知机制（实现要点）**：调度器需订阅"配置已应用"通知。现有 `update_config`（命令路径）与文件 watcher 都会导致配置更新，需统一经一条 `tokio::sync::watch`/broadcast 通道通知调度器（与唤醒信号、暂停位图变化共用，见 §6），避免调度器轮询。`update_config` 与调度器 deadline 计算的并发通过 engine 锁/调度器内部互斥串行化。

---

## 14. 前端接口

### 命令（Tauri command，全部注册于 `generate_handler!`）

```
get_rotation_config / update_rotation_config
list_pools / create_pool / update_pool / delete_pool     # 池 CRUD（含成员有序）
set_active_pool(key, pool_id)                            # 单元绑定激活池
set_rotation_enabled(key, enabled)                       # 单元/全局（"all"）开关
next_wallpaper(key?)                                     # 手动下一张（当前/主单元）
```

**手动下一张的目标单元（DR-36）**：`next_wallpaper` 缺省作用于**主显示器单元**（`PerMonitor` 下主屏）或唯一单元（`AllSame`/`Span`），可显式传 key 指定其他屏。

> 注（DR-40/E）`set_active_pool` / `set_rotation_enabled` 在 `Arrangement≠PerMonitor`（即 AllSame/Span 单单元）时须校验目标 key 与当前调度单元一致，拒绝指向不存在单元的 key，避免孤儿单元。

**首次运行默认（DR-37）**：无 playback / 无显式池时 → `active_pool = None`（回退隐式"全部"池）+ `rotation.enabled = false` + `interval_minutes = 30` + `order = shuffle_bag` + `on_boot = false`。避免开箱即动。

### 事件

- 复用 `wallpaper-state-changed`（状态刷新）。
- 新增 `wallpaper-rotated { key, wallpaper_id }`（DR-30），供 UI 高亮当前壁纸。
- AllSame 多屏会 emit N 个状态事件，前端合并去重。

### UI 元素

- **设置面板**：轮换开关（全局）、间隔、算法、on_boot、编排模式。
- **池编辑器**：池 CRUD + 成员拖拽排序（自定义顺序载体）。
- **单元配置**：每屏/全体绑定激活池、单元轮换开关。
- **壁纸卡片**：显示所属池；Web 壁纸标注"不参与轮换"。
- **手动"下一张"**：UI 按钮 + 托盘菜单（DR-28，托盘现有 open/pause_resume/quit 三项，新增"下一张壁纸"）。

---

## 15. 性能与资源预算

- 采样纯内存，开销可忽略；重头在 `set_wallpaper`（渲染器创建）。
- **定时间隔下限 clamp = 1 分钟（60s）**（DR-16），防短间隔反复 spawn/kill 视频进程、与冷启动竞态。
- apply 串行执行（DR-17），杜绝并发换图。
- AllSame 顺序 apply N 屏；多视频同时冷启动的内存峰值需在文档测试阶段验证。
- playback 写入防抖（复用 `maybe_save_config` 模式）（DR-29）。
- 事件广播容量足够，前端合并去重。
- 轮换动作打 tracing 日志（换到哪张、为什么、失败原因）（DR-32）。

---

## 16. 启动时序

```
Tauri setup
  ├─ 调度器初始化（见 §5·初始化）：读 RotationConfig/playback → 启动对账（DR-34）→ 等 WorkerW → 枚举显示器建/对齐单元
  ├─ resolve_boot() → 一次 apply 流程（§9）
  └─ spawn 调度器主循环（§5），订阅 config / 唤醒 / 暂停变化事件通道（§13）
```

与现有 Explorer 重启监控 / `WM_DISPLAYCHANGE` / `WM_DPICHANGED` 处理协调：Explorer 重启后壁纸重嵌由现有逻辑负责，调度器状态不受影响。

---

## 17. 测试策略

| 测试点 | 覆盖 |
|---|---|
| 采样算法（纯函数） | 顺序游标 id 锚定、洗牌袋不重复/袋空重洗、纯随机、Web 剔除、<2 空闲 |
| boot 解析决策树 | on_boot × enabled × 池≥2 × current 已删 全组合；current 已删 + 轮换开 → 推进填充避免空屏（DR-21） |
| 暂停联动 | PauseReason 位图交互（FULLSCREEN/BATTERY/TRAY 叠加）、恢复后重计、新壁纸跟随暂停 |
| skip-broken | apply 失败保持旧壁纸 + 游标推进 |
| 池编辑一致性 | 增删/重排/换 active_pool 后 bag/cursor 失效重建 |
| 编排迁移 | PerMonitor↔AllSame/Span 状态合并/分发 |
| 删除一致性 | 从池/游标/袋/current 全清理 |
| 并发 | 手动 set 与轮换同时触发 → guard 串行化 |
| 配置热重载 | 改 interval/order/arrangement 实时生效 |
| 启动对账 | 孤儿 key / 引用已删壁纸或池的 current、active_pool、order_cursor、bag_remaining 清理与回退 |
| 切换中间态 | 自动轮换无壁纸窗口（原子交换，DR-33） |
| AllSame 换图一致性 | N 次串行 swap 期间各屏短暂不一致为预期（不接受视为 bug） |
| 首次运行 | 无 playback/池 → 默认全部池 + 不自动轮换 |

---

## 18. 决策记录（DR）

| # | 决策 | 状态 |
|---|---|---|
| DR-1 | 调度器 = 换壁纸动作的唯一决策者（WHEN/WHICH/HOW），渲染下放 set_wallpaper | 已定 |
| DR-2 | 编排重构为 PerMonitor/AllSame/Span → 调度单元；同步/去重被吸收 | 已定 |
| DR-3 | 池即有序播放列表（自定义顺序载体），顺序算法追随它 | 已定 |
| DR-4 | 三种算法全做：顺序循环/洗牌袋/纯随机 | 已定 |
| DR-5 | 游标 id 锚定（非 index），重排后从当前 id 继续 | 已定 |
| DR-6 | 关轮换：全局 `rotation.enabled` + 每单元 `enabled` | 已定 |
| DR-7 | 触发系统可组合；本期五源：定时/开机/唤醒/手动/暂停位图变化 | 已定 |
| DR-8 | 定时用 deadline（sleep_until），无漂移 | 已定 |
| DR-9 | 轮换跟随暂停；恢复后重计不补时；手动不受暂停限制 | 已定 |
| DR-10 | 手动设池外壁纸 = 临时覆盖，下次触发换回池内 | 已定 |
| DR-11 | 删除一致性：删壁纸同步清池/游标/袋/current；当前被删换下一张 | 已定 |
| DR-12 | 开机只做一次解析一次 apply；恢复当前为无条件基础行为 | 已定 |
| DR-13 | 唤醒重解析 + 取消未完成 sleep，杜绝双触发 | 已定 |
| DR-14 | 池过滤后 <2 张 → 单元空闲 | 已定 |
| DR-15 | playback.toml 独立于 config，不参与热重载 | 已定 |
| DR-16 | interval_minutes 下限 clamp = 1 分钟（60s） | 已定 |
| DR-17 | 串行化 guard 放 set_wallpaper 命令入口，手动+自动统一排队 | 已定 |
| DR-18 | 类型参与规则三层；Web 默认不参与轮换，手动/恢复豁免 | 已定 |
| DR-19 | 坏壁纸 skip-broken：保持旧壁纸，游标推进跳过 | 已定 |
| DR-20 | 暂停态下完成的轮换，新壁纸跟随暂停 | 已定 |
| DR-21 | 开机恢复壁纸已删：轮换开且池内有有效壁纸 → 推进填充避免空屏（不受 on_boot 门控）；否则清理留空 | 已定 |
| DR-22 | 编排切换迁移规则（合并到 all / 分发到各屏，游标袋重建） | 已定 |
| DR-23 | 手动设池内壁纸 → 游标定位到该 id 之后；池外 → 游标不动 | 已定 |
| DR-24 | 池成员去重 | 已定 |
| DR-25 | 轮换新壁纸继承该屏当前缩放模式（per-display 记忆） | 已定 |
| DR-26 | apply 完成后应用全局 speed（新渲染器不继承 speed 的缺口） | 已定 |
| DR-27 | 音量经共享 VolumeControl 自动继承，无需特殊处理 | 已定 |
| DR-28 | 托盘新增"下一张壁纸"菜单项 | 已定 |
| DR-29 | playback 写频防抖（复用 maybe_save_config 模式） | 已定 |
| DR-30 | 新增 `wallpaper-rotated` 事件；复用 `wallpaper-state-changed` | 已定 |
| DR-31 | 单实例保护已存在（`ensure_single_instance`，CreateMutexW，main.rs），天然防双调度器竞争 | 已确认 |
| DR-32 | 调度器日志（tracing）+ 损坏文件回退 + 任务 panic 自愈 | 已定 |
| DR-33 | 切换中间态：原子交换（方案 C）——旧壁纸保持显示作占位，新壁纸嵌入 WorkerW 底部（HWND_BOTTOM）并锁外等待首帧就绪后才 terminate 旧，杜绝无壁纸窗口；无需暂停、不离开槽位、回滚零代价。详见 §8.4 | 已定 |
| DR-34 | 启动对账：孤儿 key、失效引用（current/active_pool/order_cursor/bag_remaining）清理回退（显示器 id 会变、三文件非原子） | 已定 |
| DR-35 | 隐式"全部"池 = 全集，随库变化，不随池成员 | 已定 |
| DR-36 | 手动下一张缺省作用于主显示器单元 | 已定 |
| DR-37 | 首次运行默认：全部池 + rotation.enabled=false + 30min + shuffle_bag | 已定 |
| DR-38 | 暂停状态为事件唤醒源：pause_reasons 清空（resume_all_fast 后）经共用通道唤醒调度器重算 deadline，不补时 | 已定 |
| DR-39 | AllSame 换图窗口期各屏瞬时 新/旧 混杂为已知接受项（顺序逐屏原子交换） | 已定 |
| DR-40 | 原子交换双窗安全：以 active_wallpapers map 为唯一权威，pending 新窗对布局逻辑不可见，不屏蔽事件；桌面重建丢弃 in-flight swap + commit 前父窗口校验 | 已定 |

---

## 19. 本期不做（二期）

- 作息 / 锁屏 / 电源切换池等会切换激活池的触发源。
- 加权随机采样。
- 轮换倒计时 / 进度 UI。
- 类型感知的间隔建议（池内动态壁纸占比提示）——可选优化，非必需。
