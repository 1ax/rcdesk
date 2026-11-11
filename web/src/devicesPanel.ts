/**
 * The owner's device list and the "+ Добавить" panel (add by MAC / add a
 * router-waker). A feature factory split out of `app.ts`'s `mount()`: it finds
 * its own DOM in `root`, keeps its state in the closure and talks to the rest
 * of `mount()` only through `DevicesPanelDeps` and the returned `DevicesPanel`.
 */
import type { DeviceEntry } from "./generated/DeviceEntry";
import type { WakeNetwork } from "./generated/WakeNetwork";
import type { WakeState } from "./wake";
import type { SignalingClient } from "./signaling";
import { clearAuthRequired } from "./hostTrust";
import {
  canConnect,
  deviceDot,
  deviceLabel,
  deviceStatusLabel,
  sortDevices,
  splitDevices,
} from "./myDevices";
import { mikrotikScript, openwrtScript } from "./routerScripts";
import { WAKE_TIMEOUT_MS, startWake, wakeStatusFor } from "./wake";
import { addDeviceErrorLabel, findNetwork, networkLabel, networkValue } from "./wakeDevices";

export interface DevicesPanelDeps {
  /** The signaling connection (list/rename/forget/wake/add messages go out through it). */
  signaling: SignalingClient;
  /** The `mount()` root; the panel looks its own elements up here. */
  root: Element;
  /** `#device-list`: shared with `mount()` (`beginSession` disables its connect buttons). */
  deviceListEl: HTMLUListElement;
  /** Wake-on-LAN bookkeeping, shared with the `devices`/`wake_sent`/`error` handlers in `mount()`. */
  wakeState: WakeState;
  /** True once the signaling socket closed (every connect/wake affordance is disabled). */
  isConnectionLost: () => boolean;
  /** True while a connect request awaits its `joined` (row connect buttons are disabled). */
  isConnectPending: () => boolean;
  /** "Подключиться" on a row: `mount()` runs `video.play()` and `beginSession` synchronously. */
  onConnect: (entry: DeviceEntry) => void;
  /** The owner's `Storage` (module-level in `app.ts`), `null` when unavailable. */
  ownerStorage: () => Storage | null;
}

export interface DevicesPanel {
  /** Replaces the device list (from `authenticated`/`devices`/reset); does not redraw. */
  setDevices: (list: DeviceEntry[]) => void;
  /** Rebuilds `#device-list` and shows/hides `#devices`. */
  render: () => void;
  /** A `devices` push answers `add_manual_device`: closes the manual form if it was sending. */
  closeSentManualForm: () => void;
  /** `wake_networks` answer: fills the form while it is loading. */
  onWakeNetworks: (networks: WakeNetwork[]) => void;
  /** `router_waker_added` answer: shows the one-time token panel if a router form was sending. */
  onRouterWakerAdded: (token: string) => void;
  /** Server `error` while a form is sending: shows it under the form; true when consumed. */
  onAddError: (message: string) => boolean;
}

type AddKind = "manual" | "router";

