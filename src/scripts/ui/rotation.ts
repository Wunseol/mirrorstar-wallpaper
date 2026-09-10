import {
  createPool,
  deletePool,
  getDisplays,
  getRotationConfig,
  getUnitStates,
  listPools,
  nextWallpaper,
  setActivePool,
  setRotationEnabled,
  updatePool,
  updateRotationConfig,
} from "../ipc";
import { appState } from "../state";
import type { Arrangement, DisplayInfo, Order, Pool, RotationConfig, UnitState } from "../types";
import { log } from "../utils/logger";
import { showStatus } from "./utils";

// ═══════════════════════════════════════════════════════════════════════════════
// 壁纸轮换调度器前端 UI（Task 10）
//
// 本模块承载三类纯函数（可单测）+ 轮换设置 / 池编辑器 / 单元配置 / 下一张按钮的
// DOM 逻辑。静态控件的变更监听在 main.ts init() 中追加（遵循"事件绑定集中"约定）；
// 本模块内仅在此处动态创建的 DOM（池列表、单元配置）绑定委托/元素级监听器。
// ═══════════════════════════════════════════════════════════════════════════════

// ── 纯函数：采样算法中文标签（10.1）────────────────────────────────────────────

/** Order → 中文标签映射（顺序循环 / 洗牌袋 / 纯随机） */
export const ORDER_LABELS: Readonly<Record<Order, string>> = {
  sequential: "顺序循环",
  shuffle_bag: "洗牌袋",
  pseudo_random: "纯随机",
};

/** 合法的采样算法集合（与 Order 类型保持同步，供运行时窄化校验） */
const ORDERS: readonly Order[] = ["sequential", "shuffle_bag", "pseudo_random"];

/** 判断字符串是否为合法 Order；用于 select.value（string）到 Order 的安全窄化 */
export function isOrder(v: string): v is Order {
  return (ORDERS as readonly string[]).includes(v);
}

/** 取 Order 的中文标签；未知值原样返回（防御性） */
export function orderLabel(order: Order): string {
  return ORDER_LABELS[order] ?? order;
}

// ── 纯函数：池名映射（10.4）────────────────────────────────────────────────────

/**
 * 将壁纸所属池 id 列表映射为池名列表（按池在 pools 中的 id 查找，跳过已删除的池）。
 * 无所属池时返回空数组（卡片无需额外标注）。
 */
export function poolNamesForWallpaper(
  groups: string[] | undefined | null,
  pools: readonly Pool[],
): string[] {
  if (!groups || groups.length === 0) return [];
  const nameById = new Map(pools.map((p) => [p.id, p.name]));
  const names: string[] = [];
  for (const id of groups) {
    const n = nameById.get(id);
    if (n) names.push(n);
  }
  return names;
}

// ── 纯函数：成员有序去重 / 重排（10.2）────────────────────────────────────────

/**
 * 成员 id 去重（保留首次出现顺序，对齐后端 Pool::validate DR-24）。
 */
export function dedupeMembers(ids: string[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const id of ids) {
    if (!seen.has(id)) {
      seen.add(id);
      out.push(id);
    }
  }
  return out;
}

/**
 * 将成员列表中的元素从 `from` 移动到 `to`（原地不动时返回副本）。
 * 索引越界返回原数组副本。用于池成员拖拽排序（顺序即播放顺序）。
 */
export function reorderMembers(ids: string[], from: number, to: number): string[] {
  if (from < 0 || to < 0 || from >= ids.length || to >= ids.length || from === to) {
    return ids.slice();
  }
  const next = ids.slice();
  const [moved] = next.splice(from, 1) as [string];
  next.splice(to, 0, moved);
  return next;
}

// ── 纯函数：单元 key 解析（10.3）────────────────────────────────────────────

/**
 * 依据编排解析调度单元 key ± 标签（对齐 backend validate_unit_key DR-40）：
 * - PerMonitor：每个显示器一个单元（key=显示器 id）
 * - AllSame / Span：唯全局单元 "all"
 */
export function unitKeysForArrangement(
  arrangement: Arrangement,
  displays: readonly DisplayInfo[],
): Array<{ key: string; label: string }> {
  if (arrangement === "per_monitor") {
    if (displays.length === 0) return [];
    return displays.map((d) => ({
      key: d.id,
      label: d.is_primary ? `[主] ${d.name}` : d.name,
    }));
  }
  return [{ key: "all", label: "全部屏幕" }];
}

