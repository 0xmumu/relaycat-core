// xterm's textarea is an ephemeral IME/clipboard helper, not an editor buffer.
// Blur, paste, Enter and Ctrl-C may clear or replace it wholesale. Only a
// strict append can represent text that the keydown(229) fallback should send.
export function appendedTextareaText(oldValue: string, newValue: string): string {
  return newValue.startsWith(oldValue) ? newValue.substring(oldValue.length) : "";
}

export interface ImeTerminalLike {
  textarea?: HTMLTextAreaElement;
  input(data: string, wasUserInput?: boolean): void;
  attachCustomKeyEventHandler(customKeyEventHandler: (event: KeyboardEvent) => boolean): void;
  onData(listener: (data: string) => void): unknown;
}

export function attachTerminalImeHandler(
  term: ImeTerminalLike,
  onInput: (data: string) => void,
): void {
  let pending229Baseline: string | undefined;
  let imeComposing = false;
  let pending229FlushTimer: ReturnType<typeof setTimeout> | undefined;
  let suppressNext229AfterComposition = false;
  const modifierCodes = new Set([
    "ShiftLeft",
    "ShiftRight",
    "ControlLeft",
    "ControlRight",
    "AltLeft",
    "AltRight",
    "MetaLeft",
    "MetaRight",
  ]);
  const modifierKeysDown = new Set<string>();
  type EarlyImeInput = {
    data: string;
    consumed: boolean;
    awaitingKeydown: boolean;
  };
  const earlyImeInputs: EarlyImeInput[] = [];
  let lastOnData:
    | { data: string; textareaLength: number; at: number }
    | undefined;
  const trackModifier = (ev: KeyboardEvent, pressed: boolean) => {
    const keyId = ev.code || ev.key;
    if (!modifierCodes.has(keyId)) return;
    if (pressed) modifierKeysDown.add(keyId);
    else modifierKeysDown.delete(keyId);
  };
  term.textarea?.addEventListener("keydown", (ev) => trackModifier(ev, true), true);
  term.textarea?.addEventListener("keyup", (ev) => trackModifier(ev, false), true);
  const flushPending229 = () => {
    if (pending229Baseline === undefined || imeComposing) return;
    const value = term.textarea?.value ?? "";
    const data = appendedTextareaText(pending229Baseline, value);
    if (!data) return;
    pending229Baseline = undefined;
    term.input(data, true);
  };
  const schedulePending229Flush = () => {
    if (pending229FlushTimer !== undefined) return;
    pending229FlushTimer = setTimeout(() => {
      pending229FlushTimer = undefined;
      flushPending229();
    }, 0);
  };
  const queueEarlyImeInput = (data: string) => {
    const textareaLength = term.textarea?.value.length ?? -1;
    const now = performance.now();
    const alreadySent =
      lastOnData !== undefined &&
      lastOnData.data === data &&
      lastOnData.textareaLength === textareaLength &&
      now - lastOnData.at <= 50;
    const candidate: EarlyImeInput = {
      data,
      consumed: alreadySent,
      awaitingKeydown: true,
    };
    earlyImeInputs.push(candidate);
    setTimeout(() => {
      if (!candidate.consumed) {
        candidate.consumed = true;
        term.input(data, true);
      }
    }, 16);
    setTimeout(() => {
      const index = earlyImeInputs.indexOf(candidate);
      if (index >= 0) earlyImeInputs.splice(index, 1);
    }, 500);
  };
  term.attachCustomKeyEventHandler((ev) => {
    if (ev.type === "keydown" && ev.keyCode === 229) {
      if (imeComposing || ev.isComposing) {
        pending229Baseline = undefined;
      } else if (suppressNext229AfterComposition) {
        suppressNext229AfterComposition = false;
        pending229Baseline = undefined;
      } else {
        const earlyInput = earlyImeInputs.find((candidate) => candidate.awaitingKeydown);
        if (earlyInput) {
          earlyInput.awaitingKeydown = false;
        } else if (pending229Baseline === undefined) {
          pending229Baseline = term.textarea?.value ?? "";
          queueMicrotask(schedulePending229Flush);
        }
      }
      return false;
    }
    if (ev.type === "keyup") {
      schedulePending229Flush();
    }
    return true;
  });
  // On blur xterm resets the hidden textarea, and modifier keyups / pending
  // IME state may never be delivered while focus is elsewhere.
  term.textarea?.addEventListener("blur", () => {
    pending229Baseline = undefined;
    imeComposing = false;
    suppressNext229AfterComposition = false;
    if (pending229FlushTimer !== undefined) {
      clearTimeout(pending229FlushTimer);
      pending229FlushTimer = undefined;
    }
    for (const candidate of earlyImeInputs) candidate.consumed = true;
    earlyImeInputs.length = 0;
    modifierKeysDown.clear();
    lastOnData = undefined;
  });
  term.textarea?.addEventListener("compositionstart", () => {
    imeComposing = true;
    pending229Baseline = undefined;
    suppressNext229AfterComposition = false;
    if (pending229FlushTimer !== undefined) {
      clearTimeout(pending229FlushTimer);
      pending229FlushTimer = undefined;
    }
  });
  term.textarea?.addEventListener("compositionend", () => {
    imeComposing = false;
    pending229Baseline = undefined;
    suppressNext229AfterComposition = true;
    if (pending229FlushTimer !== undefined) {
      clearTimeout(pending229FlushTimer);
      pending229FlushTimer = undefined;
    }
    setTimeout(() => {
      suppressNext229AfterComposition = false;
    }, 100);
  });
  term.textarea?.addEventListener("input", (ev) => {
    const input = ev as InputEvent;
    if (
      !imeComposing &&
      input.inputType === "insertText" &&
      input.data &&
      !input.defaultPrevented &&
      modifierKeysDown.size > 0 &&
      pending229Baseline === undefined
    ) {
      queueEarlyImeInput(input.data);
    } else if (pending229Baseline !== undefined && !imeComposing) {
      schedulePending229Flush();
    }
  });

  term.onData((data) => {
    lastOnData = {
      data,
      textareaLength: term.textarea?.value.length ?? -1,
      at: performance.now(),
    };
    const earlyInput = earlyImeInputs.find(
      (candidate) => !candidate.consumed && candidate.data === data,
    );
    if (earlyInput) {
      earlyInput.consumed = true;
    }
    if (pending229Baseline !== undefined && !imeComposing) {
      const value = term.textarea?.value ?? "";
      if (data === appendedTextareaText(pending229Baseline, value))
        pending229Baseline = undefined;
    }
    onInput(data);
  });
}