export function createDevicesPanel(deps: DevicesPanelDeps): DevicesPanel {
  const { signaling, root, deviceListEl, wakeState, ownerStorage } = deps;
  const devicesEl = root.querySelector<HTMLDivElement>("#devices")!;
  const addManualBtn = root.querySelector<HTMLButtonElement>("#add-manual-btn")!;
  const addRouterBtn = root.querySelector<HTMLButtonElement>("#add-router-btn")!;
  const wakeAddButtonsEl = root.querySelector<HTMLDetailsElement>("#wake-add-buttons")!;
  const wakersEl = root.querySelector<HTMLDivElement>("#wakers")!;
  const wakerListEl = root.querySelector<HTMLUListElement>("#waker-list")!;
  const wakeAddPanelEl = root.querySelector<HTMLDivElement>("#wake-add-panel")!;

  // The owner's device list (slice 3.1e), from `Authenticated`/`Devices` --
  // drives `renderDeviceList`. Empty until the first of either arrives.
  let devices: DeviceEntry[] = [];
  // The device currently being renamed inline (its row shows a text input
  // instead of its label), or `null` when no row is in that state. Only one
  // row at a time.
  let renamingDeviceId: string | null = null;
  // The device whose "Удалить" button currently reads "Точно?", awaiting a
  // second click to confirm (see the document-level `click` listener below,
  // which resets this when the user clicks anything else).
  let confirmingForgetId: string | null = null;
  // The device whose "⋯" menu is open (the list is rebuilt on every `devices`
  // push, so this state lives here, not in the DOM).
  let menuOpenId: string | null = null;
  // D84/D85: "add device by MAC" / "add router-waker" forms. `addKind` is the
  // form being opened/shown; `addStage` walks networks-requested -> form ->
  // sending. `routerPanel` holds the one-time token only until "Готово".
  let addKind: AddKind | null = null;
  let addStage: "idle" | "loading" | "form" | "sending" = "idle";
  let addNetworks: WakeNetwork[] = [];
  let addError = "";
  let routerPanel: { token: string; name: string; script: "mikrotik" | "openwrt" } | null = null;
  let addDraft = { name: "", mac: "", network: "" };
  let wakeTimer: ReturnType<typeof setInterval> | undefined;

  /** Rebuilds `#device-list` from `devices` (see `sortDevices`/`deviceLabel`/
   * `deviceStatusLabel`/`canConnect` in `myDevices.ts`) and shows/hides
   * `#devices` -- only worth showing once the owner has at least one linked
   * device. Rebuilt from scratch on every call, the same approach as
   * `renderDisplayPicker` above; the list is small and this runs at most a
   * few times a minute (see `startDeviceListLoop`). */
  function renderDeviceList(): void {
    const sorted = sortDevices(devices);
    const { computers, wakers } = splitDevices(sorted);
    devicesEl.hidden = sorted.length === 0;
    wakersEl.hidden = wakers.length === 0;
    // D34: a pushed `devices` can arrive mid-rename -- keep what was typed.
    const prevInput = devicesEl.querySelector<HTMLInputElement>(".device-rename-input");
    const kept = prevInput
      ? {
          value: prevInput.value,
          focused: document.activeElement === prevInput,
          start: prevInput.selectionStart,
          end: prevInput.selectionEnd,
        }
      : null;
    deviceListEl.innerHTML = "";
    wakerListEl.innerHTML = "";
    const nowSecs = Math.floor(Date.now() / 1000);

    for (const entry of [...computers, ...wakers]) {
      const listEl = entry.kind === "router" ? wakerListEl : deviceListEl;
      const li = document.createElement("li");
      li.className = "device-item device-row";
      li.dataset.deviceId = entry.device_id;

      if (renamingDeviceId === entry.device_id) {
        const input = document.createElement("input");
        input.className = "device-rename-input";
        input.value = kept?.value ?? deviceLabel(entry);
        input.addEventListener("keydown", (event) => {
          if (event.key === "Enter") {
            commitRename(entry.device_id, input.value);
          } else if (event.key === "Escape") {
            renamingDeviceId = null;
            renderDeviceList();
          }
        });
        const saveBtn = document.createElement("button");
        saveBtn.className = "btn-secondary device-rename-save";
        saveBtn.textContent = "Сохранить";
        saveBtn.addEventListener("click", () => commitRename(entry.device_id, input.value));
        li.appendChild(input);
        li.appendChild(saveBtn);
        listEl.appendChild(li);
        if (kept?.focused) {
          input.focus();
          input.setSelectionRange(kept.start, kept.end);
        }
        continue;
      }

      const dot = document.createElement("span");
      dot.className = "device-dot";
      dot.dataset.state = deviceDot(entry);
      dot.setAttribute("aria-hidden", "true");

      const info = document.createElement("div");
      info.className = "device-info";
      const nameEl = document.createElement("span");
      nameEl.className = "device-name";
      nameEl.textContent = deviceLabel(entry);
      const statusEl = document.createElement("span");
      statusEl.className = "device-status";
      const wake = wakeStatusFor(wakeState, entry, Date.now());
      statusEl.textContent = wake.status ?? deviceStatusLabel(entry, nowSecs);
      info.appendChild(nameEl);
      info.appendChild(statusEl);

      const connectRowBtn = document.createElement("button");
      connectRowBtn.className = "btn device-connect";
      connectRowBtn.textContent = "Подключиться";
      connectRowBtn.disabled = deps.isConnectionLost() || deps.isConnectPending() || !canConnect(entry);
      connectRowBtn.addEventListener("click", () => {
        deps.onConnect(entry);
      });

      const wakeBtn = document.createElement("button");
      wakeBtn.className = "btn device-wake";
      wakeBtn.textContent = wake.view.buttonLabel;
      wakeBtn.disabled = deps.isConnectionLost() || wake.view.buttonDisabled;
      wakeBtn.addEventListener("click", () => {
        startWake(wakeState, entry.device_id, Date.now());
        signaling.send({ type: "wake_device", device_id: entry.device_id });
        ensureWakeTicker();
        renderDeviceList();
      });

      const renameBtn = document.createElement("button");
      renameBtn.className = "menu-item device-rename";
      renameBtn.textContent = "Переименовать";
      renameBtn.addEventListener("click", () => {
        renamingDeviceId = entry.device_id;
        menuOpenId = null;
        renderDeviceList();
      });

      const forgetBtn = document.createElement("button");
      forgetBtn.className = "menu-item device-forget";
      forgetBtn.dataset.deviceId = entry.device_id;
      forgetBtn.textContent = confirmingForgetId === entry.device_id ? "Точно?" : "Удалить";
      forgetBtn.addEventListener("click", () => {
        if (confirmingForgetId === entry.device_id) {
          signaling.send({ type: "forget_device", device_id: entry.device_id });
          // Slice E2: drop the remembered "host requires a password" flag.
          const forgetStorage = ownerStorage();
          if (forgetStorage !== null && entry.kind === "host") {
            clearAuthRequired(forgetStorage, entry.device_id);
          }
          confirmingForgetId = null;
          menuOpenId = null;
        } else {
          confirmingForgetId = entry.device_id;
        }
        renderDeviceList();
      });

      const menu = document.createElement("div");
      menu.className = "device-menu";
      menu.dataset.deviceId = entry.device_id;
      const menuOpen = menuOpenId === entry.device_id;
      const menuBtn = document.createElement("button");
      menuBtn.className = "device-menu-btn";
      menuBtn.textContent = "⋯";
      menuBtn.setAttribute("aria-label", "Действия");
      menuBtn.setAttribute("aria-haspopup", "menu");
      menuBtn.setAttribute("aria-expanded", String(menuOpen));
      menuBtn.addEventListener("click", () => {
        menuOpenId = menuOpen ? null : entry.device_id;
        renderDeviceList();
      });
      menu.appendChild(menuBtn);
      if (menuOpen) {
        const pop = document.createElement("div");
        pop.className = "device-menu-pop";
        pop.appendChild(renameBtn);
        pop.appendChild(forgetBtn);
        menu.appendChild(pop);
      }

      li.appendChild(dot);
      li.appendChild(info);
      if (entry.kind === "host") {
        li.appendChild(wake.view.showButton ? wakeBtn : connectRowBtn);
      } else if (entry.kind === "manual" && wake.view.showButton) {
        li.appendChild(wakeBtn);
      }
      li.appendChild(menu);
      listEl.appendChild(li);
    }
  }

  /** Renders the add-device form / router-script panel into `#wake-add-panel`
   * from the closure state above. Called only on state changes (not on every
   * `devices` push) so typed text survives list refreshes. */
  function renderAddPanel(): void {
    wakeAddPanelEl.innerHTML = "";
    const idle = addStage === "idle" && routerPanel === null;
    wakeAddButtonsEl.hidden = !idle;
    wakeAddPanelEl.hidden = idle;
    if (idle) return;

    if (routerPanel !== null) {
      const panel = routerPanel;
      const note = document.createElement("p");
      note.className = "hint";
      note.textContent = "Токен показывается один раз — скопируйте скрипт сейчас";
      const tabs = document.createElement("div");
      tabs.className = "wake-add-row";
      const pre = document.createElement("pre");
      pre.className = "wake-script";
      const origin = location.origin;
      const scriptText = (): string =>
        panel.script === "mikrotik"
          ? mikrotikScript(origin, panel.token, panel.name)
          : openwrtScript(origin, panel.token, panel.name);
      try {
        pre.textContent = scriptText();
      } catch {
        // W-15: bad token format -- show a short message instead of crashing.
        pre.textContent = "Не удалось собрать скрипт: неверный токен";
      }
      for (const [id, label] of [
        ["mikrotik", "MikroTik"],
        ["openwrt", "OpenWrt"],
      ] as const) {
        const b = document.createElement("button");
        b.className = panel.script === id ? "btn btn-secondary wake-tab active" : "btn btn-secondary wake-tab";
        b.textContent = label;
        b.addEventListener("click", () => {
          panel.script = id;
          renderAddPanel();
        });
        tabs.appendChild(b);
      }
      const copyStatus = document.createElement("p");
      copyStatus.className = "hint";
      const copyBtn = document.createElement("button");
      copyBtn.className = "btn btn-secondary";
      copyBtn.textContent = "Копировать";
      copyBtn.addEventListener("click", () => {
        const selectAll = (): void => {
          const range = document.createRange();
          range.selectNodeContents(pre);
          const sel = window.getSelection();
          sel?.removeAllRanges();
          sel?.addRange(range);
        };
        navigator.clipboard.writeText(scriptText()).then(
          () => {
            copyStatus.textContent = "Скопировано";
          },
          () => {
            selectAll();
            copyStatus.textContent = "Не удалось скопировать — текст выделен, скопируйте вручную";
          },
        );
      });
      const doneBtn = document.createElement("button");
      doneBtn.className = "btn";
      doneBtn.textContent = "Готово";
      doneBtn.addEventListener("click", () => {
        routerPanel = null;
        renderAddPanel();
      });
      const row = document.createElement("div");
      row.className = "wake-add-row";
      row.appendChild(copyBtn);
      row.appendChild(doneBtn);
      wakeAddPanelEl.append(note, tabs);
      if (panel.script === "mikrotik") {
        // D85a: RouterOS 7.17+ often ships in device-mode home, where the
        // scheduler and /tool fetch are forbidden and the import fails.
        const modeNote = document.createElement("p");
        modeNote.className = "hint";
        modeNote.textContent =
          "Нужен режим устройства advanced: /system device-mode print должен показать " +
          "scheduler: yes и fetch: yes. Иначе — /system device-mode update mode=advanced " +
          "и в течение 5 минут коротко нажать кнопку на роутере.";
        wakeAddPanelEl.appendChild(modeNote);
      }
      wakeAddPanelEl.append(pre, row, copyStatus);
      return;
    }

    const closeBtn = (label: string): HTMLButtonElement => {
      const b = document.createElement("button");
      b.className = "btn btn-secondary";
      b.textContent = label;
      b.addEventListener("click", () => {
        addKind = null;
        addStage = "idle";
        addError = "";
        renderAddPanel();
      });
      return b;
    };

    if (addStage === "loading") {
      const p = document.createElement("p");
      p.className = "hint";
      p.textContent = "Загрузка…";
      wakeAddPanelEl.appendChild(p);
      return;
    }

    if (addNetworks.length === 0) {
      const p = document.createElement("p");
      p.className = "hint";
      p.textContent = "Нет известных сетей: нужен компьютер с rcdesk 0.3.4+ в этой сети";
      wakeAddPanelEl.append(p, closeBtn("Закрыть"));
      return;
    }

    const sending = addStage === "sending";
    const select = document.createElement("select");
    select.className = "wake-add-field";
    for (const n of addNetworks) {
      const o = document.createElement("option");
      o.value = networkValue(n);
      o.textContent = networkLabel(n);
      select.appendChild(o);
    }
    select.value = addDraft.network || networkValue(addNetworks[0]!);
    const nameInput = document.createElement("input");
    nameInput.className = "wake-add-field";
    nameInput.maxLength = 64;
    nameInput.placeholder = "Имя";
    nameInput.setAttribute("aria-label", "Имя");
    nameInput.value = addDraft.name;
    const macInput = document.createElement("input");
    macInput.className = "wake-add-field";
    macInput.placeholder = "aa:bb:cc:dd:ee:ff";
    macInput.setAttribute("aria-label", "MAC");
    macInput.autocomplete = "off";
    macInput.value = addDraft.mac;
    const keep = (): void => {
      addDraft = { name: nameInput.value, mac: macInput.value, network: select.value };
    };
    for (const el of [select, nameInput, macInput]) {
      el.disabled = sending;
      el.addEventListener("input", keep);
      el.addEventListener("change", keep);
    }
    const errEl = document.createElement("p");
    errEl.className = "hint wake-add-error";
    errEl.textContent = addError;
    const addBtn = document.createElement("button");
    addBtn.className = "btn";
    addBtn.textContent = "Добавить";
    addBtn.disabled = sending || deps.isConnectionLost();
    addBtn.addEventListener("click", () => {
      keep();
      const net = findNetwork(addNetworks, select.value);
      if (net === undefined) return;
      const name = nameInput.value.trim();
      addError = "";
      addStage = "sending";
      if (addKind === "router") {
        routerNameForPanel = name;
        signaling.send({ type: "add_router_waker", name, network: net.network, prefix: net.prefix });
      } else {
        signaling.send({
          type: "add_manual_device",
          name,
          mac: macInput.value.trim(),
          network: net.network,
          prefix: net.prefix,
        });
      }
      renderAddPanel();
    });
    const row = document.createElement("div");
    row.className = "wake-add-row";
    row.append(addBtn, closeBtn("Отмена"));
    wakeAddPanelEl.append(select, nameInput);
    if (addKind === "manual") wakeAddPanelEl.appendChild(macInput);
    wakeAddPanelEl.append(errEl, row);
  }

  let routerNameForPanel = "";

  /** Click on one of the "add" buttons: ask for the known networks first. */
  function openAddForm(kind: AddKind): void {
    addKind = kind;
    addStage = "loading";
    addError = "";
    addDraft = { name: "", mac: "", network: "" };
    signaling.send({ type: "list_wake_networks" });
    renderAddPanel();
  }
  addManualBtn.addEventListener("click", () => {
    wakeAddButtonsEl.open = false;
    openAddForm("manual");
  });
  addRouterBtn.addEventListener("click", () => {
    wakeAddButtonsEl.open = false;
    openAddForm("router");
  });

  /** Starts the 1 s redraw ticker for the "Включаю… м:сс" counter (one timer
   * at most); it stops itself once no wake is within its timeout. */
  function ensureWakeTicker(): void {
    if (wakeTimer !== undefined) return;
    wakeTimer = setInterval(() => {
      const now = Date.now();
      const active = [...wakeState.waiting.values()].some(
        (w) => now - w.startedMs < WAKE_TIMEOUT_MS,
      );
      if (!active && wakeTimer !== undefined) {
        clearInterval(wakeTimer);
        wakeTimer = undefined;
        // The wake window closed: the row's buttons change, rebuild once.
        renderDeviceList();
        return;
      }
      updateWakeRows();
    }, 1000);
  }

  /** W-6: the 1 s tick patches only the status text and the wake button of
   * the rows in place (by `data-device-id`), so clicks and focus survive. A
   * structural change (the wake button appears/disappears) falls back to a
   * full `renderDeviceList()`. */
  function updateWakeRows(): void {
    const now = Date.now();
    const nowSecs = Math.floor(now / 1000);
    for (const li of deviceListEl.querySelectorAll<HTMLLIElement>("li[data-device-id]")) {
      const id = li.dataset.deviceId;
      const entry = devices.find((d) => d.device_id === id);
      if (entry === undefined) {
        renderDeviceList();
        return;
      }
      const statusEl = li.querySelector<HTMLElement>(".device-status");
      if (statusEl === null) continue; // rename row
      const wake = wakeStatusFor(wakeState, entry, now);
      const wakeBtn = li.querySelector<HTMLButtonElement>(".device-wake");
      const wantsWake = (entry.kind === "host" || entry.kind === "manual") && wake.view.showButton;
      if ((wakeBtn !== null) !== wantsWake) {
        renderDeviceList();
        return;
      }
      statusEl.textContent = wake.status ?? deviceStatusLabel(entry, nowSecs);
      if (wakeBtn !== null) {
        wakeBtn.textContent = wake.view.buttonLabel;
        wakeBtn.disabled = deps.isConnectionLost() || wake.view.buttonDisabled;
      }
    }
  }

  /** Sends the renamed alias (or `null` to clear it, when blank after
   * trimming) and leaves rename mode. */
  function commitRename(deviceId: string, value: string): void {
    const alias = value.trim();
    signaling.send({
      type: "rename_device",
      device_id: deviceId,
      alias: alias === "" ? null : alias,
    });
    renamingDeviceId = null;
    renderDeviceList();
  }

  // Resets a pending "Точно?" delete confirmation when the user clicks
  // anything other than that same button (see `forgetBtn` above) -- a click
  // on the confirming button itself is handled, and re-renders, before this
  // listener runs (document is last in the bubbling chain), and `closest`
  // matches the (possibly now-detached) target element itself first, so
  // that click is never mistaken for "clicked elsewhere".
  // The same listener closes the open "⋯" row menu (a click outside that
  // device's `.device-menu`) and the "+ Добавить" menu (a click outside it).
  document.addEventListener("click", (event) => {
    const target = event.target as Element | null;
    if (wakeAddButtonsEl.open && !target?.closest("#wake-add-buttons")) {
      wakeAddButtonsEl.open = false;
    }
    let changed = false;
    if (
      menuOpenId !== null &&
      !target?.closest(`.device-menu[data-device-id="${CSS.escape(menuOpenId)}"]`)
    ) {
      menuOpenId = null;
      changed = true;
    }
    if (
      confirmingForgetId !== null &&
      !target?.closest(`.device-forget[data-device-id="${CSS.escape(confirmingForgetId)}"]`)
    ) {
      confirmingForgetId = null;
      changed = true;
    }
    if (changed) renderDeviceList();
  });

  // Esc closes the open row menu and the "+ Добавить" menu.
  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    wakeAddButtonsEl.open = false;
    if (menuOpenId !== null) {
      menuOpenId = null;
      renderDeviceList();
    }
  });

  return {
    setDevices(list) {
      devices = list;
    },
    render: renderDeviceList,
    closeSentManualForm() {
      if (addStage === "sending" && addKind === "manual") {
        addKind = null;
        addStage = "idle";
        renderAddPanel();
      }
    },
    onWakeNetworks(networks) {
      if (addStage !== "loading") return;
      addNetworks = networks;
      addStage = "form";
      renderAddPanel();
    },
    onRouterWakerAdded(token) {
      if (addStage !== "sending" || addKind !== "router") return;
      addKind = null;
      addStage = "idle";
      routerPanel = { token, name: routerNameForPanel, script: "mikrotik" };
      renderAddPanel();
    },
    onAddError(message) {
      if (addStage === "sending") {
        const label = addDeviceErrorLabel(message);
        if (label !== null) {
          addStage = "form";
          addError = label;
          renderAddPanel();
          return true;
        }
      }
      return false;
    },
  };
}