// ── 模块级状态 ────────────────────────────────────────────────────────────────

let poolList: Pool[] = [];
let currentArrangement: Arrangement = "per_monitor";
let currentDisplays: DisplayInfo[] = [];

/** 供测试 / 其它模块读取当前池缓存 */
export function getPoolList(): Pool[] {
  return poolList;
}

/**
 * 同步池缓存到模块与 appState（供壁纸卡片标注池名 & 单元配置下拉复用），
 * 并派发 `rotation-pools-changed` 事件，由 main.ts 监听触发壁纸列表重新标注。
 */
function setPools(pools: Pool[]): void {
  poolList = pools;
  appState.pools = pools;
  window.dispatchEvent(new CustomEvent("rotation-pools-changed"));
}

// ── 10.1 轮换设置：局部更新（串行化，对齐 config-panel.patchConfig）──────────────

/**
 * 模块级 promise 链：串行化所有 patchRotation 调用。
 * update_rotation_config 后端整体替换 rotation，前端为 getRotationConfig +
 * 浅合并 + updateRotationConfig 的 read-modify-write，并发时可能覆盖，故串行执行。
 */
let rotationChain: Promise<void> = Promise.resolve();

async function doPatchRotation(patch: Partial<RotationConfig>): Promise<void> {
  const c = await getRotationConfig();
  await updateRotationConfig({ ...c, ...patch });
}

export function patchRotation(patch: Partial<RotationConfig>): Promise<void> {
  const run = rotationChain.then(() => doPatchRotation(patch));
  rotationChain = run.catch(() => {});
  return run;
}

// ── 10.3 单元配置：按编排渲染激活池 / 单元开关 ─────────────────────────────────

async function resolveArrangement(): Promise<void> {
  try {
    const c = await getRotationConfig();
    currentArrangement = c.arrangement;
  } catch (e) {
    log.warn("读取轮换配置失败，使用默认编排 per_monitor", e);
    currentArrangement = "per_monitor";
  }
}

async function resolveDisplays(): Promise<void> {
  try {
    currentDisplays = await getDisplays();
  } catch (e) {
    log.warn("获取显示器列表失败，单元配置回退为全局单元", e);
    currentDisplays = [];
  }
}

/**
 * 渲染单元配置面板。按当前编排列出每个调度单元，提供：
 * - 激活池 select（""=全部，DR-35；渲染前回读后端真实状态回填初始值）
 * - 单元轮换开关 checkbox（同上，回填后端真实开关态）
 * 变更即调用 set_active_pool / set_rotation_enabled。
 */
export async function renderUnitConfig(): Promise<void> {
  const container = document.getElementById("unit-config");
  if (!container) return;
  await Promise.all([resolveArrangement(), resolveDisplays()]);
  // 回读后端真实单元配置，避免 UI 默认态（全部 / 关）覆盖已绑定/开启的设置（Task 6.3）。
  const states = await readUnitStates();
  const stateByKey = new Map(
    states.map((s) => [s.key, s] as const),
  );
  const units = unitKeysForArrangement(currentArrangement, currentDisplays);
  container.replaceChildren();

  if (poolList.length === 0) {
    // 无池可绑定时仍显示单元开关，激活池下拉仅"全部"（无池则无激活状态可回填）
    for (const unit of units) {
      container.appendChild(renderUnitItem(unit.key, unit.label, [], stateByKey.get(unit.key)));
    }
    return;
  }
  for (const unit of units) {
    container.appendChild(renderUnitItem(unit.key, unit.label, poolList, stateByKey.get(unit.key)));
  }
}

/** 拉取单元真实状态；失败时提示并回退为不回填（默认态），不静默覆盖后端配置。 */
async function readUnitStates(): Promise<UnitState[]> {
  try {
    return await getUnitStates();
  } catch (e) {
    log.warn("回读单元配置状态失败，单元面板按默认态渲染", e);
    showStatus("回读单元配置失败", "error");
    return [];
  }
}