export function truncateUtf8(text: string, maxBytes: number): string {
  const bytes = new TextEncoder().encode(text);
  if (bytes.length <= maxBytes) return text;
  let end = Math.max(0, maxBytes);
  while (end > 0) {
    try { return new TextDecoder("utf-8", { fatal: true }).decode(bytes.slice(0, end)); }
    catch { end -= 1; }
  }
  return "";
}

export function splitToolArgs(raw: string): string[] {
  const args: string[] = [];
  let current = "";
  let inSingle = false;
  let inDouble = false;
  let tokenStarted = false;

  for (let i = 0; i < raw.length; i++) {
    const ch = raw[i];
    if (ch === "\\" && !inSingle && i + 1 < raw.length) {
      const next = raw[i + 1];
      if (inDouble) {
        if (next === '"' || next === "\\") {
          current += next;
          tokenStarted = true;
          i++;
          continue;
        }
      } else {
        current += next;
        tokenStarted = true;
        i++;
        continue;
      }
    }
    if (ch === "'" && !inDouble) {
      inSingle = !inSingle;
      tokenStarted = true;
      continue;
    }
    if (ch === '"' && !inSingle) {
      inDouble = !inDouble;
      tokenStarted = true;
      continue;
    }
    if (!inSingle && !inDouble && /\s/.test(ch)) {
      if (tokenStarted) {
        args.push(current);
        current = "";
        tokenStarted = false;
      }
      continue;
    }
    current += ch;
    tokenStarted = true;
  }

  if (tokenStarted) {
    args.push(current);
  }
  return args;
}