function renderUnitItem(
  key: string,
  label: string,
  pools: readonly Pool[],
  state: UnitState | undefined,
): HTMLDivElement {
  const item = document.createElement("div");
  item.className = "unit-config-item";
  item.dataset.unitKey = key;

  const title = document.createElement("div");
  title.className = "unit-config-title";
  title.textContent = label;
  item.appendChild(title);

  const poolRow = document.createElement("div");
  poolRow.className = "unit-config-row";
  const poolSelect = document.createElement("select");
  poolSelect.className = "unit-active-pool";
  const allOption = document.createElement("option");
  allOption.value = "";
  allOption.textContent = "全部壁纸池";
  poolSelect.appendChild(allOption);
  for (const p of pools) {
    const opt = document.createElement("option");
    opt.value = p.id;
    opt.textContent = p.name;
    poolSelect.appendChild(opt);
  }
  // 回填后端真实激活池（state 缺省/无匹配 → 空串=全部，DR-35）
  poolSelect.value = state?.active_pool ?? "";
  poolSelect.addEventListener("change", () => {
    // 空串 → null（全部，DR-35）
    const poolId = poolSelect.value === "" ? null : poolSelect.value;
    runGuard(setActivePool(key, poolId), `设置单元 ${label} 激活池失败`);
  });
  const poolLabel = document.createElement("label");
  poolLabel.textContent = "激活池";
  poolLabel.appendChild(poolSelect);
  poolRow.appendChild(poolLabel);
  item.appendChild(poolRow);

  const enabledRow = document.createElement("div");
  enabledRow.className = "unit-config-row";
  const checkbox = document.createElement("input");
  checkbox.type = "checkbox";
  checkbox.className = "unit-rotation-enabled";
  // 回填后端真实开关态（state 缺省 → 关）
  checkbox.checked = state?.enabled ?? false;
  checkbox.addEventListener("change", () => {
    runGuard(setRotationEnabled(key, checkbox.checked), `设置单元 ${label} 轮换开关失败`);
  });
  const cbLabel = document.createElement("label");
  cbLabel.appendChild(checkbox);
  cbLabel.append("单元轮换开关");
  enabledRow.appendChild(cbLabel);
  item.appendChild(enabledRow);

  return item;
}

/** 执行异步 IPC 并统一错误提示（本模块内 UI 回调公用） */
function runGuard(promise: Promise<unknown>, errorMsg: string): void {
  promise.catch((e) => {
    log.error(errorMsg, e);
    showStatus(errorMsg, "error");
  });
}

// ── 10.2 池编辑器：CRUD + 成员拖拽排序 ─────────────────────────────────────────

/**
 * 拉取全部池并渲染池列表。池列表容器使用事件委托处理成员拖拽 / 增删 / 重命名 / 删除。
 * 每次池变更后重新渲染，保证 DOM 与 state 一致。
 */
export async function loadPools(): Promise<void> {
  const container = document.getElementById("pool-list");
  if (!container) return;
  try {
    const pools = await listPools();
    setPools(pools);
    renderPoolList(container, pools);
    // 池变更影响单元配置的激活池下拉与壁纸卡片池名标注
    void renderUnitConfig();
  } catch (e) {
    log.error("加载轮换池失败:", e);
    showStatus("加载轮换池失败", "error");
  }
}

/**
 * 部署/复用池列表的委托监听器（成员移除、拖拽、重命名、删除）。
 * 复用元素引用跟踪（对齐 wallpaper-list 的 delegatedGrid 模式），避免重复注册。
 */
let delegatedPoolList: HTMLElement | null = null;
function attachPoolDelegatedListeners(container: HTMLElement): void {
  if (delegatedPoolList === container) return;
  delegatedPoolList = container;

  container.addEventListener("click", (event: MouseEvent) => {
    const target = event.target as HTMLElement;
    const memberRemove = target.closest<HTMLElement>(".rotation-member-remove");
    if (memberRemove) {
      event.stopPropagation();
      void removePoolMember(memberRemove);
      return;
    }
    const renameBtn = target.closest<HTMLElement>(".rotation-pool-rename");
    if (renameBtn) {
      event.stopPropagation();
      void renamePool(renameBtn);
      return;
    }
    const deleteBtn = target.closest<HTMLElement>(".rotation-pool-delete");
    if (deleteBtn) {
      event.stopPropagation();
      void deletePoolById(deleteBtn);
    }
  });

  container.addEventListener("change", (event: Event) => {
    const target = event.target as HTMLElement;
    const memberAdd = target.closest<HTMLSelectElement>(".rotation-pool-member-add");
    if (memberAdd) {
      void addPoolMember(memberAdd);
    }
  });

  // 成员拖拽（原生 HTML5 drag）：
  // dragstart 记录被拖项索引；dragover 定位目标索引；drop 提交重排。
  container.addEventListener("dragstart", (event: DragEvent) => {
    const li = (event.target as HTMLElement).closest<HTMLElement>(".rotation-member");
    if (!li) return;
    event.dataTransfer?.setData("text/plain", String(li.dataset.index ?? ""));
    li.classList.add("dragging");
  });
  container.addEventListener("dragend", (event: DragEvent) => {
    const li = (event.target as HTMLElement).closest<HTMLElement>(".rotation-member");
    li?.classList.remove("dragging");
  });
  container.addEventListener("dragover", (event: DragEvent) => {
    event.preventDefault(); // 允许 drop
  });
  container.addEventListener("drop", (event: DragEvent) => {
    event.preventDefault();
    const fromStr = event.dataTransfer?.getData("text/plain");
    if (fromStr === undefined) return;
    const from = Number(fromStr);
    const li = (event.target as HTMLElement).closest<HTMLElement>(".rotation-member");
    if (!li) return;
    const to = Number(li.dataset.index ?? "");
    void handleMemberDrop(li, from, to);
  });
}

/** 渲染全部池。全局监听器仅注册一次（delegatedPoolList 去重）。 */
function renderPoolList(container: HTMLElement, pools: readonly Pool[]): void {
  attachPoolDelegatedListeners(container);
  container.replaceChildren();
  for (const pool of pools) {
    container.appendChild(renderPoolCard(pool));
  }
  if (pools.length === 0) {
    const empty = document.createElement("div");
    empty.className = "rotation-pool-empty";
    empty.textContent = "暂无轮换池，输入名称新建";
    container.appendChild(empty);
  }
}

function renderPoolCard(pool: Pool): HTMLDivElement {
  const card = document.createElement("div");
  card.className = "rotation-pool";
  card.dataset.poolId = pool.id;

  const header = document.createElement("div");
  header.className = "rotation-pool-header";
  const name = document.createElement("span");
  name.className = "rotation-pool-name";
  name.textContent = pool.name;
  name.title = pool.name;
  const actions = document.createElement("div");
  actions.className = "rotation-pool-actions";
  actions.innerHTML = `
    <button class="rotation-pool-rename" title="重命名">✎</button>
    <button class="rotation-pool-delete" title="删除">✕</button>
  `;
  header.appendChild(name);
  header.appendChild(actions);
  card.appendChild(header);

  // 成员下拉（添加成员）
  const addRow = document.createElement("div");
  addRow.className = "rotation-pool-add";
  const addSelect = document.createElement("select");
  addSelect.className = "rotation-pool-member-add";
  const placeholder = document.createElement("option");
  placeholder.value = "";
  placeholder.textContent = "+ 添加壁纸到池";
  addSelect.appendChild(placeholder);
  const memberSet = new Set(pool.member_ids);
  for (const wp of appState.allWallpapers) {
    if (memberSet.has(wp.id)) continue;
    const opt = document.createElement("option");
    opt.value = wp.id;
    opt.textContent = extractName(wp.file_path);
    addSelect.appendChild(opt);
  }
  addRow.appendChild(addSelect);
  card.appendChild(addRow);

  // 成员有序列表（拖拽排序）
  const list = document.createElement("ul");
  list.className = "rotation-pool-members";
  const nameById = new Map(appState.allWallpapers.map((w) => [w.id, w.file_path]));
  pool.member_ids.forEach((id, index) => {
    const li = document.createElement("li");
    li.className = "rotation-member";
    li.draggable = true;
    li.dataset.index = String(index);
    const filePath = nameById.get(id);
    const text = document.createElement("span");
    text.className = "rotation-member-name";
    text.textContent = filePath ? extractName(filePath) : "（已删除）";
    text.title = filePath ?? "";
    const remove = document.createElement("button");
    remove.className = "rotation-member-remove";
    remove.textContent = "✕";
    remove.setAttribute("aria-label", "移出池");
    li.appendChild(text);
    li.appendChild(remove);
    list.appendChild(li);
  });
  card.appendChild(list);

  return card;
}

function extractName(filePath: string): string {
  const parts = filePath.split(/[/\\]/);
  return parts[parts.length - 1] || filePath;
}

/** 从当前 DOM 上移除某成员并提交 update_pool */
async function removePoolMember(removeBtn: HTMLElement): Promise<void> {
  const li = removeBtn.closest<HTMLElement>(".rotation-member");
  const card = removeBtn.closest<HTMLElement>(".rotation-pool");
  const pool = poolList.find((p) => p.id === card?.dataset.poolId);
  if (!pool || !li) return;
  const idx = Number(li.dataset.index ?? "");
  if (Number.isNaN(idx) || idx < 0 || idx >= pool.member_ids.length) return;
  const next = pool.member_ids.slice();
  next.splice(idx, 1);
  try {
    await updatePool(pool.id, null, next);
    showStatus("已移出池", "success");
    await loadPools();
  } catch (e) {
    log.error("从池移除成员失败:", e);
    showStatus("从池移除成员失败", "error");
  }
}

/** 添加成员：从下拉选中并提交 update_pool（有序去重由后端负责，前端也预去重） */
async function addPoolMember(select: HTMLSelectElement): Promise<void> {
  const card = select.closest<HTMLElement>(".rotation-pool");
  const pool = poolList.find((p) => p.id === card?.dataset.poolId);
  const id = select.value;
  if (!pool || !id) return;
  const next = dedupeMembers([...pool.member_ids, id]);
  try {
    await updatePool(pool.id, null, next);
    showStatus("已添加成员", "success");
    await loadPools();
  } catch (e) {
    log.error("添加池成员失败:", e);
    showStatus("添加池成员失败", "error");
  }
}

/** 拖拽重排成员：local reorder → 提交有序 member_ids */
async function handleMemberDrop(li: HTMLElement, from: number, to: number): Promise<void> {
  const card = li.closest<HTMLElement>(".rotation-pool");
  const pool = poolList.find((p) => p.id === card?.dataset.poolId);
  if (!pool || Number.isNaN(from) || Number.isNaN(to)) return;
  if (from === to) return;
  const next = reorderMembers(pool.member_ids, from, to);
  try {
    await updatePool(pool.id, null, next);
    showStatus("已更新播放顺序", "success");
    await loadPools();
  } catch (e) {
    log.error("重排池成员失败:", e);
    showStatus("重排池成员失败", "error");
  }
}

/** 重命名池：prompt 输入新名称，非空提交 */
async function renamePool(btn: HTMLElement): Promise<void> {
  const card = btn.closest<HTMLElement>(".rotation-pool");
  const pool = poolList.find((p) => p.id === card?.dataset.poolId);
  if (!pool) return;
  const newName = prompt("请输入新的池名称：", pool.name);
  if (newName === null || newName.trim() === "") return;
  try {
    await updatePool(pool.id, newName.trim(), null);
    showStatus("已重命名", "success");
    await loadPools();
  } catch (e) {
    log.error("重命名池失败:", e);
    showStatus("重命名池失败", "error");
  }
}

/** 删除池：确认后删除 */
async function deletePoolById(btn: HTMLElement): Promise<void> {
  const card = btn.closest<HTMLElement>(".rotation-pool");
  const pool = poolList.find((p) => p.id === card?.dataset.poolId);
  if (!pool) return;
  if (!confirm(`确认删除轮换池"${pool.name}"？其成员壁纸不会被删除。`)) return;
  try {
    await deletePool(pool.id);
    showStatus("轮换池已删除", "success");
    await loadPools();
  } catch (e) {
    log.error("删除轮换池失败:", e);
    showStatus("删除轮换池失败", "error");
  }
}

/** 创建池：从输入框读名称（可为空，后端自动编"池 N"），空成员 */
export async function createPoolFromInput(input: HTMLInputElement): Promise<void> {
  const name = input.value.trim();
  try {
    await createPool(name === "" ? null : name, []);
    input.value = "";
    showStatus(name === "" ? "已新建轮换池" : `已新建池"${name}"`, "success");
    await loadPools();
  } catch (e) {
    log.error("创建轮换池失败:", e);
    showStatus("创建轮换池失败", "error");
  }
}

/** 供 create_pool 行为注入到 main.ts 的事件回调时复用（按钮监听在 main.ts） */
export function setupPoolCreate(): void {
  const input = document.getElementById("pool-name-input") as HTMLInputElement | null;
  const btn = document.getElementById("pool-create-btn");
  if (!input || !btn) return;
  const submit = () => {
    void createPoolFromInput(input);
  };
  btn.addEventListener("click", submit);
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") submit();
  });
}

// ── 10.5 手动"下一张"按钮 ────────────────────────────────────────────────────

/** 绑定"下一张"按钮：调用 next_wallpaper()（缺省主屏单元 DR-36）。 */
export function setupNextWallpaperButton(): void {
  const btn = document.getElementById("next-wallpaper-btn") as HTMLButtonElement | null;
  if (!btn) return;
  btn.addEventListener("click", () => {
    runGuard(nextWallpaper(), "手动切换下一张失败");
  });
}